use rusqlite::Connection;

use super::{ALL, Migration, apply};
use crate::{scratch::Scratch, storage::Database};

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

fn sandbox() -> std::io::Result<Scratch> {
    Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))
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
fn unrecognized_missing_and_foreign_migrations_are_not_reset_or_adopted() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "UPDATE schema_migrations SET sql = sql || ' DROP TABLE projects;'",
        [],
    )?;
    assert!(
        apply(db, ALL)
            .unwrap_err()
            .to_string()
            .contains("cannot reconcile")
    );
    db.execute("UPDATE schema_migrations SET sql = ?1", [ALL[0].sql])?;
    db.execute("UPDATE schema_migrations SET version = 2", [])?;
    assert!(
        apply(db, ALL)
            .unwrap_err()
            .to_string()
            .contains("unrecognized")
    );
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
fn v1_reconciliation_copies_data_and_rejects_incompatible_columns() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('kept', x'2f776f726b', 1)",
        [],
    )?;
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('removed', x'2f6f6c64', 1)",
        [],
    )?;
    db.execute("DELETE FROM projects WHERE name = 'removed'", [])?;
    let old = ALL[0].sql.replace(
        "CREATE INDEX ix_revisions_ready ON task_revisions(task_id, lifecycle, revision DESC);\n",
        "",
    );
    assert_ne!(old, ALL[0].sql);
    db.execute("DROP INDEX ix_revisions_ready", [])?;
    db.execute("UPDATE schema_migrations SET sql = ?1", [&old])?;
    apply(db, ALL)?;
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        ALL[0].sql
    );
    assert_eq!(count(db, "schema_migrations"), 1);
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('next', x'2f6e657874', 1)",
        [],
    )?;
    assert_eq!(
        db.last_insert_rowid(),
        3,
        "AUTOINCREMENT must not reuse deleted IDs"
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'ix_revisions_ready'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );

    // The recorded definition is still genuine, but a required new column has
    // no value for an existing project. The entire attempted rebuild rolls back.
    let incompatible = Box::leak(
        ALL[0]
            .sql
            .replace("    agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1))\n) STRICT;", "    agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1)),\n    required_new TEXT NOT NULL\n) STRICT;")
            .into_boxed_str(),
    );
    let newer = [Migration {
        name: ALL[0].name,
        sql: incompatible,
    }];
    let message = apply(db, &newer).unwrap_err().to_string();
    assert!(message.contains("cannot reconcile schema 1"), "{message}");
    assert!(message.contains("previous binary"), "{message}");
    assert_eq!(count(db, "projects"), 2);
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        ALL[0].sql
    );
    assert!(db.prepare("SELECT required_new FROM projects").is_err());
    Ok(())
}

#[test]
fn v1_reconciliation_adds_nullable_column_without_losing_rows() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('kept', x'2f776f726b', 1)",
        [],
    )?;
    let with_column = Box::leak(
        ALL[0]
            .sql
            .replace(
                "    agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1))\n) STRICT;",
                "    agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1)),\n    description TEXT\n) STRICT;",
            )
            .into_boxed_str(),
    );
    let edited = [Migration {
        name: ALL[0].name,
        sql: with_column,
    }];
    apply(db, &edited)?;
    assert_eq!(
        db.query_row("SELECT name, description FROM projects", [], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?,
        ("kept".into(), None)
    );
    assert_eq!(count(db, "schema_migrations"), 1);
    assert!(!db.prepare("PRAGMA foreign_key_check")?.exists([])?);
    Ok(())
}

#[test]
fn v1_reconciliation_rejects_silent_row_loss_and_rolls_back() -> Result {
    let scratch = sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let db = store.connection_mut();
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES ('kept', x'2f776f726b', 1)",
        [],
    )?;
    let changed = Box::leak(
        format!(
            "{}\nCREATE TRIGGER discard_copied_project AFTER INSERT ON projects \
             BEGIN DELETE FROM projects WHERE id = NEW.id; END;",
            ALL[0].sql
        )
        .into_boxed_str(),
    );
    let edited = [Migration {
        name: ALL[0].name,
        sql: changed,
    }];
    let message = apply(db, &edited).unwrap_err().to_string();
    assert!(
        message.contains("retained rows changed in projects"),
        "{message}"
    );
    assert_eq!(count(db, "projects"), 1);
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        ALL[0].sql
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'discard_copied_project'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
    Ok(())
}

#[test]
fn v1_reconciliation_refuses_real_values_that_cast_to_the_same_blob() -> Result {
    let mut db = Connection::open_in_memory()?;
    let old: &'static str = Box::leak(
        format!(
            "{}\nCREATE TABLE real_samples(value REAL) STRICT;",
            ALL[0].sql
        )
        .into_boxed_str(),
    );
    let old_migration = [Migration {
        name: ALL[0].name,
        sql: old,
    }];
    apply(&mut db, &old_migration)?;
    let first = 0.0_f64;
    let second = -0.0_f64;
    assert_ne!(first.to_bits(), second.to_bits());
    let first_blob: String =
        db.query_row("SELECT hex(CAST(?1 AS BLOB))", [first], |row| row.get(0))?;
    let second_blob: String =
        db.query_row("SELECT hex(CAST(?1 AS BLOB))", [second], |row| row.get(0))?;
    assert_eq!(
        first_blob, second_blob,
        "the former check would miss this change"
    );
    db.execute(
        "INSERT INTO real_samples(value) VALUES (?1), (?2)",
        (first, second),
    )?;

    let edited = Box::leak(
        format!("{old}\nCREATE INDEX ix_real_samples ON real_samples(value);").into_boxed_str(),
    );
    let new_migration = [Migration {
        name: ALL[0].name,
        sql: edited,
    }];
    let message = apply(&mut db, &new_migration).unwrap_err().to_string();
    assert!(message.contains("REAL values in real_samples"), "{message}");
    assert!(message.contains("database left unchanged"), "{message}");
    assert_eq!(count(&db, "real_samples"), 2);
    assert_eq!(
        db.query_row("SELECT sql FROM schema_migrations", [], |row| row
            .get::<_, String>(0))?,
        old
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name = 'ix_real_samples'",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        0
    );
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
         INSERT INTO projects(id, name, root_path, created_at_ms) VALUES (1, 'Keep project', x'2f776f726b', 100);
         INSERT INTO tasks(id, project_id, order_key, lifecycle, origin, created_at_ms, updated_at_ms)
             VALUES (1, 1, '8', 'new', 'human', 100, 100);
         INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, created_at_ms)
             VALUES (1, 1, 'draft', 'ai', 'Keep task', 'Keep goal', 'ai', 'test', 100);
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
                agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1)),
                archived_at_ms INTEGER CHECK (archived_at_ms >= created_at_ms)
              ) STRICT;
              INSERT INTO projects_new(id, name, root_path, created_at_ms, agent_network)
                  SELECT id, name, root_path, created_at_ms, agent_network FROM projects;
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

#[test]
fn v1_accepts_sqlite_quoted_table_declaration_but_rejects_changed_constraints() -> Result {
    for changed_constraint in [false, true] {
        let scratch = sandbox()?;
        let recorded = ALL[0].sql.replace(
            "CREATE INDEX ix_revisions_ready ON task_revisions(task_id, lifecycle, revision DESC);\n",
            "",
        );
        let mut live = recorded.replace(
            "CREATE TABLE task_revisions (",
            "CREATE TABLE \"task_revisions\" (",
        );
        if changed_constraint {
            live = live.replace("CHECK (revision > 0)", "CHECK (revision > 1)");
        }
        let mut db = Connection::open(scratch.path().join("ktask.sqlite3"))?;
        apply(
            &mut db,
            &[Migration {
                name: ALL[0].name,
                sql: Box::leak(live.into_boxed_str()),
            }],
        )?;
        // The original ledger is unquoted; SQLite's rebuilt table declaration is quoted.
        db.execute("UPDATE schema_migrations SET sql = ?1", [&recorded])?;
        db.execute("INSERT INTO projects(name, root_path, created_at_ms) VALUES ('retained', x'2f776f726b', 1)", [])?;
        let result = apply(&mut db, ALL);
        if changed_constraint {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("live schema does not match")
            );
            assert_eq!(
                db.query_row("SELECT sql FROM schema_migrations", [], |r| r
                    .get::<_, String>(0))?,
                recorded
            );
        } else {
            result?;
            assert_eq!(
                db.query_row("SELECT sql FROM schema_migrations", [], |r| r
                    .get::<_, String>(0))?,
                ALL[0].sql
            );
            assert!(
                db.prepare("SELECT 1 FROM sqlite_schema WHERE name='ix_revisions_ready'")?
                    .exists([])?
            );
        }
        assert_eq!(count(&db, "projects"), 1);
        assert_eq!(
            db.query_row("SELECT name FROM projects", [], |r| r.get::<_, String>(0))?,
            "retained"
        );
    }
    Ok(())
}
