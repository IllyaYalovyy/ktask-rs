use std::path::PathBuf;

use rusqlite::Connection;

use super::{ALL, Migration, apply};
use crate::{scratch::Scratch, storage::Database};

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

fn sandbox() -> std::io::Result<Scratch> {
    Scratch::new(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/tmp/test-scratch"))
}

fn count(connection: &Connection, table: &str) -> i64 {
    connection
        .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn upgrade_preserves_existing_data_and_downgrade_is_rejected() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('Keep me', x'2f776f726b', 1)",
        [],
    )?;
    let future = [
        Migration {
            name: ALL[0].name,
            sql: ALL[0].sql,
        },
        Migration {
            name: "0002_test_upgrade",
            sql: "ALTER TABLE projects ADD COLUMN archived_at_ms INTEGER CHECK (archived_at_ms >= created_at_ms);",
        },
    ];
    apply(db, &future)?;
    apply(db, &future)?;
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(count(db, "schema_migrations"), 2);
    assert_eq!(
        db.query_row("SELECT name FROM projects", [], |r| r.get::<_, String>(0))?,
        "Keep me"
    );
    assert!(
        db.query_row("SELECT archived_at_ms FROM projects", [], |r| r
            .get::<_, Option<i64>>(0))?
            .is_none()
    );
    assert!(
        apply(db, ALL)
            .unwrap_err()
            .to_string()
            .contains("newer schema")
    );
    assert_eq!(count(db, "projects"), 1);
    Ok(())
}

#[test]
fn failed_upgrade_rolls_back_ddl_data_and_migration_ledger() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('Keep me', x'2f776f726b', 1)",
        [],
    )?;
    let broken = [
        Migration {
            name: ALL[0].name,
            sql: ALL[0].sql,
        },
        Migration {
            name: "0002_broken",
            sql: "CREATE TABLE should_not_survive(id INTEGER) STRICT; DELETE FROM projects; INSERT INTO missing_table VALUES (1);",
        },
    ];
    assert!(apply(db, &broken).is_err());
    assert!(db.prepare("SELECT * FROM should_not_survive").is_err());
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(count(db, "schema_migrations"), 1);
    drop(store);
    assert_eq!(
        count(Database::open_in(scratch.path())?.connection(), "projects"),
        1
    );
    Ok(())
}

#[test]
fn modified_missing_and_foreign_migrations_are_not_reset_or_adopted() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "UPDATE schema_migrations SET sql = sql || ' -- changed'",
        [],
    )?;
    assert!(apply(db, ALL).unwrap_err().to_string().contains("differs"));
    db.execute("UPDATE schema_migrations SET sql = ?1", [ALL[0].sql])?;
    db.execute("UPDATE schema_migrations SET version = 2", [])?;
    assert!(apply(db, ALL).unwrap_err().to_string().contains("differs"));
    db.execute("UPDATE schema_migrations SET version = 1", [])?;
    db.execute("DELETE FROM schema_migrations", [])?;
    assert!(apply(db, ALL).is_err());
    assert_eq!(count(db, "projects"), 0);

    let foreign_root = scratch.path().join("foreign");
    std::fs::create_dir(&foreign_root)?;
    let foreign = Connection::open(foreign_root.join("ktask.sqlite3"))?;
    foreign.execute_batch(
        "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES ('keep');",
    )?;
    assert!(Database::open_in(&foreign_root).is_err());
    assert_eq!(count(&foreign, "unrelated"), 1);
    assert_eq!(
        foreign.pragma_query_value(None, "application_id", |r| r.get::<_, i64>(0))?,
        0
    );
    foreign.pragma_update(None, "application_id", 12345)?;
    assert!(Database::open_in(&foreign_root).is_err());
    assert_eq!(count(&foreign, "unrelated"), 1);
    Ok(())
}

#[test]
fn interrupted_first_migration_leaves_an_empty_retryable_database() -> Result {
    let scratch = sandbox()?;
    let mut db = Connection::open(scratch.path().join("ktask.sqlite3"))?;
    db.pragma_update(None, "foreign_keys", true)?;
    let broken = [Migration {
        name: "0001_invalid",
        sql: "CREATE TABLE partial(id INTEGER) STRICT; INVALID SQL;",
    }];
    assert!(apply(&mut db, &broken).is_err());
    assert_eq!(
        db.pragma_query_value(None, "application_id", |r| r.get::<_, i64>(0))?,
        0
    );
    assert!(db.prepare("SELECT * FROM partial").is_err());
    assert!(db.prepare("SELECT * FROM schema_migrations").is_err());
    drop(db);
    let reopened = Database::open_in(scratch.path())?;
    assert_eq!(count(reopened.connection(), "schema_migrations"), 1);
    Ok(())
}

#[test]
fn table_rebuild_keeps_referenced_data_and_invalid_rebuild_rolls_back() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute_batch(
        "BEGIN;
         INSERT INTO projects VALUES (1, 'Keep project', x'2f776f726b', 100);
         INSERT INTO tasks(id, project_id, order_key, lifecycle, origin, created_at_ms, updated_at_ms)
             VALUES (1, 1, '8', 'new', 'human', 100, 100);
         INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_at_ms)
             VALUES (1, 1, 'draft', 'ai', 'Keep task', 'Keep goal', 100);
         INSERT INTO acceptance_criteria VALUES (1, 1, 1, 'Keep check');
         COMMIT;",
    )?;
    let rebuild = Migration {
        name: "0002_test_rebuild",
        sql: "CREATE TABLE projects_new (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL UNIQUE CHECK (length(trim(name)) > 0),
                root_path BLOB NOT NULL UNIQUE CHECK (length(root_path) > 0),
                created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
                archived_at_ms INTEGER CHECK (archived_at_ms >= created_at_ms)
              ) STRICT;
              INSERT INTO projects_new(id, name, root_path, created_at_ms)
                  SELECT id, name, root_path, created_at_ms FROM projects;
              DROP TABLE projects;
              ALTER TABLE projects_new RENAME TO projects;",
    };
    let mut migrations = vec![
        Migration {
            name: ALL[0].name,
            sql: ALL[0].sql,
        },
        rebuild,
    ];
    apply(db, &migrations)?;
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(count(db, "queue_tasks"), 1);
    assert!(db.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?);
    assert!(!db.prepare("PRAGMA foreign_key_check")?.exists([])?);
    migrations.push(Migration {
        name: "0003_invalid_data",
        sql: "DELETE FROM projects;",
    });
    assert!(
        apply(db, &migrations)
            .unwrap_err()
            .to_string()
            .contains("foreign-key violations")
    );
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(count(db, "schema_migrations"), 2);
    assert!(db.pragma_query_value(None, "foreign_keys", |r| r.get::<_, bool>(0))?);
    assert!(db.execute("DELETE FROM projects", []).is_err());
    Ok(())
}
