//! The gates a task is proved against, and what running one leaves behind.
//!
//! A gate is a runner-executed command with a timeout, a directory to run in and
//! an environment to run with (VISION.md §8) — never a loose string another
//! component has to interpret. `docs/DESIGN.md` names the kinds under *Core
//! types* and VISION.md §8 names the same set as the commands a verification
//! profile holds. [`GateKind`] is the eight that leaves, and
//! `docs/adr/0034-the-gate-kind-set-is-what-the-documents-name.md` records why
//! that is the set rather than the count the task prompt gave.
//!
//! A [`Profile`] is the gates one project runs, and it is loaded rather than
//! assembled by whoever happens to need a gate: a profile missing the mandatory
//! `Verify` gate is refused at load time, so "verification was configured out"
//! is not a state the runner can ever reach. There are two doors and no third:
//! [`Profile::from_toml`] reads the document an operator wrote, and
//! [`profile_from`] builds the gates out of the commands in
//! [`crate::Config`]. [`Profile::get`] is the only way to reach a gate, and a
//! kind answers with at most one — the identity of a gate is its kind.
//!
//! Running one is [`run_gate`]: the gate's own words spawned as a process, both
//! of its output pipes read as the bytes arrive, and its
//! [`Gate::timeout_secs`] enforced as a budget rather than advice. What that run
//! leaves behind is a [`GateResult`], and nothing about it is inferred from
//! another component's report — the status, the signal, the elapsed time and the
//! retained output are all measured here.
//!
//! Reading and writing go through TOML, the format every configuration document
//! in this project uses. A profile is read from text rather than from a path
//! because the document that carries it is a project's configuration document,
//! whose location [`crate::config`] owns.
//!
//! A [`GateResult`] is the record of one gate run: the verdict, the exit status,
//! and the retained output. It is written as the one object `docs/DESIGN.md`
//! gives the `GateFinished` entry under its `result` key, which keeps a gate's
//! own `kind` out of the object the payload tag already owns (ADR-0011 measured
//! that collision), and a run that ran out of its budget carries its own flag
//! rather than a borrowed exit code (ADR-0036). The catalog entry itself is not
//! this module's to add: `docs/DESIGN.md` admits an entry only alongside the
//! `state::apply` arms that answer it, and those belong to the task that emits
//! the event.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::{Bus, Config, Error, Result, Stream};

/// Which mechanical check a gate performs.
///
/// The kind, not the command, is what identifies a gate: an event names the
/// gate that started by its kind, a failure names the gate that refused by its
/// kind, and a human acknowledges a gate by kind. That is why the set is an
/// enum rather than a string a configuration file is free to get wrong.
///
/// The set is what the documents name. [`GateKind::Baseline`] through
/// [`GateKind::Privacy`] are the seven spelled out under *Core types* in
/// `docs/DESIGN.md`; [`GateKind::Flake`] is the eighth, which VISION.md §8
/// lists among the commands a verification profile holds and which
/// `docs/DESIGN.md` already carries a setting for (`flake_runs`). The kind a
/// later task will call `targeted_test_command` is [`GateKind::Targeted`], the
/// name `docs/DESIGN.md` spells for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateKind {
    /// Prove the project was green before the task started, so a failure later
    /// can be attributed to the work rather than inherited from it.
    Baseline,
    /// The fast check of an edit loop: the tests the change touches, run while
    /// the agent is still working rather than after it stopped.
    Targeted,
    /// The complete local suite. Mandatory: no profile is loadable without it,
    /// because a task is never done on an agent's say-so.
    Verify,
    /// The lints, run by the runner rather than trusted from a report.
    Lint,
    /// The formatting check.
    Format,
    /// The build, including every target.
    Build,
    /// The privacy scan: staged files, tracked files, and the outgoing commit
    /// range checked for forbidden paths and content patterns.
    Privacy,
    /// The affected tests run repeatedly or in a randomized order, to surface a
    /// test that passes once and fails on the fifth run.
    Flake,
}

impl GateKind {
    /// Every gate kind, in the order `docs/DESIGN.md` lists them.
    ///
    /// The ledger the tests count against: a kind added or removed without
    /// being named in a document moves this array and fails them.
    pub const ALL: [GateKind; 8] = [
        Self::Baseline,
        Self::Targeted,
        Self::Verify,
        Self::Lint,
        Self::Format,
        Self::Build,
        Self::Privacy,
        Self::Flake,
    ];

    /// The kind as an operator writes it on a command line and reads in a
    /// failure: lower-case, one word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Targeted => "targeted",
            Self::Verify => "verify",
            Self::Lint => "lint",
            Self::Format => "format",
            Self::Build => "build",
            Self::Privacy => "privacy",
            Self::Flake => "flake",
        }
    }
}

impl fmt::Display for GateKind {
    /// The lower-case word, which is what [`crate::Error::Gate`] holds as its
    /// `kind` text (ADR-0001) and what `rerun-gate --gate` names a gate by.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One mechanical check: the command to run and the conditions it must run
/// under. Each gate carries its own timeout, directory and environment, because
/// a privacy scan over the repository and a cold workspace build are not the
/// same job and must not be given the same budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// Which check this is. A profile holds at most one gate per kind.
    pub kind: GateKind,
    /// The program and its arguments, as separate words. A command is stored
    /// split because splitting a string is a decision about quoting, and the
    /// configuration is where that decision should have been made already.
    pub command: Vec<String>,
    /// How long this command may run before it is killed and the gate reported
    /// as timed out rather than as failed.
    pub timeout_secs: u64,
    /// The directory to run in, or `None` for the project root the run was
    /// given. A gate that checks one crate says so here rather than by
    /// beginning its command with `cd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Variables set for this command alone, on top of the environment the
    /// supervisor passes down. Ordered, so a written profile is byte-identical
    /// to the one it was read from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// What one run of one gate produced.
///
/// This is durable data rather than an intermediate: `docs/DESIGN.md` gives the
/// `GateFinished` catalog entry the payload `result: GateResult`, so somebody
/// reading a journal written long after this binary is gone has to be able to
/// tell, from these fields alone, whether the gate refused, crashed, or never
/// finished. Three decisions follow from that, recorded in ADR-0036.
///
/// - A run that ran out of its budget says so through `timed_out` rather than
///   through a borrowed exit code. The runner killed the process group, so the
///   status it collected reads "killed by a signal" — which is also what an
///   out-of-memory kill reads, and something else again from the `exit_code` of
///   a command that ran to completion and refused. VISION.md §8 gives each gate
///   its own timeout, and [`Gate::timeout_secs`] promises the gate is then
///   reported as timed out rather than as failed; this field is where that
///   promise can be kept.
/// - An absent half of the exit status is written as `null`, never left out, so
///   every row this tool writes carries both keys; on the read side a key that
///   is not there at all means the same nothing, which is how the event
///   envelope treats its own absent task. [`Gate`] skips its absent halves
///   instead, because those live in a document an operator edits by hand.
/// - A payload holding a field this type does not have is refused rather than
///   decoded without it, the way [`Config`], [`Gate`] and [`Profile`] refuse an
///   unknown key.
///
/// `passed` is stored rather than derived, because it is the runner's verdict
/// and not always the exit status: a [`GateKind::Flake`] gate decides over five
/// runs and a [`GateKind::Privacy`] gate reads its own report. Deriving it
/// belongs to the code that runs a gate, which is the only place that holds the
/// facts it is derived from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateResult {
    /// Which gate ran. The gate names itself from the inside, so a journal
    /// holding several results says which is which without a key per entry.
    pub kind: GateKind,
    /// Whether the gate is satisfied — the runner's verdict, recorded rather
    /// than recomputed here.
    pub passed: bool,
    /// The status the command exited with, or `None` when it never produced
    /// one: a process killed by a signal has no exit code, and a killed process
    /// is the only way a gate stops without one.
    pub exit_code: Option<i32>,
    /// The signal that ended the process, when a signal ended it. Held as a
    /// number rather than a name because the number is what `wait` reported;
    /// naming it is a rendering decision, and the journal is not where a
    /// rendering is made.
    pub signal: Option<i32>,
    /// How long the command ran, in milliseconds, measured by the runner rather
    /// than reported by the command.
    pub duration_ms: u64,
    /// Everything the command wrote to stdout, retained whole rather than
    /// summarised: VISION.md §8 keeps raw output alongside the parsed form,
    /// and a gate failure that cannot show its own output cannot be acted on.
    pub stdout: String,
    /// Everything the command wrote to stderr, retained whole for the same
    /// reason as stdout, and kept separate because the two streams mean
    /// different things to every tool that writes them.
    pub stderr: String,
    /// Whether the gate ran out of its budget and was killed. Its own fact
    /// rather than an inference from the two optional fields above, so that a
    /// timeout, an outside kill and a non-zero exit stay three different answers
    /// in the record.
    pub timed_out: bool,
}

/// The gates one project runs, in the order they run.
///
/// A profile is loaded, not assembled at the point of use: the rules below are
/// what make "the gates were configured out" impossible rather than unlikely.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The configured gates, in the order a run executes them. Empty is
    /// readable but never loadable: [`Profile::validate`] refuses it for the
    /// missing mandatory gate, which is the answer an operator can act on.
    #[serde(default)]
    pub gates: Vec<Gate>,
}

impl Profile {
    /// The gate configured for `kind`, or `None` when the project configured
    /// none. Never a default standing in for a gate nobody set: a caller that
    /// needs one has to say what happens without it.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|gate| gate.kind == kind)
    }

    /// Reads a profile from a TOML document and applies [`Profile::validate`]
    /// to it, so a document that sets a partial set of gates is refused before
    /// any task is started rather than discovered at verification time.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a document that is not a profile — malformed TOML,
    /// a key a gate or a profile does not have, a gate kind nobody defined —
    /// and for a profile that breaks one of the rules [`Profile::validate`]
    /// holds.
    pub fn from_toml(document: &str) -> Result<Self> {
        let profile: Self = toml::from_str(document).map_err(|error| Error::Config {
            key: "profile".to_owned(),
            detail: format!("the verification profile is not readable: {error}"),
        })?;
        profile.validate()?;
        Ok(profile)
    }

    /// Writes the profile as a TOML document, the form a project's
    /// configuration document holds it in.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a field holds something TOML cannot write, which
    /// for a profile means a path or a variable name that is not text.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|error| Error::Config {
            key: "profile".to_owned(),
            detail: format!("the verification profile cannot be written as TOML: {error}"),
        })
    }

    /// Refuses a profile that could not decide what `done` means.
    ///
    /// Two rules, both structural rather than a matter of taste. The mandatory
    /// [`GateKind::Verify`] gate has to be there: VISION.md §8 makes the
    /// complete local suite non-optional, so a profile is not free to omit it,
    /// and the strictness belongs here rather than in every caller that would
    /// otherwise have to remember the rule. And one kind answers to one gate:
    /// [`Profile::get`] promises a single gate per kind, and a profile holding
    /// two for one kind would have that promise decided by table order.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the first rule broken: a kind configured more
    /// than once, or the missing mandatory gate.
    pub fn validate(&self) -> Result<()> {
        for kind in GateKind::ALL {
            let configured = self.gates.iter().filter(|gate| gate.kind == kind).count();
            if configured > 1 {
                return Err(Error::Config {
                    key: "gates".to_owned(),
                    detail: format!(
                        "the `{kind}` gate is configured {configured} times; a kind answers to \
                         one gate"
                    ),
                });
            }
        }
        if self.get(GateKind::Verify).is_none() {
            return Err(Error::Config {
                key: "gates".to_owned(),
                detail: format!(
                    "no `{}` gate is configured; the complete local suite is mandatory and \
                     cannot be configured out",
                    GateKind::Verify
                ),
            });
        }
        Ok(())
    }
}

/// The gates a configuration configures, in the order a run executes them.
///
/// This is where a [`Profile`] comes from, which is what makes the gates a task
/// is proved against somebody's settings rather than this function's opinion: a
/// gate with no command in the configuration is not in the profile, and a gate
/// in the profile carries the command words and the timeout the configuration
/// gave it. The keys and their order are VISION.md §8's, and one kind answers to
/// exactly one key — so [`Profile::validate`]'s two rules cannot arise from a
/// configuration, one because a kind appears once here by construction and the
/// other because the missing mandatory gate is refused below, where the refusal
/// can name the key an operator has to write.
///
/// A gate built from configuration sets no working directory and no environment.
/// Configuration decides *what* runs; the run decides where it runs and what it
/// sees, which is the difference between a project's settings and one attempt's
/// circumstances.
///
/// # Errors
///
/// [`Error::Config`] naming the configuration key at fault: no `verify_command`,
/// which VISION.md §8 makes mandatory, or a gate configured with no command
/// words, which is a gate the runner has nothing to execute.
pub fn profile_from(config: &Config) -> Result<Profile> {
    let configured: [(Option<&[String]>, GateKind, &str); 8] = [
        (
            config.baseline_command.as_deref(),
            GateKind::Baseline,
            "baseline_command",
        ),
        (
            config.targeted_test_command.as_deref(),
            GateKind::Targeted,
            "targeted_test_command",
        ),
        (
            config.verify_command.as_deref(),
            GateKind::Verify,
            "verify_command",
        ),
        (
            config.lint_command.as_deref(),
            GateKind::Lint,
            "lint_command",
        ),
        (
            config.format_command.as_deref(),
            GateKind::Format,
            "format_command",
        ),
        (
            config.build_command.as_deref(),
            GateKind::Build,
            "build_command",
        ),
        (
            config.privacy_command.as_deref(),
            GateKind::Privacy,
            "privacy_command",
        ),
        (
            config.flake_command.as_deref(),
            GateKind::Flake,
            "flake_command",
        ),
    ];
    let mut gates = Vec::new();
    for (command, kind, key) in configured {
        let Some(words) = command else {
            if kind == GateKind::Verify {
                return Err(Error::Config {
                    key: key.to_owned(),
                    detail: format!(
                        "no `{key}` is configured; the complete local suite is mandatory \
                         and cannot be configured out"
                    ),
                });
            }
            continue;
        };
        if words.is_empty() {
            return Err(Error::Config {
                key: key.to_owned(),
                detail: format!(
                    "`{key}` configures the `{kind}` gate with no command words, so the \
                     runner has nothing to execute"
                ),
            });
        }
        gates.push(Gate {
            kind,
            command: words.to_vec(),
            timeout_secs: config.gate_timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }
    Ok(Profile { gates })
}

/// How long the collector waits for the next chunk before it looks at the clock.
///
/// The budget is enforced between chunks rather than by a third thread, so this
/// is what a gate's timeout is enforced to: twenty milliseconds against a budget
/// `docs/DESIGN.md` sets at 1800 seconds, four orders of magnitude inside it.
const CHUNK_POLL: Duration = Duration::from_millis(20);

/// How long a killed gate's pipes are still listened to.
///
/// Once the signal lands the gate itself is gone, so the only thing that can
/// still hold a pipe open is a descendant that outlived it. Until the group kill
/// arrives to take that stranger down too, the wait is bounded rather than
/// endless: a supervisor blocked on a process it does not own has stopped
/// supervising, and output that did arrive is worth more than output a stranger
/// may never write.
const POST_KILL_GRACE: Duration = Duration::from_secs(2);

/// Run one gate and return the record of having run it.
///
/// The command is spawned as its own words, never through a shell, in `root` or
/// in the gate's own [`Gate::working_dir`] when it names one, with [`Gate::env`]
/// laid over the environment this process was given. Its stdin is closed: a
/// runner-executed command has nobody to ask, and a gate left reading a terminal
/// it was never handed holds the queue with it. Both pipes are read on their own
/// threads, so a gate that fills one cannot stall because nobody was emptying
/// the other, and each chunk is handed on as it arrives rather than when the
/// command finishes.
///
/// [`Gate::timeout_secs`] is a budget, not a hint. When it runs out the command
/// is killed and the run is reported with [`GateResult::timed_out`] set and
/// whatever output had been read by then — which is why the result keeps an exit
/// code, a signal and a timeout flag as three separate facts (ADR-0036).
///
/// `bus` is where a gate's output belongs as it arrives, for the output pane
/// `docs/CONTRACT.md` calls the primary thing an operator watches. It is
/// accepted and not written to, and the gap is deliberate rather than
/// oversight: the catalog in `docs/DESIGN.md` has no entry that can carry a
/// chunk of command output, [`crate::Recorder`] is the one door into [`Bus`]
/// (ADR-0030), and an event whose sequence its emitter chose rather than the
/// journal stamped is what ADR-0016 forbids. The chunks themselves are produced
/// and delivered all the same — `run_gate_streaming` below is what this
/// function calls, and a caller that publishes has one callback to change.
/// Giving gate output a catalog entry, or the bus a second door, is a decision
/// about durable data that this task does not own: ADR-0037 records the options
/// and what each costs. Until that decision is made, passing a bus changes
/// nothing an observer can see.
///
/// # Errors
///
/// [`Error::Gate`] naming the gate and what could not be done: a gate with no
/// command words to execute, or a program or working directory that could not be
/// started. A command that runs and refuses is not an error — it is a
/// [`GateResult`] with [`GateResult::passed`] false, because a refused gate is
/// still an answer.
pub fn run_gate(gate: &Gate, root: &Path, bus: Option<&Bus>) -> Result<GateResult> {
    // Accepted, not written to: the catalog entry that would carry a chunk does not exist yet.
    _ = bus;
    run_gate_streaming(gate, root, &mut |_stream, _chunk| {})
}

/// Run one gate, handing every output chunk to `on_chunk` as it arrives.
///
/// [`run_gate`] is written in terms of this. The callback runs on the calling
/// thread: each reader thread copies chunks into one channel and the collector
/// here is the only thing that drains it, so chunks from both pipes arrive in
/// the order they were observed and whatever is listening needs no lock of its
/// own.
fn run_gate_streaming(
    gate: &Gate,
    root: &Path,
    on_chunk: &mut dyn FnMut(Stream, &str),
) -> Result<GateResult> {
    let (program, arguments) = gate.command.split_first().ok_or_else(|| Error::Gate {
        kind: gate.kind.to_string(),
        detail: "the gate holds no command words, so there is nothing to execute".to_owned(),
    })?;
    let directory = working_directory(gate, root);
    let mut child = Command::new(program)
        .args(arguments)
        .current_dir(&directory)
        .envs(&gate.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|failure| Error::Gate {
            kind: gate.kind.to_string(),
            detail: format!(
                "could not start `{program}` in `{}`: {failure}",
                directory.display()
            ),
        })?;
    let started = Instant::now();

    let (sender, chunks) = mpsc::channel();
    let stdout = own_pipe(child.stdout.take(), gate, "stdout")?;
    let stderr = own_pipe(child.stderr.take(), gate, "stderr")?;
    read_one_pipe(stdout, Stream::Stdout, sender.clone())?;
    read_one_pipe(stderr, Stream::Stderr, sender)?;

    let mut watch = Watch::new(child, started);
    let mut kept = Output::default();
    loop {
        match chunks.recv_timeout(CHUNK_POLL) {
            Ok(chunk) => {
                on_chunk(chunk.stream, &chunk.text);
                kept.push(&chunk);
            }
            // Both readers reached the end of their pipe, so every byte the gate
            // wrote has been handed over and kept.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        watch.poll()?;
        watch.enforce_budget(gate.timeout_secs)?;
        if watch.grace_elapsed() {
            break;
        }
    }

    // Nothing left to read, and the process is still the run's to wait for. The
    // budget still governs that wait: a gate that closed its own pipes and kept
    // working is no less subject to its timeout than one that never wrote.
    while watch.status.is_none() && !watch.grace_elapsed() {
        watch.poll()?;
        watch.enforce_budget(gate.timeout_secs)?;
        if watch.status.is_none() && !watch.grace_elapsed() {
            thread::sleep(CHUNK_POLL);
        }
    }
    // A reader still copying bytes nobody asked it to stop holding a pipe for
    // has nowhere left to copy them to, so it ends with the pipe rather than
    // with this run.
    drop(chunks);

    let status = watch.status;
    let (exit_code, signal) = match status {
        Some(status) => (status.code(), terminating_signal(status)),
        None => (None, None),
    };
    Ok(GateResult {
        kind: gate.kind,
        passed: status.as_ref().is_some_and(ExitStatus::success) && !watch.timed_out,
        exit_code,
        signal,
        duration_ms: elapsed_millis(started.elapsed()),
        stdout: kept.stdout,
        stderr: kept.stderr,
        timed_out: watch.timed_out,
    })
}

/// The directory a gate's command runs in: the one it names itself, absolute or
/// below `root`, and `root` when it names none.
fn working_directory(gate: &Gate, root: &Path) -> PathBuf {
    match &gate.working_dir {
        Some(directory) if directory.is_absolute() => directory.clone(),
        Some(directory) => root.join(directory),
        None => root.to_path_buf(),
    }
}

/// Take a pipe this function asked the standard library to create.
///
/// Asking for both above makes absence impossible. The refusal is here so the one
/// place it could ever fire answers with an error rather than a panic.
fn own_pipe<T>(piped: Option<T>, gate: &Gate, which: &str) -> Result<T> {
    piped.ok_or_else(|| Error::Gate {
        kind: gate.kind.to_string(),
        detail: format!("the `{}` gate's {which} was never piped", gate.kind),
    })
}

/// Copy one pipe into the collector a line at a time, until it closes.
///
/// A line is the chunk because it is the unit every tool that writes a
/// diagnostic means, and because a chunk that stopped in the middle of a
/// multi-byte character would put a broken character on a screen. Splitting an
/// over-long line and stripping the control characters a pane must never
/// interpret belong to the renderer, per `docs/CONTRACT.md`.
fn read_one_pipe(
    pipe: impl Read + Send + 'static,
    stream: Stream,
    chunks: Sender<Chunk>,
) -> Result<()> {
    let name = match stream {
        Stream::Stdout => "ktask-gate-stdout",
        Stream::Stderr => "ktask-gate-stderr",
    };
    let reader = thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            let mut buffered = BufReader::new(pipe);
            let mut line: Vec<u8> = Vec::new();
            loop {
                line.clear();
                // Zero bytes read is the far end closing. A failed read is a
                // pipe that broke, and either way everything already sent has
                // been sent: what a gate wrote is never thrown away here.
                let Ok(read) = buffered.read_until(b'\n', &mut line) else {
                    break;
                };
                if read == 0 {
                    break;
                }
                // Bytes that are not UTF-8 become the replacement character
                // rather than a panic or a dropped line (VISION.md §13).
                let text = String::from_utf8_lossy(&line).into_owned();
                // A closed collector means the run is over and stopped listening.
                if chunks.send(Chunk { stream, text }).is_err() {
                    break;
                }
            }
        })?;
    drop(reader);
    Ok(())
}

/// The child process, and the two things worth watching about it while it runs.
struct Watch {
    /// The gate's own process.
    child: Child,
    /// When it was spawned, which is what both the budget and the duration
    /// measure from.
    started: Instant,
    /// What it exited with, once it has said so.
    status: Option<ExitStatus>,
    /// Whether its budget ran out.
    timed_out: bool,
    /// When to stop listening to a killed gate's pipes, or `None` while the gate
    /// still holds them itself.
    grace: Option<Instant>,
}

impl Watch {
    /// Watch `child`, timing everything from `started`.
    fn new(child: Child, started: Instant) -> Self {
        Self {
            child,
            started,
            status: None,
            timed_out: false,
            grace: None,
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

    /// Spend the budget: record that it is spent, and signal the command if it
    /// is still running.
    ///
    /// The flag is set whether or not the command was still there to be
    /// signalled. A gate that returned inside the instant between the deadline
    /// and the signal still ran past its budget, and `timed_out` is the fact a
    /// response is chosen from.
    fn enforce_budget(&mut self, budget_secs: u64) -> Result<()> {
        if self.timed_out || self.started.elapsed() < Duration::from_secs(budget_secs) {
            return Ok(());
        }
        self.timed_out = true;
        if self.status.is_none() {
            self.child.kill()?;
            self.grace = Some(Instant::now() + POST_KILL_GRACE);
        }
        Ok(())
    }

    /// Whether a killed gate has been listened to for as long as it is allowed.
    fn grace_elapsed(&self) -> bool {
        self.grace.is_some_and(|until| Instant::now() >= until)
    }
}

/// One piece of a gate's output, as it arrived.
struct Chunk {
    /// Which of the two pipes it came from. The distinction is unrecoverable
    /// once both are appended to one string, which is why it travels with the
    /// text.
    stream: Stream,
    /// The line it arrived as, with anything that was not UTF-8 replaced.
    text: String,
}

/// What a gate wrote, retained whole.
#[derive(Default)]
struct Output {
    /// Everything that arrived on stdout.
    stdout: String,
    /// Everything that arrived on stderr.
    stderr: String,
}

impl Output {
    /// Add one chunk to the stream it arrived on.
    fn push(&mut self, chunk: &Chunk) {
        match chunk.stream {
            Stream::Stdout => self.stdout.push_str(&chunk.text),
            Stream::Stderr => self.stderr.push_str(&chunk.text),
        }
    }
}

/// The signal that ended a process, when a signal ended it.
///
/// `None` for a process that ran to its own end, which is a different answer
/// from the exit code above and is held separately for that reason.
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

/// How long a run took, as the millisecond count the result holds.
fn elapsed_millis(elapsed: Duration) -> u64 {
    u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::{Gate, GateKind, GateResult, Profile, profile_from, run_gate, run_gate_streaming};
    use crate::{Bus, Config, Error, Stream};
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    /// One gate, spelled the way a TOML document spells it.
    fn gate_document(kind: GateKind, command: &str) -> String {
        format!("[[gates]]\nkind = \"{kind:?}\"\ncommand = [{command:?}]\ntimeout_secs = 1800\n")
    }

    /// A document holding `gates` and nothing else.
    fn profile_document(gates: impl IntoIterator<Item = (GateKind, &'static str)>) -> String {
        gates
            .into_iter()
            .map(|(kind, command)| gate_document(kind, command))
            .collect()
    }

    /// The mandatory gate, as a value rather than as a document.
    fn verify() -> Gate {
        Gate {
            kind: GateKind::Verify,
            command: vec!["./scripts/quality.sh".to_owned()],
            timeout_secs: 1800,
            working_dir: None,
            env: BTreeMap::new(),
        }
    }

    /// A profile assembled rather than loaded.
    fn assembled(gates: Vec<Gate>) -> Profile {
        Profile { gates }
    }

    /// Loads a profile, failing the test with the refusal if it will not load.
    fn loaded(document: &str) -> Profile {
        Profile::from_toml(document).unwrap_or_else(|error| {
            panic!("a profile carrying the mandatory gate must load, refused with: {error}")
        })
    }

    #[test]
    fn the_gate_kinds_are_the_ones_the_documents_name() {
        let documented = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
            GateKind::Flake,
        ];
        assert_eq!(GateKind::ALL, documented);
    }

    #[test]
    fn each_gate_kind_prints_the_word_an_operator_writes() {
        let spelled: Vec<String> = GateKind::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            spelled,
            [
                "baseline", "targeted", "verify", "lint", "format", "build", "privacy", "flake",
            ]
        );
    }

    #[test]
    fn a_gate_kind_survives_the_encoding_the_journal_uses() {
        for kind in GateKind::ALL {
            let text = serde_json::to_string(&kind).expect("a gate kind is writable as JSON");
            let read_back: GateKind = serde_json::from_str(&text)
                .expect("a written gate kind reads back as the kind it was written from");
            assert_eq!(read_back, kind);
        }
        assert_eq!(
            serde_json::to_string(&GateKind::Targeted).unwrap(),
            "\"Targeted\"",
            "a kind is written by its variant name, the way every other stored enum here is"
        );
    }

    #[test]
    fn a_kind_the_type_does_not_have_is_refused_rather_than_held_as_text() {
        let document = "[[gates]]\nkind = \"made_up\"\ncommand = [\"true\"]\ntimeout_secs = 1\n";
        let error =
            Profile::from_toml(document).expect_err("a gate kind nobody defined is not a gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("made_up")),
            "an unknown gate kind must be refused, naming the kind it refused: {error}"
        );
    }

    #[test]
    fn a_profile_hands_back_the_gate_configured_for_a_kind_and_only_that_one() {
        let document = profile_document([
            (GateKind::Baseline, "./scripts/check-prereqs.sh"),
            (GateKind::Verify, "./scripts/quality.sh"),
        ]);
        let profile = loaded(&document);

        assert_eq!(
            profile.get(GateKind::Baseline),
            Some(&Gate {
                kind: GateKind::Baseline,
                command: vec!["./scripts/check-prereqs.sh".to_owned()],
                timeout_secs: 1800,
                working_dir: None,
                env: BTreeMap::new(),
            })
        );
        assert_eq!(profile.get(GateKind::Verify), Some(&verify()));
        assert_eq!(
            profile.get(GateKind::Flake),
            None,
            "a gate nobody configured must not be invented from a default"
        );
    }

    #[test]
    fn a_profile_without_the_mandatory_verify_gate_is_refused_whatever_it_holds() {
        for kind in GateKind::ALL {
            if kind == GateKind::Verify {
                continue;
            }
            let error = Profile::from_toml(&profile_document([(kind, "true")]))
                .expect_err("a profile with no verify gate is not loadable");
            assert!(
                matches!(error, Error::Config { ref key, ref detail }
                    if key == "gates" && detail.contains("verify")),
                "a missing verify gate must be named as the missing gate, was: {error}"
            );
        }
    }

    #[test]
    fn a_profile_that_sets_no_gates_at_all_is_refused_by_the_same_rule() {
        let error = Profile::from_toml("").expect_err("an empty profile holds no verify gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("verify")),
            "an empty profile must be refused for the reason an operator can act \
             on, was: {error}"
        );
    }

    #[test]
    fn the_mandatory_rule_holds_for_an_assembled_profile_too() {
        assert!(
            Profile::validate(&assembled(vec![verify()])).is_ok(),
            "a profile holding the mandatory gate is valid"
        );

        let lint = Gate {
            kind: GateKind::Lint,
            ..verify()
        };
        let error = Profile::validate(&assembled(vec![lint]))
            .expect_err("an assembled profile is no more excused than a loaded one");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("verify")),
            "the refusal must name the gate that is missing, was: {error}"
        );
    }

    #[test]
    fn a_kind_configured_twice_is_refused_because_one_gate_answers_to_it() {
        let document = profile_document([
            (GateKind::Verify, "true"),
            (GateKind::Lint, "cargo clippy"),
            (GateKind::Lint, "cargo clippy --fix"),
        ]);
        let error =
            Profile::from_toml(&document).expect_err("two gates cannot both be the lint gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("lint")),
            "the refusal must name the kind that was configured twice, was: {error}"
        );
    }

    #[test]
    fn every_field_of_a_profile_survives_a_trip_through_toml() {
        let mut env = BTreeMap::new();
        env.insert("RUST_BACKTRACE".to_owned(), "1".to_owned());
        env.insert("CARGO_PROFILE_OVERRIDE".to_owned(), "off".to_owned());
        let written = assembled(vec![
            Gate {
                kind: GateKind::Privacy,
                command: vec!["git".to_owned(), "diff".to_owned(), "--check".to_owned()],
                timeout_secs: 30,
                working_dir: Some(PathBuf::from("/repo")),
                env: env.clone(),
            },
            verify(),
        ]);

        let document = written.to_toml().expect("a profile is writable as TOML");
        let read_back = loaded(&document);
        assert_eq!(read_back, written);
        assert_eq!(
            read_back.get(GateKind::Privacy).map(|gate| &gate.env),
            Some(&env),
            "an environment that arrives reordered or dropped is not the environment that was set"
        );
    }

    #[test]
    fn a_gate_written_in_toml_holds_its_own_timeout_directory_and_environment() {
        let document = r#"
[[gates]]
kind = "Flake"
command = ["cargo", "nextest", "run", "--repeated-count", "5"]
timeout_secs = 900
working_dir = "crates/ktask-core"
env = { KTASK_SEED = "7" }

[[gates]]
kind = "Verify"
command = ["./scripts/quality.sh"]
timeout_secs = 1800
"#;
        let profile = loaded(document);
        let flake = profile.get(GateKind::Flake).expect("the flake gate is set");
        assert_eq!(flake.command[3], "--repeated-count");
        assert_eq!(flake.command[4], "5");
        assert_eq!(flake.timeout_secs, 900);
        assert_eq!(
            flake.working_dir.as_deref(),
            Some(Path::new("crates/ktask-core"))
        );
        assert_eq!(flake.env.get("KTASK_SEED").map(String::as_str), Some("7"));
    }

    #[test]
    fn a_gate_may_leave_out_the_optional_keys_and_reads_them_back_as_none() {
        let profile = loaded(&profile_document([
            (GateKind::Format, "cargo"),
            (GateKind::Verify, "./scripts/quality.sh"),
        ]));
        let format = profile
            .get(GateKind::Format)
            .expect("the format gate is set");
        assert_eq!(format.working_dir, None);
        assert!(format.env.is_empty());
    }

    #[test]
    fn a_field_that_is_not_a_gate_field_is_refused() {
        let document = gate_document(GateKind::Verify, "true") + "retries = 3\n";
        let error =
            Profile::from_toml(&document).expect_err("a gate does not grow a field nobody defined");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("retries")),
            "an unknown gate field must be refused, naming the field it refused: {error}"
        );
    }

    #[test]
    fn a_key_that_is_not_a_profile_field_is_refused() {
        // Written first rather than appended: a bare key after a table header
        // belongs to that table, and this one has to be the profile's own.
        let document =
            "flake_runs = 9\n".to_owned() + &profile_document([(GateKind::Verify, "true")]);
        let error = Profile::from_toml(&document)
            .expect_err("a profile does not grow a key nobody defined either");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("flake_runs")),
            "an unknown profile key must be refused, naming the key it refused: {error}"
        );
    }

    #[test]
    fn a_profile_is_written_with_its_gates_in_the_order_it_holds_them() {
        let written = assembled(vec![
            Gate {
                kind: GateKind::Lint,
                ..verify()
            },
            verify(),
        ]);
        let document = written.to_toml().expect("a profile is writable as TOML");
        let value: toml::Value = toml::from_str(&document).expect("a profile writes valid TOML");
        let gates = value
            .get("gates")
            .and_then(toml::Value::as_array)
            .expect("a profile writes its gates as a table array");
        assert_eq!(gates.len(), 2);
        assert_eq!(
            gates[0].get("kind").and_then(toml::Value::as_str),
            Some("Lint"),
            "the order a profile holds its gates in is the order it runs them in"
        );
        assert_eq!(
            gates[1].get("kind").and_then(toml::Value::as_str),
            Some("Verify")
        );
    }

    #[test]
    fn a_gate_writes_only_the_keys_it_actually_sets() {
        let document = assembled(vec![verify()])
            .to_toml()
            .expect("a profile is writable as TOML");
        let value: toml::Value = toml::from_str(&document).expect("a profile writes valid TOML");
        let gate = &value
            .get("gates")
            .and_then(toml::Value::as_array)
            .expect("a profile writes its gates as a table array")[0];
        assert_eq!(
            gate.as_table()
                .expect("a gate writes as a table")
                .keys()
                .collect::<Vec<_>>(),
            ["command", "kind", "timeout_secs"],
            "a gate that writes an empty environment and a null directory reads back as              having set them: {document}"
        );
    }

    #[test]
    fn a_gate_writes_no_placeholder_for_a_field_it_does_not_set() {
        // TOML skips an absent `Option` on its own, so only an encoding that
        // writes the key can tell this attribute from having none at all.
        let json = serde_json::to_string(&verify()).expect("a gate is writable as JSON");
        assert!(
            !json.contains("working_dir") && !json.contains("env"),
            "a field nobody set must not arrive as a null or an empty map, which reads              back as a decision that was made: {json}"
        );
    }

    #[test]
    fn a_command_the_gate_was_given_survives_as_the_words_it_was_given() {
        let document = gate_document(GateKind::Build, "cargo build --workspace --locked")
            + &profile_document([(GateKind::Verify, "true")]);
        let profile = loaded(&document);
        let build = profile.get(GateKind::Build).expect("the build gate is set");
        assert_eq!(
            build.command,
            vec!["cargo build --workspace --locked".to_owned()],
            "a command is the words it was written with, not a string split by whoever runs it"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_profile_holding_a_path_that_is_not_text_is_refused_rather_than_written() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let mut gate = verify();
        gate.working_dir = Some(PathBuf::from(OsString::from_vec(vec![0xff])));
        let error = assembled(vec![gate])
            .to_toml()
            .expect_err("a path that is not text has no TOML spelling");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("TOML")),
            "an unwritable profile must come back as a configuration refusal, not a panic: {error}"
        );
    }

    /// Writes one gate's command into a configuration, by kind — the same
    /// mapping `profile_from` reads back, spelled out here so a test that
    /// configures a gate says which key it believes it wrote.
    fn set_command(config: &mut Config, kind: GateKind, words: &[&str]) {
        let command: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        match kind {
            GateKind::Baseline => config.baseline_command = Some(command),
            GateKind::Targeted => config.targeted_test_command = Some(command),
            GateKind::Verify => config.verify_command = Some(command),
            GateKind::Lint => config.lint_command = Some(command),
            GateKind::Format => config.format_command = Some(command),
            GateKind::Build => config.build_command = Some(command),
            GateKind::Privacy => config.privacy_command = Some(command),
            GateKind::Flake => config.flake_command = Some(command),
        }
    }

    /// A configuration whose only settings are the one-word gate commands listed.
    fn configured(gates: &[(GateKind, &str)]) -> Config {
        let mut config = Config::default();
        for (kind, command) in gates {
            set_command(&mut config, *kind, &[*command]);
        }
        config
    }

    #[test]
    fn the_profile_holds_exactly_the_gates_the_configuration_configures() {
        let mut config = configured(&[
            (GateKind::Verify, "./scripts/quality.sh"),
            (GateKind::Lint, "cargo"),
        ]);
        set_command(
            &mut config,
            GateKind::Build,
            &["cargo", "build", "--locked"],
        );

        let profile = profile_from(&config)
            .expect("a configuration that names the mandatory gate builds a profile");

        let kinds: Vec<GateKind> = profile.gates.iter().map(|gate| gate.kind).collect();
        assert_eq!(kinds, [GateKind::Verify, GateKind::Lint, GateKind::Build]);
        for kind in [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Format,
            GateKind::Privacy,
            GateKind::Flake,
        ] {
            assert!(
                profile.get(kind).is_none(),
                "`{kind}` was not configured, so the profile must not invent it"
            );
        }
        let build = profile
            .get(GateKind::Build)
            .expect("the build gate was configured");
        assert_eq!(
            build.command,
            ["cargo", "build", "--locked"],
            "a gate runs the words the configuration wrote, not a command rebuilt from them"
        );
        for gate in &profile.gates {
            assert!(
                gate.working_dir.is_none() && gate.env.is_empty(),
                "a configuration sets a gate's command, not the directory or the \
                 environment it runs in: {:?}",
                gate.kind
            );
        }
    }

    #[test]
    fn a_configured_gate_orders_the_profile_the_way_the_vision_lists_them() {
        let config = configured(&[
            (GateKind::Flake, "flake"),
            (GateKind::Privacy, "privacy"),
            (GateKind::Build, "build"),
            (GateKind::Format, "format"),
            (GateKind::Lint, "lint"),
            (GateKind::Verify, "verify"),
            (GateKind::Targeted, "targeted"),
            (GateKind::Baseline, "baseline"),
        ]);

        let profile =
            profile_from(&config).expect("a configuration of every gate builds a profile");

        let kinds: Vec<GateKind> = profile.gates.iter().map(|gate| gate.kind).collect();
        assert_eq!(
            kinds,
            GateKind::ALL,
            "the order the runner executes the gates in is the order VISION.md §8 lists \
             them, whatever order they were configured in"
        );
        for kind in GateKind::ALL {
            let gate = profile.get(kind).expect("every gate was configured");
            assert_eq!(
                gate.command,
                [kind.as_str()],
                "the `{kind}` gate must run the command its own key was configured with, \
                 not another gate's"
            );
        }
    }

    #[test]
    fn a_configuration_without_a_verify_command_is_refused_naming_the_key_it_needs() {
        let config = configured(&[(GateKind::Lint, "cargo"), (GateKind::Build, "cargo")]);
        let error = profile_from(&config)
            .expect_err("verification cannot be configured out, so this is no profile");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "verify_command" && detail.contains("mandatory")
                    && detail.contains("cannot be configured out")),
            "a missing verify command must name the key an operator has to write: {error}"
        );
        assert!(
            matches!(profile_from(&Config::default()), Err(Error::Config { ref key, .. })
                if key == "verify_command"),
            "an unconfigured project has no verification suite, which is a refusal rather \
             than a profile that verifies nothing"
        );
    }

    #[test]
    fn every_gate_of_a_configured_profile_carries_the_configured_gate_timeout() {
        let mut config = configured(&[(GateKind::Verify, "true"), (GateKind::Flake, "true")]);
        config.gate_timeout_secs = 600;

        let profile = profile_from(&config)
            .expect("a configuration naming the mandatory gate builds a profile");

        for gate in &profile.gates {
            assert_eq!(
                gate.timeout_secs, 600,
                "`{}` must be given the budget the configuration holds for gates, not a \
                 budget this function chose",
                gate.kind
            );
        }
    }

    #[test]
    fn a_gate_configured_with_no_command_words_is_refused_naming_that_gate() {
        let mut config = configured(&[(GateKind::Verify, "true")]);
        set_command(&mut config, GateKind::Lint, &[]);

        let error = profile_from(&config)
            .expect_err("a gate with no words in it has nothing for the runner to execute");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "lint_command" && detail.contains("no command")),
            "the refusal must name the gate an operator has to fix: {error}"
        );
    }

    #[test]
    fn each_gate_key_is_the_one_named_when_that_gate_has_no_command_words() {
        let fields: [(GateKind, &str); 8] = [
            (GateKind::Baseline, "baseline_command"),
            (GateKind::Targeted, "targeted_test_command"),
            (GateKind::Verify, "verify_command"),
            (GateKind::Lint, "lint_command"),
            (GateKind::Format, "format_command"),
            (GateKind::Build, "build_command"),
            (GateKind::Privacy, "privacy_command"),
            (GateKind::Flake, "flake_command"),
        ];
        for (kind, field) in fields {
            let mut config = configured(&[(GateKind::Verify, "verify")]);
            set_command(&mut config, kind, &[]);
            let error =
                profile_from(&config).expect_err("a configured gate with no words cannot run");
            assert!(
                matches!(error, Error::Config { ref key, .. } if key == field),
                "refusing the `{kind}` gate must name `{field}`, the key an operator has to \
                 correct; said `{error}`"
            );
        }
    }

    /// A gate that ran, exited 0 and is satisfied — what a runner records when a
    /// command finishes inside its budget.
    fn passed_verify() -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 1_845_003,
            stdout: "test result: ok. 441 passed\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    /// A gate that ran out of its budget: the runner killed the process group, so
    /// the command never reported a status of its own.
    fn timed_out_verify() -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: None,
            signal: Some(9),
            duration_ms: 1_800_000,
            stdout: "test result: ok. 440 passed\n".to_owned(),
            stderr: String::new(),
            timed_out: true,
        }
    }

    /// A gate the command itself refused: a status arrived, and it was not 0.
    fn failed_lint() -> GateResult {
        GateResult {
            kind: GateKind::Lint,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 8_112,
            stdout: String::new(),
            stderr: "error: unexpected cfg condition\n".to_owned(),
            timed_out: false,
        }
    }

    /// Every shape a run can end in, for the tests that hold for all of them.
    fn every_outcome() -> [GateResult; 3] {
        [passed_verify(), timed_out_verify(), failed_lint()]
    }

    #[test]
    fn a_gate_result_writes_the_field_names_the_event_catalog_documents() {
        let written = serde_json::to_value(passed_verify())
            .expect("a gate result is writable as the JSON the journal stores");
        assert_eq!(
            written,
            serde_json::json!({
                "kind": "Verify",
                "passed": true,
                "exit_code": 0,
                "signal": null,
                "duration_ms": 1_845_003,
                "stdout": "test result: ok. 441 passed\n",
                "stderr": "",
                "timed_out": false,
            }),
            "`docs/DESIGN.md` gives `GateFinished` the payload `result: GateResult`, so every \
             field name written here is durable data the journal outlives this binary with"
        );
    }

    #[test]
    fn a_gate_result_survives_the_encoding_the_journal_uses() {
        for result in every_outcome() {
            let text = serde_json::to_string(&result)
                .expect("a gate result is writable as the JSON the journal stores");
            let read_back: GateResult = serde_json::from_str(&text)
                .expect("a written gate result reads back as the result it was written from");
            assert_eq!(
                read_back, result,
                "a result must read back with every field it was written with; wrote {text}"
            );
        }
    }

    #[test]
    fn a_gate_that_ran_out_of_time_is_told_apart_from_a_kill_and_from_a_refusal() {
        let timeout = timed_out_verify();
        let mut killed_by_strangers = timeout.clone();
        killed_by_strangers.timed_out = false;

        let timeout_written = serde_json::to_value(&timeout)
            .expect("a timed-out result is writable as the JSON the journal stores");
        let killed_written = serde_json::to_value(&killed_by_strangers)
            .expect("a killed result is writable as the JSON the journal stores");
        assert_ne!(
            timeout_written, killed_written,
            "a gate killed by the runner's own budget and a gate killed by something else \
             look identical in the wait status, so the payload has to carry the difference"
        );
        assert_eq!(
            timeout_written.get("timed_out"),
            Some(&serde_json::Value::Bool(true)),
            "a budget running out is recorded as its own fact, not inferred from an exit code"
        );
        assert_eq!(
            killed_written.get("timed_out"),
            Some(&serde_json::Value::Bool(false))
        );

        let refused = serde_json::to_value(failed_lint())
            .expect("a refused gate is writable as the JSON the journal stores");
        assert_eq!(
            refused.get("timed_out"),
            Some(&serde_json::Value::Bool(false))
        );
        assert_eq!(refused.get("exit_code"), Some(&serde_json::Value::from(1)));
        assert_ne!(
            timeout_written, refused,
            "a gate that never finished and a gate that finished and refused are different \
             failures and must not share a payload"
        );
    }

    #[test]
    fn an_absent_half_of_the_exit_status_is_written_as_null_rather_than_left_out() {
        let timeout = serde_json::to_value(timed_out_verify())
            .expect("a timed-out result is writable as the JSON the journal stores");
        let fields = timeout
            .as_object()
            .expect("a gate result is written as an object");
        assert!(
            fields.contains_key("exit_code"),
            "a killed process has no exit code, which is written as null the way the event \
             envelope writes its absent task; leaving the key out would make the two mean \
             the same thing to a reader"
        );
        assert_eq!(fields.get("exit_code"), Some(&serde_json::Value::Null));

        let refused = serde_json::to_value(failed_lint())
            .expect("a refused gate is writable as the JSON the journal stores");
        let fields = refused
            .as_object()
            .expect("a gate result is written as an object");
        assert!(
            fields.contains_key("signal"),
            "a command that ran to completion was killed by no signal, and that is a written \
             null, not an absent key"
        );
        assert_eq!(fields.get("signal"), Some(&serde_json::Value::Null));
    }

    #[test]
    fn an_exit_status_key_that_is_not_there_at_all_reads_back_as_the_same_nothing() {
        let written = serde_json::to_string(&timed_out_verify())
            .expect("a timed-out result is writable as the JSON the journal stores");
        let thinned = written.replace("\"exit_code\":null,", "");
        assert_ne!(
            written, thinned,
            "the two documents have to differ in their keys, or the read below proves nothing"
        );

        let read_back: GateResult = serde_json::from_str(&thinned).expect(
            "a row missing a half the run never had is readable, the way a record that omits \
             the envelope's task_id is",
        );
        assert_eq!(
            read_back,
            timed_out_verify(),
            "an absent `exit_code` and a null one are the same fact — the killed command \
             reported no status — so the read side does not need the key the write side \
             always spells"
        );
    }

    #[test]
    fn a_gate_result_nests_under_the_result_key_the_catalog_documents() {
        let result = passed_verify();
        let written = serde_json::to_value(&result)
            .expect("a gate result is writable as the JSON the journal stores");
        let payload = serde_json::json!({"kind": "GateFinished", "result": written});

        assert_eq!(
            payload.get("kind"),
            Some(&serde_json::Value::from("GateFinished")),
            "the tag is the catalog's answer to what happened"
        );
        assert_eq!(
            payload.pointer("/result/kind"),
            Some(&serde_json::Value::from("Verify")),
            "the gate's own `kind` belongs inside the nested object: `#[serde(tag = \"kind\")]` \
             already owns the outer key, and two keys of one name in one object is the \
             collision ADR-0011 measured"
        );
        let nested = payload.get("result").expect("the payload carries a result");
        let read_back: GateResult = serde_json::from_value(nested.clone())
            .expect("the object inside a `GateFinished` payload reads back as the result");
        assert_eq!(read_back, result);
    }

    #[test]
    fn a_payload_that_is_not_a_gate_result_is_refused() {
        let complete = r#"{"kind":"Lint","passed":false,"exit_code":1,"signal":null,"duration_ms":8112,"stdout":"","stderr":"","timed_out":false}"#;
        serde_json::from_str::<GateResult>(complete)
            .expect("the shape above is a gate result, so the refusals below need a witness");
        let wrong: [(String, &str); 4] = [
            (
                complete.replace("\"duration_ms\":8112", "\"duration_ms\":\"8112\""),
                "a duration written as text is not a duration",
            ),
            (
                complete.replace("\"timed_out\":false", "\"timed_out\":false,\"cached\":true"),
                "a field this type does not have is data from another version, not a result",
            ),
            (
                complete.replace("\"duration_ms\":8112,", ""),
                "a run whose duration nobody measured is a run that was not timed",
            ),
            (
                complete.replace("\"kind\":\"Lint\"", "\"kind\":\"linting\""),
                "a gate kind nobody defined is refused, not held as text",
            ),
        ];
        for (payload, why) in wrong {
            assert!(
                serde_json::from_str::<GateResult>(&payload).is_err(),
                "{payload} must be refused rather than decoded: {why}"
            );
        }
    }
    // Running a gate: the subprocess, its two streams, its budget, and what a
    // run leaves behind.

    /// The shell the fixture gates run under, absolute so that no test inherits
    /// whatever `PATH` the harness happened to start with.
    const SHELL: &str = "/bin/sh";

    /// A gate built around a shell script, for the tests that run one rather
    /// than read one off a document.
    fn gate_running(script: &str) -> Gate {
        Gate {
            kind: GateKind::Verify,
            command: vec![SHELL.to_owned(), "-c".to_owned(), script.to_owned()],
            timeout_secs: 30,
            working_dir: None,
            env: BTreeMap::new(),
        }
    }

    /// The same gate with its budget cut to `budget_secs` seconds.
    fn gate_within_budget(script: &str, budget_secs: u64) -> Gate {
        Gate {
            timeout_secs: budget_secs,
            ..gate_running(script)
        }
    }

    /// One chunk a running gate handed over, and how old the run was when it
    /// arrived.
    type Chunk = (Duration, Stream, String);

    /// Run `script` handing every chunk to a collector, so that a test sees
    /// output arrive while the gate is running rather than only what a finished
    /// run kept.
    fn gate_capturing(script: &str) -> (GateResult, Vec<Chunk>) {
        let started = Instant::now();
        let mut arrived: Vec<Chunk> = Vec::new();
        let result = run_gate_streaming(
            &gate_running(script),
            Path::new("/"),
            &mut |stream, text| {
                arrived.push((started.elapsed(), stream, text.to_owned()));
            },
        )
        .expect("a gate whose program exists runs, or the fixture is wrong");
        (result, arrived)
    }

    /// The chunks that came from one of the two streams, in arrival order.
    fn chunks_from(arrived: &[Chunk], stream: Stream) -> Vec<&str> {
        arrived
            .iter()
            .filter(|(_, source, _)| *source == stream)
            .map(|(_, _, text)| text.as_str())
            .collect()
    }

    #[test]
    fn a_gate_that_finishes_reports_the_status_it_exited_with() {
        let result = run_gate(
            &gate_running("echo out; echo err >&2"),
            Path::new("/"),
            None,
        )
        .expect("a gate whose program exists runs rather than errors");

        assert_eq!(
            result.kind,
            GateKind::Verify,
            "a result names the gate that ran"
        );
        assert!(result.passed, "a command that exited 0 satisfied its gate");
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(
            result.signal, None,
            "a command that ran to completion was killed by nobody"
        );
        assert!(!result.timed_out, "finishing is not running out of time");
        assert_eq!(result.stdout, "out\n");
        assert_eq!(result.stderr, "err\n");
        assert!(
            result.duration_ms > 0 && result.duration_ms < 30_000,
            "the result records how long the command ran, not how long it was allowed to: {}ms",
            result.duration_ms
        );
    }

    #[test]
    fn a_gate_that_refuses_reports_the_status_it_refused_with() {
        let result = run_gate(&gate_running("echo no; exit 3"), Path::new("/"), None)
            .expect("a command that refuses is an answer, not a failure to ask");

        assert!(
            !result.passed,
            "a non-zero status is a gate that is not satisfied"
        );
        assert_eq!(result.exit_code, Some(3));
        assert_eq!(result.signal, None);
        assert!(
            !result.timed_out,
            "a command that refused on its own did not run out of time"
        );
        assert_eq!(
            result.stdout, "no\n",
            "the words a refusal came with are retained"
        );
    }

    #[test]
    fn a_gate_that_runs_out_of_its_budget_is_killed_and_its_earlier_output_is_captured() {
        let gate = gate_within_budget("echo early; echo late >&2; exec sleep 30", 1);
        let started = Instant::now();
        let result = run_gate(&gate, Path::new("/"), None)
            .expect("a gate that outlives its budget is reported");
        let waited = started.elapsed();

        assert!(
            result.timed_out,
            "the budget was spent, and that is the fact a response is chosen from"
        );
        assert!(!result.passed, "a gate that ran out of time never passes");
        assert_eq!(
            result.stdout, "early\n",
            "output written before the kill is captured rather than lost with the process"
        );
        assert_eq!(result.stderr, "late\n");
        assert_eq!(
            result.exit_code, None,
            "a killed command never reported a status of its own"
        );
        assert_eq!(
            result.signal,
            Some(9),
            "the signal that ended it is what the wait reported"
        );
        assert!(
            result.duration_ms >= 900 && waited < Duration::from_secs(10),
            "the timeout is enforced, not merely recorded: waited {waited:?} for a budget of 1s"
        );
    }

    #[test]
    fn a_surviving_descendant_cannot_hold_a_timed_out_gate_open() {
        let gate = gate_within_budget("echo early; (sleep 6; echo late) & exec sleep 30", 1);
        let result = run_gate(&gate, Path::new("/"), None)
            .expect("a descendant the supervisor does not own cannot break the run");

        assert!(result.timed_out);
        assert_eq!(
            result.stdout, "early\n",
            "the run stops waiting for a pipe a stranger is holding and reports what did arrive: \
             the group kill that takes the stranger down too is the next task"
        );
    }

    #[test]
    fn a_gate_hands_its_output_over_while_it_is_still_running() {
        let (result, arrived) = gate_capturing("echo first; sleep 3; echo second");

        assert_eq!(
            chunks_from(&arrived, Stream::Stdout),
            ["first\n", "second\n"]
        );
        let ages: Vec<Duration> = arrived.iter().map(|(age, _, _)| *age).collect();
        assert!(
            ages[0] < Duration::from_secs(2),
            "the first line reached the reader while the gate was still running: {ages:?}"
        );
        assert!(
            ages[1] >= Duration::from_secs(1),
            "the second line arrived behind the pause it was waiting on, not in one flush at \
             the end: {ages:?}"
        );
        assert_eq!(result.stdout, "first\nsecond\n");
    }

    #[test]
    fn a_chunk_arrives_on_the_stream_it_was_written_to() {
        let (result, arrived) = gate_capturing("echo a; echo b >&2; echo c; echo d >&2");

        assert_eq!(
            chunks_from(&arrived, Stream::Stdout),
            ["a\n", "c\n"],
            "stdout keeps its own order and carries none of stderr's"
        );
        assert_eq!(chunks_from(&arrived, Stream::Stderr), ["b\n", "d\n"]);
        assert_eq!(result.stdout, "a\nc\n");
        assert_eq!(result.stderr, "b\nd\n");
    }

    #[test]
    fn a_last_line_that_never_got_its_newline_is_still_captured() {
        let (result, arrived) = gate_capturing("printf 'complete\ntrailing'");

        assert_eq!(
            chunks_from(&arrived, Stream::Stdout),
            ["complete\n", "trailing"],
            "a final line with no terminator is still output, and is still handed over"
        );
        assert_eq!(result.stdout, "complete\ntrailing");
    }

    #[test]
    fn bytes_that_are_not_text_arrive_replaced_rather_than_dropped() {
        let (result, arrived) = gate_capturing("printf 'a\\377b\\n'");

        assert_eq!(
            result.stdout, "a\u{FFFD}b\n",
            "invalid UTF-8 is replaced, never dropped"
        );
        assert_eq!(chunks_from(&arrived, Stream::Stdout), ["a\u{FFFD}b\n"]);
    }

    #[test]
    fn a_gate_runs_in_the_directory_it_names_or_the_one_it_was_given() {
        let parent = tempfile::tempdir().expect("a scratch directory below the system temp one");
        let nested = parent.path().join("nested");
        std::fs::create_dir(&nested).expect("a directory inside the scratch one");

        let at_root = run_gate(&gate_running("touch at-root"), parent.path(), None)
            .expect("a gate runs in the root it was handed");
        assert!(
            parent.path().join("at-root").is_file(),
            "with no directory of its own a gate runs where the run said: {}",
            at_root.stderr
        );

        let mut named = gate_running("touch in-nested");
        named.working_dir = Some(PathBuf::from("nested"));
        let in_nested = run_gate(&named, parent.path(), None)
            .expect("a relative directory is resolved against the root");
        assert!(
            nested.join("in-nested").is_file() && !parent.path().join("in-nested").is_file(),
            "a gate names its own directory relative to the root it was given: {}",
            in_nested.stderr
        );

        let mut absolute = gate_running("touch absolute");
        absolute.working_dir = Some(nested.clone());
        let by_path = run_gate(&absolute, Path::new("/"), None)
            .expect("an absolute directory is used exactly as it was written");
        assert!(
            nested.join("absolute").is_file(),
            "an absolute directory is not joined onto anything: {}",
            by_path.stderr
        );
    }

    #[test]
    fn the_environment_a_gate_configures_arrives_with_the_command() {
        let mut gate =
            gate_running("echo \"$KTASK_GATE_MARKER\"; test -n \"$PATH\" && echo path-travels");
        gate.env
            .insert("KTASK_GATE_MARKER".to_owned(), "from-the-gate".to_owned());
        let result =
            run_gate(&gate, Path::new("/"), None).expect("a gate runs with its own environment");

        assert!(result.passed, "{}", result.stderr);
        assert_eq!(
            result.stdout, "from-the-gate\npath-travels\n",
            "a gate's own variables go on top of the environment the supervisor passes down, \
             they do not replace it"
        );
    }

    #[test]
    fn a_gate_that_reads_its_stdin_finds_it_closed_rather_than_waiting() {
        let result = run_gate(&gate_within_budget("cat", 5), Path::new("/"), None)
            .expect("a gate that reads is a gate somebody has to answer");

        assert!(
            result.passed && result.stdout.is_empty() && !result.timed_out,
            "stdin is closed rather than inherited, so a gate is never left reading the terminal \
             the supervisor was started on ({result:?})"
        );
    }

    #[test]
    fn a_command_that_is_not_there_is_an_error_naming_the_gate_and_the_program() {
        let missing = Gate {
            command: vec!["ktask-no-such-gate-program".to_owned()],
            ..gate_running("true")
        };
        let error = run_gate(&missing, Path::new("/"), None)
            .expect_err("a program that does not exist is a clear error, never a panic");
        assert_eq!(
            error.to_string(),
            "gate `verify` failed: could not start `ktask-no-such-gate-program` in `/`: No such \
             file or directory (os error 2)"
        );
    }

    #[test]
    fn a_directory_that_is_not_there_is_refused_naming_the_directory() {
        let mut nowhere = gate_running("true");
        nowhere.working_dir = Some(PathBuf::from("no-such-directory"));
        let error = run_gate(&nowhere, Path::new("/"), None)
            .expect_err("a directory that does not exist is as clear as a missing program");
        assert_eq!(
            error.to_string(),
            "gate `verify` failed: could not start `/bin/sh` in `/no-such-directory`: No such \
             file or directory (os error 2)"
        );
    }

    #[test]
    fn a_gate_holding_no_command_words_is_refused_before_anything_is_spawned() {
        let empty = Gate {
            command: Vec::new(),
            ..gate_running("true")
        };
        let error = run_gate(&empty, Path::new("/"), None)
            .expect_err("a gate with no words has nothing to run");
        assert_eq!(
            error.to_string(),
            "gate `verify` failed: the gate holds no command words, so there is nothing to execute"
        );
    }

    #[test]
    fn a_listening_bus_changes_nothing_about_what_a_gate_reports() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();

        let alone =
            run_gate(&gate_running("echo same"), Path::new("/"), None).expect("a gate runs alone");
        let mut attached = run_gate(&gate_running("echo same"), Path::new("/"), Some(&bus))
            .expect("the same gate runs the same way with a live view attached");

        // How long each run took is a measurement of two different runs, not a
        // fact a viewer can influence. Every other field is compared as it stands.
        let (attached_ms, alone_ms) = (attached.duration_ms, alone.duration_ms);
        attached.duration_ms = alone_ms;
        assert_eq!(
            attached, alone,
            "attaching a viewer changes nothing about what ran, and both runs were still timed: \
             {attached_ms}ms watched against {alone_ms}ms alone"
        );
        let (events, dropped) = watcher.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "nothing is published yet: the event catalog has no entry that can carry a gate's \
             output, so its chunks have nowhere honest to go ({} events, {dropped} dropped)",
            events.len()
        );
    }
}
