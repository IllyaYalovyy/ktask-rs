use crate::support::{self, Result, Terminal};
use rusqlite::Connection;
use serde_json::Value;
use std::{fs, os::unix::ffi::OsStrExt, path::Path, process::Command};

const PREVIOUS_V1: &str = include_str!("../fixtures/previous_v1.sql");
const PREVIOUS_DATABASE: &[u8] = include_bytes!("../fixtures/previous_v1.sqlite3");
const BINARY: &str = env!("CARGO_BIN_EXE_ktask-dev");

fn database(root: &Path) -> Result<Connection> {
    let state = root.join("state/ktask-dev");
    fs::create_dir_all(&state)?;
    let db_path = state.join("ktask.sqlite3");
    fs::write(&db_path, PREVIOUS_DATABASE)?;
    let db = Connection::open(db_path)?;
    // The binary-created project lived in the fixture builder's private sandbox.
    db.execute(
        "UPDATE projects SET root_path = ?1",
        [root.as_os_str().as_bytes()],
    )?;
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        PREVIOUS_V1
    );
    Ok(db)
}

fn run(binary: &Path, root: &Path, args: &[&str]) -> Result<std::process::Output> {
    Ok(Command::new(binary)
        .arg("--json")
        .args(args)
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .current_dir(root)
        .output()?)
}

fn retained_data(db: &Connection) -> Result<Vec<Vec<Vec<rusqlite::types::Value>>>> {
    let mut data = Vec::new();
    for table in ["tasks", "task_revisions", "attempts", "events"] {
        let columns = match table {
            "tasks" => {
                "id,project_id,order_key,lifecycle,origin,row_version,created_at_ms,updated_at_ms,closed_at_ms,resolution_reason,resolved_by_kind,resolved_by"
            }
            "task_revisions" => {
                "task_id,revision,lifecycle,kind,title,goal,change_reason,created_by_kind,created_by,attempt_limit,attempt_timeout_ms,created_at_ms,decided_at_ms,decided_by_kind,decided_by"
            }
            "attempts" => {
                "id,project_id,task_id,revision,number,state,execution_token,started_at_ms,finished_at_ms,active_ms,wait_reason,waiting_since_ms,resume_at_ms,outcome,failure_category,base_commit,head_commit,report"
            }
            _ => "*",
        };
        let mut query = db.prepare(&format!("SELECT {columns} FROM {table} ORDER BY rowid"))?;
        data.push(
            query
                .query_map([], |row| {
                    (0..row.as_ref().column_count())
                        .map(|index| row.get(index))
                        .collect()
                })?
                .collect::<rusqlite::Result<_>>()?,
        );
    }
    Ok(data)
}

#[test]
fn previous_v1_opens_in_real_binary_with_history_and_tui_screen() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let db = database(root)?;
    let db_path = root.join("state/ktask-dev/ktask.sqlite3");
    let before = retained_data(&db)?;
    drop(db);
    let result = run(Path::new(BINARY), root, &["show", "1"])?;
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let reply: Value = serde_json::from_slice(&result.stdout)?;
    assert_eq!(reply["ok"], true);
    let db = Connection::open(&db_path)?;
    assert_eq!(before, retained_data(&db)?);
    for table in ["tasks", "task_revisions", "attempts"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| row
                .get::<_, i64>(0))?,
            1,
            "{table}"
        );
    }
    assert_eq!(
        db.query_row("SELECT title FROM task_revisions", [], |row| row
            .get::<_, String>(0))?,
        "Retained task"
    );
    assert_eq!(
        db.query_row("SELECT outcome FROM attempts", [], |row| row
            .get::<_, String>(0))?,
        "Old result"
    );
    assert_eq!(
        db.query_row(
            "SELECT message FROM events WHERE message = 'Retained event'",
            [],
            |row| row.get::<_, String>(0)
        )?,
        "Retained event"
    );
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        include_str!("../../migrations/0001_relational_state.sql")
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'ix_revisions_ready'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    drop(db);

    let mut terminal = Terminal::open_real(BINARY, root, 80, 25)?;
    let screen = terminal.screen("retained current task", |screen| {
        screen.contains("Retained task") && screen.contains("FAILED")
    })?;
    println!("RECONCILED CURRENT SCREEN\n{screen}");
    terminal.send("q")?;
    terminal.finish()?;
    Ok(())
}

#[test]
fn incompatible_v1_is_refused_without_changes_by_real_binary() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let db = database(root)?;
    db.execute("CREATE INDEX unexpected_live_index ON tasks(id)", [])?;
    let before: Vec<(String, String)> = {
        let mut query =
            db.prepare("SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?;
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    let previous_attempt: String =
        db.query_row("SELECT execution_token FROM attempts", [], |row| row.get(0))?;
    drop(db);
    let result = run(Path::new(BINARY), root, &["show", "1"])?;
    assert_eq!(result.status.code(), Some(1));
    assert!(result.stdout.is_empty());
    let reply: Value = serde_json::from_slice(&result.stderr)?;
    assert_eq!(reply["error"]["code"], "incompatible_database");
    let message = reply["error"]["message"].as_str().unwrap();
    assert!(message.contains("database left unchanged"), "{message}");
    assert!(message.contains("previous binary"), "{message}");
    let db = Connection::open(root.join("state/ktask-dev/ktask.sqlite3"))?;
    let after: Vec<(String, String)> = {
        let mut query =
            db.prepare("SELECT name, sql FROM sqlite_schema WHERE sql IS NOT NULL ORDER BY name")?;
        query
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?
    };
    assert_eq!(before, after);
    assert_eq!(
        db.query_row("SELECT execution_token FROM attempts", [], |row| row
            .get::<_, String>(0))?,
        previous_attempt
    );
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        PREVIOUS_V1
    );
    Ok(())
}
