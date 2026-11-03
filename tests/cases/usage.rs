//! Codex reports a resumed session's running totals; each step must store only its own share.
use crate::storage::task;
use crate::support::{self, Result};
use ktask_rs_mk3::application::{ModelConfig, Service, VersionedTask};
use ktask_rs_mk3::storage::{Database, usage::TokenUsage};

fn totals(input: i64, cached: i64, output: i64) -> Result<TokenUsage> {
    Ok(TokenUsage::from_inclusive(
        Some(input + cached),
        Some(cached),
        Some(0),
        Some(output),
    )?)
}

#[test]
fn resumed_codex_session_steps_store_their_own_share_and_a_new_session_starts_over() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path().canonicalize()?;
    let db = Database::open_in(&root.join("state"))?;
    db.connection().execute(
        "INSERT INTO projects(name,root_path,created_at_ms) VALUES ('usage',?1,100)",
        [root.as_os_str().as_encoded_bytes()],
    )?;
    let project_id = db.connection().last_insert_rowid();
    let id = task(db.connection(), project_id, "1")?;
    db.connection().execute(
        "UPDATE revision_models SET provider='codex', model=role, effort=NULL WHERE task_id=?1",
        [id],
    )?;
    let mut service = Service::find(db, &root)?.expect("registered project");
    let run = service.begin_run(VersionedTask { id, version: 1 }, "usage", "base", true)?;
    let model = ModelConfig {
        provider: "codex".into(),
        model: "execution".into(),
        effort: None,
    };
    // Measured with gpt-6-luna: each resumed turn reports the session's running totals.
    let first = service.start_ai_step(&run, "execution", &model, "codex", false)?;
    service.record_session(&first, "thread-1")?;
    let share = service.step_share(&first, totals(14_395, 0, 5)?)?;
    assert_eq!(share, totals(14_395, 0, 5)?);
    service.record_usage(&first, &share, None)?;
    service.finish_step(&first, true, "first")?;

    let second = service.start_ai_step(&run, "execution", &model, "codex", false)?;
    assert_eq!(second.provider_session.as_deref(), Some("thread-1"));
    let share = service.step_share(&second, totals(14_409, 14_400, 10)?)?;
    assert_eq!(share, totals(14, 14_400, 5)?);
    service.record_usage(&second, &share, None)?;
    service.finish_step(&second, true, "second")?;

    let third = service.start_ai_step(&run, "execution", &model, "codex", false)?;
    let share = service.step_share(&third, totals(15_304, 28_400, 15)?)?;
    assert_eq!(share, totals(895, 14_000, 5)?);
    service.record_usage(&third, &share, None)?;
    service.finish_step(&third, true, "third")?;
    let usage = service.project_usage()?;
    assert_eq!(
        (usage.input, usage.cache_read, usage.output),
        (Some(15_304), Some(28_400), Some(15))
    );

    // A count below what the session recorded means the provider started over.
    let restarted = service.start_ai_step(&run, "execution", &model, "codex", false)?;
    assert_eq!(
        service.step_share(&restarted, totals(900, 0, 2)?)?,
        totals(900, 0, 2)?
    );
    Ok(())
}
