use super::*;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[test]
fn physical_accounting_identifies_per_project_runner_segments() -> Result<()> {
    let (sandbox, service, _) = closed_service()?;
    let state = sandbox.path().join("state");
    std::fs::write(
        state.join(format!("runner-{}.log", service.project_id)),
        vec![b'a'; 8192],
    )?;
    std::fs::write(
        state.join(format!("runner-{}.log.1", service.project_id)),
        vec![b'b'; 8192],
    )?;
    let preview = service.storage_preview()?;
    assert!(preview.runner_log_bytes >= 16 * 1024);
    Ok(())
}

#[test]
fn alternating_projects_do_not_recharge_unchanged_runner_logs() -> Result<()> {
    let (sandbox, service, _) = closed_service()?;
    let state = sandbox.path().join("state");
    let other_root = sandbox.path().join("other-project");
    std::fs::create_dir(&other_root)?;
    let other = Service::open(
        crate::storage::Database::open_in(&state)?,
        &other_root,
        None,
    )?;
    std::fs::write(
        state.join(format!("runner-{}.log", service.project_id)),
        vec![b'a'; 8192],
    )?;
    let other_log = state.join(format!("runner-{}.log", other.project_id));
    std::fs::write(&other_log, vec![b'b'; 96 * 1024])?;
    for index in 0..300 {
        std::fs::create_dir_all(state.join(format!("evidence/held-{index}")))?;
    }
    probe_known_allocation(&service.db, service.project_id)?;
    probe_known_allocation(&other.db, other.project_id)?;
    let claimed = projected_allocation(&service.db)?;
    let generation = admission_generation(service.db.connection())?;
    for _ in 0..6 {
        probe_known_allocation(&service.db, service.project_id)?;
        probe_known_allocation(&other.db, other.project_id)?;
    }
    assert_eq!(admission_generation(service.db.connection())?, generation);
    assert_eq!(projected_allocation(&service.db)?.bytes, claimed.bytes);
    let concurrent = std::thread::spawn(move || -> Result<Service> {
        for _ in 0..40 {
            probe_known_allocation(&other.db, other.project_id)?;
            std::thread::yield_now();
        }
        Ok(other)
    });
    // Each scan step also grows the WAL, and a concurrent probe legitimately charges a
    // growth of 64 KiB or more, which the settling scan adds to its measurement. The
    // scan must settle; unchanged runner logs never add a charge.
    let mut settled = false;
    let mut after_probes = 0;
    while !settled && after_probes < 30 {
        settled = bounded_allocation_scan(&service.db)? == ScanProgress::Settled;
        if concurrent.is_finished() {
            after_probes += 1;
        }
    }
    let other = concurrent.join().expect("runner probe panicked")?;
    assert!(settled, "unchanged logs kept invalidating the bounded scan");
    std::fs::rename(
        &other_log,
        state.join(format!("runner-{}.log.1", other.project_id)),
    )?;
    std::fs::write(&other_log, vec![b'c'; 4096])?;
    probe_known_allocation(&other.db, other.project_id)?;
    let rotated = projected_allocation(&other.db)?.bytes;
    assert!(rotated >= claimed.bytes);
    drop(other);
    let reopened = Service::open(
        crate::storage::Database::open_in(&state)?,
        &other_root,
        None,
    )?;
    probe_known_allocation(&reopened.db, reopened.project_id)?;
    assert_eq!(projected_allocation(&reopened.db)?.bytes, rotated);
    Ok(())
}

#[test]
fn completed_allocation_scan_remeasures_successive_vacuum_batches() -> Result<()> {
    let (_sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "vacuum",
        "fixture-base",
        false,
    )?;
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,?2)", params![run.attempt,"x".repeat(4 * 1024 * 1024)])?;
    service
        .db
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    service.db.connection().execute(
        "DELETE FROM attempt_output WHERE attempt_id=?1",
        [run.attempt],
    )?;
    service
        .db
        .connection()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let initial = allocation(service.db.state_dir())?;
    let policy = service.retention_policy()?;
    service.set_storage_policy(
        policy,
        policy.days,
        (initial.bytes * 3 / 2).max(1_048_576),
        policy.max_files,
    )?;
    let free: i64 = service
        .db
        .connection()
        .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    assert!(
        free > 512,
        "fixture did not leave multiple vacuum batches: {free}"
    );
    for _ in 0..8 {
        if bounded_allocation_scan(&service.db)? == ScanProgress::Settled {
            break;
        }
    }
    assert_eq!(scan_state(service.db.connection())?.complete, 2);
    assert!(
        service
            .preview_with_allocation(
                service.project_id,
                CLEANUP_BATCH,
                false,
                &projected_allocation(&service.db)?
            )?
            .launch_capacity_reached
    );
    let mut recovered = false;
    for _ in 0..20 {
        let (preview, _) = service.automatic_housekeeping_progress(1)?;
        if !preview.launch_capacity_reached {
            recovered = true;
            break;
        }
    }
    let physical = allocation(service.db.state_dir())?;
    assert!(
        recovered,
        "vacuum freed bytes but cached capacity remained blocked: physical={} projected={}",
        physical.bytes,
        projected_allocation(&service.db)?.bytes
    );
    assert!(physical.bytes < initial.bytes);
    Ok(())
}

#[test]
fn database_without_incremental_vacuum_converts_once_at_a_safe_point() -> Result<()> {
    let (_sandbox, mut service, _) = closed_service()?;
    let db = service.db.connection();
    db.execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")?;
    let mode = |db: &Connection| -> rusqlite::Result<i64> {
        db.pragma_query_value(None, "auto_vacuum", |r| r.get(0))
    };
    assert_eq!(mode(db)?, 0);
    let note = service
        .storage_preview()?
        .vacuum_note
        .expect("limitation is reported");
    assert!(note.contains("auto_vacuum off"), "{note}");
    // Live work in the instance defers the conversion.
    service.db.connection().execute(
        "INSERT INTO evidence_pins(id,project_id,directory_identity,owner_identity,guardian_identity) VALUES ('00000000000000000000000000000001',?1,'d','o','g')",
        [service.project_id],
    )?;
    assert_eq!(service.enable_incremental_vacuum()?, None);
    assert_eq!(mode(service.db.connection())?, 0);
    service
        .db
        .connection()
        .execute("DELETE FROM evidence_pins", [])?;
    assert!(service.enable_incremental_vacuum()?.is_some());
    assert_eq!(mode(service.db.connection())?, 2);
    assert!(service.storage_preview()?.vacuum_note.is_none());
    assert_eq!(service.enable_incremental_vacuum()?, None);
    Ok(())
}

#[test]
fn failed_vacuum_conversion_is_deferred_reported_and_never_an_error() -> Result<()> {
    let (sandbox, mut service, _) = closed_service()?;
    service
        .db
        .connection()
        .execute_batch("PRAGMA auto_vacuum=NONE; VACUUM;")?;
    // VACUUM cannot run inside a transaction: a deterministic non-busy failure.
    service.db.connection().execute_batch("BEGIN")?;
    assert_eq!(service.enable_incremental_vacuum()?, None);
    service.db.connection().execute_batch("ROLLBACK")?;
    let note = service.storage_preview()?.vacuum_note.expect("reported");
    assert!(
        note.contains("failed and is deferred") && note.contains("transaction"),
        "{note}"
    );
    // This process does not retry, even once the obstacle is gone.
    assert_eq!(service.enable_incremental_vacuum()?, None);
    let mode: i64 = service
        .db
        .connection()
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    assert_eq!(mode, 0);
    drop(service);
    let mut next = Service::open(
        crate::storage::Database::open_in(&sandbox.path().join("state"))?,
        &sandbox.path().join("project"),
        None,
    )?;
    assert!(next.enable_incremental_vacuum()?.is_some());
    assert!(next.storage_preview()?.vacuum_note.is_none());
    Ok(())
}

/// Hold enough entries that a bounded scan needs several calls, and start one.
fn scan_in_progress(sandbox: &crate::scratch::Scratch, service: &Service) -> Result<i64> {
    for index in 0..200 {
        std::fs::create_dir_all(sandbox.path().join(format!("state/evidence/held-{index}")))?;
    }
    bump_admission_generation(service.db.connection())?;
    assert_eq!(bounded_allocation_scan(&service.db)?, ScanProgress::Visited);
    Ok(scan_state(service.db.connection())?.walk.visited)
}

/// Advance the scan to its settlement, proving it never restarts from zero.
fn settle(service: &Service, mut visited: i64) -> Result<ScanState> {
    for _ in 0..20 {
        let progress = bounded_allocation_scan(&service.db)?;
        let scan = scan_state(service.db.connection())?;
        assert!(scan.walk.visited >= visited, "the scan restarted");
        visited = scan.walk.visited;
        if progress == ScanProgress::Settled {
            return Ok(scan);
        }
    }
    panic!("bounded scan did not settle");
}

#[test]
fn admission_during_a_scan_is_kept_by_its_settlement_and_competing_scans_are_rejected() -> Result<()>
{
    let (sandbox, service, _) = closed_service()?;
    let visited = scan_in_progress(&sandbox, &service)?;
    let walking = scan_state(service.db.connection())?;
    let tx = Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
    admit_capacity(&tx, 0, 1024 * 1024, 1, 0, 0)?;
    tx.commit()?;
    let scan = settle(&service, visited)?;
    assert_eq!(scan.complete, 2);
    let settled = projected_allocation(&service.db)?;
    assert!(
        settled.bytes >= scan.walk.usage.bytes + 1024 * 1024,
        "the settled projection lost an admission made during the scan: {} < {} + 1 MiB",
        settled.bytes,
        scan.walk.usage.bytes
    );
    // A competing scan holding an older revision, or this one again, cannot settle.
    assert!(!settle_scan(&service.db, walking.revision, &walking.walk)?);
    assert!(!settle_scan(&service.db, scan.revision, &scan.walk)?);
    assert_eq!(projected_allocation(&service.db)?.bytes, settled.bytes);
    // Admitted claims keep the result conservative without another full walk.
    assert_eq!(bounded_allocation_scan(&service.db)?, ScanProgress::Settled);
    assert_eq!(projected_allocation(&service.db)?.bytes, settled.bytes);
    Ok(())
}

#[test]
fn completed_artifact_invalidates_measurement_before_releasing_reservation() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let attempt = service
        .begin_run(
            VersionedTask { id: task, version },
            "artifact-transition",
            "fixture-base",
            false,
        )?
        .attempt;
    let relative = format!("evidence/attempt-{attempt}/step-1/file-1-0");
    let tx = Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
    admit_capacity(&tx, 0, 0, 0, 8192, 1)?;
    tx.execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'proof','text/plain',?2,8192,?3,?4,1)", params![attempt,relative,"a".repeat(64),"b".repeat(40)])?;
    tx.commit()?;
    // The walk starts while the copy is reserved; it completes before the walk ends,
    // and the walk never sees its file.
    let visited = scan_in_progress(&sandbox, &service)?;
    service.finish_reservation(&relative, true)?;
    let scan = settle(&service, visited)?;
    let claim = 8192 + 4 * 4096;
    assert!(
        projected_allocation(&service.db)?.bytes >= scan.walk.usage.bytes + claim,
        "settlement dropped the claim of an artifact published during the scan"
    );
    let stored: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM artifacts WHERE relative_path=?1",
        [relative],
        |r| r.get(0),
    )?;
    assert_eq!(stored, 1);
    Ok(())
}

#[test]
fn bounded_allocation_cursor_survives_concurrent_output_and_restart() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let evidence = sandbox.path().join("state/evidence");
    for index in 0..1200 {
        std::fs::create_dir_all(evidence.join(format!("held-{index}")))?;
    }
    let other_root = sandbox.path().join("other-project");
    std::fs::create_dir(&other_root)?;
    let state = sandbox.path().join("state");
    let mut other = Service::open(
        crate::storage::Database::open_in(&state)?,
        &other_root,
        None,
    )?;
    let definition = service.task(task)?.unwrap().definition().definition.clone();
    let other_task = other.create(&[definition], None, true)?[0];
    let version = other.current_task()?.unwrap().version;
    let run = other.begin_run(
        VersionedTask {
            id: other_task,
            version,
        },
        "scan-output",
        "fixture-base",
        false,
    )?;
    let delivery = std::thread::spawn(move || -> Result<u128> {
        let mut longest = 0;
        for _ in 0..32 {
            let started = std::time::Instant::now();
            other.output(&run, "stdout", "concurrent output")?;
            longest = longest.max(started.elapsed().as_millis());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok(longest)
    });
    let mut calls = 0;
    let mut previous_visited = 0;
    for _ in 0..8 {
        let _ = bounded_allocation_scan(&service.db)?;
        calls += 1;
        // Concurrent output can contend the very first call before it records a scan.
        let visited: i64 = service
            .db
            .connection()
            .query_row(
                "SELECT entries_visited FROM instance_allocation_scan WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        assert!(visited.saturating_sub(previous_visited) <= CLEANUP_BATCH);
        previous_visited = visited;
    }
    drop(service);
    let longest = delivery.join().expect("output thread panicked")?;
    let service = Service::open(
        crate::storage::Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    loop {
        let progress = bounded_allocation_scan(&service.db)?;
        calls += 1;
        // Concurrent output can contend the very first call before it records a scan.
        let visited: i64 = service
            .db
            .connection()
            .query_row(
                "SELECT entries_visited FROM instance_allocation_scan WHERE id=1",
                [],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        assert!(visited.saturating_sub(previous_visited) <= CLEANUP_BATCH);
        previous_visited = visited;
        if progress == ScanProgress::Settled {
            break;
        }
        assert!(calls < 100, "bounded scan did not settle");
    }
    let scan = scan_state(service.db.connection())?;
    let physical = allocation(service.db.state_dir())?;
    assert_eq!(scan.walk.usage.files, physical.files);
    assert!(scan.walk.visited > 1200);
    assert!(longest < 5000, "concurrent output blocked {longest} ms");
    println!(
        "bounded allocation visited={} checkpoints={calls} longest_output_commit_ms={longest}",
        scan.walk.visited
    );
    Ok(())
}

#[test]
fn capacity_cleanup_keeps_cross_attempt_check_proof_and_counts_allocation() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    let source = fixture
        .first_directory
        .join(format!("step-{}.log", fixture.first_step));
    std::fs::write(&source, "required check transcript")?;
    service.db.connection().execute(
        "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'check','text/plain',?2,25,1)",
        params![fixture.first_attempt, format!("evidence/attempt-{}/step-{}.log", fixture.first_attempt, fixture.first_step)],
    )?;
    service.db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'source output')",
        [fixture.first_attempt],
    )?;
    service.db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'eligible output')",
        [fixture.later_attempt],
    )?;
    let (task, revision): (i64, i64) = service.db.connection().query_row(
        "SELECT task_id,revision FROM attempts WHERE id=?1",
        [fixture.later_attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    service.db.connection().execute(
        "INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,reused_from_step_id,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,81,'check','succeeded','true','.',1000,?5,1,2)",
        params![fixture.later_attempt,service.project_id,task,revision,fixture.first_step],
    )?;
    let policy = service.retention_policy()?;
    service.set_storage_policy(policy, 0, 1_048_576, 10_000)?;
    let large_log = fixture.state.join("runner.log");
    std::fs::write(&large_log, vec![1_u8; 2 * 1024 * 1024])?;
    let before = service.storage_preview()?;
    assert!(before.over_capacity);
    assert!(before.runner_log_bytes >= 2 * 1024 * 1024);
    assert!(before.database_bytes > 0);
    assert!(before.directory_entries >= 80);
    assert_eq!(before.candidates.len(), 1);
    assert_eq!(before.candidates[0].attempt_id, fixture.later_attempt);
    let after = service.automatic_housekeeping()?;
    assert!(
        after.over_capacity,
        "protected allocation must remain visible"
    );
    assert!(source.exists());
    assert_eq!(
        std::fs::read_to_string(&source)?,
        "required check transcript"
    );
    let retained: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(retained, 1);
    Ok(())
}

#[test]
fn automatic_housekeeping_drains_large_closed_history_and_empty_directories() -> Result<()> {
    let (sandbox, mut service, first) = closed_service()?;
    let definition = Definition {
        title: "History".into(),
        goal: "Bound retained evidence".into(),
        kind: Kind::Ai,
        category: None,
        lenses: None,
        criteria: vec!["Done".into()],
        models: std::array::from_fn(|_| {
            Some(ModelConfig {
                provider: "claude".into(),
                model: "sonnet".into(),
                effort: None,
            })
        }),
        attempt_limit: Some(1),
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    let mut tasks = vec![first];
    tasks.extend(service.create(&vec![definition; 204], None, true)?);
    let state = sandbox.path().join("state");
    let noisy_output = "x".repeat(32 * 1024);
    for task in tasks {
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            &format!("history-{task}"),
            "fixture-base",
            false,
        )?;
        service.output(&run, "system", &noisy_output)?;
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        let directory = state.join(format!("evidence/attempt-{}", run.attempt));
        std::fs::create_dir_all(&directory)?;
        let relative = format!("evidence/attempt-{}/step-999.log", run.attempt);
        std::fs::write(state.join(&relative), "proof")?;
        service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'proof','text/plain',?2,5,1)", params![run.attempt,relative])?;
    }
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let before = service.storage_preview()?;
    assert_eq!(before.candidates.len(), 205);
    assert!(before.allocated_bytes > 6 * 1024 * 1024);
    let other_root = sandbox.path().join("other-project");
    std::fs::create_dir(&other_root)?;
    let mut other = Service::open(
        crate::storage::Database::open_in(&state)?,
        &other_root,
        None,
    )?;
    let other_definition = service
        .task(first)?
        .unwrap()
        .definition()
        .definition
        .clone();
    let other_task = other.create(&[other_definition], None, true)?[0];
    let version = other.current_task()?.unwrap().version;
    let other_run = other.begin_run(
        VersionedTask {
            id: other_task,
            version,
        },
        "concurrent-output",
        "fixture-base",
        false,
    )?;
    let other_attempt = other_run.attempt;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let ready = barrier.clone();
    let delivery = std::thread::spawn(move || -> Result<(Service, u128)> {
        ready.wait();
        let mut longest_ms = 0;
        for _ in 0..32 {
            let started = std::time::Instant::now();
            other.output(&other_run, "stdout", &"live".repeat(128))?;
            longest_ms = longest_ms.max(started.elapsed().as_millis());
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        Ok((other, longest_ms))
    });
    barrier.wait();
    for _ in 0..16 {
        let after = service.automatic_housekeeping()?;
        if after.candidates.is_empty()
            && after.evidence_bytes == 0
            && after.reusable_database_bytes == 0
        {
            break;
        }
    }
    let (other, longest_ms) = delivery.join().expect("output thread panicked")?;
    let delivered: i64 = other.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [other_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(delivered, 32);
    println!("concurrent project output rows={delivered} longest_commit_ms={longest_ms}");
    assert!(
        longest_ms < 5000,
        "other project's output blocked for {longest_ms} ms"
    );
    let after = service.storage_preview()?;
    let mode: i64 = service
        .db
        .connection()
        .pragma_query_value(None, "auto_vacuum", |r| r.get(0))?;
    let pages: i64 = service
        .db
        .connection()
        .pragma_query_value(None, "page_count", |r| r.get(0))?;
    let free: i64 = service
        .db
        .connection()
        .pragma_query_value(None, "freelist_count", |r| r.get(0))?;
    println!(
        "allocation before={} db={} wal={}; after={} db={} wal={}; mode={mode} pages={pages} free={free}",
        before.allocated_bytes,
        before.database_bytes,
        before.wal_bytes,
        after.allocated_bytes,
        after.database_bytes,
        after.wal_bytes
    );
    assert!(after.candidates.is_empty());
    assert_eq!(after.evidence_bytes, 0);
    assert!(after.allocated_files < before.allocated_files);
    assert!(
        after.database_bytes < 4 * 1024 * 1024,
        "SQLite allocation did not return after output pruning: {} bytes",
        after.database_bytes
    );
    let history: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM tasks WHERE project_id=?1 AND closed_at_ms IS NOT NULL",
        [service.project_id],
        |r| r.get(0),
    )?;
    assert_eq!(history, 205);
    Ok(())
}

#[test]
fn paused_and_waiting_attempts_keep_output_while_closed_history_is_cleaned() -> Result<()> {
    for state in ["paused", "waiting"] {
        let fixture = sweep_fixture()?;
        let mut service = fixture.service;
        let definition = Definition {
            title: "Resumable".into(),
            goal: "Keep current evidence".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models: std::array::from_fn(|_| {
                Some(ModelConfig {
                    provider: "claude".into(),
                    model: "sonnet".into(),
                    effort: None,
                })
            }),
            attempt_limit: Some(2),
            timeout_ms: None,
            stage_timeout_ms: None,
        };
        let task = service.create(&[definition], None, true)?[0];
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            &format!("resumable-{state}"),
            "fixture-base",
            false,
        )?;
        service.output(&run, "system", "keep current proof")?;
        service.finish_run(&run, state, None, "Waiting for continuation", None)?;
        service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'closed proof')", [fixture.first_attempt])?;
        let preview = service.storage_preview()?;
        assert!(!preview.deferred_for_live_work, "{state}");
        assert!(
            preview
                .candidates
                .iter()
                .all(|c| c.attempt_id != run.attempt)
        );
        assert!(
            preview
                .candidates
                .iter()
                .any(|c| c.attempt_id == fixture.first_attempt)
        );
        service.cleanup_storage(&preview)?;
        let retained: i64 = service.db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        assert_eq!(retained, 1, "{state}");
    }
    Ok(())
}

#[test]
fn shared_admission_stops_live_output_and_artifacts_with_recovery_headroom() -> Result<()> {
    let (sandbox, mut first, task) = closed_service()?;
    let second_root = sandbox.path().join("second-project");
    std::fs::create_dir(&second_root)?;
    let state = sandbox.path().join("state");
    let mut second = Service::open(
        crate::storage::Database::open_in(&state)?,
        &second_root,
        None,
    )?;
    let definition = first.task(task)?.unwrap().definition().definition.clone();
    let second_task = second.create(&[definition], None, true)?[0];
    let observed = first.storage_preview()?;
    let limit = (observed.allocated_bytes + 1024 * 1024).max(1_048_576);
    let policy = first.retention_policy()?;
    first.set_storage_policy(policy, 30, limit, 10_000)?;
    let other_preview = second.storage_preview()?;
    assert_eq!(other_preview.effective_max_bytes, limit);
    assert_eq!(other_preview.policy.max_bytes, DEFAULT_MAX_BYTES);
    let version = first.current_task()?.unwrap().version;
    let run_first = first.begin_run(
        VersionedTask { id: task, version },
        "shared-first",
        "fixture-base",
        false,
    )?;
    let version = second.current_task()?.unwrap().version;
    let run_second = second.begin_run(
        VersionedTask {
            id: second_task,
            version,
        },
        "shared-second",
        "fixture-base",
        false,
    )?;
    let chunk = "x".repeat(16 * 1024);
    let mut accepted = 0;
    let mut denied = false;
    for _ in 0..100 {
        let (service, run) = if accepted % 2 == 0 {
            (&first, &run_first)
        } else {
            (&second, &run_second)
        };
        match service.output(run, "stdout", &chunk) {
            Ok(()) => accepted += 1,
            Err(Error::Capacity(refusal))
                if refusal.reason.starts_with("Resource limit: shared storage") =>
            {
                denied = true;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    assert!(
        denied && accepted >= 2,
        "both projects should share a finite allowance"
    );
    let source = sandbox.path().join("proof.bin");
    std::fs::write(&source, vec![b'p'; 128 * 1024])?;
    let relative = format!("evidence/attempt-{}/step-999/file-1-0", run_second.attempt);
    assert!(
        matches!(second.publish_artifact(run_second.attempt, &run_second.token, "proof.bin", &source, &relative, &"a".repeat(40)), Err(Error::Capacity(refusal)) if refusal.reason.starts_with("Resource limit: shared storage"))
    );
    assert!(
        !state.join(&relative).exists(),
        "denied publication created evidence"
    );
    assert!(
        first.storage_preview()?.allocated_bytes < limit,
        "recovery reserve was consumed"
    );
    let first_rows: i64 = first.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [run_first.attempt],
        |r| r.get(0),
    )?;
    let second_rows: i64 = second.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [run_second.attempt],
        |r| r.get(0),
    )?;
    assert!(first_rows > 0 && second_rows > 0);
    Ok(())
}

#[test]
fn long_output_attempt_is_admitted_against_physical_growth() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let ballast = std::fs::File::create(sandbox.path().join("state/ballast"))?;
    nix::fcntl::posix_fallocate(&ballast, 0, 300 * 1024 * 1024).map_err(io)?;
    let policy = service.retention_policy()?;
    service.set_storage_policy(policy, 30, 1 << 30, 100_000)?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "long-output",
        "fixture-base",
        false,
    )?;
    let before = allocation(service.db.state_dir())?.bytes;
    service.db.connection().execute(
        "UPDATE instance_storage_admission SET projected_bytes=?1,generation=generation+1",
        [before],
    )?;
    let row = "x".repeat(2048);
    for _ in 0..120 {
        let batch = vec![("stdout".to_owned(), row.clone()); 100];
        service.output_rows(&run, &batch)?;
    }
    let physical = allocation(service.db.state_dir())?.bytes - before;
    let projected = projected_allocation(&service.db)?.bytes - before;
    assert!(physical >= 24_000_000, "{physical}");
    assert!(projected <= 2 * physical, "{projected} vs {physical}");
    Ok(())
}

#[test]
fn shared_file_budget_rejects_publication_before_creating_more_directories() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let observed = service.storage_preview()?;
    let limit = (observed.allocated_files + 12).max(16);
    let policy = service.retention_policy()?;
    service.set_storage_policy(policy, 30, policy.max_bytes, limit)?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "file-budget",
        "fixture-base",
        false,
    )?;
    let source = sandbox.path().join("empty-proof");
    std::fs::write(&source, b"")?;
    let mut accepted = 0;
    let mut rejected = None;
    for index in 0..100 {
        let relative = format!("evidence/attempt-{}/step-999/file-1-{index}", run.attempt);
        match service.publish_artifact(
            run.attempt,
            &run.token,
            &format!("proof-{index}"),
            &source,
            &relative,
            &"a".repeat(40),
        ) {
            Ok(()) => accepted += 1,
            Err(Error::Capacity(refusal))
                if refusal.reason.starts_with("Resource limit: shared storage") =>
            {
                rejected = Some(relative);
                break;
            }
            Err(error) => return Err(error),
        }
    }
    assert!(accepted > 0 && rejected.is_some());
    assert!(
        !sandbox
            .path()
            .join("state")
            .join(rejected.unwrap())
            .exists()
    );
    assert!(service.storage_preview()?.allocated_files < limit);
    Ok(())
}

#[test]
fn cleanup_reclaims_after_a_wal_reader_releases_its_snapshot() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    let first_task: i64 = service.db.connection().query_row(
        "SELECT task_id FROM attempts WHERE id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    let other_state = fixture._sandbox.path().join("other-instance");
    let mut other = Service::open(
        crate::storage::Database::open_in(&other_state)?,
        &fixture.project,
        None,
    )?;
    let other_definition = service
        .task(first_task)?
        .unwrap()
        .definition()
        .definition
        .clone();
    let other_task = other.create(&[other_definition], None, true)?[0];
    let other_identity = std::fs::metadata(other_state.join("ktask.sqlite3"))?;
    let database = fixture.state.join("ktask.sqlite3");
    let identity = std::fs::metadata(&database)?;
    service.db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,?2)",
        params![fixture.first_attempt, "x".repeat(512 * 1024)],
    )?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let reader = crate::storage::Database::open_in(&fixture.state)?;
    reader.connection().execute_batch("BEGIN")?;
    let visible: i64 = reader.connection().query_row(
        "SELECT length(content) FROM attempt_output WHERE attempt_id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(visible, 512 * 1024);
    service.automatic_housekeeping()?;
    let still_visible: i64 = reader.connection().query_row(
        "SELECT length(content) FROM attempt_output WHERE attempt_id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(
        still_visible, visible,
        "reader snapshot changed during cleanup"
    );
    let retained: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(retained, 0);
    let pinned_wal = service.storage_preview()?.wal_bytes;
    assert!(
        pinned_wal > 64 * 1024,
        "reader did not pin a substantial WAL: {pinned_wal}"
    );
    reader.connection().execute_batch("COMMIT")?;
    service.automatic_housekeeping()?;
    let after = std::fs::metadata(&database)?;
    assert_eq!((identity.dev(), identity.ino()), (after.dev(), after.ino()));
    let history: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempts WHERE id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(history, 1);
    assert!(other.task(other_task)?.is_some());
    let after_other = std::fs::metadata(other_state.join("ktask.sqlite3"))?;
    assert_eq!(
        (other_identity.dev(), other_identity.ino()),
        (after_other.dev(), after_other.ino())
    );
    let reclaimed_wal = service.storage_preview()?.wal_bytes;
    assert!(
        reclaimed_wal < pinned_wal && reclaimed_wal <= 64 * 1024,
        "WAL was not reclaimed after reader release: {pinned_wal} -> {reclaimed_wal}"
    );
    Ok(())
}

#[test]
fn cleanup_waits_for_a_contending_writer_without_replacing_the_database() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    let database = fixture.state.join("ktask.sqlite3");
    let identity = std::fs::metadata(&database)?;
    service.db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'system',1,'writer contention')",
        [fixture.first_attempt],
    )?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let writer = crate::storage::Database::open_in(&fixture.state)?;
    writer.connection().execute_batch("BEGIN IMMEDIATE")?;
    let cleanup = std::thread::spawn(move || -> Result<(Service, Preview)> {
        let preview = service.automatic_housekeeping()?;
        Ok((service, preview))
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    writer.connection().execute_batch("COMMIT")?;
    let (service, _) = cleanup.join().expect("cleanup thread panicked")?;
    let after = std::fs::metadata(&database)?;
    assert_eq!((identity.dev(), identity.ino()), (after.dev(), after.ino()));
    let retained: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [fixture.first_attempt],
        |r| r.get(0),
    )?;
    assert_eq!(retained, 0);
    Ok(())
}

fn closed_service() -> Result<(crate::scratch::Scratch, Service, i64)> {
    let sandbox = crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
    let project = sandbox.path().join("project");
    std::fs::create_dir(&project)?;
    let state = sandbox.path().join("state");
    let mut service = Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
    let models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: None,
        })
    });
    let task = service.create(
        &[Definition {
            title: "Closed".into(),
            goal: "Test cleanup".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models,
            attempt_limit: Some(1),
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?[0];
    Ok((sandbox, service, task))
}

struct SweepFixture {
    service: Service,
    first_attempt: i64,
    first_step: i64,
    later_attempt: i64,
    first_directory: PathBuf,
    later_directory: PathBuf,
    state: PathBuf,
    project: PathBuf,
    _sandbox: crate::scratch::Scratch,
}

fn sweep_fixture() -> Result<SweepFixture> {
    let (sandbox, mut service, first_task) = closed_service()?;
    let project = sandbox.path().join("project");
    let state = sandbox.path().join("state");
    let version = service.current_task()?.unwrap().version;
    let first = service.begin_run(
        VersionedTask {
            id: first_task,
            version,
        },
        "earlier-failure",
        "fixture-base",
        false,
    )?;
    service.finish_run(&first, "failed", Some("check"), "Failed", None)?;
    let version = service.current_task()?.unwrap().version;
    service.mark_done(
        VersionedTask {
            id: first_task,
            version,
        },
        "Resolved",
    )?;
    let models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: None,
        })
    });
    let later_task = service.create(
        &[Definition {
            title: "Later".into(),
            goal: "Sweep".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models,
            attempt_limit: Some(1),
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?[0];
    let version = service.current_task()?.unwrap().version;
    let later = service.begin_run(
        VersionedTask {
            id: later_task,
            version,
        },
        "later-success",
        "fixture-base",
        false,
    )?;
    service.finish_run(&later, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let evidence = state.join("evidence");
    let first_directory = evidence.join(format!("attempt-{}", first.attempt));
    let later_directory = evidence.join(format!("attempt-{}", later.attempt));
    std::fs::create_dir_all(&first_directory)?;
    std::fs::create_dir_all(&later_directory)?;
    let first_revision: i64 = service.db.connection().query_row(
        "SELECT revision FROM attempts WHERE id=?1",
        [first.attempt],
        |row| row.get(0),
    )?;
    service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,1,'check','succeeded','true','.',1000,1,2)",params![first.attempt,service.project_id,first_task,first_revision])?;
    let first_step = service.db.connection().last_insert_rowid();
    std::fs::create_dir(first_directory.join(format!("step-{first_step}")))?;
    let later_revision: i64 = service.db.connection().query_row(
        "SELECT revision FROM attempts WHERE id=?1",
        [later.attempt],
        |row| row.get(0),
    )?;
    for ordinal in 1..=80 {
        service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)",params![later.attempt,service.project_id,later_task,later_revision,ordinal])?;
        let step = service.db.connection().last_insert_rowid();
        std::fs::create_dir(later_directory.join(format!("step-{step}")))?;
    }
    Ok(SweepFixture {
        service,
        first_attempt: first.attempt,
        first_step,
        later_attempt: later.attempt,
        first_directory,
        later_directory,
        state,
        project,
        _sandbox: sandbox,
    })
}

fn evidence_counts(root: &Path) -> Result<(usize, usize)> {
    if !root.exists() {
        return Ok((0, 0));
    }
    let mut files = 0;
    let mut directories = 0;
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            directories += 1;
            let children = evidence_counts(&entry.path())?;
            files += children.0;
            directories += children.1;
        } else {
            files += 1;
        }
    }
    Ok((files, directories))
}

fn drain_sweep_by_receipt(service: &mut Service, evidence: &Path) -> Result<Vec<bool>> {
    let mut limits = Vec::new();
    for _ in 0..6 {
        let preview = service.storage_preview()?;
        let receipt = service.cleanup_storage(&preview)?;
        let (files, directories) = evidence_counts(evidence)?;
        println!(
            "sweep receipt: files={files} directories={directories} batch_limit={}",
            receipt.batch_limit_reached
        );
        limits.push(receipt.batch_limit_reached);
        if !receipt.batch_limit_reached {
            return Ok(limits);
        }
    }
    Err(Error::Incompatible("Bounded sweep did not finish".into()))
}

fn reserve_first(service: &Service, attempt: i64, step: i64) -> Result<()> {
    let relative = format!("evidence/attempt-{attempt}/step-{step}/file-1-0");
    service.db.connection().execute("INSERT INTO artifact_reservations(attempt_id,name,media_type,relative_path,size_bytes,checksum_sha256,source_tree,created_at_ms) VALUES (?1,'interrupted','text/plain',?2,0,?3,?4,1)",params![attempt,relative,"0".repeat(64),"0".repeat(40)])?;
    Ok(())
}

#[test]
fn output_batches_keep_original_size_and_complete_marker() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "output-batches",
        "fixture-base",
        false,
    )?;
    for _ in 0..80 {
        service.output(&run, "stdout", "row")?;
    }
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let preview = service.storage_preview()?;
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    let remaining: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [run.attempt],
        |r| r.get(0),
    )?;
    assert_eq!(remaining, 16);
    let usage: (i64, i64) = service.db.connection().query_row(
        "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
        [run.attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(usage, (48, 16));
    let marker: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM output_pruning WHERE attempt_id=?1",
        [run.attempt],
        |r| r.get(0),
    )?;
    assert_eq!(marker, 0);
    let partial = &service.task(task)?.unwrap().attempts[0];
    assert!(!partial.output_pruned);
    assert_eq!(partial.output_pruning_bytes, 192);
    assert_eq!(partial.output_pruning_records, 64);
    assert!(
        service
            .browse_output_at(
                run.attempt,
                0,
                0,
                None,
                service.output_watermark(run.attempt)?
            )?
            .contains("PARTIALLY PRUNED: 192")
    );
    drop(service);
    let mut service = Service::open(
        crate::storage::Database::open_in(&sandbox.path().join("state"))?,
        &sandbox.path().join("project"),
        None,
    )?;
    assert_eq!(
        service.task(task)?.unwrap().attempts[0].output_pruning_bytes,
        192
    );
    assert_eq!(
        service.task(task)?.unwrap().attempts[0].output_pruning_records,
        64
    );
    let next = service.storage_preview()?;
    let second = service.cleanup_storage(&next)?;
    assert_eq!(second.bytes, 48);
    let size: i64 = service.db.connection().query_row(
        "SELECT size_bytes FROM output_pruning WHERE attempt_id=?1",
        [run.attempt],
        |r| r.get(0),
    )?;
    assert_eq!(size, 240);
    let progress: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM output_pruning_progress WHERE attempt_id=?1",
        [run.attempt],
        |r| r.get(0),
    )?;
    assert_eq!(progress, 0);
    let admission: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM evidence_cleanup_admissions WHERE attempt_id=?1",
        [run.attempt],
        |row| row.get(0),
    )?;
    assert_eq!(admission, 0);
    let complete = &service.task(task)?.unwrap().attempts[0];
    assert!(complete.output_pruned);
    assert_eq!(complete.output_pruning_bytes, 0);
    assert_eq!(complete.output_pruning_records, 0);
    Ok(())
}

#[test]
fn empty_output_rows_distinguish_deletion_from_pending_intent() -> Result<()> {
    let (_sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "empty-output",
        "fixture-base",
        false,
    )?;
    for _ in 0..65 {
        service.output(&run, "stdout", "")?;
    }
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let preview = service.storage_preview()?;
    service.cleanup_storage(&preview)?;
    let partial = &service.task(task)?.unwrap().attempts[0];
    assert_eq!(partial.output_pruning_bytes, 0);
    assert_eq!(partial.output_pruning_records, 64);
    let usage: (i64, i64) = service.db.connection().query_row(
        "SELECT bytes,rows FROM attempt_output_usage WHERE attempt_id=?1",
        [run.attempt],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(usage, (0, 1));
    assert!(
        service
            .browse_output_at(
                run.attempt,
                0,
                0,
                None,
                service.output_watermark(run.attempt)?
            )?
            .contains("PARTIALLY PRUNED: 0")
    );
    Ok(())
}

#[test]
fn empty_step_cleanup_resumes_inside_large_attempt() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "many-steps",
        "fixture-base",
        false,
    )?;
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let directory = sandbox
        .path()
        .join(format!("state/evidence/attempt-{}", run.attempt));
    std::fs::create_dir_all(&directory)?;
    let revision: i64 = service.db.connection().query_row(
        "SELECT revision FROM attempts WHERE id=?1",
        [run.attempt],
        |row| row.get(0),
    )?;
    for ordinal in 1..=80 {
        service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)", params![run.attempt,service.project_id,task,revision,ordinal])?;
        let step = service.db.connection().last_insert_rowid();
        std::fs::create_dir(directory.join(format!("step-{step}")))?;
    }
    let preview = service.storage_preview()?;
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    assert_eq!(std::fs::read_dir(&directory)?.count(), 16);
    let active: i64 = service.db.connection().query_row(
        "SELECT active_attempt_id FROM evidence_sweep WHERE project_id=?1",
        [service.project_id],
        |row| row.get(0),
    )?;
    assert_eq!(active, run.attempt);
    let preview = service.storage_preview()?;
    let second = service.cleanup_storage(&preview)?;
    assert_eq!(second.attempts, 1);
    assert!(!directory.exists());
    Ok(())
}

#[test]
fn pending_artifact_keeps_output_eligible_across_batches() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "pending-output",
        "fixture-base",
        false,
    )?;
    for _ in 0..80 {
        service.output(&run, "stdout", "row")?;
    }
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 365000)?;
    let directory = sandbox
        .path()
        .join(format!("state/evidence/attempt-{}", run.attempt));
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("step-987.log"), "pending")?;
    service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,pruning_requested_at_ms) VALUES (?1,'pending','text/plain',?2,7,1,2)", params![run.attempt,format!("evidence/attempt-{}/step-987.log",run.attempt)])?;
    let preview = service.storage_preview()?;
    assert_eq!(preview.candidates.len(), 1);
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    let preview = service.storage_preview()?;
    assert_eq!(preview.candidates.len(), 1);
    let second = service.cleanup_storage(&preview)?;
    assert_eq!(second.bytes, 51);
    assert!(
        !directory.exists(),
        "admitted cleanup left an empty directory after the policy changed"
    );
    let remaining: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [run.attempt],
        |row| row.get(0),
    )?;
    assert_eq!(remaining, 0);
    Ok(())
}

#[test]
fn sweep_receipt_requests_wrap_for_earlier_newly_closed_attempt() -> Result<()> {
    let (sandbox, mut service, first_task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let first = service.begin_run(
        VersionedTask {
            id: first_task,
            version,
        },
        "earlier-failure",
        "fixture-base",
        false,
    )?;
    service.finish_run(&first, "failed", Some("check"), "Failed", None)?;
    let version = service.current_task()?.unwrap().version;
    service.mark_done(
        VersionedTask {
            id: first_task,
            version,
        },
        "Resolved",
    )?;
    let models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: None,
        })
    });
    let later_task = service.create(
        &[Definition {
            title: "Later".into(),
            goal: "Sweep".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models,
            attempt_limit: Some(1),
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?[0];
    let version = service.current_task()?.unwrap().version;
    let later = service.begin_run(
        VersionedTask {
            id: later_task,
            version,
        },
        "later-success",
        "fixture-base",
        false,
    )?;
    service.finish_run(&later, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let evidence = sandbox.path().join("state/evidence");
    let first_directory = evidence.join(format!("attempt-{}", first.attempt));
    let later_directory = evidence.join(format!("attempt-{}", later.attempt));
    std::fs::create_dir_all(&first_directory)?;
    std::fs::create_dir_all(&later_directory)?;
    let revision: i64 = service.db.connection().query_row(
        "SELECT revision FROM attempts WHERE id=?1",
        [later.attempt],
        |row| row.get(0),
    )?;
    for ordinal in 1..=80 {
        service.db.connection().execute("INSERT INTO attempt_steps(attempt_id,project_id,task_id,revision,ordinal,role,state,command,working_directory,timeout_ms,started_at_ms,finished_at_ms) VALUES (?1,?2,?3,?4,?5,'check','succeeded','true','.',1000,1,2)",params![later.attempt,service.project_id,later_task,revision,ordinal])?;
        let step = service.db.connection().last_insert_rowid();
        std::fs::create_dir(later_directory.join(format!("step-{step}")))?;
    }
    service.db.connection().execute(
        "UPDATE tasks SET lifecycle='blocked',closed_at_ms=NULL WHERE id=?1",
        [first_task],
    )?;
    let preview = service.storage_preview()?;
    let first_receipt = service.cleanup_storage(&preview)?;
    assert!(first_receipt.batch_limit_reached);
    assert_eq!(std::fs::read_dir(&later_directory)?.count(), 16);
    service.db.connection().execute(
        "UPDATE tasks SET lifecycle='failed_skipped',closed_at_ms=?2 WHERE id=?1",
        params![first_task, now_ms()],
    )?;
    let mut receipts = Vec::new();
    for _ in 0..4 {
        let preview = service.storage_preview()?;
        let receipt = service.cleanup_storage(&preview)?;
        let done = !receipt.batch_limit_reached;
        receipts.push(receipt.batch_limit_reached);
        if done {
            break;
        }
    }
    assert_eq!(
        receipts,
        vec![true, false],
        "cleanup must request the wrapped pass and then finish"
    );
    assert!(!first_directory.exists());
    assert!(!later_directory.exists());
    Ok(())
}

#[test]
fn released_reservation_restarts_bounded_sweep_after_reopening() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    reserve_first(&service, fixture.first_attempt, fixture.first_step)?;
    let preview = service.storage_preview()?;
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    assert_eq!(evidence_counts(&fixture.state.join("evidence"))?, (0, 19));
    assert!(fixture.first_directory.exists());
    drop(service);
    let mut service = Service::open(
        crate::storage::Database::open_in(&fixture.state)?,
        &fixture.project,
        None,
    )?;
    let reservations: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1",
        [fixture.first_attempt],
        |row| row.get(0),
    )?;
    assert_eq!(
        reservations, 0,
        "reopening must reconcile the interrupted publication"
    );
    let dirty: bool = service.db.connection().query_row(
        "SELECT dirty FROM evidence_sweep WHERE project_id=?1",
        [service.project_id],
        |row| row.get(0),
    )?;
    assert!(dirty);
    let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
    assert!(limits.len() >= 2, "the later attempt exceeds one batch");
    assert!(
        !fixture.first_directory.exists(),
        "an earlier released reservation was stranded"
    );
    assert!(!fixture.later_directory.exists());
    assert_eq!(evidence_counts(&fixture.state.join("evidence"))?, (0, 0));
    Ok(())
}

#[test]
fn reserved_attempt_does_not_keep_receipts_open() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    reserve_first(&service, fixture.first_attempt, fixture.first_step)?;
    let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
    assert!(limits.len() >= 2);
    assert!(fixture.first_directory.exists());
    assert!(!fixture.later_directory.exists());
    let reservations: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM artifact_reservations WHERE attempt_id=?1",
        [fixture.first_attempt],
        |row| row.get(0),
    )?;
    assert_eq!(reservations, 1);
    Ok(())
}

#[test]
fn lowering_retention_restarts_behind_cursor() -> Result<()> {
    let fixture = sweep_fixture()?;
    let mut service = fixture.service;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 365000)?;
    service.db.connection().execute(
        "INSERT INTO evidence_cleanup_admissions(attempt_id,admitted_at_ms) VALUES (?1,1)",
        [fixture.later_attempt],
    )?;
    let preview = service.storage_preview()?;
    let first = service.cleanup_storage(&preview)?;
    assert!(first.batch_limit_reached);
    assert!(fixture.first_directory.exists());
    assert_eq!(std::fs::read_dir(&fixture.later_directory)?.count(), 16);
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
    assert!(!fixture.first_directory.exists());
    assert!(!fixture.later_directory.exists());
    Ok(())
}

#[test]
fn completed_sweep_reopens_after_age_deadline_without_a_mutation() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let definition = service.task(task)?.unwrap().definition().definition.clone();
    let version = service.current_task()?.unwrap().version;
    let first = service.begin_run(
        VersionedTask { id: task, version },
        "aging-first",
        "fixture-base",
        false,
    )?;
    service.finish_run(&first, "succeeded", None, "Done", None)?;
    let next = service.create(&[definition], None, true)?[0];
    let version = service.current_task()?.unwrap().version;
    let second = service.begin_run(
        VersionedTask { id: next, version },
        "aging-second",
        "fixture-base",
        false,
    )?;
    service.finish_run(&second, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 1)?;
    let deadline = now_ms() + 3000;
    service.db.connection().execute(
        "UPDATE attempts SET started_at_ms=?2,finished_at_ms=?3 WHERE id=?1",
        params![first.attempt, deadline - 86_400_001, deadline - 86_400_000],
    )?;
    let state = sandbox.path().join("state");
    let first_dir = state.join(format!("evidence/attempt-{}", first.attempt));
    let second_dir = state.join(format!("evidence/attempt-{}", second.attempt));
    std::fs::create_dir_all(&first_dir)?;
    std::fs::create_dir_all(&second_dir)?;
    service.automatic_housekeeping()?;
    let (complete, next_eligible): (bool, i64) = service.db.connection().query_row(
        "SELECT complete,next_eligible_at_ms FROM evidence_sweep WHERE project_id=?1",
        [service.project_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert!(complete);
    assert_eq!(next_eligible, deadline);
    assert!(first_dir.exists() && second_dir.exists());
    let changes_before = service.db.connection().total_changes();
    let mut budget = CLEANUP_BATCH;
    sweep_empty_evidence(
        service.db.connection(),
        service.db.state_dir(),
        service.project_id,
        &mut budget,
        false,
        false,
    )?
    .record(service.db.connection())?;
    assert_eq!(
        service.db.connection().total_changes(),
        changes_before,
        "a pre-deadline sweep restarted"
    );
    assert!(first_dir.exists() && second_dir.exists());
    drop(service);
    std::thread::sleep(std::time::Duration::from_millis(
        (deadline - now_ms()).max(0) as u64 + 30,
    ));
    let mut reopened = Service::open(
        crate::storage::Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    reopened.automatic_housekeeping()?;
    assert!(
        !first_dir.exists(),
        "the newly eligible empty directory was stranded"
    );
    assert!(second_dir.exists(), "a younger directory was removed");
    Ok(())
}

#[test]
fn preserved_step_does_not_keep_receipts_open() -> Result<()> {
    use std::os::unix::fs::symlink;
    let fixture = sweep_fixture()?;
    let target = fixture.state.join("unrelated");
    std::fs::create_dir(&target)?;
    std::fs::write(target.join("keep"), "preserved")?;
    let step = fixture
        .first_directory
        .join(format!("step-{}", fixture.first_step));
    std::fs::remove_dir(&step)?;
    symlink(&target, &step)?;
    let mut service = fixture.service;
    let limits = drain_sweep_by_receipt(&mut service, &fixture.state.join("evidence"))?;
    assert!(limits.len() >= 2);
    assert!(step.symlink_metadata()?.file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(target.join("keep"))?, "preserved");
    assert!(!fixture.later_directory.exists());
    Ok(())
}

#[test]
fn directory_phase_rechecks_protection_acquired_after_file_phase() -> Result<()> {
    let sandbox = crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
    let project = sandbox.path().join("project");
    std::fs::create_dir(&project)?;
    let state = sandbox.path().join("state");
    let mut service = Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
    let models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: None,
        })
    });
    let task = service.create(
        &[Definition {
            title: "Closed".into(),
            goal: "Test cleanup".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Done".into()],
            models,
            attempt_limit: Some(1),
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?[0];
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "retention-guard",
        "fixture-base",
        false,
    )?;
    service.finish_run(&run, "succeeded", None, "Done", None)?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    let directory = state.join(format!("evidence/attempt-{}", run.attempt));
    std::fs::create_dir_all(&directory)?;
    // This call represents the directory phase after the file transaction
    // committed. Another connection acquired a project-level provider call.
    let other = crate::storage::Database::open_in(&state)?;
    other.connection().execute("INSERT INTO model_calls(project_id,role,provider,model,request_key,state,started_at_ms) VALUES (?1,'planning','claude','sonnet','guard-call','running',1)", [service.project_id])?;
    let tx = Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
    let mut budget = CLEANUP_BATCH;
    assert!(
        sweep_empty_evidence(
            &tx,
            service.db.state_dir(),
            service.project_id,
            &mut budget,
            false,
            false,
        )
        .is_err()
    );
    tx.rollback()?;
    assert!(directory.exists());
    other.connection().execute(
        "UPDATE model_calls SET state='succeeded',finished_at_ms=2 WHERE request_key='guard-call'",
        [],
    )?;
    let tx = Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
    let mut budget = CLEANUP_BATCH;
    sweep_empty_evidence(
        &tx,
        service.db.state_dir(),
        service.project_id,
        &mut budget,
        false,
        false,
    )?
    .record(&tx)?;
    tx.commit()?;
    assert!(!directory.exists());
    Ok(())
}

/// Closed attempts of fresh copies of `task`'s definition, each succeeded and finished.
fn closed_attempts(service: &mut Service, task: i64, count: usize) -> Result<Vec<i64>> {
    let definition = service.task(task)?.unwrap().definition().definition.clone();
    let mut tasks = vec![task];
    if count > 1 {
        tasks.extend(service.create(&vec![definition; count - 1], None, true)?);
    }
    let mut attempts = Vec::new();
    for task in tasks {
        let version = service.current_task()?.unwrap().version;
        let run = service.begin_run(
            VersionedTask { id: task, version },
            &format!("closed-{task}"),
            "fixture-base",
            false,
        )?;
        service.finish_run(&run, "succeeded", None, "Done", None)?;
        attempts.push(run.attempt);
    }
    Ok(attempts)
}

/// Bytes this thread has read through the read(2) family, as SQLite reads its pages.
fn thread_read_bytes() -> Result<u64> {
    let counters = std::fs::read_to_string("/proc/thread-self/io")?;
    Ok(counters
        .lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .and_then(|value| value.parse().ok())
        .expect("rchar counter"))
}

#[test]
fn step_launch_reads_no_output_and_walks_no_tree() -> Result<()> {
    const MIB: i64 = 1024 * 1024;
    let (sandbox, mut service, task) = closed_service()?;
    let attempts = closed_attempts(&mut service, task, 20)?;
    let row = "x".repeat(MIB as usize);
    let tx = Transaction::new_unchecked(service.db.connection(), TransactionBehavior::Immediate)?;
    for attempt in &attempts {
        for sequence in 1..=5 {
            tx.execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,'stdout',1,?3)", params![attempt, sequence, row])?;
        }
    }
    // As recorded before the counters existed.
    tx.execute("DELETE FROM attempt_output_usage", [])?;
    tx.commit()?;
    drop(service);
    let state = sandbox.path().join("state");
    let project = sandbox.path().join("project");
    let before = thread_read_bytes()?;
    let service = Service::open(crate::storage::Database::open_in(&state)?, &project, None)?;
    let backfill = thread_read_bytes()? - before;
    let usage: (i64, i64, i64) = service.db.connection().query_row(
        "SELECT count(*),sum(bytes),sum(rows) FROM attempt_output_usage",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!(usage, (20, 100 * MIB, 100), "open backfilled every attempt");
    let before = thread_read_bytes()?;
    drop(Service::open(
        crate::storage::Database::open_in(&state)?,
        &project,
        None,
    )?);
    let reopen = thread_read_bytes()? - before;
    assert!(reopen < 10 * MIB as u64, "a later open read {reopen} bytes");
    // Any walk of the state tree fails below its depth limit.
    let mut deep = state.join("evidence/deep");
    for _ in 0..70 {
        deep.push("d");
    }
    std::fs::create_dir_all(&deep)?;
    assert!(allocation(service.db.state_dir()).is_err());
    let before = thread_read_bytes()?;
    let capacity = service.work_capacity()?;
    let launch = thread_read_bytes()? - before;
    assert!(capacity.refusal().is_none());
    let pages = usage.0 as u64 + 64;
    assert!(
        launch <= pages * 4096,
        "a step launch read {launch} bytes, more than {pages} pages"
    );
    // The instrument does see the output scan the launch avoids.
    let before = thread_read_bytes()?;
    let content: i64 = service.db.connection().query_row(
        "SELECT sum(length(CAST(content AS BLOB))) FROM attempt_output",
        [],
        |r| r.get(0),
    )?;
    let scan = thread_read_bytes()? - before;
    assert_eq!(content, 100 * MIB);
    assert!(
        scan >= 100 * MIB as u64,
        "the instrument missed a scan: {scan}"
    );
    println!(
        "STEP LAUNCH output_rows=100 output_bytes={content} usage_rows={} launch_read_bytes={launch} output_scan_read_bytes={scan} backfill_open_read_bytes={backfill} later_open_read_bytes={reopen}",
        usage.0
    );
    Ok(())
}

/// How long a thread (`/proc/<pid>/task/<tid>`) has been runnable but waiting for a CPU:
/// the second field of its `schedstat`. Zero where the kernel keeps no scheduler statistics.
fn scheduler_delay(thread: &Path) -> std::time::Duration {
    std::fs::read_to_string(thread.join("schedstat"))
        .ok()
        .and_then(|stat| stat.split_whitespace().nth(1)?.parse().ok())
        .map_or(std::time::Duration::ZERO, std::time::Duration::from_nanos)
}

#[test]
fn cleanup_holds_the_write_lock_only_briefly() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let attempts = closed_attempts(&mut service, task, 4)?;
    let content = vec![b'e'; 64 * 1024];
    for attempt in &attempts {
        for step in 1..=4 {
            let directory = state.join(format!("evidence/attempt-{attempt}/step-{step}"));
            std::fs::create_dir_all(&directory)?;
            for index in 0..10 {
                std::fs::write(directory.join(format!("file-1-{index}")), &content)?;
                service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'proof','text/plain',?2,?3,1)", params![attempt, format!("evidence/attempt-{attempt}/step-{step}/file-1-{index}"), content.len() as i64])?;
            }
        }
        for sequence in 1..=40 {
            service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,?2,'stdout',1,?3)", params![attempt, sequence, "o".repeat(16 * 1024)])?;
        }
    }
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    // SQLite fsyncs a new WAL header inside whichever transaction first writes after the
    // WAL restarts, and each cleanup call ends with a checkpoint that lets it restart. That
    // sync's latency is the filesystem's (above 90 ms on a loaded btrfs), not cleanup's, so
    // the writer makes that first write between cleanup calls, and no automatic checkpoint
    // restarts the WAL within a call. Every cleanup transaction and checkpoint still counts.
    service
        .db
        .connection()
        .execute_batch("PRAGMA wal_autocheckpoint=0; CREATE TABLE writer_turn(n INTEGER); INSERT INTO writer_turn VALUES (0);")?;
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let restart = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // This thread runs cleanup; the writer reads its scheduler statistics.
    let cleanup_thread = PathBuf::from("/proc").join(std::fs::read_link("/proc/thread-self")?);
    let writer = {
        let stop = stop.clone();
        let restart = restart.clone();
        let state = state.clone();
        std::thread::spawn(
            move || -> Result<(std::time::Duration, std::time::Duration, u32)> {
                let db = crate::storage::Database::open_in(&state)?;
                // NORMAL still syncs the header of a restarted WAL; the commit needs no other sync.
                db.connection()
                    .execute_batch("PRAGMA wal_autocheckpoint=0; PRAGMA synchronous=NORMAL")?;
                // BEGIN IMMEDIATE may also spend time syncing a new WAL header after it
                // acquires the lock. Measure the busy-handler interval, which is the part
                // waiting for cleanup's lock, rather than attributing that I/O to cleanup.
                std::thread_local! {
                    static BUSY_STARTED: std::cell::Cell<Option<(std::time::Instant, std::time::Duration)>> = const { std::cell::Cell::new(None) };
                    static CLEANUP_THREAD: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
                }
                CLEANUP_THREAD.with(|thread| *thread.borrow_mut() = Some(cleanup_thread));
                fn preempted() -> std::time::Duration {
                    CLEANUP_THREAD.with(|thread| {
                        thread
                            .borrow()
                            .as_deref()
                            .map_or(std::time::Duration::ZERO, scheduler_delay)
                    }) + scheduler_delay(Path::new("/proc/thread-self"))
                }
                fn poll(retries: i32) -> bool {
                    if retries == 0 {
                        BUSY_STARTED.with(|started| {
                            started.set(Some((std::time::Instant::now(), preempted())))
                        });
                    }
                    std::thread::sleep(std::time::Duration::from_millis(1));
                    retries < 5000
                }
                db.connection().busy_handler(Some(poll))?;
                // A loaded machine can also keep the lock holder or this writer runnable but off
                // a CPU; that time is the machine's. Cleanup's work and I/O under the lock count.
                let mut longest = std::time::Duration::ZERO;
                let mut longest_wall = std::time::Duration::ZERO;
                let mut turns = 0;
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let started = std::time::Instant::now();
                    db.connection().execute_batch("BEGIN IMMEDIATE")?;
                    let wall = started.elapsed();
                    let busy = BUSY_STARTED.with(|started| started.take()).map_or(
                        std::time::Duration::ZERO,
                        |(started, delayed)| {
                            started
                                .elapsed()
                                .saturating_sub(preempted().saturating_sub(delayed))
                        },
                    );
                    longest_wall = longest_wall.max(wall);
                    longest = longest.max(busy);
                    // Requested only after a cleanup call returned, so this lock was taken after
                    // its checkpoint: this write is the first since the WAL could restart.
                    let first = restart.load(std::sync::atomic::Ordering::SeqCst);
                    if first {
                        db.connection()
                            .execute_batch("UPDATE writer_turn SET n=n+1")?;
                    }
                    db.connection().execute_batch("COMMIT")?;
                    if first {
                        restart.store(false, std::sync::atomic::Ordering::SeqCst);
                    }
                    turns += 1;
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                Ok((longest, longest_wall, turns))
            },
        )
    };
    let restart_wal = || {
        restart.store(true, std::sync::atomic::Ordering::SeqCst);
        let asked = std::time::Instant::now();
        while restart.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(
                !writer.is_finished() && asked.elapsed() < std::time::Duration::from_secs(5),
                "the writer did not make the first write after the checkpoint"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    };
    let started = std::time::Instant::now();
    let mut batches = 0;
    let cleaned = (|| -> Result<()> {
        loop {
            restart_wal();
            let preview = service.storage_preview()?;
            let receipt = service.cleanup_storage(&preview)?;
            batches += 1;
            if preview.candidates.is_empty() && !receipt.batch_limit_reached {
                return Ok(());
            }
            assert!(batches < 20, "cleanup did not finish");
        }
    })();
    let elapsed = started.elapsed();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (longest, longest_wall, turns) = writer.join().expect("writer panicked")?;
    cleaned?;
    let (files, rows): (i64, i64) = service.db.connection().query_row(
        "SELECT (SELECT count(*) FROM artifacts WHERE pruned_at_ms IS NULL),(SELECT count(*) FROM attempt_output)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!((files, rows), (0, 0));
    assert_eq!(evidence_counts(&state.join("evidence"))?, (0, 0));
    println!(
        "CLEANUP WRITER WAIT files=160 output_rows=160 batches={batches} cleanup_ms={} writer_turns={turns} longest_wait_ms={:.1} longest_wall_ms={:.1}",
        elapsed.as_millis(),
        longest.as_secs_f64() * 1000.0,
        longest_wall.as_secs_f64() * 1000.0
    );
    assert!(turns > 0);
    assert!(
        longest < std::time::Duration::from_millis(50),
        "a concurrent writer waited {longest:?} for cleanup"
    );
    Ok(())
}

#[test]
fn housekeeping_skips_and_reports_a_non_conforming_artifact_path() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let attempt = closed_attempts(&mut service, task, 1)?[0];
    let directory = state.join(format!("evidence/attempt-{attempt}/step-1"));
    std::fs::create_dir_all(&directory)?;
    let valid = directory.join("file-1-0");
    std::fs::write(&valid, "generated evidence")?;
    let outside = state.join("unsafe");
    std::fs::create_dir_all(&outside)?;
    std::fs::write(outside.join("step-101.log"), "not ours")?;
    service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'foreign','text/plain','unsafe/step-101.log',8,1)", [attempt])?;
    service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'proof','text/plain',?2,18,1)", params![attempt, format!("evidence/attempt-{attempt}/step-1/file-1-0")])?;
    service.db.connection().execute("INSERT INTO attempt_output(attempt_id,sequence,stream,recorded_at_ms,content) VALUES (?1,1,'stdout',1,'retained')", [attempt])?;
    let policy = service.retention_policy()?;
    service.set_retention(policy, 0)?;
    for _ in 0..3 {
        service.automatic_housekeeping()?;
    }
    assert!(!valid.exists() && !directory.exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("step-101.log"))?,
        "not ours"
    );
    let state_of = |name: &str| -> Result<(bool, bool)> {
        Ok(service.db.connection().query_row(
            "SELECT pruning_requested_at_ms IS NOT NULL,pruned_at_ms IS NOT NULL FROM artifacts WHERE name=?1",
            [name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    };
    assert_eq!(state_of("proof")?, (false, true));
    assert_eq!(
        state_of("foreign")?,
        (true, false),
        "the foreign path stays pending"
    );
    let output: i64 = service.db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
        [attempt],
        |r| r.get(0),
    )?;
    assert_eq!(output, 0, "output was not cleaned beside the skipped path");
    assert_eq!(
        service.reported_evidence.iter().collect::<Vec<_>>(),
        [&format!(
            "Unsafe artifact path \"unsafe/step-101.log\" of attempt {attempt} skipped"
        )],
        "reported once across housekeeping calls"
    );
    // The interactive command still reports it as an error after cleaning the rest.
    let preview = service.storage_preview()?;
    assert!(service.cleanup_storage(&preview).is_err());
    Ok(())
}

#[test]
fn breach_transcript_is_bounded_and_ends_at_the_recorded_breach() -> Result<()> {
    let (_sandbox, mut service, task) = closed_service()?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(
        VersionedTask { id: task, version },
        "breach-transcript",
        "fixture-base",
        false,
    )?;
    for line in 0..4000 {
        service.output(&run, "stdout", &format!("line {line}\n"))?;
    }
    service.record_breach(
        &run,
        &crate::resource::Breach {
            limit: crate::resource::Limit::ScratchBytes,
            configured: 1,
            measured: 2,
            cause: "copy".into(),
            summary: String::new(),
            reason: "Resource limit: test".into(),
        },
    )?;
    service.output(&run, "stdout", "after the breach\n")?;

    let breach = service.pending_breach(&run)?.expect("recorded breach");
    let tail = service.output_tail(&run, breach.output_end, 1024)?;

    assert!(tail.len() <= 1024, "{} bytes", tail.len());
    assert!(tail.len() > 1000, "{} bytes", tail.len());
    assert!(tail.ends_with("line 3999\n"), "{tail}");
    assert!(!tail.contains("after the breach"));
    assert_eq!(service.output_tail(&run, 0, 1024)?, "");
    Ok(())
}

#[test]
fn shared_payloads_keep_ids_and_judge_paths_through_interrupted_pruning() -> Result<()> {
    let (sandbox, mut first, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let other_root = sandbox.path().join("other-project");
    std::fs::create_dir(&other_root)?;
    let mut second = Service::open(Database::open_in(&state)?, &other_root, None)?;
    second.set_check("screens", "true", 1000)?;
    let second_task = second.create(
        &[first.task(task)?.unwrap().definition().definition.clone()],
        None,
        true,
    )?[0];
    let version = first.current_task()?.unwrap().version;
    let first_run = first.begin_run(VersionedTask { id: task, version }, "first", "base", false)?;
    let version = second.current_task()?.unwrap().version;
    let second_run = second.begin_run(
        VersionedTask {
            id: second_task,
            version,
        },
        "second",
        "base",
        false,
    )?;
    let tree_a = "a".repeat(40);
    let tree_b = "b".repeat(40);
    let check = &second.checks()?[0];
    let step = second.start_check(&second_run, check, "fixture")?;
    second.finish_check(&step, true, "screens recorded", Some(0), None)?;
    second.record_check_tree(&step, &tree_b)?;
    let transcript = format!("evidence/attempt-{}/step-1.log", first_run.attempt);
    let artifact = format!("evidence/attempt-{}/step-999/file-1-0", second_run.attempt);
    let observation = format!(
        "evidence/attempt-{}/step-{}/file-1-0",
        second_run.attempt, step.id
    );
    let source = sandbox.path().join("payload");
    let content = "retained proof\n".repeat(4096);
    std::fs::write(&source, &content)?;
    first.publish_artifact(
        first_run.attempt,
        &first_run.token,
        "check",
        &source,
        &transcript,
        &tree_a,
    )?;
    second.publish_artifact(
        second_run.attempt,
        &second_run.token,
        "changed-screens.txt",
        &source,
        &artifact,
        &tree_a,
    )?;
    second.publish_artifact(
        second_run.attempt,
        &second_run.token,
        "changed-screens.txt",
        &source,
        &observation,
        &tree_b,
    )?;
    let transcript_path = state.join(&transcript);
    let artifact_path = state.join(&artifact);
    assert_eq!(
        std::fs::metadata(&transcript_path)?.ino(),
        std::fs::metadata(&artifact_path)?.ino()
    );
    assert_eq!(std::fs::metadata(&artifact_path)?.nlink(), 2);
    assert!(!state.join(&observation).exists());

    let packet = sandbox.path().join("packet");
    let summary = second.review_bundle(second_run.attempt, &state, &packet, &tree_b, "", true)?;
    let index = std::fs::read_to_string(packet.join("artifacts.jsonl"))?;
    assert_eq!(index.lines().count(), 1, "{index}");
    let entry: serde_json::Value = serde_json::from_str(index.trim()).unwrap();
    assert_eq!(entry["matches_current_tree"], true);
    let artifact_id = entry["id"].as_i64().unwrap();
    assert!(summary.contains(&format!("changed-screens report, artifact #{artifact_id}")));
    assert_eq!(
        std::fs::read_to_string(entry["path"].as_str().unwrap())?,
        content
    );

    first.finish_run(&first_run, "succeeded", None, "done", None)?;
    let policy = first.retention_policy()?;
    first.set_storage_policy(policy, 0, 2 * 1024 * 1024, 10_000)?;
    let preview = first.storage_preview()?;
    assert_eq!(preview.logical_artifact_bytes, content.len() as i64);
    assert_eq!(preview.distinct_artifact_bytes, content.len() as i64);
    assert_eq!(preview.eligible_file_bytes, 0);
    assert!(preview.protected_shared_file_bytes > 0);

    // Simulate the crash window after unlink and before the pruning receipt commits.
    let observed = allocation(first.db.state_dir())?;
    let intent = deletion::record_intent(
        first.db.connection(),
        first.db.state_dir(),
        first.project_id,
        &preview,
        &observed,
        PreviewMode {
            candidate_limit: i64::MAX,
            accounting: false,
        },
    )?;
    assert_eq!(intent.selected.len(), 1);
    assert!(
        deletion::unlink(first.db.state_dir(), &intent.selected)
            .failure
            .is_none()
    );
    drop(first);
    let mut first = Service::open(
        Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    let interrupted = first.storage_preview()?;
    assert_eq!(interrupted.candidates[0].pending_artifacts, 1);
    first.cleanup_storage(&interrupted)?;
    assert!(!transcript_path.exists());
    assert_eq!(std::fs::metadata(&artifact_path)?.nlink(), 1);
    assert_eq!(second.open_text_artifact(artifact_id)?.1, content);
    assert_eq!(
        std::fs::read_to_string(entry["path"].as_str().unwrap())?,
        content
    );
    Ok(())
}

#[test]
fn compaction_resumes_legacy_payloads_and_counts_one_inode_across_scan_pages() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (sandbox, mut service, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let other_root = sandbox.path().join("publisher");
    std::fs::create_dir(&other_root)?;
    let mut publisher = Service::open(Database::open_in(&state)?, &other_root, None)?;
    let other_task = publisher.create(
        &[service.task(task)?.unwrap().definition().definition.clone()],
        None,
        true,
    )?[0];
    let version = publisher.current_task()?.unwrap().version;
    let published_run = publisher.begin_run(
        VersionedTask {
            id: other_task,
            version,
        },
        "publisher",
        "base",
        false,
    )?;
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(VersionedTask { id: task, version }, "legacy", "base", false)?;
    let payload = vec![b'x'; 16 * 1024];
    let parent = state.join(format!("evidence/attempt-{}", run.attempt));
    std::fs::create_dir_all(&parent)?;
    let files = 70;
    for index in 0..files {
        let relative = format!("evidence/attempt-{}/step-{}.log", run.attempt, index + 1);
        let path = state.join(&relative);
        std::fs::write(&path, &payload)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o444))?;
        service.db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms)
            VALUES (?1,'check','text/plain',?2,?3,1)", params![run.attempt,relative,payload.len() as i64])?;
    }
    let first = parent.join("step-1.log");
    let relative = format!("evidence/attempt-{}/step-72.log", published_run.attempt);
    publisher.publish_artifact(
        published_run.attempt,
        &published_run.token,
        "new check",
        &first,
        &relative,
        &"b".repeat(40),
    )?;
    let published = state.join(&relative);
    assert_ne!(
        std::fs::metadata(&first)?.ino(),
        std::fs::metadata(&published)?.ino()
    );
    let interrupted = parent.join("step-17.log.dedup");
    std::fs::hard_link(&first, &interrupted)?;
    let before = service.storage_preview()?;
    assert_eq!(service.compact_artifacts()?, CompactionProgress::More);
    assert!(
        interrupted.exists(),
        "one page must stop before the next 16 files"
    );
    assert!(std::fs::metadata(&first)?.nlink() < files);
    drop(service);
    let mut service = Service::open(
        Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    let policy = service.retention_policy()?;
    service.set_storage_policy(policy, 0, 2 * 1024 * 1024, 10_000)?;
    for _ in 0..20 {
        service.automatic_housekeeping_progress(1)?;
    }
    assert!(!interrupted.exists());
    assert_eq!(std::fs::metadata(&first)?.nlink(), files + 1);
    assert_eq!(
        std::fs::metadata(&first)?.ino(),
        std::fs::metadata(&published)?.ino()
    );
    assert_eq!(std::fs::read(&first)?, payload);
    assert_eq!(std::fs::read(&published)?, payload);
    let after = service.storage_preview()?;
    assert_eq!(after.logical_artifact_bytes, before.logical_artifact_bytes);
    assert_eq!(after.distinct_artifact_bytes, payload.len() as i64);
    assert!(after.protected_open_bytes >= after.logical_artifact_bytes);
    assert!(
        before.evidence_bytes - after.evidence_bytes >= (files as i64 - 1) * payload.len() as i64
    );
    assert_eq!(
        projected_allocation(&service.db)?.evidence,
        after.evidence_bytes
    );
    service.finish_run(&run, "succeeded", None, "done", None)?;
    let preview = service.storage_preview()?;
    service.cleanup_storage(&preview)?;
    assert!(!first.exists());
    assert_eq!(std::fs::read(published)?, payload);
    Ok(())
}

#[test]
fn checksummed_legacy_compaction_preserves_published_inode_across_bounded_pages() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let (sandbox, mut legacy, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let publisher_root = sandbox.path().join("publisher");
    std::fs::create_dir(&publisher_root)?;
    let mut publisher = Service::open(Database::open_in(&state)?, &publisher_root, None)?;
    let publisher_task = publisher.create(
        &[legacy.task(task)?.unwrap().definition().definition.clone()],
        None,
        true,
    )?[0];
    let version = publisher.current_task()?.unwrap().version;
    let published_run = publisher.begin_run(
        VersionedTask {
            id: publisher_task,
            version,
        },
        "publisher",
        "base",
        false,
    )?;
    let version = legacy.current_task()?.unwrap().version;
    let run = legacy.begin_run(VersionedTask { id: task, version }, "legacy", "base", false)?;
    // Deliberately larger than a page and not block-aligned.
    let payload = "retained evidence".repeat(2049);
    let checksum = format!("{:x}", Sha256::digest(&payload));
    let size = payload.len();
    let first = format!("evidence/attempt-{}/step-1.log", run.attempt);
    let second = format!("evidence/attempt-{}/step-2.log", run.attempt);
    std::fs::create_dir_all(state.join(&first).parent().unwrap())?;
    for path in [&first, &second] {
        std::fs::write(state.join(path), &payload)?;
        std::fs::set_permissions(state.join(path), std::fs::Permissions::from_mode(0o444))?;
        legacy.db.connection().execute(
            "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,checksum_sha256)
             VALUES (?1,'legacy','text/plain',?2,?3,1,?4)",
            params![run.attempt,path,size as i64,checksum],
        )?;
    }
    let inode = std::fs::metadata(state.join(&first))?.ino();
    assert_ne!(inode, std::fs::metadata(state.join(&second))?.ino());
    let published = format!("evidence/attempt-{}/step-3.log", published_run.attempt);
    publisher.publish_artifact(
        published_run.attempt,
        &published_run.token,
        "new check",
        &state.join(&first),
        &published,
        &"b".repeat(40),
    )?;
    assert_eq!(std::fs::metadata(state.join(&published))?.ino(), inode);

    const BUDGET: u64 = 4096;
    assert_eq!(
        legacy.compact_artifact_page(BUDGET)?,
        (CompactionProgress::More, BUDGET)
    );
    assert_ne!(std::fs::metadata(state.join(&second))?.ino(), inode);
    drop(legacy);
    let legacy = Service::open(
        Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    let mut total = BUDGET;
    let mut finished = false;
    for _ in 0..24 {
        let (progress, read) = legacy.compact_artifact_page(BUDGET)?;
        assert!(read <= BUDGET, "read {read} with a {BUDGET}-byte budget");
        total += read;
        assert_ne!(progress, CompactionProgress::PublicationBusy);
        if progress == CompactionProgress::Done {
            finished = true;
            break;
        }
    }
    assert!(finished);
    assert_eq!(total, 2 * size as u64, "both paths were verified once");
    assert_eq!(std::fs::metadata(state.join(&second))?.ino(), inode);
    assert_eq!(std::fs::metadata(state.join(&published))?.ino(), inode);

    publisher.finish_run(&published_run, "succeeded", None, "done", None)?;
    publisher.set_storage_policy(publisher.retention_policy()?, 0, 2 * 1024 * 1024, 10_000)?;
    let preview = publisher.storage_preview()?;
    publisher.cleanup_storage(&preview)?;
    assert!(!state.join(published).exists());
    assert_eq!(std::fs::read_to_string(state.join(first))?, payload);
    assert_eq!(std::fs::read_to_string(state.join(second))?, payload);
    Ok(())
}

#[test]
fn publication_and_restarted_compaction_find_the_fifth_readable_source() -> Result<()> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;

    let (sandbox, mut legacy, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let publisher_root = sandbox.path().join("publisher");
    std::fs::create_dir(&publisher_root)?;
    let mut publisher = Service::open(Database::open_in(&state)?, &publisher_root, None)?;
    let publisher_task = publisher.create(
        &[legacy.task(task)?.unwrap().definition().definition.clone()],
        None,
        true,
    )?[0];
    let version = publisher.current_task()?.unwrap().version;
    let published_run = publisher.begin_run(
        VersionedTask {
            id: publisher_task,
            version,
        },
        "publisher",
        "base",
        false,
    )?;
    let version = legacy.current_task()?.unwrap().version;
    let run = legacy.begin_run(VersionedTask { id: task, version }, "legacy", "base", false)?;
    let relative = |step| format!("evidence/attempt-{}/step-{step}.log", run.attempt);
    std::fs::create_dir_all(state.join(relative(1)).parent().unwrap())?;
    let payload = b"retained proof";
    let checksum = format!("{:x}", Sha256::digest(payload));
    for step in 1..=6 {
        let path = state.join(relative(step));
        if step >= 3 {
            let bytes = if step < 5 { b"corrupt proof!" } else { payload };
            std::fs::write(&path, bytes)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444))?;
        }
        legacy.db.connection().execute(
            "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,checksum_sha256,needs_compaction)
             VALUES (?1,'legacy','text/plain',?2,?3,1,?4,?5)",
            params![run.attempt, relative(step), payload.len() as i64, checksum, step == 6],
        )?;
    }
    let fourth: i64 = legacy.db.connection().query_row(
        "SELECT id FROM artifacts WHERE relative_path=?1",
        [relative(4)],
        |row| row.get(0),
    )?;
    let fifth = state.join(relative(5));
    let duplicate = state.join(relative(6));
    let inode = std::fs::metadata(&fifth)?.ino();
    assert_ne!(std::fs::metadata(&duplicate)?.ino(), inode);
    let published = format!("evidence/attempt-{}/step-7.log", published_run.attempt);
    publisher.publish_artifact(
        published_run.attempt,
        &published_run.token,
        "new check",
        &fifth,
        &published,
        &"b".repeat(40),
    )?;
    assert_eq!(std::fs::metadata(state.join(&published))?.ino(), inode);

    let (progress, read) = legacy.compact_artifact_page(4096)?;
    assert_eq!(progress, CompactionProgress::More);
    assert_eq!(read, 3 * payload.len() as u64, "target and corrupt sources");
    let cursor: i64 = legacy.db.connection().query_row(
        "SELECT source_after_id FROM artifact_compaction_hashes WHERE source_after_id<>0",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(cursor, fourth);
    drop(legacy);
    let legacy = Service::open(
        Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    assert_eq!(
        legacy.compact_artifact_page(4096)?,
        (CompactionProgress::Done, payload.len() as u64)
    );
    assert_eq!(std::fs::metadata(&duplicate)?.ino(), inode);
    assert_eq!(std::fs::read(&duplicate)?, payload);
    Ok(())
}

#[test]
fn named_artifact_repeats_reuse_the_readable_replacement() -> Result<()> {
    let (sandbox, mut service, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(VersionedTask { id: task, version }, "writer", "base", false)?;
    let source = sandbox.path().join("payload");
    std::fs::write(&source, "retained proof")?;
    let path = |step| format!("evidence/attempt-{}/step-{step}/file-1-0", run.attempt);
    let publish = |service: &Service, step| {
        service.publish_artifact(
            run.attempt,
            &run.token,
            "proof.txt",
            &source,
            &path(step),
            &"b".repeat(40),
        )
    };
    let listed_ids = |service: &mut Service| -> Result<Vec<i64>> {
        let task = service.task(task)?.unwrap();
        Ok(task
            .attempts
            .last()
            .unwrap()
            .artifacts
            .iter()
            .map(|artifact| artifact.id)
            .collect())
    };

    publish(&service, 1)?;
    let original = listed_ids(&mut service)?[0];
    std::fs::remove_file(state.join(path(1)))?;
    publish(&service, 2)?;
    let listing = listed_ids(&mut service)?;
    assert_eq!(listing.len(), 2);
    assert_eq!(listing[0], original);
    let replacement = listing[1];
    for step in 3..=4 {
        publish(&service, step)?;
        let observed: i64 = service.db.connection().query_row(
            "SELECT artifact_id FROM artifact_observations WHERE relative_path=?1",
            [path(step)],
            |row| row.get(0),
        )?;
        assert_eq!(observed, replacement);
        assert_eq!(listed_ids(&mut service)?, listing);
    }
    assert_eq!(service.open_text_artifact(replacement)?.1, "retained proof");
    Ok(())
}

#[test]
fn capacity_retry_reclaims_eligible_foreign_evidence_and_keeps_protected_links() -> Result<()> {
    let (sandbox, mut archive, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let other_root = sandbox.path().join("producer");
    std::fs::create_dir(&other_root)?;
    let mut producer = Service::open(Database::open_in(&state)?, &other_root, None)?;
    let producer_task = producer.create(
        &[archive.task(task)?.unwrap().definition().definition.clone()],
        None,
        true,
    )?[0];
    let version = archive.current_task()?.unwrap().version;
    let finished = archive.begin_run(VersionedTask { id: task, version }, "old", "base", false)?;
    let version = producer.current_task()?.unwrap().version;
    let running = producer.begin_run(
        VersionedTask {
            id: producer_task,
            version,
        },
        "live",
        "base",
        false,
    )?;
    let unique = sandbox.path().join("unique");
    let shared = sandbox.path().join("shared");
    std::fs::write(&unique, vec![b'u'; 768 * 1024])?;
    std::fs::write(&shared, vec![b's'; 128 * 1024])?;
    let old_unique = format!("evidence/attempt-{}/step-1.log", finished.attempt);
    let old_shared = format!("evidence/attempt-{}/step-2.log", finished.attempt);
    let live_shared = format!("evidence/attempt-{}/step-3.log", running.attempt);
    let tree = "a".repeat(40);
    archive.publish_artifact(
        finished.attempt,
        &finished.token,
        "unique",
        &unique,
        &old_unique,
        &tree,
    )?;
    archive.publish_artifact(
        finished.attempt,
        &finished.token,
        "shared",
        &shared,
        &old_shared,
        &tree,
    )?;
    producer.publish_artifact(
        running.attempt,
        &running.token,
        "shared",
        &shared,
        &live_shared,
        &tree,
    )?;
    archive.finish_run(&finished, "succeeded", None, "done", None)?;
    for _ in 0..4 {
        producer.automatic_housekeeping_progress(1)?;
    }
    let allocated = projected_allocation(&producer.db)?.bytes;
    let policy = archive.retention_policy()?;
    archive.set_storage_policy(policy, 0, allocated.max(1_048_576), 10_000)?;
    let rows = vec![("stdout".into(), "new output\n".repeat(8192))];
    assert!(matches!(
        producer.output_rows(&running, &rows),
        Err(Error::Capacity(_))
    ));
    // With an already settled scan, the publication lock alone prevents
    // deleting the eligible foreign attempt.
    let mut settled = false;
    for _ in 0..16 {
        if bounded_allocation_scan(&producer.db)? == ScanProgress::Settled {
            settled = true;
            break;
        }
    }
    assert!(settled);
    assert!(archive.storage_preview()?.eligible_file_bytes >= 768 * 1024);
    let mut controls = Service::open(Database::open_in(&state)?, &other_root, None)?;
    let cursor = producer.control_cursor(&running)?;
    let lock = super::super::artifacts::publication_lock(&state)?;
    lock.lock()?;
    let (_, blocked) = archive.reconcile_storage()?;
    assert_eq!(blocked.cleanup, CleanupProgress::PublicationBusy);

    let mut checks = 0;
    let interrupted = producer.retry_storage_until(
        |service| service.output_rows(&running, &rows),
        |service| {
            checks += 1;
            let paused = service.control_cursor(&running)? > cursor;
            if checks == 1 {
                // Deliver Pause between polls with publication still blocked.
                controls.request_current_pause()?;
            }
            assert!(checks < 3, "cleanup waited through Pause");
            Ok(paused.then_some("pause"))
        },
    )?;
    assert_eq!(interrupted, std::ops::ControlFlow::Break("pause"));
    assert_eq!(producer.output_cursor(&running)?, 0);
    assert!(state.join(&old_unique).exists());

    let mut lock = Some(lock);
    let mut checks = 0;
    let resumed = producer.retry_storage_until(
        |service| service.output_rows(&running, &rows),
        |_| {
            checks += 1;
            assert!(checks < 16, "capacity never recovered");
            if checks == 2 {
                assert!(state.join(&old_unique).exists());
                drop(lock.take());
            }
            Ok(None::<()>)
        },
    )?;
    assert_eq!(resumed, std::ops::ControlFlow::Continue(()));
    assert!(!state.join(old_unique).exists());
    assert!(!state.join(old_shared).exists());
    assert_eq!(
        std::fs::read(state.join(live_shared))?,
        std::fs::read(shared)?
    );
    assert_eq!(producer.output_cursor(&running)?, 1);
    Ok(())
}

#[test]
fn capacity_retry_waits_for_compaction_when_live_work_defers_deletion() -> Result<()> {
    use crate::resource::StorageBudget;

    let (sandbox, mut service, task) = closed_service()?;
    let state = sandbox.path().join("state");
    let version = service.current_task()?.unwrap().version;
    let run = service.begin_run(VersionedTask { id: task, version }, "live", "base", false)?;
    let source = sandbox.path().join("payload");
    let payload = vec![b'p'; 768 * 1024];
    std::fs::write(&source, &payload)?;
    let published = format!("evidence/attempt-{}/step-1.log", run.attempt);
    let legacy = format!("evidence/attempt-{}/step-2.log", run.attempt);
    service.publish_artifact(
        run.attempt,
        &run.token,
        "proof",
        &source,
        &published,
        &"a".repeat(40),
    )?;
    std::fs::write(state.join(&legacy), &payload)?;
    service.db.connection().execute(
        "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms,checksum_sha256)
         SELECT attempt_id,name,media_type,?1,size_bytes,created_at_ms,checksum_sha256 FROM artifacts
         WHERE relative_path=?2",
        params![legacy, published],
    )?;
    evidence_allocation_changed(service.db.connection())?;
    let inode = std::fs::metadata(state.join(&published))?.ino();
    let legacy_path = state.join(&legacy);
    assert_ne!(inode, std::fs::metadata(&legacy_path)?.ino());

    let mut controls = Service::open(
        Database::open_in(&state)?,
        &sandbox.path().join("project"),
        None,
    )?;
    let lock = super::super::artifacts::publication_lock(&state)?;
    lock.lock()?;
    // Checkpoint setup writes before measuring the budget so WAL reclamation
    // cannot satisfy the refused write on its own.
    service.reconcile_storage()?;
    assert_eq!(bounded_allocation_scan(&service.db)?, ScanProgress::Settled);
    let allocated = projected_allocation(&service.db)?.bytes;
    let budget = Reserve::Work
        .smallest_budget(StorageBudget::StorageMaxBytes, allocated)
        .saturating_sub(128 * 1024)
        .max(1_048_576);
    service.set_storage_policy(service.retention_policy()?, 0, budget, 10_000)?;
    let rows = vec![("stdout".into(), "new output\n".repeat(8192))];
    assert!(matches!(
        service.output_rows(&run, &rows),
        Err(Error::Capacity(_))
    ));
    assert_eq!(bounded_allocation_scan(&service.db)?, ScanProgress::Settled);
    let (preview, progress) = service.automatic_housekeeping_progress(1)?;
    assert!(preview.deferred_for_live_work);
    assert_eq!(preview.eligible_file_bytes, 0);
    assert_eq!(progress.cleanup, CleanupProgress::PublicationBusy);
    assert!(matches!(
        service.retry_storage(|service| service.output_rows(&run, &rows)),
        Err(Error::Capacity(_))
    ));

    let cursor = service.control_cursor(&run)?;
    let mut polls = 0;
    let interrupted = service.retry_storage_until(
        |service| service.output_rows(&run, &rows),
        |service| {
            polls += 1;
            let paused = service.control_cursor(&run)? > cursor;
            if polls == 1 {
                controls.request_current_pause()?;
            }
            assert!(polls < 3, "compaction waited through Pause");
            Ok(paused.then_some(()))
        },
    )?;
    assert_eq!(interrupted, std::ops::ControlFlow::Break(()));
    assert_eq!(service.output_cursor(&run)?, 0);
    assert_ne!(inode, std::fs::metadata(&legacy_path)?.ino());

    let mut lock = Some(lock);
    let mut polls = 0;
    let admitted = service.retry_storage_until(
        |service| service.output_rows(&run, &rows),
        |_| {
            polls += 1;
            assert!(polls < 16, "compaction did not recover capacity");
            if polls == 2 {
                drop(lock.take());
            }
            Ok(None::<()>)
        },
    )?;
    assert_eq!(admitted, std::ops::ControlFlow::Continue(()));
    assert_eq!(service.output_cursor(&run)?, 1);
    assert_eq!(std::fs::metadata(&legacy_path)?.ino(), inode);
    assert_eq!(std::fs::read(&legacy_path)?, payload);
    assert_eq!(std::fs::read(state.join(published))?, payload);
    Ok(())
}
