use std::collections::BTreeMap;

use ktask_rs_mk3::storage::Database;
use rusqlite::{Batch, Connection, fallible_iterator::FallibleIterator, types::Value};

use crate::support::{self, Result};

const DOC: &str = include_str!("../../docs/STORAGE.md");

#[derive(Debug, PartialEq, Eq)]
struct Column {
    name: String,
    data_type: String,
    nullable: bool,
}

#[derive(Default)]
struct ForeignKey {
    parent: String,
    from: Vec<String>,
    to: Vec<String>,
}

fn examples() -> BTreeMap<&'static str, &'static str> {
    DOC.split("```sql\n")
        .skip(1)
        .map(|block| {
            let (sql, _) = block.split_once("```").expect("closed SQL fence");
            let (label, _) = sql.split_once('\n').unwrap();
            (label.strip_prefix("-- name: ").expect("named example"), sql)
        })
        .collect()
}

#[test]
fn schema_document_covers_every_column_key_index_view_and_erd_relationship() -> Result {
    let sandbox = support::sandbox()?;
    let store = Database::open_in(sandbox.path())?;
    let db = store.connection();
    let definitions = DOC
        .split_once("## Table definitions\n")
        .unwrap()
        .1
        .split_once("\n## Views\n")
        .unwrap()
        .0;
    let sections: BTreeMap<_, _> = definitions
        .split("### ")
        .skip(1)
        .map(|section| section.split_once('\n').unwrap())
        .collect();
    let tables = db.prepare("SELECT name FROM sqlite_schema WHERE type = 'table' AND name NOT GLOB 'sqlite_*' ORDER BY name")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    assert_eq!(tables.len(), 23);
    assert_eq!(
        sections.keys().copied().collect::<Vec<_>>(),
        tables.iter().map(String::as_str).collect::<Vec<_>>()
    );
    let mut expected_edges = BTreeMap::new();
    for table in tables {
        let section = sections[table.as_str()];
        let mut columns = Vec::new();
        let mut primary_key = Vec::new();
        let mut query = db.prepare(&format!("PRAGMA table_info({table})"))?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let name: String = row.get(1)?;
            let pk: i64 = row.get(5)?;
            if pk > 0 {
                primary_key.push((pk, name.clone()));
            }
            columns.push(Column {
                name,
                data_type: row.get(2)?,
                nullable: !row.get::<_, bool>(3)? && pk == 0,
            });
        }
        let documented: Vec<_> = section
            .lines()
            .filter(|line| line.starts_with("| `"))
            .map(|line| {
                let fields: Vec<_> = line.split('|').map(str::trim).collect();
                assert!(matches!(fields[3], "yes" | "no"));
                Column {
                    name: fields[1].trim_matches('`').into(),
                    data_type: fields[2].into(),
                    nullable: fields[3] == "yes",
                }
            })
            .collect();
        assert_eq!(documented, columns, "columns of {table}");
        primary_key.sort();
        let pk = primary_key
            .iter()
            .map(|(_, name)| name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            section.contains(&format!("- PK `({pk})`.")),
            "PK of {table}"
        );

        let mut keys: BTreeMap<i64, ForeignKey> = BTreeMap::new();
        let mut query = db.prepare(&format!("PRAGMA foreign_key_list({table})"))?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let key = keys.entry(row.get(0)?).or_default();
            key.parent = row.get(2)?;
            key.from.push(row.get::<_, String>(3)?);
            key.to.push(row.get::<_, String>(4)?);
            assert_eq!(row.get::<_, String>(5)?, "NO ACTION");
            assert_eq!(row.get::<_, String>(6)?, "NO ACTION");
        }
        assert_eq!(
            section.matches("- FK `").count(),
            keys.len(),
            "FK count for {table}"
        );
        for key in keys.values() {
            let declaration = format!(
                "- FK `({})` → `{}({})`",
                key.from.join(", "),
                key.parent,
                key.to.join(", ")
            );
            assert!(
                section.contains(&declaration),
                "{table}: missing {declaration}"
            );
            *expected_edges
                .entry((key.parent.clone(), table.clone()))
                .or_insert(0) += 1;
        }
    }
    let mut diagram_edges = BTreeMap::new();
    for block in DOC.split("```mermaid\n").skip(1) {
        for line in block.split_once("```").unwrap().0.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() >= 4 && fields[1].contains("--") {
                *diagram_edges
                    .entry((fields[0].to_owned(), fields[2].to_owned()))
                    .or_insert(0) += 1;
            }
        }
    }
    assert_eq!(
        diagram_edges, expected_edges,
        "ERD must show every FK, including parallel composite references"
    );
    let objects = db.prepare("SELECT name FROM sqlite_schema WHERE type = 'view' OR (type = 'index' AND sql IS NOT NULL)")?
        .query_map([], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for object in objects {
        assert!(
            DOC.contains(&format!("`{object}")),
            "undocumented index/view {object}"
        );
    }
    Ok(())
}

#[test]
fn every_documented_sql_statement_prepares_against_the_real_schema() -> Result {
    let sandbox = support::sandbox()?;
    let store = Database::open_in(sandbox.path())?;
    let examples = examples();
    assert_eq!(
        examples.len(),
        DOC.matches("```sql\n").count(),
        "duplicate example names"
    );
    assert!(examples.len() >= 20);
    for (name, sql) in examples {
        let mut batch = Batch::new(store.connection(), sql);
        let mut count = 0;
        while batch
            .next()
            .unwrap_or_else(|error| panic!("{name}: {error}"))
            .is_some()
        {
            count += 1;
        }
        assert!(count > 0, "empty {name}");
    }
    Ok(())
}

// Executes the actual fenced SQL, with explicit per-statement binding and RETURNING capture.
// This is a documentation test harness, not the application's future command service.
fn run(
    db: &Connection,
    name: &str,
    values: &mut BTreeMap<String, Value>,
) -> Result<Vec<BTreeMap<String, Value>>> {
    let sql = examples()[name];
    let mut batch = Batch::new(db, sql);
    let mut output = Vec::new();
    while let Some(mut statement) = batch.next()? {
        for index in 1..=statement.parameter_count() {
            let parameter = statement.parameter_name(index).unwrap();
            let value = values
                .get(parameter)
                .unwrap_or_else(|| panic!("{name}: missing {parameter}"));
            statement.raw_bind_parameter(index, value)?;
        }
        let names: Vec<_> = statement
            .column_names()
            .iter()
            .map(|name| name.to_string())
            .collect();
        if names.is_empty() {
            statement.raw_execute()?;
        } else {
            let mut rows = statement.raw_query();
            while let Some(row) = rows.next()? {
                let mut result = BTreeMap::new();
                for (index, column) in names.iter().enumerate() {
                    result.insert(column.clone(), row.get::<_, Value>(index)?);
                }
                let capture = match name {
                    "register-project" => Some((":project_id", "id")),
                    "create-task" => Some((":task_id", "id")),
                    "start-planning-call" => Some((":call_id", "id")),
                    _ => None,
                };
                if let Some((parameter, column)) = capture {
                    values.insert(parameter.into(), result[column].clone());
                }
                output.push(result);
            }
        }
    }
    Ok(output)
}

fn set(values: &mut BTreeMap<String, Value>, name: &str, value: impl Into<Value>) {
    values.insert(name.into(), value.into());
}

fn verify_execution_reads(db: &Connection, values: &mut BTreeMap<String, Value>) -> Result {
    assert_eq!(run(db, "project-checks", values)?.len(), 1);
    assert_eq!(run(db, "attempt-steps", values)?.len(), 2);
    assert_eq!(
        run(db, "criterion-evidence", values)?[0]["state"],
        Value::Text("passed".into())
    );
    set(values, ":role", "validation".to_owned());
    set(values, ":session_id", 1);
    assert_eq!(run(db, "reusable-sessions", values)?.len(), 1);
    assert_eq!(run(db, "session-attempts", values)?.len(), 1);
    assert_eq!(run(db, "output-tail", values)?.len(), 1);
    assert!(run(db, "open-questions", values)?.is_empty());
    assert!(run(db, "attempt-artifacts", values)?.is_empty());
    Ok(())
}

#[test]
fn documented_approval_execution_override_and_read_patterns_work_together() -> Result {
    use super::storage::{scalar, task};
    use ktask_rs_mk3::storage::execution::StartAttempt;

    let sandbox = support::sandbox()?;
    let mut store = Database::open_in(sandbox.path())?;
    let mut values = BTreeMap::new();
    for (name, value) in [
        (":name", "Example project"),
        (":kind", "ai"),
        (":title", "Example task"),
        (":goal", "Implement the requested change"),
        (":criterion", "Tests pass"),
        (":message", "User command"),
        (":execution_token", "documentation-token"),
        (":outcome", "Checks passed"),
        (":search", ""),
        (":scope", "all"),
        (":order_key", "8"),
        (":origin", "planner"),
        (":actor_kind", "ai"),
        (":actor", "planner"),
        (":reason", "Accepted known limitation"),
        (":provider", "test"),
        (":model", "planner"),
        (":effort", "high"),
        (":request_key", "planning-call"),
    ] {
        set(&mut values, name, value.to_owned());
    }
    for (name, value) in [
        (":now_ms", 100),
        (":expected_version", 1),
        (":revision", 1),
        (":limit", 20),
        (":after_event_id", 0),
        (":after_sequence", 0),
        (":input_tokens", 10),
        (":cache_read_tokens", 16279),
        (":cache_write_tokens", 4542),
        (":output_tokens", 42),
        (":cost_microusd", 123),
    ] {
        set(&mut values, name, value);
    }
    for name in [
        ":after_key",
        ":before_ms",
        ":before_id",
        ":attempt_limit",
        ":attempt_timeout_ms",
    ] {
        set(&mut values, name, Value::Null);
    }
    set(&mut values, ":root_path", b"/work/example".to_vec());
    let db = store.connection();
    run(db, "register-project", &mut values)?;
    assert_eq!(run(db, "list-projects", &mut values)?.len(), 1);
    db.execute_batch(
        "INSERT INTO project_models VALUES
        (1, 'execution', 'test', 'executor', 'medium'),
        (1, 'review', 'test', 'reviewer', 'high'),
        (1, 'validation', 'test', 'validator', 'high'),
        (1, 'remediation', 'test', 'remediator', 'high');",
    )?;
    run(db, "create-task", &mut values)?;
    assert_eq!(
        run(db, "current-task", &mut values)?[0]["lifecycle"],
        Value::Text("new".into())
    );
    run(db, "approve-revision", &mut values)?;
    assert_eq!(run(db, "revision-models", &mut values)?.len(), 4);
    assert_eq!(run(db, "queue-page", &mut values)?.len(), 1);
    let attempt = store.start_attempt(StartAttempt {
        task_id: 1,
        expected_version: 2,
        execution_token: "documentation-token",
        now_ms: 120,
    })?;
    set(&mut values, ":attempt_id", attempt);
    let db = store.connection();
    assert_eq!(
        run(db, "current-task", &mut values)?[0]["status"],
        Value::Text("running".into())
    );
    db.execute_batch("INSERT INTO project_checks VALUES (1, 1, 'tests', 'make check', '.', 60000, 1);
        INSERT INTO ai_sessions VALUES (1, 1, 1, 'validation', 'test', 'dev', 'chat-1', 'open', 120, 130);
        INSERT INTO attempt_steps(attempt_id, project_id, task_id, revision, ordinal, role, state,
            project_check_id, command, working_directory, timeout_ms, exit_code, started_at_ms, finished_at_ms, report)
        VALUES (1, 1, 1, 1, 1, 'check', 'succeeded', 1, 'make check', '.', 60000, 0, 120, 130, 'Tests pass');
        INSERT INTO attempt_steps(attempt_id, project_id, task_id, revision, ordinal, role, state,
            session_id, provider, model, effort, invocation, started_at_ms, finished_at_ms, report)
        VALUES (1, 1, 1, 1, 2, 'validation', 'succeeded', 1, 'test', 'validator', 'high', 'start', 120, 130, 'Goal met');
        INSERT INTO criterion_results VALUES (2, 1, 1, 1, 'passed', 'Observed expected result');
        INSERT INTO attempt_output VALUES (1, 1, 'stdout', 130, 'PASS');
        UPDATE attempts SET active_ms = 10 WHERE id = 1;")?;
    verify_execution_reads(db, &mut values)?;
    set(&mut values, ":expected_version", 3);
    set(&mut values, ":now_ms", 140);
    // These recipes test relational writes; the future completion command checks full review evidence.
    run(db, "finish-success", &mut values)?;
    assert!(run(db, "current-task", &mut values)?.is_empty());
    assert!(run(db, "queue-page", &mut values)?.is_empty());
    assert_eq!(run(db, "history-page", &mut values)?.len(), 1);
    assert_eq!(
        run(db, "task-attempts", &mut values)?[0]["revision"],
        Value::Integer(1)
    );
    assert_eq!(run(db, "event-tail", &mut values)?.len(), 4);
    assert_eq!(run(db, "export-definitions", &mut values)?.len(), 1);
    run(db, "start-planning-call", &mut values)?;
    set(&mut values, ":now_ms", 150);
    run(db, "finish-model-call", &mut values)?;
    // Several calls must not multiply each other's costs through joins to steps/events.
    db.execute_batch(
        "INSERT INTO model_calls(project_id, task_id, revision, role, provider, model,
        request_key, state, started_at_ms, finished_at_ms, cost_microusd)
        VALUES (1, 1, 1, 'remediation', 'test', 'capable', 'call-2', 'succeeded', 120, 130, 456),
               (1, 1, 1, 'planning', 'test', 'capable', 'call-3', 'succeeded', 120, 130, NULL);",
    )?;
    let failed = task(db, 1, "c")?;
    db.execute_batch(
        "INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token,
        started_at_ms, finished_at_ms, outcome, failure_category)
        VALUES (1, 2, 1, 1, 'failed', 'failed-call', 140, 145, 'Failed', 'check');",
    )?;
    set(&mut values, ":task_id", failed);
    set(&mut values, ":expected_version", 1);
    run(db, "override-failure", &mut values)?;
    assert_eq!(
        scalar(db, "SELECT count(*) FROM attempts WHERE state = 'failed'")?,
        1
    );
    let statistics = run(db, "project-statistics", &mut values)?;
    assert_eq!(statistics[0]["succeeded"], Value::Integer(1));
    assert_eq!(statistics[0]["overridden"], Value::Integer(1));
    assert_eq!(statistics[0]["active_ms"], Value::Integer(10));
    assert_eq!(statistics[0]["known_cost_microusd"], Value::Integer(579));
    assert_eq!(statistics[0]["unreported_calls"], Value::Integer(1));
    let usage = run(db, "token-totals", &mut values)?;
    assert_eq!(usage[0]["known_total_input"], Value::Integer(20831));
    assert_eq!(usage[0]["unreported_input_calls"], Value::Integer(2));
    assert!(!db.prepare("PRAGMA foreign_key_check")?.exists([])?);
    Ok(())
}
