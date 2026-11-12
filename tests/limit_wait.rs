//! A provider rate-limit wait idles on a few tiny queries instead of reloading the task.
//! Its own test binary: the read measurement is the process's I/O counter, so nothing else
//! may run beside it.
#[allow(dead_code)]
mod support;

use ktask_rs_mk3::application::execution::Run;
use ktask_rs_mk3::application::{Definition, Kind, ModelConfig, Service, VersionedTask};
use ktask_rs_mk3::runner::wait_for_limit_reset;
use ktask_rs_mk3::storage::Database;
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use support::Result;

/// The read measurement counts the whole process, so these tests take turns.
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());
static STATEMENTS: AtomicUsize = AtomicUsize::new(0);

fn count(event: TraceEvent<'_>) {
    if matches!(event, TraceEvent::Stmt(..)) {
        STATEMENTS.fetch_add(1, Ordering::Relaxed);
    }
}

fn bytes_read() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .expect("io counters")
        .lines()
        .find_map(|line| line.strip_prefix("rchar: "))
        .expect("rchar")
        .trim()
        .parse()
        .expect("number")
}

fn definition() -> Definition {
    Definition {
        title: "Rate limited".into(),
        goal: "Wait for the provider".into(),
        criteria: vec!["Resumes".into()],
        kind: Kind::Ai,
        category: None,
        lenses: None,
        models: std::array::from_fn(|role| {
            Some(ModelConfig {
                provider: "codex".into(),
                model: format!("fixture-{role}"),
                effort: None,
            })
        }),
        attempt_limit: Some(5),
        timeout_ms: Some(600_000),
        stage_timeout_ms: None,
    }
}

/// A task with three attempts of 2,000 output rows each, the last waiting for the provider
/// until `resume_in_ms` from now. The service's connection reads every page from the file
/// (no cache between statements, as when another process keeps changing the database) and
/// counts the statements it runs.
fn waiting(root: &Path, resume_in_ms: u64) -> Result<(Service, Run)> {
    let state = root.join("state");
    let db = Database::open_in(&state)?;
    db.connection().pragma_update(None, "cache_size", 0)?;
    db.connection()
        .trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(count));
    let mut service = Service::open(db, root, None)?;
    let id = service.create(&[definition()], None, true)?[0];
    let mut last = None;
    for attempt in 1..=3 {
        let version = service.current_task()?.expect("current task").version;
        let run = service.begin_run(
            VersionedTask { id, version },
            &format!("attempt-{attempt}"),
            "base",
            true,
        )?;
        let rows: Vec<_> = (0..2000)
            .map(|n| {
                (
                    "stdout".to_string(),
                    format!("output line {n} {}", "x".repeat(150)),
                )
            })
            .collect();
        service.output_rows(&run, &rows)?;
        if attempt < 3 {
            service.finish_run(&run, "failed", Some("agent"), "failed", None)?;
        }
        last = Some(run);
    }
    let run = last.expect("three attempts");
    service.rate_wait(&run, None)?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis() as u64;
    Database::open_in(&state)?.connection().execute(
        "UPDATE attempts SET resume_at_ms = ?2 WHERE id = ?1",
        rusqlite::params![run.attempt, (now + resume_in_ms) as i64],
    )?;
    Ok((service, run))
}

#[test]
fn rate_limit_wait_reads_a_few_small_rows_instead_of_reloading_the_task() -> Result {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = support::sandbox()?;
    let root = scratch.path().canonicalize()?;
    let (mut service, run) = waiting(&root, 5_000)?;
    let cursor = service.control_cursor(&run)?;

    // What every 100 ms poll cost before: the whole task, 3 x 2,000 output rows included.
    let (queries, read) = (STATEMENTS.load(Ordering::Relaxed), bytes_read());
    assert_eq!(service.current_task()?.expect("task").attempts.len(), 3);
    let (reload_queries, reload) = (
        STATEMENTS.load(Ordering::Relaxed) - queries,
        bytes_read() - read,
    );
    assert!(reload > 1_000_000, "one reload read only {reload} bytes");

    let (queries, read) = (STATEMENTS.load(Ordering::Relaxed), bytes_read());
    let started = Instant::now();
    assert!(wait_for_limit_reset(
        &service,
        &run,
        cursor,
        &AtomicBool::new(false)
    )?);
    let elapsed = started.elapsed();
    let (queries, read) = (
        STATEMENTS.load(Ordering::Relaxed) - queries,
        bytes_read() - read,
    );
    assert!(elapsed >= Duration::from_secs(4), "{elapsed:?}");
    // The notice (about ten statements), then a cursor read and a task read per half second.
    assert!(queries <= 40, "{queries} queries");
    assert!(read < 500_000, "{read} bytes read; a reload reads {reload}");
    eprintln!(
        "5 s wait: {queries} statements, {read} bytes; one task reload: {reload_queries} statements, {reload} bytes"
    );
    Ok(())
}

/// How long the wait takes to end after `interrupt`, which acts from outside the waiter.
fn interrupted_by(interrupt: impl FnOnce(&Path, &Run, &AtomicBool) -> Result) -> Result<bool> {
    let _turn = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let scratch = support::sandbox()?;
    let root = scratch.path().canonicalize()?;
    let (service, run) = waiting(&root, 3_600_000)?;
    let cursor = service.control_cursor(&run)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (done, outcome) = mpsc::channel();
    let waiter = {
        let (stop, run) = (stop.clone(), run.clone());
        std::thread::spawn(move || {
            let result = wait_for_limit_reset(&service, &run, cursor, &stop);
            done.send((Instant::now(), result.map_err(|e| e.to_string())))
                .expect("report");
        })
    };
    std::thread::sleep(Duration::from_millis(600));
    assert!(
        outcome.try_recv().is_err(),
        "wait ended before it was interrupted"
    );
    let at = Instant::now();
    interrupt(&root, &run, &stop)?;
    let (ended, result) = outcome
        .recv_timeout(Duration::from_secs(5))
        .expect("wait ended");
    waiter.join().expect("waiter");
    assert!(
        ended.duration_since(at) < Duration::from_secs(1),
        "slow interruption"
    );
    Ok(result.expect("wait result"))
}

#[test]
fn stop_interrupts_the_wait_within_a_second() -> Result {
    let stopped = interrupted_by(|_, _, stop| {
        stop.store(true, Ordering::Relaxed);
        Ok(())
    })?;
    assert!(!stopped);
    Ok(())
}

#[test]
fn pause_interrupts_the_wait_within_a_second() -> Result {
    let paused = interrupted_by(|root, run, _| {
        Database::open_in(&root.join("state"))?.connection().execute(
            "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
             SELECT project_id,task_id,id,0,'runner.pause','info','Pause requested' FROM attempts WHERE id=?1",
            [run.attempt],
        )?;
        Ok(())
    })?;
    assert!(!paused);
    Ok(())
}

#[test]
fn resume_now_ends_the_wait_within_a_second() -> Result {
    let resumed = interrupted_by(|root, _, _| {
        let mut owner = Service::open(Database::open_in(&root.join("state"))?, root, None)?;
        owner.end_limit_wait()?;
        Ok(())
    })?;
    assert!(resumed);
    Ok(())
}
