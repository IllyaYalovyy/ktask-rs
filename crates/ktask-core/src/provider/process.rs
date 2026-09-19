//! Running one provider's process: its prompt in, its output out as it is read,
//! and its whole process group down when either clock runs out.
//!
//! [`run_streaming`] is where a provider adapter stops describing a CLI and
//! starts operating one. It is the code VISION.md §12's launch adapters
//! (`claude`, `codex`) sit on, and the code that decides what happens when an
//! agent produces nothing for a while — the operational risk VISION.md §16 ranks
//! first ("hung agent"), and the reason the `dummy` adapter scripts a `hang` step
//! at all (ADR-0052): a session that never answers is a normal case for a
//! supervisor, not an exotic one.
//!
//! Four rules shape it, and each costs something. ADR-0053 records each beside
//! the alternatives it was chosen over.
//!
//! **The group, not the process.** A CLI that can spawn a helper can orphan one,
//! and a timeout that stopped only the CLI leaves a helper holding a lock file, a
//! port, or a credential the next attempt needs. So the child leads a process
//! group of its own — [`Command::process_group`] is set before it runs one
//! instruction — and every signal goes to the group. That is ADR-0038's reasoning
//! for a gate, with the same call and the same SIGTERM-then-SIGKILL ladder, and
//! it buys the property this task is stated as: no orphan survives a timeout. A
//! descendant that called `setsid` left the group by definition and nothing here
//! can reach it, so the wait for the group to report itself empty is bounded
//! rather than endless — a supervisor blocked on a process it does not own has
//! stopped supervising.
//!
//! **Two clocks, and the idle one is re-armed by output.** `hard_timeout` bounds
//! the session however productive it is; `idle_timeout` bounds the *silence*
//! between output, which is the failure a hang is. They cannot share a deadline:
//! the idle deadline is last-chunk-plus-budget, so a session that prints once a
//! second is alive however long it runs, while a session that prints once and
//! then sleeps dies with its silence measured (VISION.md §7 tells a slow agent
//! from a gone one, and T058 asserts both halves). The deadline moves in the
//! collector that both readers hand chunks to — never in a reader as it reads,
//! because then whichever pipe drained first would be deciding how long the
//! session lives — and it is capped at the hard deadline, because output buys a
//! session time and not immunity.
//!
//! **Order, across the two streams.** [`Outcome`] keeps stdout and stderr apart
//! because the distinction is unrecoverable once merged and is what a failure
//! classification reads first; an arrival order is only worth having if the two
//! kinds stay tellable. So each reader publishes its own lines to the bus itself,
//! as it reads them, under the bus's own lock, rather than queueing them for one
//! collector to forward: an observer sees an interleaving in the order the bytes
//! were observed, and a chunk cannot be lost in a queue a timeout fired over. A
//! line is the chunk because `EventKind::AgentOutput` is one line of output
//! (ADR-0052) and because half a multi-byte character is neither renderable nor
//! attributable — carriage returns, control characters and over-long lines stay
//! the renderer's problem, as `docs/CONTRACT.md` §4 rules. Bytes that are not
//! UTF-8 become the replacement character rather than a panic or a dropped line
//! (VISION.md §13).
//!
//! **A session this supervisor stopped is an error, not a result.**
//! [`Outcome::exit_code`] is an `i32` and ADR-0050 refused to make it optional,
//! assigning "killed by a signal rather than exited" to this task and to the
//! error channel. So either clock expiring, and any session that ended on a
//! signal nobody here sent, comes back as [`Error::Provider`] naming what was
//! exceeded — an idle expiry naming the silence it measured, because a hang is
//! classified from how quiet a session was, not merely from the fact that it
//! stopped. That reservation is also what keeps the one dishonest number
//! available to this file out of any answer: a killed process has no exit status,
//! the only exited status a kill can produce is zero, and a zero an operator
//! reads as "the CLI finished happily" is the substitution ADR-0049 refuses
//! elsewhere. What was read before a stop is kept and counted, and reaches an
//! answer through that error: [`Outcome`] has no field a partial capture could
//! honestly live in, and choosing what an attempt record stores is T068's call,
//! not this file's.
//!
//! [`Command::process_group`]: std::os::unix::process::CommandExt::process_group
//! [`Error::Provider`]: crate::Error::Provider

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use time::OffsetDateTime;

use crate::{AttemptId, Bus, Error, Event, EventKind, EventSeq, Outcome, Result, Stream};

/// The grace a signalled group gets to answer SIGTERM before SIGKILL goes to it.
///
/// Ten seconds, the grace ADR-0038 gives a gate for the same reason: a CLI
/// mid-request wants to flush what it was writing, and a grace short enough to
/// skip that loses the output this timeout is about to report.
const TERM_GRACE: Duration = Duration::from_secs(10);

/// How long to wait for a signalled group to report itself empty, after each
/// signal.
///
/// Two seconds, then this file stops waiting and says so. A descendant that
/// called `setsid` is out of reach forever, and the alternative to a bound is a
/// supervisor that never returns.
const POST_KILL_GRACE: Duration = Duration::from_secs(2);

/// How often the collector looks at its queue when nothing has arrived.
///
/// Two milliseconds: one turn of the loop that both advances the clocks and
/// notices an exit status, so it is the shortest wait worth taking.
const CHUNK_POLL: Duration = Duration::from_millis(2);

/// The most bytes of one stream this function keeps.
///
/// A megabyte per stream is well past any provider's whole answer, and the bound
/// is what stops a CLI that dumps without pausing from growing this process
/// without limit. The margin is the interesting part: the retained text stays a
/// true prefix and *later* lines are what fall off, because a session's first
/// words are what a classifier reads and a prefix can be quoted from where a
/// window that skipped about could not.
const MAX_CAPTURED_BYTES: usize = 1024 * 1024;

/// How much prompt one session may be handed.
///
/// Four megabytes is a large document and a small model's context. A prompt past
/// this cannot be written into a pipe nobody is draining, so it is refused before
/// the CLI is started — a session that never started has nothing to clean up —
/// rather than left to block on a full buffer with both clocks already running.
const MAX_PROMPT_BYTES: usize = 4 * 1024 * 1024;

/// Run one provider session: spawn `cmd`, hand it `stdin_data`, stream what it
/// prints, and stop it when either clock runs out.
///
/// This is the attempt-taking half of this file, `run_streaming_as`, with the
/// attempt its output may be published under left unset, and that is the whole of the difference: a command, a
/// prompt, two durations and a bus name no attempt, and neither does an
/// [`crate::Invocation`] (ADR-0052 met the same absence one level up and refused
/// to invent an identity for it). A line published without one would be
/// attributed to an attempt number nobody handed out, so an unattributed session
/// captures everything it printed and publishes nothing. Whoever holds the
/// attempt calls `run_streaming_as` with it; ADR-0053 records that gap in
/// `Provider::invoke` and reports it rather than answering a question this
/// signature never asked.
///
/// # Errors
///
/// The same refusals as `run_streaming_as`, which is what this calls.
pub fn run_streaming(
    cmd: &mut Command,
    stdin_data: Option<&str>,
    idle_timeout: Duration,
    hard_timeout: Duration,
    bus: Option<&Bus>,
) -> Result<Outcome> {
    run_streaming_as(cmd, stdin_data, idle_timeout, hard_timeout, bus, None)
}

/// Run one provider session, publishing what it prints under `attempt`.
///
/// `attempt` is the answer this function cannot find for itself, and `None`
/// publishes nothing — see [`run_streaming`]. Nothing else about a session
/// depends on whether anybody is watching, because a provider that behaves
/// differently when watched is a provider whose scenario cannot be reproduced
/// headlessly: ADR-0050's contract for a `bus: Option<&Bus>`, and the reason a
/// bus here is somewhere output can go and never a reason to run, to wait, or to
/// kill differently.
///
/// `cmd` is an adapter's own argv, never a shell's. This function adds three
/// things to it: stdin is piped when `stdin_data` is `Some` and closed at the
/// spawn when it is `None` (a CLI that reads no prompt must not be left waiting
/// for a question nobody will ask), both output pipes are piped so they can be
/// read while the session is still running, and the child leads a process group
/// of its own.
///
/// `stdin_data` goes on a thread of its own and the write end is closed once the
/// whole prompt has been handed over; with no prompt there is no pipe, no thread,
/// and nothing for the session to wait on. A thread, because a CLI that never reads
/// its prompt fills the pipe's buffer, and a blocking write from here would sit
/// inside the session with both clocks running. A CLI that closes the read end
/// unread — which is what a CLI taking its prompt as an argument does — refuses
/// the write, and that is *its* answer about the prompt rather than this
/// function's failure: the session's exit status and two streams are what it
/// answered with. The refusal is named in an answer when one is being given about
/// a session that stopped, because a CLI that never read its prompt is a common
/// reason a session looks silent.
///
/// Both pipes are read to the end on threads of their own, so a session that
/// fills one cannot stall because nobody was emptying the other. Each line read is
/// published and kept in the order it was read, and resets the idle deadline to
/// then plus `idle_timeout`, capped at the hard deadline.
///
/// # Errors
///
/// [`Error::Provider`], naming the CLI and what could not be had: a `cmd` with no
/// program to run; a prompt above [`MAX_PROMPT_BYTES`]; a pipe or a reader thread
/// that could not be created; a program that could not be started; `hard_timeout`
/// spent, with what it took to stop it; `idle_timeout` spent, with the silence
/// that was measured; a session ended by a signal rather than an exit status; and
/// a process group this process has no leave to signal.
///
/// A session that ran and answered is not an error whatever it answered. An exit
/// code of 1, a refusal, a limit message and text that is not UTF-8 are all facts
/// an [`Outcome`] holds. Its `usage` and `session_id` come back `None`, because
/// what a session *reported* is the adapter's reading of a format only the
/// adapter knows, and a generic shell guessing at it is the substitution ADR-0049
/// refuses.
pub(crate) fn run_streaming_as(
    cmd: &mut Command,
    stdin_data: Option<&str>,
    idle_timeout: Duration,
    hard_timeout: Duration,
    bus: Option<&Bus>,
    attempt: Option<AttemptId>,
) -> Result<Outcome> {
    let argv = argv_of(cmd);
    let provider = argv.first().cloned().unwrap_or_default();
    let prompt = stdin_data.unwrap_or_default();
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(Error::Provider {
            provider,
            detail: format!(
                "the prompt is {}, past the {} this hands to one session",
                bytes(prompt.len()),
                bytes(MAX_PROMPT_BYTES)
            ),
        });
    }

    cmd.stdin(if stdin_data.is_none() {
        Stdio::null()
    } else {
        Stdio::piped()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    own_process_group(cmd);
    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|refusal| Error::Provider {
        provider: provider.clone(),
        detail: format!("could not start `{}`: {refusal}", argv.join(" ")),
    })?;

    // Both clocks are absolute and both start at the spawn: the hard one because
    // the session began now, the idle one because nothing has been read since.
    let hard_deadline = started + hard_timeout;
    let mut last_activity = started;
    let mut idle_deadline = started + idle_timeout;

    // A session handed no prompt was given no pipe to read one from, so there is
    // nothing to write and no thread that could hold the write end open beside
    // it: a CLI that reads no prompt is expected to answer, not to wait on a
    // question nobody is going to ask.
    let mut prompt_writer: Option<PromptWriter> = None;
    if stdin_data.is_some() {
        prompt_writer = Some(write_prompt(
            prompt,
            own_pipe(child.stdin.take(), &provider, "stdin")?,
        )?);
    }
    let (sender, chunks) = mpsc::channel();
    read_to_the_end(
        own_pipe(child.stdout.take(), &provider, "stdout")?,
        Stream::Stdout,
        &sender,
        bus,
        attempt,
    )?;
    read_to_the_end(
        own_pipe(child.stderr.take(), &provider, "stderr")?,
        Stream::Stderr,
        &sender,
        bus,
        attempt,
    )?;
    drop(sender);

    let mut watch = Watch::new(child);
    let mut kept = Capture::default();
    // Which clock expired, the duration it measured, and the budget it passed.
    // An answer names all three, because silence and unproductive work are
    // different failures and a caller cannot tell them apart from a word like
    // "timed out".
    let mut overrun: Option<Overrun> = None;

    // Everything the session wrote, as it arrived, until both pipes ended or a
    // stop took away the reason to keep listening. Chunks are observed here and
    // only here, so which stream answered decides nothing about how long the
    // session lives.
    loop {
        match chunks.recv_timeout(CHUNK_POLL) {
            Ok((stream, text)) => {
                kept.push(stream, &text);
                let now = Instant::now();
                last_activity = now;
                idle_deadline = (now + idle_timeout).min(hard_deadline);
            }
            // Both readers reached the end of their pipe, so every byte the
            // session wrote has been read. The idle clock has nothing left to
            // wait for, and only the hard deadline still governs the wait below.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        watch.poll()?;
        let now = Instant::now();
        if now >= hard_deadline {
            overrun = Some(Overrun::new(Clock::Hard, now - started, hard_timeout));
            watch.stop_group()?;
        } else if now >= idle_deadline {
            overrun = Some(Overrun::new(Clock::Idle, now - last_activity, idle_timeout));
            watch.stop_group()?;
        }
        if watch.listen_is_over() {
            break;
        }
    }

    // Nothing left to read, and the process is still this run's to wait for. The
    // clocks still govern that wait: a session that closed its own pipes and kept
    // working is no less subject to its timeout than one that never wrote, and
    // idle has no meaning once both pipes have ended.
    loop {
        watch.poll()?;
        let now = Instant::now();
        if now >= hard_deadline {
            if overrun.is_none() {
                overrun = Some(Overrun::new(Clock::Hard, now - started, hard_timeout));
            }
            watch.stop_group()?;
        }
        if watch.status.is_some() || watch.listen_is_over() {
            break;
        }
        thread::sleep(CHUNK_POLL);
    }
    // Nothing is waited for any more, so the answer is about to be given, and the
    // ladder above only runs while something is still being waited for: a child
    // that neither writes nor heeds signals is waited for by nobody, and without
    // this it would still be running when this function said it was finished —
    // the exact outcome this file exists to make impossible.
    watch.ensure_group_stopped()?;
    // A reader still copying bytes nobody asked it to stop holding a pipe for has
    // nowhere left to copy them to, and a writer still holding the write end open
    // would keep the child waiting for a prompt that arrived.
    drop(chunks);
    let prompt_refused = prompt_writer.and_then(PromptWriter::join_now);

    verdict(
        &provider,
        &watch,
        kept,
        overrun,
        &prompt_note(prompt_refused),
    )
}

/// What the run answers with: why it stopped listening, or what the session
/// returned.
///
/// A session that ran and answered is an [`Outcome`] whatever it answered — an
/// exit code of 1, a refusal, text that is not UTF-8 — and a session this run
/// had to stop, or that died rather than exited, is an error. Those are the only
/// two shapes, and nothing here reports a session as finished when the reason it
/// stopped is that this run stopped it: ADR-0050 assigned "killed by a signal
/// rather than exited" to the error channel precisely so a killed session cannot
/// be recorded as a passing one.
fn verdict(
    provider: &str,
    watch: &Watch,
    kept: Capture,
    overrun: Option<Overrun>,
    prompt_refused: &str,
) -> Result<Outcome> {
    if let Some(overrun) = overrun {
        return Err(Error::Provider {
            provider: provider.to_owned(),
            detail: format!(
                "{}; {}; read {} of stdout and {} of stderr before it stopped{}{}",
                overrun.phrase(),
                watch.killed_by.map_or_else(
                    || "its process group was stopped".to_owned(),
                    |killed| format!("its process group was {}", killed.phrase()),
                ),
                bytes(kept.stdout.len()),
                bytes(kept.stderr.len()),
                kept.dropped_note(),
                prompt_refused
            ),
        });
    }
    let Some(status) = watch.status else {
        // Unreachable while both clocks are armed: a session that neither reports
        // a status nor overruns a deadline is waited on forever, and the wait
        // above ends on one or the other. It is an error rather than a panic
        // because a supervisor that panics loses the run it was supervising.
        return Err(Error::Provider {
            provider: provider.to_owned(),
            detail: format!("reported no exit status and no timeout: {}", kept.summary()),
        });
    };
    match (status.code(), terminating_signal(status)) {
        (Some(exit_code), None) => Ok(Outcome {
            exit_code,
            stdout: kept.stdout,
            stderr: kept.stderr,
            // `None`, twice, for one reason: nothing asked this session what it
            // spent or what it was called, and what its text says about either is
            // the adapter's reading of a format only the adapter knows —
            // VISION.md §12 makes both of them detected capabilities, and
            // ADR-0049 refuses a guess standing in for a report.
            usage: None,
            session_id: None,
        }),
        // ADR-0050 assigned exactly this state — a session killed by a signal
        // rather than exited — to this task and to the error channel, so it is
        // not squeezed into an exit code here. Ours or the kernel's, a session
        // that did not return a status of its own has no status to report.
        (_, Some(signal)) => Err(Error::Provider {
            provider: provider.to_owned(),
            detail: format!(
                "was terminated by signal {signal} before it reported an exit status; {}",
                kept.summary()
            ),
        }),
        (None, None) => Err(Error::Provider {
            provider: provider.to_owned(),
            detail: format!(
                "reported an exit status that is neither a code nor a signal; {}",
                kept.summary()
            ),
        }),
    }
}

/// A clock that expired, and what it was looking at when it did.
struct Overrun {
    /// Which of the two clocks ran out.
    clock: Clock,
    /// How long the session had been silent, or been running.
    measured: Duration,
    /// The budget it passed, which is what the measurement is compared against.
    budget: Duration,
}

impl Overrun {
    /// Record that `clock` expired with `measured` against `budget`.
    fn new(clock: Clock, measured: Duration, budget: Duration) -> Self {
        Self {
            clock,
            measured,
            budget,
        }
    }

    /// The clause an error opens with: which clock, what it measured, and what
    /// that measurement passed.
    fn phrase(&self) -> String {
        match self.clock {
            Clock::Idle => format!(
                "silent for {}, past its {} idle timeout",
                millis(self.measured),
                millis(self.budget)
            ),
            Clock::Hard => format!(
                "ran for {}, past its {} hard timeout",
                millis(self.measured),
                millis(self.budget)
            ),
        }
    }
}

/// Which clock expired, and with what failure it leaves the caller holding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clock {
    /// Nothing arrived for `idle_timeout` — the shape of a hang, and the one
    /// VISION.md §12's `hang` scenario scripts.
    Idle,
    /// The session ran for `hard_timeout`, however productive it was.
    Hard,
}

/// The child, its process group, and what has been done to stop it.
struct Watch {
    /// The session's own process.
    child: Child,
    /// The group it leads, which is what gets signalled. [`own_process_group`]
    /// made it the leader, so this is the child's own id.
    group: u32,
    /// What it exited with, once it has said so.
    status: Option<ExitStatus>,
    /// Whether either clock expired.
    timed_out: bool,
    /// How the group was stopped, which is what an answer says stopped the
    /// session. Only a signal aimed at a group that was still there raises it, so
    /// an answer cannot claim a kill that stopped nothing.
    killed_by: Option<Killed>,
    /// What has been done to stop it, and when each step was taken.
    response: Response,
}

/// How a session's process group was stopped, named the way an answer names it.
///
/// A name rather than a `Signal`, because the question an answer is asked is "how
/// did this session come to be stopped" and `nix`'s own enum is non-exhaustive:
/// a variant added there would silently stop matching here, and a supervisor
/// whose explanation of a kill can stop being exhaustive is a supervisor that
/// stops explaining.
#[derive(Debug, Clone, Copy)]
enum Killed {
    /// SIGTERM, which its group answered: nothing here had to kill it.
    Term,
    /// SIGKILL, which nothing can answer, aimed at a group SIGTERM had not stopped.
    Kill,
}

impl Killed {
    /// What was done to it, in the clause an answer puts after "its process group
    /// was".
    fn phrase(self) -> &'static str {
        match self {
            Self::Term => "stopped by SIGTERM",
            Self::Kill => "killed with SIGKILL, which SIGTERM had not stopped",
        }
    }
}

/// What has been done to a session that overran a clock.
#[derive(Debug, Clone, Copy)]
enum Response {
    /// Nothing: it is inside both clocks, and it still holds its own pipes.
    Nothing,
    /// The group was sent SIGTERM at the instant this carries.
    Terminated(Instant),
    /// The group was sent SIGKILL at the instant this carries.
    Killed(Instant),
}

impl Watch {
    /// Watch `child`, which leads the group named by its own id.
    fn new(child: Child) -> Self {
        let group = child.id();
        Self {
            child,
            group,
            status: None,
            timed_out: false,
            killed_by: None,
            response: Response::Nothing,
        }
    }

    /// Take the status if the process has reported one.
    fn poll(&mut self) -> Result<()> {
        if self.status.is_none()
            && let Some(status) = self.child.try_wait()?
        {
            self.status = Some(status);
        }
        Ok(())
    }

    /// Stop the group: SIGTERM the first time, SIGKILL once its grace is spent.
    ///
    /// Both go to the group, so whatever the session spawned goes down with it
    /// rather than inheriting the mess. The ladder runs on the clock rather than
    /// on whether the leader has reported a status, because the leader's death is
    /// not the group's death: a descendant that deafens itself to SIGTERM keeps
    /// the group, and its id, alive after the leader is gone, and the group is
    /// what this supervisor is answerable for. A group that has emptied in the
    /// meantime answers `ESRCH`, which [`signal_group`] reads as the outcome it
    /// was sent for.
    ///
    /// `timed_out` is set whether or not there was anything left to signal: a
    /// session that ended in the instant between the deadline and the signal still
    /// overran its clock, and which clock was spent is what an answer is built
    /// from.
    fn stop_group(&mut self) -> Result<()> {
        self.timed_out = true;
        let now = Instant::now();
        match self.response {
            Response::Nothing => {
                signal_group(self.group, Signal::SIGTERM)?;
                self.response = Response::Terminated(now);
                self.killed_by = Some(Killed::Term);
            }
            Response::Terminated(since) if now >= since + TERM_GRACE => {
                signal_group(self.group, Signal::SIGKILL)?;
                self.response = Response::Killed(now);
                self.killed_by = Some(Killed::Kill);
            }
            Response::Terminated(_) | Response::Killed(_) => {}
        }
        Ok(())
    }

    /// Whether there is no longer a reason to keep reading the session's pipes.
    ///
    /// A group that has emptied is done with now, which is why this asks the group
    /// rather than only the clock: the ordinary path costs one syscall and waits
    /// for no grace. A signalled group otherwise gets [`TERM_GRACE`] to write what
    /// it writes on the way out and [`POST_KILL_GRACE`] for those words to arrive,
    /// and a stranger that left the group can extend neither.
    fn listen_is_over(&mut self) -> bool {
        let deadline = match self.response {
            Response::Nothing => return false,
            Response::Terminated(since) => since + TERM_GRACE + POST_KILL_GRACE,
            Response::Killed(since) => since + POST_KILL_GRACE,
        };
        group_is_empty(self.group) || Instant::now() >= deadline
    }

    /// Signal the group with SIGKILL if a clock expired and this is about to
    /// report itself finished.
    ///
    /// [`Watch::stop_group`] only runs while something is still being waited for,
    /// and a child that neither writes nor heeds signals is waited for by nobody.
    /// A group with no member left is asked nothing: an empty group is the
    /// guarantee — nothing that holds a lock, a port or a credential survives —
    /// and a SIGKILL aimed at nothing would leave this reporting a kill that
    /// stopped nothing, which is a different answer from the one an operator
    /// needs. Whether a session came down when it was asked or had to be killed
    /// is the difference between a CLI that behaves and one that ignores being
    /// stopped, and it is worth the one syscall that reads the group.
    fn ensure_group_stopped(&mut self) -> Result<()> {
        if !self.timed_out || group_is_empty(self.group) {
            return Ok(());
        }
        signal_group(self.group, Signal::SIGKILL)?;
        self.killed_by = Some(Killed::Kill);
        Ok(())
    }
}

/// One piece of a session's output, as it arrived on one of its two pipes.
type Chunk = (Stream, String);

/// What a session wrote, within [`MAX_CAPTURED_BYTES`] per stream.
#[derive(Default)]
struct Capture {
    /// Everything retained from stdout, in order.
    stdout: String,
    /// Everything retained from stderr, in order.
    stderr: String,
    /// Bytes of stdout this file let go of, and the same for stderr. A session
    /// that printed past the bound is one whose answer could not be held whole,
    /// and "whole" is a claim worth being able to refuse.
    dropped_stdout: usize,
    /// The stderr counterpart of [`Capture::dropped_stdout`].
    dropped_stderr: usize,
}

impl Capture {
    /// Retain one chunk on the stream it arrived on, dropping it whole once that
    /// stream is full. Lines fall off the end rather than being cut, so what is
    /// kept stays a prefix somebody can quote from.
    fn push(&mut self, stream: Stream, text: &str) {
        let (kept, dropped) = match stream {
            Stream::Stdout => (&mut self.stdout, &mut self.dropped_stdout),
            Stream::Stderr => (&mut self.stderr, &mut self.dropped_stderr),
        };
        if kept.len() + text.len() <= MAX_CAPTURED_BYTES {
            kept.push_str(text);
        } else {
            *dropped += text.len();
        }
    }

    /// What fell off the end, phrased only if something did.
    fn dropped_note(&self) -> String {
        let dropped = self.dropped_stdout + self.dropped_stderr;
        if dropped == 0 {
            return String::new();
        }
        format!(
            ", and {} more than the {} kept per stream were dropped",
            bytes(dropped),
            bytes(MAX_CAPTURED_BYTES)
        )
    }

    /// How much was read, for an answer about a session that is not being stopped.
    fn summary(&self) -> String {
        format!(
            "{} of stdout and {} of stderr were read",
            bytes(self.stdout.len()),
            bytes(self.stderr.len())
        )
    }
}

/// The thread writing a prompt, and the refusal it may have been given.
struct PromptWriter(JoinHandle<Option<String>>);

impl PromptWriter {
    /// Wait for the write to finish, and report why it could not if it could not.
    ///
    /// A thread still holding the write end open would keep the child waiting for
    /// a prompt that has already arrived, so this is called before the last word
    /// about a session is said. A refusal comes back rather than being raised: a
    /// CLI that answers without reading its prompt refused on purpose, and the
    /// session's own answer is the answer.
    fn join_now(self) -> Option<String> {
        let Self(handle) = self;
        handle
            .join()
            .unwrap_or_else(|_| Some("the thread writing it panicked".to_owned()))
    }
}

/// Hand `prompt` to `stdin` on a thread of this run's own, closing it afterwards.
fn write_prompt(prompt: &str, stdin: ChildStdin) -> Result<PromptWriter> {
    let prompt = prompt.to_owned();
    let handle = thread::Builder::new()
        .name("ktask-provider-stdin".to_owned())
        .spawn(move || {
            let mut stdin = stdin;
            // A `SIGPIPE` would take this whole process down, and the supervisor
            // is the thing a run depends on: a piped write end reports `EPIPE`
            // instead, which is what a closed reader is.
            match stdin
                .write_all(prompt.as_bytes())
                .and_then(|()| stdin.flush())
            {
                Ok(()) => None,
                Err(refusal) => Some(refusal.to_string()),
            }
        })
        .map_err(|refusal| thread_refused("write the prompt", &refusal))?;
    Ok(PromptWriter(handle))
}

/// Copy one pipe into the collector, a line at a time, publishing each as it is
/// read.
///
/// The reader publishes rather than the collector, because the promise being kept
/// is about the instant a line was *observed*: an event stamped and handed to the
/// bus here carries the interleaving the two streams actually arrived in, whereas
/// one handed through a queue is ordered by when somebody got round to emptying
/// it. A line is what a reader hands on, for the reason in the module docs.
///
/// The thread ends with its pipe. Once the collector is dropped a send fails and
/// the copy stops, which is the bound that keeps a session killed for having
/// printed too much from also being a session that outlives its own reader.
fn read_to_the_end(
    pipe: impl std::io::Read + Send + 'static,
    stream: Stream,
    chunks: &Sender<Chunk>,
    bus: Option<&Bus>,
    attempt: Option<AttemptId>,
) -> Result<()> {
    let name = match stream {
        Stream::Stdout => "ktask-provider-stdout",
        Stream::Stderr => "ktask-provider-stderr",
    };
    let bus = bus.cloned();
    let chunks = chunks.clone();
    let reader = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut buffered = BufReader::new(pipe);
            let mut line: Vec<u8> = Vec::new();
            loop {
                line.clear();
                // Zero bytes read is the far end closing; a failed read is a pipe
                // that broke. Either way everything already published has been
                // published: what a session wrote is never thrown away here.
                let Ok(read) = buffered.read_until(b'\n', &mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                // Bytes that are not UTF-8 become the replacement character
                // rather than a panic or a dropped line (VISION.md §13).
                let text = String::from_utf8_lossy(&line).into_owned();
                if let (Some(bus), Some(attempt)) = (&bus, attempt) {
                    bus.publish(Event {
                        seq: EventSeq::new(0),
                        ts: OffsetDateTime::now_utc(),
                        // Unattributed, and not because nobody asked: this layer
                        // is handed no task, and an event claiming one would be a
                        // claim about a run it cannot see (ADR-0053).
                        task_id: None,
                        kind: EventKind::AgentOutput {
                            attempt,
                            stream,
                            // The catalog entry is one line of output, and the
                            // break that ended it is not part of what the line
                            // says — the reading the `dummy` adapter's published
                            // lines already take (ADR-0052).
                            text: text.trim_end_matches(['\n', '\r']).to_owned(),
                        },
                    });
                }
                if chunks.send((stream, text)).is_err() {
                    break;
                }
            }
        })
        .map_err(|refusal| thread_refused(name, &refusal))?;
    drop(reader);
    Ok(())
}

/// Take a pipe this function asked the standard library to create.
///
/// Asking above makes absence impossible; the refusal is here so the one place it
/// could ever fire answers with an error rather than a panic.
fn own_pipe<T>(piped: Option<T>, provider: &str, which: &str) -> Result<T> {
    piped.ok_or_else(|| Error::Provider {
        provider: provider.to_owned(),
        detail: format!("the session's {which} was never piped"),
    })
}

/// The error a thread this run needs could not be started for.
fn thread_refused(what: &str, refusal: &std::io::Error) -> Error {
    Error::Provider {
        provider: String::new(),
        detail: format!("could not start the thread created to {what}: {refusal}"),
    }
}

/// Put `command` in a process group of which it will be the leader.
///
/// `0` asks the kernel for the child's own id as its group id, and the spawn does
/// it before the child runs one instruction: there is no window in which the
/// session is still in this process's group, so no window in which a timeout
/// cannot reach it. Its children join by inheritance, which is the point — one
/// signal covers everything it spawned, without this process holding a list of
/// pids it can neither trust nor re-query. ADR-0038 records this against a
/// `setpgid` after the spawn, which would leave that window open.
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

/// Send `signal` to every process in the group led by `group`.
///
/// A group that has emptied answers `ESRCH`, read as the outcome the signal was
/// sent for rather than as a failure: a supervisor that reported an error because
/// it could not kill something already dead would be reporting on its own
/// bookkeeping instead of on the run. Anything else — a group this process has no
/// leave to signal — is a real refusal and travels as one.
fn signal_group(group: u32, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(group.cast_signed()), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(refusal) => Err(Error::from(std::io::Error::from(refusal))),
    }
}

/// Whether the group led by `group` has no member left to signal.
///
/// Signal `0` asks that question without asking anything to happen, and `ESRCH` is
/// the answer "nothing is there". It is what lets a timeout return as soon as its
/// group is empty instead of after a grace nobody needed.
fn group_is_empty(group: u32) -> bool {
    matches!(
        killpg(Pid::from_raw(group.cast_signed()), None),
        Err(Errno::ESRCH)
    )
}

/// A refusal to write the prompt, phrased only if there was one.
///
/// It is a detail of an answer about a stopped session and never a failure of its
/// own, for the reason in [`run_streaming_as`]'s documentation.
fn prompt_note(refused: Option<String>) -> String {
    match refused {
        Some(refusal) => format!(", and the prompt it was given was refused ({refusal})"),
        None => String::new(),
    }
}

/// How long a duration is, in the milliseconds an answer names it with.
fn millis(elapsed: Duration) -> String {
    let millis = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
    format!("{millis}ms")
}

/// How many bytes a count is, in the words an answer names it with.
///
/// The exact count, with no rounding to a rounder unit: these figures are quoted
/// beside a bound that a session either passed or did not, and a rendering that
/// rounds `1048576` and `1048577` to the same words cannot be read as a
/// measurement of anything.
fn bytes(count: usize) -> String {
    format!("{count} bytes")
}

/// The signal that ended a process, when a signal ended it.
///
/// `None` for a process that ran to its own end, which is a different answer from
/// an exit code and is kept apart from it for that reason.
#[cfg(unix)]
fn terminating_signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;

    status.signal()
}

/// The signal that ended a process, on a platform that reports none.
#[cfg(not(unix))]
fn terminating_signal(_status: ExitStatus) -> Option<i32> {
    None
}

/// The words `cmd` will be run with, program first, so an answer can name the
/// command rather than a paraphrase of it.
fn argv_of(cmd: &Command) -> Vec<String> {
    let mut argv = vec![cmd.get_program().to_string_lossy().into_owned()];
    argv.extend(cmd.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    argv
}

#[cfg(test)]
mod sessions {
    // Every test here runs a real session: a shell is spawned in its own process
    // group, its two pipes are read while it works, and something is either
    // waited for or killed. Each assertion is something a caller, a watching bus,
    // or the kernel's own process table can be asked to confirm — the status a
    // session reported, the order two streams arrived in, the bytes a capture
    // kept, or the fact that nothing the session spawned is still running.

    use super::*;
    use std::path::Path;

    /// The shell the fixture sessions run under, absolute so that no test
    /// inherits whatever `PATH` the harness happened to start with.
    const SHELL: &str = "/bin/sh";

    /// A pair of clocks generous enough that nothing a healthy session does is
    /// mistaken for a hang, and short enough that a bug in this file fails a
    /// test in seconds instead of stalling the suite.
    const ROOMY_IDLE: Duration = Duration::from_secs(5);
    const ROOMY_HARD: Duration = Duration::from_secs(20);

    /// An idle budget short enough that a test can watch it expire, and the
    /// margin both idle-watchdog tests below measure themselves against.
    ///
    /// Four hundred milliseconds: long enough that a scheduler which delivers a
    /// `sleep 0.05` a few frames late is not mistaken for a hung session, and
    /// short enough that a hang is caught in well under a second.
    const SILENCE_BUDGET: Duration = Duration::from_millis(400);

    /// A session whose whole program is `script`.
    fn session(script: &str) -> Command {
        let mut cmd = Command::new(SHELL);
        cmd.arg("-c").arg(script);
        cmd
    }

    /// Run `script` inside both generous clocks and hand back what it answered
    /// with. A session that runs out of a generous clock is a bug in this file,
    /// and the failure says which session.
    fn it_answers(script: &str) -> Outcome {
        run(&mut session(script), None).unwrap_or_else(|failure| {
            panic!("`{script}` was to run and answer, not fail: {failure}")
        })
    }

    /// The generous-clock run of `cmd`, with `prompt` handed to it.
    fn run(cmd: &mut Command, prompt: Option<&str>) -> Result<Outcome> {
        run_streaming(cmd, prompt, ROOMY_IDLE, ROOMY_HARD, None)
    }

    /// The detail of a provider failure, which is where every answer about a
    /// session that did not answer is written.
    fn refusal(error: &Error) -> String {
        let Error::Provider { provider, detail } = &error else {
            panic!(
                "a session that could not be brought to an answer fails as a provider error, not {error:?}"
            );
        };
        assert_eq!(
            provider, SHELL,
            "the failure names the CLI that was run, which is what a report and a \
             preflight both read: {error}"
        );
        detail.clone()
    }

    /// The pid a session wrote to `file`, which every fixture here makes the pid
    /// of the process the test is watching.
    fn pid_written_to(file: &Path) -> u32 {
        let written = std::fs::read_to_string(file)
            .unwrap_or_else(|failure| panic!("`{}` should exist: {failure}", file.display()));
        written.trim().parse().unwrap_or_else(|failure| {
            panic!(
                "`{}` should hold a pid, it holds {written:?}: {failure}",
                file.display()
            )
        })
    }

    /// The process group `pid` leads or belongs to, as the kernel's own table
    /// says.
    fn process_group_of(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // Field two is the command name in parentheses, which can itself hold a
        // space and a parenthesis, so the fields after it are counted from the
        // last `)` rather than from the front. What follows is state, the parent
        // pid, then the group id.
        let after = stat.get(stat.rfind(')')? + 1..)?;
        let mut fields = after.split_whitespace();
        let (_state, _parent, group) = (fields.next()?, fields.next()?, fields.next()?);
        group.parse().ok()
    }

    /// Whether `pid` is still executing. A zombie counts as gone: it has stopped
    /// executing and holds nothing but a status its parent has not read, and what
    /// a timed-out session must not leave behind is something that goes on
    /// running.
    fn is_running(pid: u32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        match stat.rfind(')') {
            Some(close) => {
                let after = stat.get(close + 1..).unwrap_or_default();
                !matches!(after.split_whitespace().next(), Some("Z" | "X"))
            }
            None => true,
        }
    }

    /// Every live member of the group led by `group`.
    fn group_members(group: u32) -> Vec<u32> {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return Vec::new();
        };
        entries
            .filter_map(std::result::Result::ok)
            .filter_map(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
            })
            .filter(|pid| is_running(*pid) && process_group_of(*pid) == Some(group))
            .collect()
    }

    /// Wait up to `limit` for the group led by `group` to have no live member
    /// left, and hand back whoever is still there.
    ///
    /// A signal is not synchronous: the kernel takes a process down on the next
    /// tick it is given. Waiting is what keeps a real assertion from failing on a
    /// scheduler that had not gotten there yet.
    fn wait_until_group_is_empty(group: u32, limit: Duration) -> Vec<u32> {
        let deadline = Instant::now() + limit;
        let mut left = group_members(group);
        while !left.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
            left = group_members(group);
        }
        left
    }

    #[test]
    fn a_session_answers_with_its_own_exit_code_and_both_streams_whole() {
        let outcome = it_answers("printf 'one\\n'; printf 'bad\\n' >&2; exit 3");

        assert_eq!(
            outcome.exit_code, 3,
            "the session chose 3, so 3 is the answer this reports"
        );
        assert_eq!(outcome.stdout, "one\n");
        assert_eq!(outcome.stderr, "bad\n");
        assert_eq!(
            outcome.usage, None,
            "nothing asked this session what it spent, so it claims nothing"
        );
        assert_eq!(
            outcome.session_id, None,
            "and nothing asked what it was called"
        );
    }

    #[test]
    fn a_session_that_prints_nothing_answers_nothing() {
        let outcome = it_answers("true");

        assert_eq!(outcome.exit_code, 0);
        assert!(
            outcome.stdout.is_empty() && outcome.stderr.is_empty(),
            "a session that said nothing is reported as having said nothing, not as \
             having said an empty line: {outcome:?}"
        );
    }

    #[test]
    fn an_exit_code_of_one_is_an_answer_and_not_a_failure() {
        let outcome = run(&mut session("exit 1"), None)
            .expect("a session that ran and refused is still a session that answered");

        assert_eq!(
            outcome.exit_code, 1,
            "a refusal is a fact about the work, and turning it into an error here \
             would lose the stdout that explains it"
        );
    }

    #[test]
    fn bytes_that_are_not_utf8_arrive_as_replacement_characters() {
        let outcome = it_answers("printf 'a\\377b\\n'");

        assert_eq!(
            outcome.stdout,
            "a\u{FFFD}b\n",
            "the byte was neither dropped nor allowed to fail the run: {:?}",
            outcome.stdout.as_bytes()
        );
    }

    #[test]
    fn what_was_handed_to_stdin_arrives_and_the_write_end_closes_after_it() {
        let outcome = run(
            &mut session("printf 'read:[%s]\\n' \"$(cat)\""),
            Some("hello\n"),
        )
        .expect("a session that reads its prompt answers");

        assert_eq!(
            outcome.stdout, "read:[hello]\n",
            "command substitution drops the final newline, so the prompt arrived whole \
             and `cat` saw the end of it rather than waiting forever: {:?}",
            outcome.stdout
        );
    }

    #[test]
    fn a_session_that_takes_no_prompt_is_handed_nothing_to_read() {
        let outcome = it_answers("printf 'read:[%s]\\n' \"$(cat)\"");

        assert_eq!(
            outcome.stdout, "read:[]\n",
            "with no prompt to write there is no pipe to hand over: a CLI that reads \
             stdin and was given a terminal would wait on a question nobody is going to ask"
        );
    }

    #[test]
    fn a_session_leads_a_group_of_its_own_that_everything_it_spawned_shares() {
        // The group is read from inside the session, while it is still alive to be
        // asked: a finished session is reaped and its pid is gone from the process
        // table, so a test that looked afterwards would be reading somebody else's
        // id entirely.
        let outcome = it_answers(
            r#"echo "pid=$$"
ps -o pgid= -p $$ | tr -d ' ' | sed 's/^/own_group=/'
/bin/sh -c 'echo "nested=$$"; ps -o pgid= -p $$ | tr -d " " | sed "s/^/nested_group=/"'"#,
        );
        let own = printed_number(&outcome.stdout, "pid");
        let own_group = printed_number(&outcome.stdout, "own_group");
        let nested = printed_number(&outcome.stdout, "nested");
        let nested_group = printed_number(&outcome.stdout, "nested_group");

        assert_eq!(
            own_group, own,
            "the session leads a group named after itself rather than sitting in the one \
             this test leads: a timeout that signalled this pid alone would leave a group \
             of strangers behind"
        );
        assert_eq!(
            nested_group, own_group,
            "pid {nested} was spawned by the session and joined its group by inheritance, \
             which is what lets one signal reach it"
        );
        assert_ne!(
            own_group,
            process_group_of(std::process::id()).unwrap_or(own),
            "killing a session must never be able to kill the supervisor and every other \
             session it is running"
        );
    }

    /// The `<name>=<number>` line a session printed, as a number.
    fn printed_number(output: &str, name: &str) -> u32 {
        let prefix = format!("{name}=");
        let line = output
            .lines()
            .find(|line| line.starts_with(&prefix))
            .unwrap_or_else(|| {
                panic!("a `{prefix}<number>` line was printed, stdout was {output:?}")
            });
        line[prefix.len()..]
            .trim()
            .parse()
            .unwrap_or_else(|failure| panic!("`{line}` should hold a number: {failure}"))
    }

    #[test]
    fn both_streams_reach_a_watcher_in_the_order_their_bytes_arrived() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();
        let outcome = run_streaming_as(
            &mut session("printf 'a\\n'; sleep 0.05; printf 'b\\n' >&2; sleep 0.05; printf 'c\\n'"),
            None,
            ROOMY_IDLE,
            ROOMY_HARD,
            Some(&bus),
            Some(AttemptId::new(4)),
        )
        .expect("a session that prints on both streams answers");

        let (events, dropped) = watcher.drain();
        assert_eq!(dropped, 0, "three lines cannot overflow a live view");
        let heard: Vec<(Stream, &str)> = events
            .iter()
            .map(|event| match &event.kind {
                EventKind::AgentOutput {
                    attempt,
                    stream,
                    text,
                } => {
                    assert_eq!(
                        *attempt,
                        AttemptId::new(4),
                        "the caller said which run this was, so every line carries it"
                    );
                    (*stream, text.as_str())
                }
                other => panic!("a provider's output is an AgentOutput, not {other:?}"),
            })
            .collect();

        assert_eq!(
            heard,
            vec![
                (Stream::Stdout, "a"),
                (Stream::Stderr, "b"),
                (Stream::Stdout, "c")
            ],
            "the two streams arrive interleaved, not stdout-drained-then-stderr: an \
             operator watching a hang needs the last thing it said, whichever pipe it \
             said it on"
        );
        assert!(
            events.iter().all(|event| event.task_id.is_none()),
            "this layer was handed no task, so it claims none"
        );
        assert_eq!(outcome.stdout, "a\nc\n");
        assert_eq!(outcome.stderr, "b\n");
    }

    #[test]
    fn a_line_split_across_two_writes_is_one_event_and_not_two() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();

        let outcome = run_streaming_as(
            &mut session("printf 'st'; sleep 0.1; printf 'out\\n'"),
            None,
            ROOMY_IDLE,
            ROOMY_HARD,
            Some(&bus),
            Some(AttemptId::new(1)),
        )
        .expect("a session that writes a line in two goes answers");

        let (events, _) = watcher.drain();
        assert_eq!(
            events.len(),
            1,
            "a half line is not a line: publishing what had been read so far would put \
             an empty or truncated line on a live view that cannot take it back: {events:?}"
        );
        let EventKind::AgentOutput { text, stream, .. } = &events[0].kind else {
            panic!("provider output is an AgentOutput");
        };
        assert_eq!(text, "stout", "the line arrived whole, once");
        assert_eq!(*stream, Stream::Stdout);
        assert_eq!(outcome.stdout, "stout\n");
    }

    #[test]
    fn a_session_nobody_attributed_an_attempt_to_publishes_nothing_and_answers_the_same() {
        let script = "printf 'one\\ntwo\\n'";
        let bus = Bus::new();
        let mut watcher = bus.subscribe();

        let attributed = run_streaming_as(
            &mut session(script),
            None,
            ROOMY_IDLE,
            ROOMY_HARD,
            Some(&bus),
            Some(AttemptId::new(1)),
        )
        .expect("an attributed session is heard");
        let (heard, _) = watcher.drain();
        assert_eq!(heard.len(), 2, "the attributed session was heard twice");

        let unattributed = run_streaming_as(
            &mut session(script),
            None,
            ROOMY_IDLE,
            ROOMY_HARD,
            Some(&bus),
            None,
        )
        .expect("an unattributed session answers the same way");
        let (silence, dropped) = watcher.drain();
        assert!(
            silence.is_empty() && dropped == 0,
            "a line with no attempt to attribute it is not given an invented one, which \
             would be indistinguishable from a real attempt in the one stream the \
             interface filters by attempt: {} events, {dropped} dropped",
            silence.len()
        );

        let alone = run(&mut session(script), None).expect("a session with no bus answers");
        assert_eq!(alone, attributed, "a live view is a listener, not an input");
        assert_eq!(
            unattributed, attributed,
            "and an attempt nobody handed out changes nothing about the answer either"
        );
    }

    #[test]
    fn a_session_that_stops_talking_is_stopped_and_the_silence_is_measured() {
        let started = Instant::now();
        let error = run_streaming(
            &mut session("printf 'one\\n'; exec sleep 30"),
            None,
            Duration::from_millis(300),
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that printed once and then slept is a hang, not an answer");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("idle timeout") && detail.contains("silent for"),
            "the failure names the clock that expired and the silence it measured, \
             because the silence is the fact about the agent: {detail}"
        );
        assert!(
            detail.contains("300ms"),
            "the budget it passed is named: {detail}"
        );
        assert!(
            detail.contains("read 4 bytes of stdout and 0 bytes of stderr"),
            "what was read before the stop is the evidence a hang is classified from: \
             {detail}"
        );
        assert!(
            !detail.contains("hard timeout"),
            "nothing about this session spent its hard ceiling: {detail}"
        );
        assert!(
            waited >= Duration::from_millis(300) && waited < Duration::from_secs(10),
            "the idle clock was enforced and not merely recorded: waited {waited:?} for \
             a budget of 300ms of silence"
        );
    }

    #[test]
    fn a_session_that_keeps_talking_outlives_the_idle_timeout_it_would_otherwise_hit() {
        let started = Instant::now();
        let outcome = run_streaming(
            &mut session(
                "i=1; while [ $i -le 6 ]; do printf 'line %d\\n' $i; i=$((i+1)); sleep 0.1; done",
            ),
            None,
            Duration::from_millis(500),
            ROOMY_HARD,
            None,
        )
        .expect("a session that prints inside its idle budget is slow, not gone");

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            outcome.stdout, "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n",
            "every line a talking session printed is kept"
        );
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "the idle timer is re-armed by each chunk, so a session slower in total than \
             its idle budget still finishes: it was stopped after {:?}",
            started.elapsed()
        );
    }

    /// The duration an answer names right after `phrase`, read back as the
    /// milliseconds it was written with.
    ///
    /// What a watchdog reports is the only place a caller can learn how quiet a
    /// session had gotten, so the two tests below take the number out of the words
    /// and compare it, rather than trusting that words to have been printed.
    fn reported_millis(detail: &str, phrase: &str) -> Duration {
        let at = detail
            .find(phrase)
            .unwrap_or_else(|| panic!("the answer was to name {phrase:?}: {detail}"));
        let digits: String = detail[at + phrase.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        let millis: u64 = digits.parse().unwrap_or_else(|failure| {
            panic!("`{phrase}` was to be followed by a millisecond count: {failure}: {detail}")
        });
        assert!(
            detail[at + phrase.len() + digits.len()..].starts_with("ms"),
            "milliseconds are the unit an answer names a duration with: {detail}"
        );
        Duration::from_millis(millis)
    }

    /// The first half of the outcome T058 states: a session that stops talking is
    /// stopped, and what it is reported for is the silence it fell into.
    #[test]
    fn idle_watchdog_kills_a_silent_session() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid file");
        let own_file = scratch.path().join("own");
        // One line, then its own pid, then thirty seconds of sleep — seventy-five
        // times the budget it is being watched by. The pid goes to a file and not to
        // stdout, so reading it back cannot re-arm the clock under test.
        let script = format!(
            "printf 'hello\\n'; echo $$ > '{}'; exec sleep 30",
            own_file.display()
        );
        let started = Instant::now();

        let error = run_streaming(
            &mut session(&script),
            None,
            SILENCE_BUDGET,
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that printed once and then slept through its idle budget hung");
        let detail = refusal(&error);
        let waited = started.elapsed();

        let silence = reported_millis(&detail, "silent for ");
        assert!(
            silence >= SILENCE_BUDGET,
            "the answer names the silence it measured, and a watchdog that fired before its \
             budget was spent stopped a session that had done nothing wrong: reported \
             {silence:?} against a budget of {SILENCE_BUDGET:?}: {detail}"
        );
        assert!(
            silence < Duration::from_secs(10),
            "the figure is the gap since the last line arrived — not the thirty seconds the \
             session meant to sleep, and not its whole life since the spawn: reported \
             {silence:?} for a session stopped {waited:?} after it printed: {detail}"
        );
        assert!(
            detail.contains("idle timeout"),
            "the answer names which clock expired, so a hang is tellable from an overlong \
             run without parsing a number: {detail}"
        );
        assert!(
            !detail.contains("hard timeout"),
            "this session never came near its ceiling, and an answer naming the other clock \
             would send a caller looking at the wrong one: {detail}"
        );
        assert!(
            detail.contains("read 6 bytes of stdout and 0 bytes of stderr"),
            "the one line it managed to print is the evidence a hang gets classified from, \
             and it survives the kill: {detail}"
        );
        assert!(
            detail.contains("stopped by SIGTERM"),
            "the watchdog stopped it rather than reporting on it and walking away, and the \
             answer says how: {detail}"
        );

        let own = pid_written_to(&own_file);
        assert!(
            wait_until_group_is_empty(own, Duration::from_secs(5)).is_empty(),
            "the answer called the session stopped while pid {own} was still in its group: a \
             hung CLI holding a lock file, a port or a credential would be handed to the \
             next attempt"
        );
        assert!(
            !is_running(own),
            "pid {own} was still executing after the run that reported killing it"
        );
        assert!(
            waited >= SILENCE_BUDGET && waited < Duration::from_secs(10),
            "the idle clock was enforced and not merely recorded: waited {waited:?} for a \
             budget of {SILENCE_BUDGET:?}, against a session that intended 30s and a ceiling \
             of {ROOMY_HARD:?}"
        );

        // The same hang one step later in a session that had been talking, which is the
        // case the reported figure is actually asked to describe. Eight lines inside the
        // budget and then silence: the session is old and quiet at once, and only one of
        // those two numbers is the fact a hang is classified from. A deadline or a
        // report keyed to the spawn instead of the last chunk answers this one wrong.
        let hang_session = "i=1; while [ $i -le 8 ]; do printf 'tick %d\\n' $i; \
                            i=$((i+1)); sleep 0.15; done; exec sleep 30";
        let hang_started = Instant::now();
        let hang_error = run_streaming(
            &mut session(hang_session),
            None,
            SILENCE_BUDGET,
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that answered eight times and then stopped answering is the ordinary shape of a hang");
        let hang_detail = refusal(&hang_error);
        let hang_waited = hang_started.elapsed();

        let hang_silence = reported_millis(&hang_detail, "silent for ");
        assert!(
            hang_silence >= SILENCE_BUDGET && hang_silence < SILENCE_BUDGET * 2,
            "the silence is measured from the last line that arrived, not from the moment \
             the session was spawned: it had been alive {hang_waited:?} and quiet \
             {hang_silence:?} against a budget of {SILENCE_BUDGET:?}: {hang_detail}"
        );
        assert!(
            hang_waited >= SILENCE_BUDGET * 3,
            "three budgets of talking preceded the silence, so the figure above cannot be \
             the session's age and is not held open by it: it lived {hang_waited:?} on a \
             budget of {SILENCE_BUDGET:?}: {hang_detail}"
        );
        assert!(
            hang_detail.contains("idle timeout") && !hang_detail.contains("hard timeout"),
            "the silence ended this session too, whatever it had been doing beforehand, and \
             a session that reaches its ceiling after a long conversation is a different \
             failure than this one: {hang_detail}"
        );
    }

    /// The other half of the same outcome: a session that keeps answering is slow and
    /// not gone, so it outlives the clock a silent session dies on. It prints well
    /// inside its budget every time and runs for several budgets' worth of wall
    /// clock — a deadline fixed at the spawn would have stopped it mid-sentence.
    #[test]
    fn idle_watchdog_spares_a_session_that_keeps_answering() {
        use std::fmt::Write as _;

        let started = Instant::now();
        let outcome = run_streaming(
            &mut session(
                "i=1; while [ $i -le 20 ]; do printf 'tick %d\\n' $i; i=$((i+1)); sleep 0.05; done",
            ),
            None,
            SILENCE_BUDGET,
            ROOMY_HARD,
            None,
        )
        .expect("a session that printed every 50ms against a 400ms budget is slow, not gone");
        let talked = started.elapsed();

        let mut ticks = String::new();
        for tick in 1..=20 {
            writeln!(&mut ticks, "tick {tick}").expect("a String cannot refuse a write");
        }
        assert_eq!(
            outcome.stdout, ticks,
            "every line a living session printed is kept, in order"
        );
        assert_eq!(outcome.exit_code, 0);
        assert!(
            talked >= SILENCE_BUDGET * 2,
            "the session was alive through two idle budgets' worth of silence it never fell \
             into, which is the whole point of re-arming: it finished after {talked:?} on a \
             budget of {SILENCE_BUDGET:?}"
        );
    }

    #[test]
    fn output_cannot_buy_a_session_past_its_hard_ceiling() {
        let started = Instant::now();
        let error = run_streaming(
            &mut session("while :; do printf 'x\\n'; sleep 0.02; done"),
            None,
            Duration::from_secs(1),
            Duration::from_millis(1_500),
            None,
        )
        .expect_err("a session that prints forever is the case the ceiling exists for");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("hard timeout") && !detail.contains("idle timeout"),
            "output was still arriving when it was stopped, so the clock that stopped it \
             was the ceiling and not the silence: {detail}"
        );
        assert!(
            waited >= Duration::from_millis(1_400) && waited < Duration::from_secs(10),
            "the ceiling was enforced at the moment it was spent: waited {waited:?} for a \
             ceiling of 1500ms"
        );
    }

    #[test]
    fn a_session_that_will_not_finish_is_stopped_by_its_hard_ceiling() {
        let started = Instant::now();
        let error = run_streaming(
            &mut session("printf 'first\\n'; exec sleep 60"),
            None,
            Duration::from_secs(30),
            Duration::from_millis(500),
            None,
        )
        .expect_err("a session nobody stopped runs until its ceiling stops it");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("hard timeout") && detail.contains("ran for"),
            "the failure names the ceiling and how long the session actually ran: {detail}"
        );
        assert!(
            detail.contains("read 6 bytes of stdout"),
            "what it managed to print before the ceiling is counted: {detail}"
        );
        assert!(
            waited >= Duration::from_millis(450) && waited < Duration::from_secs(10),
            "a session that slept for a minute was waited for for half a second: \
             {waited:?}"
        );
    }

    #[test]
    fn a_session_that_closed_its_pipes_and_kept_working_is_stopped_by_its_ceiling() {
        let started = Instant::now();
        let error = run_streaming(
            &mut session("exec sleep 60 >&- 2>&-"),
            None,
            Duration::from_secs(30),
            Duration::from_millis(500),
            None,
        )
        .expect_err("closing both pipes is not finishing");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("hard timeout"),
            "idle has no meaning once both pipes have ended, so the ceiling still \
             governs the wait: {detail}"
        );
        assert!(
            detail.contains("read 0 bytes of stdout and 0 bytes of stderr"),
            "nothing was written before the pipes were closed: {detail}"
        );
        assert!(
            waited < Duration::from_secs(10),
            "the session was not waited out: a supervisor that blocks on a process it \
             has already given up on is the thing that hangs next: {waited:?}"
        );
    }

    #[test]
    fn nothing_a_timed_out_session_spawned_survives_it() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid files");
        let kid_file = scratch.path().join("kid");
        let own_file = scratch.path().join("own");
        // A session that starts a child of its own, says who it was, and then runs
        // past its idle budget. The `sleep 20` is the grandchild: it holds both
        // pipes open, so a supervisor that only stopped the session's own process
        // would notice by waiting for it.
        let script = format!(
            "sleep 20 & echo $! > '{}' ; echo $$ > '{}' ; exec sleep 60",
            kid_file.display(),
            own_file.display()
        );

        let error = run_streaming(
            &mut session(&script),
            None,
            Duration::from_millis(400),
            ROOMY_HARD,
            None,
        )
        .expect_err("the session ran past its idle budget");
        assert!(
            refusal(&error).contains("idle timeout"),
            "it was the silence that stopped it"
        );

        let group = pid_written_to(&own_file);
        let kid = pid_written_to(&kid_file);
        let left = wait_until_group_is_empty(group, Duration::from_secs(5));
        assert!(
            left.is_empty(),
            "the timeout stopped the session's own process but left {left:?} alive in \
             its group: whatever file one of them has open, lock it holds or port it \
             listens on is still held, and the next session inherits the mess"
        );
        assert!(
            !is_running(kid),
            "pid {kid} was spawned by the session and outlived it"
        );
    }

    #[test]
    fn a_session_that_ignores_the_terminate_signal_is_killed() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid file");
        let own_file = scratch.path().join("own");
        // Ignored rather than caught, so the `sleep` inherits the ignoring and the
        // whole group stays put through the first signal.
        let script = format!(
            "trap '' TERM; echo $$ > '{}' ; exec sleep 90",
            own_file.display()
        );
        let started = Instant::now();

        let error = run_streaming(
            &mut session(&script),
            None,
            Duration::from_millis(400),
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that will not stop on being asked is still reported on");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("killed with SIGKILL"),
            "it was asked to stop with SIGTERM and was not, so the group was killed, and \
             the answer says which of the two stopped it: {detail}"
        );
        let own = pid_written_to(&own_file);
        assert!(
            wait_until_group_is_empty(own, Duration::from_secs(5)).is_empty(),
            "pid {own} ignored SIGTERM and was still running when the timeout was \
             reported: a run that says it finished while the session's tree still lives \
             leaks a process every attempt"
        );
        assert!(
            waited >= TERM_GRACE,
            "the terminate grace is real and not merely written down: it was escalated \
             after {waited:?}"
        );
        assert!(
            waited < TERM_GRACE + POST_KILL_GRACE,
            "the kill is aimed at the group once its own grace is spent, and the answer \
             is not held back for the second grace that exists to let a killed session's \
             words arrive: {waited:?} for a group whose own grace ended at {TERM_GRACE:?}"
        );
        assert!(
            waited < ROOMY_HARD,
            "the escalation is bounded, so a session that ignores being asked cannot hold \
             the run open: {waited:?}"
        );
    }

    #[test]
    fn a_session_that_answered_the_terminate_signal_is_not_reported_as_killed() {
        // `exec sleep` so the group has exactly one member: the process the timeout
        // signals and the run reaps. Nothing ignored being asked to stop, and no
        // straggler was left for a kill to be aimed at.
        let error = run_streaming(
            &mut session("exec sleep 60"),
            None,
            ROOMY_IDLE,
            Duration::from_millis(400),
            None,
        )
        .expect_err("a session that slept for a minute was stopped by its ceiling");
        let detail = refusal(&error);

        assert!(
            detail.contains("stopped by SIGTERM"),
            "the group came down when it was asked, so the answer says which of the two \
             signals stopped it: {detail}"
        );
        assert!(
            !detail.contains("SIGKILL"),
            "a session that never needed killing is reported as if it had ignored SIGTERM, \
             which is a different failure with a different fix: {detail}"
        );
    }

    #[test]
    fn a_session_killed_by_a_signal_did_not_answer_and_says_so() {
        let error = run_streaming(
            &mut session("printf 'x\\n'; kill -9 $$"),
            None,
            ROOMY_IDLE,
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that died on a signal has no exit status of its own to report");
        let detail = refusal(&error);

        assert!(
            detail.contains("terminated by signal 9"),
            "a session killed by a signal is not squeezed into an exit code, and least of \
             all into zero: {detail}"
        );
        assert!(
            detail.contains("2 bytes of stdout and 0 bytes of stderr were read"),
            "what it managed to print is still counted: {detail}"
        );
    }

    #[test]
    fn a_session_that_refused_its_prompt_still_answers_with_its_own_exit_code() {
        let outcome = run(
            &mut session("exec 0<&-; printf 'answered\\n'; exit 7"),
            Some("a prompt nobody wanted\n"),
        )
        .expect(
            "a CLI that takes its prompt as an argument rather than on stdin is the \
                 common case, not a failure of the run",
        );

        assert_eq!(outcome.exit_code, 7, "it answered, on its own terms");
        assert_eq!(outcome.stdout, "answered\n");
    }

    #[test]
    fn a_prompt_past_the_bound_is_refused_before_anything_is_started() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the marker");
        let marker = scratch.path().join("started");
        let script = format!("touch '{}' ; echo hi", marker.display());

        let error = run(
            &mut session(&script),
            Some(&"x".repeat(MAX_PROMPT_BYTES + 1)),
        )
        .expect_err("a prompt that cannot be written into a pipe nobody is draining is refused rather than tried");
        let detail = refusal(&error);

        assert!(
            detail.contains("past the 4194304 bytes"),
            "the bound is named as the four megabytes this file documents it to be, \
             not as some number echoed back from wherever the check came from: a \
             reader comparing an answer with a CLI's own argument limit needs the \
             bound itself on the page: {detail}"
        );
        assert!(
            detail.contains(&MAX_PROMPT_BYTES.to_string())
                && detail.contains(&(MAX_PROMPT_BYTES + 1).to_string()),
            "both numbers are named, so the reader can see how far over the bound it was: \
             {detail}"
        );
        assert!(
            !marker.exists(),
            "a session that was refused was never spawned, so there is nothing to clean \
             up and nothing to kill: {detail}"
        );
    }

    #[test]
    fn a_stream_past_the_capture_bound_keeps_its_beginning_and_drops_its_end() {
        let outcome = it_answers("yes 0123456789 | head -n 200000");

        assert_eq!(outcome.exit_code, 0);
        assert!(
            outcome.stdout.len() <= MAX_CAPTURED_BYTES,
            "the bound is what keeps a session that printed without stopping from being \
             kept without limit: {} bytes were kept",
            outcome.stdout.len()
        );
        assert!(
            outcome.stdout.starts_with("0123456789\n"),
            "what is kept is the beginning of the stream, so a reader can quote from it \
             rather than from a window that skipped the start"
        );
        assert!(
            outcome.stdout.len() > MAX_CAPTURED_BYTES - 11,
            "the capture filled up rather than stopping early: {} bytes",
            outcome.stdout.len()
        );
        assert_eq!(
            outcome.stdout.len() % 11,
            0,
            "whole lines fall off the end, so what remains is a prefix somebody can \
             quote and not a line cut in half"
        );
    }

    #[test]
    fn a_stopped_session_says_how_much_it_read_and_what_it_had_to_drop() {
        let error = run_streaming(
            &mut session("yes 0123456789 | head -n 200000; exec sleep 60"),
            None,
            Duration::from_secs(2),
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that printed a great deal and then went silent is a hang");
        let detail = refusal(&error);

        assert!(
            detail.contains("read 1048575 bytes of stdout"),
            "the answer says exactly how much was kept: {detail}"
        );
        assert!(
            detail.contains("more than the 1048576 bytes kept per stream were dropped"),
            "and it refuses to claim the capture is whole when it is not: {detail}"
        );
    }

    #[test]
    fn a_program_that_cannot_be_started_is_reported_rather_than_waited_for() {
        let started = Instant::now();
        let error = run(&mut Command::new("/definitely/not/a/program"), None)
            .expect_err("a program that does not exist is not a session that answered");
        let Error::Provider { provider, detail } = &error else {
            panic!("a session that never started fails as a provider error, not {error:?}");
        };

        assert_eq!(
            provider, "/definitely/not/a/program",
            "it names what it tried to run"
        );
        assert!(
            detail.contains("could not start"),
            "and says that the failure was in starting it: {detail}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the refusal is immediate: nothing was spawned, so nothing is waited for"
        );
    }

    #[test]
    fn a_prompt_exactly_at_the_bound_is_handed_over_rather_than_refused() {
        let outcome = run(&mut session("wc -c"), Some(&"x".repeat(MAX_PROMPT_BYTES))).expect(
            "a prompt of exactly the bound is inside it, and a session that drains \
                     its stdin can be handed all of it",
        );

        assert_eq!(
            outcome.stdout.trim(),
            MAX_PROMPT_BYTES.to_string(),
            "every byte arrived, so the bound was written to and not merely compared with"
        );
    }

    #[test]
    fn a_session_that_read_its_prompt_is_not_answered_as_though_it_refused_it() {
        let error = run_streaming(
            &mut session("read -r line; printf 'got %s\\n' \"$line\"; exec sleep 60"),
            Some("a prompt it took in\n"),
            Duration::from_millis(300),
            ROOMY_HARD,
            None,
        )
        .expect_err("it read its prompt and then went silent, which is the idle clock's business");
        let detail = refusal(&error);

        assert_eq!(
            detail.matches("prompt").count(),
            0,
            "a session that stopped after \
             taking its prompt is reported for the silence it fell into and for nothing \
             else; a run that claims a prompt refusal it never observed teaches a reader \
             to distrust the claim: {detail}"
        );
    }

    #[test]
    fn a_session_handed_a_prompt_it_never_read_says_so_in_the_answer_that_stops_it() {
        // 256 KiB cannot fit a pipe's buffer, so the writer is still blocked in its
        // write when the session closes the read end: the refusal is certain rather
        // than a race between a small write and the child's first instruction.
        let prompt = "x".repeat(256 * 1024);
        let error = run_streaming(
            &mut session("exec 0<&-; exec sleep 60"),
            Some(&prompt),
            Duration::from_millis(300),
            ROOMY_HARD,
            None,
        )
        .expect_err("it shut its prompt unread and then said nothing at all");
        let detail = refusal(&error);

        assert!(
            detail.contains("the prompt it was given was refused"),
            "a CLI that takes its prompt as an argument is the common reason a session \
             looks silent, and an answer about that silence that omits it sends the reader \
             looking for a hang that is not there: {detail}"
        );
    }

    #[test]
    fn the_capture_bound_is_per_stream_so_one_flood_cannot_starve_the_other() {
        let error = run_streaming(
            // `head` takes the flood out of the pipe it is given and writes it to
            // the session's stderr, so the flood has an end: 200000 lines of eleven
            // bytes each is 2.2 MB, twice the bound, and the session is silent from
            // then on. An endless flood would hold the idle clock open forever and
            // the ceiling would be the thing that answered.
            &mut session("yes 0123456789 | head -n 200000 1>&2; exec sleep 60"),
            None,
            Duration::from_secs(2),
            ROOMY_HARD,
            None,
        )
        .expect_err("a session that flooded its stderr and then went silent is a hang");
        let detail = refusal(&error);

        assert!(
            detail.contains("read 0 bytes of stdout and 1048575 bytes of stderr"),
            "the bound belongs to one stream at a time: stdout was never written to while \
             stderr filled a megabyte of its own, and what fell off is counted across both \
             of them: {detail}"
        );
        assert!(
            detail.contains("were dropped"),
            "and the answer still refuses to call the capture whole: {detail}"
        );
    }

    #[test]
    fn a_stranger_that_left_the_group_cannot_hold_a_timed_out_session_open() {
        // `setsid` puts the sleep in a session of its own, which is how a daemon
        // detaches. It is out of reach of a group signal and it keeps the stdout pipe
        // it inherited open for twelve seconds, so the only thing that can end this
        // run is what the group itself reports. The bound below is well inside those
        // twelve seconds and well outside the grace a group of its own is given.
        let started = Instant::now();
        let error = run_streaming(
            &mut session("setsid sleep 12 & printf 'early\\n'; exec sleep 60"),
            None,
            Duration::from_millis(300),
            ROOMY_HARD,
            None,
        )
        .expect_err("the session printed once and then slept past its idle budget");
        let detail = refusal(&error);
        let waited = started.elapsed();

        assert!(
            detail.contains("read 6 bytes of stdout"),
            "what the session printed before it was stopped is reported even though a \
             stranger is still holding the pipe open. A piece of output only becomes a \
             chunk at its newline, so this one is six bytes with it: {detail}"
        );
        assert!(
            waited < Duration::from_secs(5),
            "the run stopped listening the moment its own group was empty, rather than \
             waiting on a pipe held by a process it does not own: waited {waited:?} for a \
             session stopped 300ms in"
        );
    }

    #[test]
    fn a_session_that_exited_and_left_a_helper_deaf_to_sigterm_is_still_swept() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid files");
        let own_file = scratch.path().join("own");
        let helper_file = scratch.path().join("helper");
        let helper_script = scratch.path().join("helper.sh");
        std::fs::write(
            &helper_script,
            format!(
                "trap '' TERM\necho $$ > '{}'\nexec sleep 90\n",
                helper_file.display()
            ),
        )
        .expect("the helper script is writable");
        // The hard case for the last sweep, and the easy one to get wrong: the helper
        // keeps the group alive after the session's own process is gone, it deafens
        // itself to SIGTERM, and it holds none of the pipes — so both readers reach
        // their end at once, nothing is left to wait for but the leader, and the
        // leader reports its status long before a terminate grace would be spent.
        let script = format!(
            "echo $$ > '{own}'; sh '{helper}' </dev/null >/dev/null 2>&1 & exec sleep 90 >/dev/null 2>&1",
            own = own_file.display(),
            helper = helper_script.display(),
        );

        let error = run_streaming(
            &mut session(&script),
            None,
            ROOMY_IDLE,
            Duration::from_millis(400),
            None,
        )
        .expect_err("the session's own process outlived its ceiling");
        let detail = refusal(&error);
        assert!(
            detail.contains("hard timeout") && detail.contains("killed with SIGKILL"),
            "the ceiling stopped it, and the sweep was the thing that stopped the group: \
             {detail}"
        );

        let group = pid_written_to(&own_file);
        let helper = pid_written_to(&helper_file);
        assert!(
            wait_until_group_is_empty(group, Duration::from_secs(5)).is_empty(),
            "the helper outlived the answer about the session that started it, and {group} \
             is still the group it holds open"
        );
        assert!(
            !is_running(helper),
            "pid {helper} ignored SIGTERM, its parent was already gone, and nothing swept \
             it: whatever file or port it holds is held against the next session"
        );
    }
}
