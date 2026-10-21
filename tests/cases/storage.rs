use crate::support::{self, Result};
use ktask_rs_mk3::storage::{
    Database,
    execution::{StartAttempt, WaitAttempt},
    ordering,
};
use rusqlite::{Connection, params};
use std::{
    io::{BufRead, Read, Write},
    path::Path,
    process::{Child, Command, Stdio},
    sync::{Arc, Barrier, mpsc},
    thread,
    time::Duration,
};

pub(super) fn project(db: &Connection, name: &str) -> rusqlite::Result<i64> {
    db.execute(
        "INSERT INTO projects(name, root_path, created_at_ms) VALUES (?1, ?2, 100)",
        params![name, format!("/work/{name}").as_bytes()],
    )?;
    Ok(db.last_insert_rowid())
}

pub(super) fn task(db: &Connection, project: i64, key: &str) -> rusqlite::Result<i64> {
    let tx = db.unchecked_transaction()?;
    tx.execute("INSERT INTO tasks(project_id, order_key, lifecycle, origin, created_at_ms, updated_at_ms) VALUES (?1, ?2, 'ready', 'human', 100, 100)", params![project, key])?;
    let id = tx.last_insert_rowid();
    tx.execute("INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, created_at_ms, decided_at_ms, decided_by_kind, decided_by) VALUES (?1, 1, 'ready', 'ai', 'Task title', 'Task goal', 'human', 'test-user', 100, 100, 'human', 'test-user')", [id])?;
    tx.execute(
        "INSERT INTO acceptance_criteria VALUES (?1, 1, 1, 'Goal is met')",
        [id],
    )?;
    for role in ["execution", "review", "validation", "remediation"] {
        tx.execute("INSERT INTO revision_models VALUES (?1, 1, ?2, 'test-provider', 'test-model', 'medium')", params![id, role])?;
    }
    tx.commit()?;
    Ok(id)
}

pub(super) fn scalar(db: &Connection, sql: &str) -> rusqlite::Result<i64> {
    db.query_row(sql, [], |r| r.get(0))
}

#[test]
fn fresh_schema_is_relational_durable_isolated_and_has_no_proposal_tables() -> Result {
    let scratch = support::sandbox()?;
    let prod = Database::open_in(&scratch.path().join("ktask"))?;
    let dev = Database::open_in(&scratch.path().join("ktask-dev"))?;
    project(prod.connection(), "unique")?;
    assert_eq!(
        scalar(dev.connection(), "SELECT count(*) FROM projects")?,
        0
    );
    assert!(project(prod.connection(), "unique").is_err());
    assert_eq!(scalar(prod.connection(), "PRAGMA foreign_keys")?, 1);
    assert_eq!(scalar(prod.connection(), "PRAGMA synchronous")?, 2);
    assert_eq!(
        scalar(prod.connection(), "SELECT count(*) FROM schema_migrations")?,
        1
    );
    assert_eq!(
        scalar(
            prod.connection(),
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE '%proposal%' OR name IN ('workspace_handoffs', 'project_runs')"
        )?,
        0
    );
    drop(prod);
    let reopened = Database::open_in(&scratch.path().join("ktask"))?;
    assert_eq!(
        scalar(reopened.connection(), "SELECT count(*) FROM projects")?,
        1
    );
    assert!(Database::open_in(Path::new("relative")).is_err());
    Ok(())
}

#[test]
fn fractional_keys_support_batch_nested_prepend_append_and_retained_history() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "order")?;
    let first = task(store.connection(), p, "5")?;
    let history = task(store.connection(), p, "58")?;
    store.connection().execute(
        "UPDATE tasks SET lifecycle = 'succeeded', closed_at_ms = 100 WHERE id = ?1",
        [history],
    )?;
    let second = task(store.connection(), p, "6")?;
    let tx = store.connection_mut().transaction()?;
    let keys = ordering::keys_before(&tx, p, Some(second), 10)?;
    assert_eq!(keys.len(), 10);
    assert!(keys.windows(2).all(|w| w[0] < w[1]));
    assert!(keys.iter().all(|k| k.as_str() > "58" && k.as_str() < "6"));
    tx.commit()?;
    for key in &keys {
        task(store.connection(), p, key)?;
    }
    let mut upper = "6".to_owned();
    for _ in 0..1000 {
        let next = ordering::between(Some("5"), Some(&upper))?;
        assert!(next.as_str() > "5" && next < upper);
        upper = next;
    }
    let before = ordering::between(None, Some(&upper))?;
    assert!(before < upper);
    assert!(ordering::between(Some("f"), None)?.as_str() > "f");
    assert!(ordering::between(Some("6"), Some("5")).is_err());
    assert!(task(store.connection(), p, "0").is_err());
    assert!(
        store
            .connection()
            .execute("UPDATE tasks SET order_key = NULL WHERE id = ?1", [first])
            .is_err()
    );
    assert_eq!(
        scalar(store.connection(), "SELECT count(*) FROM queue_tasks")?,
        12
    );
    Ok(())
}

#[test]
fn execution_state_is_stored_only_on_attempt_and_current_is_derived() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "current")?;
    let failed = task(store.connection(), p, "5")?;
    task(store.connection(), p, "6")?;
    store.connection().execute("INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms, finished_at_ms, outcome, failure_category)
        VALUES (?1, ?2, 1, 1, 'failed', 'failed', 100, 110, 'Failed', 'check')", params![p, failed])?;
    assert_eq!(
        scalar(store.connection(), "SELECT id FROM current_tasks")?,
        failed
    );
    let repair = task(store.connection(), p, "4")?;
    assert_eq!(
        scalar(store.connection(), "SELECT id FROM current_tasks")?,
        repair
    );
    let db = store.connection();
    assert!(
        db.execute(
            "UPDATE tasks SET lifecycle = 'running' WHERE id = ?1",
            [repair]
        )
        .is_err()
    );
    assert!(
        db.execute(
            "UPDATE tasks SET lifecycle = 'failed' WHERE id = ?1",
            [repair]
        )
        .is_err()
    );
    db.execute("INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms)
        VALUES (?1, ?2, 1, 1, 'running', 'repair', 120)", params![p, repair])?;
    // Running attempt + ready task is now correct: the task doesn't duplicate execution status.
    assert_eq!(
        db.query_row("SELECT status FROM current_tasks", [], |r| r
            .get::<_, String>(0))?,
        "running"
    );
    let earlier = task(db, p, "3")?;
    assert_eq!(
        scalar(db, "SELECT id FROM current_tasks")?,
        repair,
        "live attempt pins current"
    );
    let tx = store.connection_mut().transaction()?;
    assert!(ordering::keys_before(&tx, p, Some(repair), 1).is_err());
    tx.rollback()?;
    store.connection().execute(
        "UPDATE attempts SET state = 'paused' WHERE task_id = ?1",
        [repair],
    )?;
    assert_eq!(
        scalar(store.connection(), "SELECT id FROM current_tasks")?,
        earlier
    );
    let tx = store.connection_mut().transaction()?;
    assert_eq!(ordering::keys_before(&tx, p, Some(earlier), 1)?.len(), 1);
    tx.commit()?;
    Ok(())
}

#[test]
fn rejected_and_retired_revisions_do_not_replace_ready_and_drafts_are_separate() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "revisions")?;
    let id = task(store.connection(), p, "5")?;
    for (number, state) in [(2, "rejected"), (3, "retired")] {
        store.connection().execute("INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by,
            created_at_ms, decided_at_ms, decided_by_kind, decided_by)
            VALUES (?1, ?2, ?3, 'ai', 'Unusable title', 'Unusable goal', 'human', 'owner', 100, 110, 'human', 'owner')",
            params![id, number, state])?;
    }
    store.connection().execute(
        "INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, created_at_ms)
        VALUES (?1, 4, 'draft', 'ai', 'Pending escalation', 'Pending goal', 'ai', 'remediator', 120)",
        [id],
    )?;
    assert_eq!(
        scalar(store.connection(), "SELECT revision FROM current_tasks")?,
        1
    );
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT pending_revision FROM current_tasks"
        )?,
        4
    );
    assert_eq!(
        store
            .connection()
            .query_row("SELECT title FROM current_tasks", [], |r| r
                .get::<_, String>(0))?,
        "Task title"
    );
    let later = task(store.connection(), p, "6")?;
    let command = |task_id, expected_version| StartAttempt {
        task_id,
        expected_version,
        execution_token: "exec",
        now_ms: 130,
    };
    assert!(store.start_attempt(command(later, 1)).is_err());
    assert!(store.start_attempt(command(id, 99)).is_err());
    store.connection().execute(
        "DELETE FROM revision_models WHERE task_id = ?1 AND revision = 1 AND role = 'review'",
        [id],
    )?;
    assert!(store.start_attempt(command(id, 1)).is_err());
    store.connection().execute(
        "INSERT INTO revision_models VALUES (?1, 1, 'review', 'test', 'reviewer', NULL)",
        [id],
    )?;
    store.start_attempt(command(id, 1))?;
    assert_eq!(
        scalar(store.connection(), "SELECT revision FROM attempts")?,
        1
    );
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT row_version FROM tasks WHERE id = 1"
        )?,
        2
    );
    assert!(store.start_attempt(command(id, 2)).is_err());
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT count(*) FROM events WHERE kind = 'attempt.started'"
        )?,
        1
    );
    Ok(())
}

#[test]
fn timed_wait_resumes_same_attempt_without_charging_waiting_time_or_retry_budget() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "wait")?;
    let id = task(store.connection(), p, "5")?;
    store.connection().execute(
        "UPDATE task_revisions SET attempt_limit = 1, attempt_timeout_ms = 100",
        [],
    )?;
    store.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 1,
        execution_token: "wait",
        now_ms: 100,
    })?;
    store.wait_attempt(WaitAttempt {
        task_id: id,
        expected_version: 2,
        execution_token: "wait",
        reason: "rate_limit",
        resume_at_ms: Some(9000),
        active_delta_ms: 10,
        now_ms: 110,
    })?;
    assert_eq!(
        scalar(store.connection(), "SELECT active_ms FROM attempts")?,
        10
    );
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT attempts_used FROM current_tasks"
        )?,
        0
    );
    let resume = |now_ms| StartAttempt {
        task_id: id,
        expected_version: 3,
        execution_token: "wait",
        now_ms,
    };
    assert!(store.resume_attempt(resume(8999)).is_err());
    store.resume_attempt(resume(9000))?;
    assert_eq!(
        scalar(store.connection(), "SELECT count(*) FROM attempts")?,
        1
    );
    assert_eq!(
        scalar(store.connection(), "SELECT active_ms FROM attempts")?,
        10
    );
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT count(*) FROM attempts WHERE wait_reason IS NULL AND resume_at_ms IS NULL"
        )?,
        1
    );
    store
        .connection()
        .execute("UPDATE attempts SET state = 'paused', active_ms = 100", [])?;
    assert!(
        store
            .resume_attempt(StartAttempt {
                task_id: id,
                expected_version: 4,
                execution_token: "wait",
                now_ms: 9100
            })
            .is_err()
    );
    Ok(())
}

#[test]
fn infrastructure_failures_do_not_exhaust_task_attempt_budget() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "budget")?;
    let id = task(store.connection(), p, "5")?;
    store.connection().execute(
        "UPDATE task_revisions SET attempt_limit = 1, attempt_timeout_ms = 100",
        [],
    )?;
    store.connection().execute("INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms,
        finished_at_ms, outcome, failure_category) VALUES (?1, ?2, 1, 1, 'failed', 'infra', 100, 110, 'Network unavailable', 'transport')", params![p, id])?;
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT attempts_used FROM current_tasks"
        )?,
        0
    );
    store.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 1,
        execution_token: "retry",
        now_ms: 120,
    })?;
    store.connection().execute(
        "UPDATE attempts SET state = 'failed', finished_at_ms = 130, outcome = 'Tests failed',
        failure_category = 'check' WHERE execution_token = 'retry'",
        [],
    )?;
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT attempts_used FROM current_tasks"
        )?,
        1
    );
    // A failed current may start its read-only judge even with no coding budget.
    store.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 2,
        execution_token: "judge-at-budget",
        now_ms: 140,
    })?;
    Ok(())
}

#[test]
fn task_override_and_revision_escalation_do_not_rewrite_attempt_failure() -> Result {
    let scratch = support::sandbox()?;
    let store = Database::open_in(scratch.path())?;
    let db = store.connection();
    let p = project(db, "retry")?;
    let id = task(db, p, "5")?;
    db.execute("INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms, finished_at_ms, outcome, failure_category) VALUES (?1, ?2, 1, 1, 'failed', 'failed-attempt', 100, 110, 'Tests failed', 'check')", params![p, id])?;
    db.execute("INSERT INTO task_revisions SELECT task_id, 2, 'ready', kind, title, goal, 'Increase execution effort', 'ai', 'remediator', NULL, NULL, NULL, 120, 120, 'ai', 'remediator' FROM task_revisions WHERE task_id = ?1", [id])?;
    db.execute("INSERT INTO revision_models SELECT task_id, 2, role, provider, 'stronger-model', 'high' FROM revision_models WHERE task_id = ?1 AND revision = 1", [id])?;
    assert_eq!(scalar(db, "SELECT revision FROM current_tasks")?, 2);
    assert_eq!(scalar(db, "SELECT revision FROM attempts")?, 1);
    assert!(
        db.execute(
            "UPDATE tasks SET lifecycle = 'succeeded_override', closed_at_ms = 130 WHERE id = ?1",
            [id]
        )
        .is_err()
    );
    db.execute("UPDATE tasks SET lifecycle = 'succeeded_override', closed_at_ms = 130, resolution_reason = 'Accepted known limitation', resolved_by_kind = 'human', resolved_by = 'owner' WHERE id = ?1", [id])?;
    assert_eq!(scalar(db, "SELECT count(*) FROM current_tasks")?, 0);
    assert_eq!(
        scalar(db, "SELECT count(*) FROM attempts WHERE state = 'failed'")?,
        1
    );
    assert_eq!(
        scalar(
            db,
            "SELECT count(*) FROM history_tasks WHERE order_key IS NOT NULL"
        )?,
        1
    );
    Ok(())
}

#[test]
fn session_reuse_steps_and_provider_usage_are_relational() -> Result {
    let scratch = support::sandbox()?;
    let store = Database::open_in(scratch.path())?;
    let db = store.connection();
    let p = project(db, "sessions")?;
    let id = task(db, p, "5")?;
    db.execute("INSERT INTO ai_sessions(project_id, task_id, role, provider, provider_scope, provider_session_id, state, created_at_ms, last_used_at_ms) VALUES (?1, ?2, 'execution', 'claude', 'dev-profile', 'provider-session', 'open', 100, 100)", params![p, id])?;
    for number in 1..=2 {
        db.execute("INSERT INTO attempts(project_id, task_id, revision, number, state, execution_token, started_at_ms, finished_at_ms, outcome, failure_category) VALUES (?1, ?2, 1, ?3, 'failed', ?4, 100, 110, 'Check failed', 'check')", params![p, id, number, format!("exec-{number}")])?;
        let attempt = db.last_insert_rowid();
        db.execute("INSERT INTO attempt_steps(attempt_id, project_id, task_id, revision, ordinal, role, state, session_id, provider, model, effort, invocation, started_at_ms, finished_at_ms) VALUES (?1, ?2, ?3, 1, 1, 'execution', 'succeeded', 1, 'claude', 'executor', 'high', ?4, 100, 110)", params![attempt, p, id, if number == 1 {"start"} else {"resume"}])?;
    }
    assert_eq!(
        scalar(db, "SELECT count(DISTINCT session_id) FROM attempt_steps")?,
        1
    );
    assert_eq!(scalar(db, "SELECT count(*) FROM attempts")?, 2);
    db.execute("UPDATE attempt_steps SET effort = NULL", [])?;
    assert!(
        db.execute("UPDATE attempt_steps SET effort = ''", [])
            .is_err()
    );
    assert!(
        db.execute("UPDATE attempt_steps SET role = 'review' WHERE id = 2", [])
            .is_err(),
        "review cannot reuse executor conversation"
    );
    db.execute("INSERT INTO model_calls(project_id, role, provider, model, effort, request_key, state, started_at_ms, finished_at_ms, input_tokens, cache_read_tokens, cache_write_tokens, cost_microusd) VALUES (?1, 'planning', 'claude', 'planner', 'high', 'goal-plan', 'succeeded', 100, 110, 10, 16279, 4542, 123)", [p])?;
    assert_eq!(
        scalar(db, "SELECT count(*) FROM model_calls WHERE task_id IS NULL")?,
        1
    );
    assert_eq!(scalar(db, "SELECT cost_microusd FROM model_calls")?, 123);
    assert!(
        db.execute("UPDATE model_calls SET cost_microusd = 0.25", [])
            .is_err()
    );
    assert!(!db.prepare("PRAGMA foreign_key_check")?.exists([])?);
    Ok(())
}

#[test]
fn draft_only_task_is_visible_but_not_runnable_and_questions_gate_resume() -> Result {
    let scratch = support::sandbox()?;
    let mut store = Database::open_in(scratch.path())?;
    let p = project(store.connection(), "questions")?;
    let id = task(store.connection(), p, "5")?;
    store.connection().execute(
        "UPDATE task_revisions SET lifecycle = 'draft', decided_at_ms = NULL,
        decided_by_kind = NULL, decided_by = NULL",
        [],
    )?;
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT count(*) FROM current_tasks WHERE revision IS NULL AND pending_revision = 1"
        )?,
        1
    );
    assert!(
        store
            .start_attempt(StartAttempt {
                task_id: id,
                expected_version: 1,
                execution_token: "question",
                now_ms: 100
            })
            .is_err()
    );
    store.connection().execute(
        "UPDATE task_revisions SET lifecycle = 'ready', decided_at_ms = 100,
        decided_by_kind = 'human', decided_by = 'owner'",
        [],
    )?;
    store.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 1,
        execution_token: "question",
        now_ms: 100,
    })?;
    store.connection().execute(
        "INSERT INTO questions(attempt_id, prompt, state, created_at_ms)
        VALUES (1, 'Choose an option', 'open', 110)",
        [],
    )?;
    store.wait_attempt(WaitAttempt {
        task_id: id,
        expected_version: 2,
        execution_token: "question",
        reason: "question",
        resume_at_ms: None,
        active_delta_ms: 10,
        now_ms: 110,
    })?;
    let resume = || StartAttempt {
        task_id: id,
        expected_version: 3,
        execution_token: "question",
        now_ms: 120,
    };
    assert!(store.resume_attempt(resume()).is_err());
    store.connection().execute(
        "UPDATE questions SET state = 'answered', answer = 'Option A',
        answered_by = 'owner', resolved_at_ms = 120",
        [],
    )?;
    store.resume_attempt(resume())?;
    assert_eq!(
        scalar(store.connection(), "SELECT count(*) FROM attempts")?,
        1
    );
    assert_eq!(
        scalar(
            store.connection(),
            "SELECT count(*) FROM attempts WHERE state = 'running'"
        )?,
        1
    );
    Ok(())
}

#[test]
fn normalized_provider_usage_has_identical_meaning_and_safe_aggregate_totals() -> Result {
    use ktask_rs_mk3::storage::usage::TokenUsage;
    let scratch = support::sandbox()?;
    let store = Database::open_in(scratch.path())?;
    let db = store.connection();
    let p = project(db, "usage")?;
    let inclusive = TokenUsage::from_inclusive(Some(20831), Some(16279), Some(4542), Some(42))?;
    let exclusive = TokenUsage::from_uncached(Some(10), Some(16279), Some(4542), Some(42))?;
    assert_eq!(inclusive, exclusive);
    assert_eq!(inclusive.total_input(), Some(20831));
    for (provider, usage) in [
        ("inclusive-provider", inclusive),
        ("exclusive-provider", exclusive),
    ] {
        db.execute("INSERT INTO model_calls(project_id, role, provider, model, request_key, state, started_at_ms, finished_at_ms,
            input_tokens, cache_read_tokens, cache_write_tokens, output_tokens)
            VALUES (?1, 'planning', ?2, 'test', ?2, 'succeeded', 100, 110, ?3, ?4, ?5, ?6)",
            params![p, provider, usage.input_tokens, usage.cache_read_tokens, usage.cache_write_tokens, usage.output_tokens])?;
    }
    assert_eq!(scalar(db, "SELECT sum(input_tokens) FROM model_calls")?, 20);
    assert_eq!(
        scalar(
            db,
            "SELECT sum(input_tokens + cache_read_tokens + cache_write_tokens) FROM model_calls"
        )?,
        41662
    );
    assert!(TokenUsage::from_inclusive(Some(10), Some(11), Some(0), None).is_err());
    assert!(TokenUsage::from_uncached(Some(-1), None, None, None).is_err());
    let unknown = TokenUsage::from_inclusive(Some(100), None, Some(0), None)?;
    assert_eq!(unknown.input_tokens, None);
    assert_eq!(unknown.total_input(), None);
    Ok(())
}

#[test]
fn usage_projection_groups_roles_and_counts_unreported_calls() -> Result {
    use ktask_rs_mk3::application::usage_report;
    let scratch = support::sandbox()?;
    let store = Database::open_in(scratch.path())?;
    let db = store.connection();
    let project_id = project(db, "usage-projection")?;
    let task_id = task(db, project_id, "5")?;
    db.execute("INSERT INTO attempts(project_id,task_id,revision,number,state,execution_token,started_at_ms,finished_at_ms,outcome,failure_category) VALUES (?1,?2,1,1,'failed','usage-run',100,200,'failed','check')", params![project_id,task_id])?;
    let attempt_id = db.last_insert_rowid();
    for (ordinal, role) in [(1, "execution"), (2, "review")] {
        db.execute("INSERT INTO ai_sessions(project_id,task_id,role,provider,provider_scope,state,created_at_ms,last_used_at_ms) VALUES (?1,?2,?3,'codex','test','open',100,100)", params![project_id,task_id,role])?;
        let session = db.last_insert_rowid();
        db.execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,session_id,provider,model,effort,invocation,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,1,?4,?5,'succeeded',?6,'codex','model','high','start',100,150)", params![attempt_id,project_id,task_id,ordinal,role,session])?;
        let step = db.last_insert_rowid();
        db.execute("INSERT INTO model_calls(project_id,task_id,revision,step_id,role,provider,model,effort,request_key,state,started_at_ms,finished_at_ms,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_microusd) VALUES (?1,?2,1,?3,?4,'codex','model','high',?5,'succeeded',100,150,10,5,0,3,?6)", params![project_id,task_id,step,role,format!("call-{role}"),if ordinal == 1 {Some(100)} else {None}])?;
    }
    db.execute("INSERT INTO model_calls(project_id,role,provider,model,request_key,state,started_at_ms,finished_at_ms,input_tokens) VALUES (?1,'planning','codex','model','planning','succeeded',100,110,7)", [project_id])?;
    let (attempt, roles) = usage_report::attempt(db, attempt_id)?;
    assert_eq!(attempt.calls, 2);
    assert_eq!(attempt.input, Some(20));
    assert_eq!(attempt.cache_read, Some(10));
    assert_eq!(attempt.output, Some(6));
    // One call reported a cost, one did not: the known cost stays visible and the other counts.
    assert_eq!(attempt.cost_microusd, Some(100));
    assert_eq!(attempt.uncosted_calls, 1);
    assert_eq!(attempt.unreported_calls, 0);
    assert_eq!(roles.len(), 2);
    assert_eq!(roles[0].provider_elapsed_ms, 50);
    let project = usage_report::project(db, project_id)?;
    assert_eq!(project.calls, 3);
    assert_eq!(project.input, Some(27));
    assert_eq!(project.cache_read, Some(10));
    assert_eq!(project.cost_microusd, Some(100));
    assert_eq!(project.uncosted_calls, 2);
    assert_eq!(project.cost_label(), "$0.000100 + 2 call(s) without cost");
    let none = usage_report::UsageTotals::default();
    assert_eq!(none.cost_label(), "none yet");
    Ok(())
}

#[test]
fn concurrent_openers_migrate_once_and_wal_readers_see_only_commits() -> Result {
    let scratch = support::sandbox()?;
    let barrier = Arc::new(Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|_| {
            let root = scratch.path().to_owned();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                let db = Database::open_in(&root).unwrap();
                assert_eq!(
                    scalar(db.connection(), "SELECT count(*) FROM schema_migrations").unwrap(),
                    1
                );
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("opener panicked");
    }
    let mut writer = Database::open_in(scratch.path())?;
    let reader = Database::open_in(scratch.path())?;
    let tx = writer.connection_mut().transaction()?;
    project(&tx, "pending")?;
    assert_eq!(
        scalar(reader.connection(), "SELECT count(*) FROM projects")?,
        0
    );
    tx.commit()?;
    assert_eq!(
        scalar(reader.connection(), "SELECT count(*) FROM projects")?,
        1
    );
    Ok(())
}

#[test]
fn database_writer_process() -> Result {
    let Some(root) = std::env::var_os("KTASK_TEST_DATABASE_ROOT") else {
        return Ok(());
    };
    let mut store = Database::open_in(Path::new(&root))?;
    project(store.connection(), "committed")?;
    let tx = store.connection_mut().transaction()?;
    project(&tx, "uncommitted")?;
    println!("DATABASE_READY");
    std::io::stdout().flush()?;
    std::io::stdin().read_exact(&mut [0])?;
    Ok(())
}

struct Writer(Child);
impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_writer_recovers_only_committed_data_without_reseeding() -> Result {
    let scratch = support::sandbox()?;
    let mut writer = Writer(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "storage::database_writer_process", "--nocapture"])
            .env_clear()
            .env("HOME", scratch.path())
            .env("TMPDIR", scratch.path())
            .env("KTASK_TEST_DATABASE_ROOT", scratch.path())
            .current_dir(scratch.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let stdout = writer.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if line == "DATABASE_READY" {
                let _ = sender.send(());
                break;
            }
        }
    });
    let ready = receiver.recv_timeout(Duration::from_secs(5));
    writer.0.kill()?;
    writer.0.wait()?;
    reader.join().unwrap();
    ready?;
    let db = Database::open_in(scratch.path())?;
    assert_eq!(
        scalar(
            db.connection(),
            "SELECT count(*) FROM projects WHERE name = 'committed'"
        )?,
        1
    );
    assert_eq!(scalar(db.connection(), "SELECT count(*) FROM projects")?, 1);
    assert_eq!(
        db.connection()
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))?,
        "ok"
    );
    Ok(())
}
