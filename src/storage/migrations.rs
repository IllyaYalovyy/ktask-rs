use rusqlite::{Connection, TransactionBehavior, params};

use super::{Error, Result};

const APPLICATION_ID: i64 = 0x4b54_534b; // KTSK

#[derive(Clone, Copy)]
pub(super) struct Migration {
    pub name: &'static str,
    pub sql: &'static str,
}

pub(super) const ALL: &[Migration] = &[Migration {
    name: "0001_relational_state",
    sql: include_str!("../../migrations/0001_relational_state.sql"),
}];

pub(super) fn apply(connection: &mut Connection, migrations: &[Migration]) -> Result<()> {
    if !connection.is_autocommit() {
        return Err(Error::Incompatible(
            "migrations require their own transaction".into(),
        ));
    }
    // SQLite's table-rebuild procedure requires this outside the transaction. All new
    // data is checked before commit, and normal enforcement is restored on both paths.
    let foreign_keys: bool =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    connection.pragma_update(None, "foreign_keys", false)?;
    let result = apply_transaction(connection, migrations);
    let restored = connection.pragma_update(None, "foreign_keys", foreign_keys);
    result.and(restored.map_err(Error::from))
}

fn apply_transaction(connection: &mut Connection, migrations: &[Migration]) -> Result<()> {
    // Serializes concurrent openers, including first launch; errors roll back the whole batch.
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let identity: i64 = transaction.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if identity == 0 {
        let objects: i64 = transaction.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
            [],
            |row| row.get(0),
        )?;
        if objects != 0 {
            return Err(Error::Incompatible(
                "unrecognized, nonempty SQLite file".into(),
            ));
        }
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.execute_batch(
            "CREATE TABLE schema_migrations (
                version INTEGER PRIMARY KEY CHECK (version > 0),
                name TEXT NOT NULL UNIQUE,
                sql TEXT NOT NULL,
                applied_at_ms INTEGER NOT NULL CHECK (applied_at_ms >= 0)
            ) STRICT;",
        )?;
    } else if identity != APPLICATION_ID {
        return Err(Error::Incompatible(
            "file belongs to another application".into(),
        ));
    }

    let applied = {
        let mut query = transaction
            .prepare("SELECT version, name, sql FROM schema_migrations ORDER BY version")?;
        query
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (index, (version, name, sql)) in applied.iter().enumerate() {
        let expected = migrations
            .get(index)
            .ok_or_else(|| Error::Incompatible("newer schema; use a newer ktask binary".into()))?;
        if *version != index as i64 + 1 || name != expected.name || sql != expected.sql {
            return Err(Error::Incompatible(format!(
                "schema {version} differs from this binary; no conversion is available and the database was left unchanged"
            )));
        }
    }
    for (index, migration) in migrations.iter().enumerate().skip(applied.len()) {
        transaction.execute_batch(migration.sql)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, sql, applied_at_ms)
             VALUES (?1, ?2, ?3, CAST(unixepoch('subsec') * 1000 AS INTEGER))",
            params![index as i64 + 1, migration.name, migration.sql],
        )?;
    }
    // Check upgraded data, not every row of potentially large histories at every CLI open.
    if applied.len() < migrations.len()
        && transaction
            .prepare("PRAGMA foreign_key_check")?
            .exists([])?
    {
        return Err(Error::Incompatible(
            "foreign-key violations; no changes applied".into(),
        ));
    }
    transaction.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
