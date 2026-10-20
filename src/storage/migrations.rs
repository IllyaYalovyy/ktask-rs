use rusqlite::{Connection, Transaction, TransactionBehavior, params};

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
        if *version != index as i64 + 1 || name != expected.name {
            return Err(Error::Incompatible(format!(
                "schema {version} is unrecognized; database left unchanged. Use the binary that created it or restore a compatible backup"
            )));
        }
        if sql != expected.sql {
            if index != 0 {
                return Err(Error::Incompatible(format!(
                    "schema {version} differs from this binary; database left unchanged. Use the binary that created it or restore a compatible backup"
                )));
            }
            reconcile_v1(&transaction, sql, expected.sql).map_err(|error| {
                Error::Incompatible(format!(
                    "cannot reconcile schema {version}: {error}; database left unchanged. Use the previous binary or restore a backup after adjusting the V1 schema"
                ))
            })?;
            transaction.execute(
                "UPDATE schema_migrations SET sql = ?1 WHERE version = 1",
                [expected.sql],
            )?;
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

type Object = (String, String, String);

fn schema_objects(connection: &Connection) -> rusqlite::Result<Vec<Object>> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql FROM sqlite_schema
         WHERE name NOT GLOB 'sqlite_*' AND name <> 'schema_migrations'
         ORDER BY type, name",
    )?;
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect()
}

fn tables(connection: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_schema
         WHERE type = 'table' AND name NOT GLOB 'sqlite_*' AND name <> 'schema_migrations'
         ORDER BY name",
    )?;
    statement.query_map([], |row| row.get(0))?.collect()
}

fn columns(connection: &Connection, table: &str) -> rusqlite::Result<Vec<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_xinfo({})", quote(table)))?;
    statement
        .query_map([], |row| {
            let hidden: i64 = row.get(6)?;
            Ok((row.get::<_, String>(1)?, hidden))
        })?
        .filter_map(|row| match row {
            Ok((name, 0)) => Some(Ok(name)),
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

fn quote(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn same_rows(
    transaction: &Transaction<'_>,
    table: &str,
    backup: &str,
    names: &[String],
) -> Result<bool> {
    // CAST(REAL AS BLOB) first formats a decimal string, so distinct floats can
    // become identical. Refuse those rows until they can be compared exactly.
    let real_columns = names
        .iter()
        .map(|name| format!("typeof({}) = 'real'", quote(name)))
        .collect::<Vec<_>>()
        .join(" OR ");
    for source in [
        format!("temp.{}", quote(backup)),
        format!("main.{}", quote(table)),
    ] {
        let real_row = format!("SELECT 1 FROM {source} WHERE {real_columns} LIMIT 1");
        if transaction.prepare(&real_row)?.exists([])? {
            return Err(Error::Incompatible(format!(
                "REAL values in {table} cannot be compared exactly"
            )));
        }
    }
    // Compare stored type and bytes as well as multiplicity. SQLite can coerce a
    // value when a retained column's affinity changes, and triggers can silently
    // remove copied rows while leaving both integrity checks green.
    let values = names
        .iter()
        .map(|name| {
            let name = quote(name);
            format!("typeof({name}), hex(CAST({name} AS BLOB))")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let groups = (1..=names.len() * 2)
        .map(|position| position.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let old = format!(
        "SELECT {values}, count(*) FROM temp.{} GROUP BY {groups}",
        quote(backup)
    );
    let new = format!(
        "SELECT {values}, count(*) FROM main.{} GROUP BY {groups}",
        quote(table)
    );
    for (left, right) in [(&old, &new), (&new, &old)] {
        let differences = format!("SELECT 1 FROM ({left} EXCEPT {right}) LIMIT 1");
        if transaction.prepare(&differences)?.exists([])? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn reconcile_v1(transaction: &Transaction<'_>, recorded: &str, desired: &str) -> Result<()> {
    let recorded_schema = Connection::open_in_memory()?;
    recorded_schema.execute_batch(recorded)?;
    if schema_objects(transaction)? != schema_objects(&recorded_schema)? {
        return Err(Error::Incompatible(
            "live schema does not match its recorded V1 definition".into(),
        ));
    }
    let desired_schema = Connection::open_in_memory()?;
    desired_schema.execute_batch(desired)?;
    if schema_objects(&recorded_schema)? == schema_objects(&desired_schema)? {
        return Ok(());
    }

    let old_tables = tables(&recorded_schema)?;
    let new_tables = tables(&desired_schema)?;
    for table in &old_tables {
        if !new_tables.contains(table) {
            return Err(Error::Incompatible(format!("table {table} was removed")));
        }
        for column in columns(&recorded_schema, table)? {
            if !columns(&desired_schema, table)?.contains(&column) {
                return Err(Error::Incompatible(format!(
                    "column {table}.{column} was removed"
                )));
            }
        }
    }

    let sequences = {
        let mut statement = transaction.prepare("SELECT name, seq FROM sqlite_sequence")?;
        statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for (index, table) in old_tables.iter().enumerate() {
        transaction.execute_batch(&format!(
            "CREATE TEMP TABLE {} AS SELECT * FROM main.{}",
            quote(&format!("ktask_v1_backup_{index}")),
            quote(table)
        ))?;
    }
    let old_objects = schema_objects(transaction)?;
    for (kind, name, _) in &old_objects {
        if kind == "view" || kind == "trigger" {
            transaction.execute_batch(&format!("DROP {} {}", kind, quote(name)))?;
        }
    }
    for table in &old_tables {
        transaction.execute_batch(&format!("DROP TABLE {}", quote(table)))?;
    }
    transaction.execute_batch(desired)?;
    for (index, table) in old_tables.iter().enumerate() {
        let names = columns(&recorded_schema, table)?;
        let list = names
            .iter()
            .map(|name| quote(name))
            .collect::<Vec<_>>()
            .join(", ");
        transaction.execute_batch(&format!(
            "INSERT INTO main.{} ({list}) SELECT {list} FROM temp.{}",
            quote(table),
            quote(&format!("ktask_v1_backup_{index}"))
        ))?;
    }
    for (index, table) in old_tables.iter().enumerate() {
        let backup = format!("ktask_v1_backup_{index}");
        if !same_rows(
            transaction,
            table,
            &backup,
            &columns(&recorded_schema, table)?,
        )? {
            return Err(Error::Incompatible(format!(
                "retained rows changed in {table}"
            )));
        }
        transaction.execute_batch(&format!("DROP TABLE temp.{}", quote(&backup)))?;
    }
    for (name, sequence) in sequences {
        transaction.execute(
            "UPDATE sqlite_sequence SET seq = max(seq, ?2) WHERE name = ?1",
            params![name, sequence],
        )?;
    }
    if transaction
        .prepare("PRAGMA foreign_key_check")?
        .exists([])?
    {
        return Err(Error::Incompatible("foreign-key violations".into()));
    }
    let integrity: String =
        transaction.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(Error::Incompatible(format!("integrity check: {integrity}")));
    }
    Ok(())
}

#[cfg(test)]
#[path = "migration_tests.rs"]
mod tests;
