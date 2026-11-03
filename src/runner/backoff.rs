//! Automatic retries of operational failures within the same attempt, and the waits that
//! precede them. Pause, Stop and the task's time limit end every wait.
use super::{Context, Outcome, context::interrupted};
use crate::{
    application::{Service, execution::Run, now_ms},
    storage::Result,
};
use std::{
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

/// How many times an operational failure is retried before its outcome stands.
const RETRIES: u32 = 2;

/// How a bounded retry is announced: what failed, and what the retry keeps.
pub(super) struct Notice {
    pub(super) failed: &'static str,
    pub(super) retained: &'static str,
}

/// Runs `once` until it ends with an outcome that is not `retryable`, retrying at most
/// [`RETRIES`] times after an announced, exponentially growing backoff. An interruption
/// during the backoff is the result.
pub(super) fn bounded<'a>(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'a>,
    notice: Notice,
    retryable: impl Fn(&Outcome) -> bool,
    mut once: impl FnMut(&mut Service, &mut Context<'a>) -> Result<Outcome>,
) -> Result<Outcome> {
    let mut retry = 0;
    loop {
        let outcome = once(service, ctx)?;
        if retry == RETRIES || !retryable(&outcome) {
            return Ok(outcome);
        }
        service.output(
            run,
            "system",
            &format!(
                "{} · automatic retry {}/{RETRIES} after {}s · {}; Pause cancels",
                notice.failed,
                retry + 1,
                1 << retry,
                notice.retained,
            ),
        )?;
        if let Some(outcome) = operational_backoff(service, run, ctx, retry)? {
            return Ok(outcome);
        }
        retry += 1;
    }
}

/// Waits 2^`retry` seconds of active time. Some outcome when Pause, Stop or the time
/// limit ended the wait first.
pub(super) fn operational_backoff(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    retry: u32,
) -> Result<Option<Outcome>> {
    let until = Instant::now() + Duration::from_secs(1 << retry);
    loop {
        if ctx.interrupted(service, run)? {
            ctx.persist_active_time(service, run)?;
            return Ok(Some(Outcome::Paused));
        }
        if let Some(outcome) = ctx.deadline_outcome(run) {
            ctx.persist_active_time(service, run)?;
            return Ok(Some(outcome));
        }
        if Instant::now() >= until {
            ctx.persist_active_time(service, run)?;
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// How often a rate-limit wait looks at the database; Pause, Stop and resume-now take
/// effect within one interval.
const LIMIT_WAIT_POLL: Duration = Duration::from_millis(500);

/// Idle until the provider limit holding `run` ends: true when the attempt may resume,
/// false when paused, stopped or the task is no longer current. A poll reads only the
/// control cursor, the current task id and its resume time.
pub fn wait_for_limit_reset(
    service: &Service,
    run: &Run,
    cursor: i64,
    stop: &AtomicBool,
) -> Result<bool> {
    service.output(run,"system","Automatic rate-limit backoff; Pause stops the scheduler without discarding the wait or session")?;
    loop {
        if interrupted(service, run, stop, cursor)? {
            service.output(
                run,
                "system",
                "Automatic continuation stopped; resume eligibility is retained",
            )?;
            return Ok(false);
        }
        let Some((task, resume_at_ms)) = service.limit_wait()? else {
            return Ok(false);
        };
        if task != run.task {
            return Ok(false);
        }
        let remaining = resume_at_ms.map(|at| at.saturating_sub(now_ms()));
        match remaining {
            Some(ms) if ms <= 0 => return Ok(true),
            Some(ms) => std::thread::sleep(LIMIT_WAIT_POLL.min(Duration::from_millis(ms as u64))),
            None => std::thread::sleep(LIMIT_WAIT_POLL),
        }
    }
}
