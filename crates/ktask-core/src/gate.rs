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
//! A run's transcript is then read as well as kept: [`parse_cargo`] turns the
//! words a cargo run writes for a [`GateKind::Verify`] gate into a
//! [`TestSummary`], and answers `None` rather than a wrong summary when the
//! bytes do not hold cargo's answer. VISION.md §8 asks for the parsed form
//! beside the raw output; a green count invented from a crate that never
//! compiled is the one result worse than no result, so refusing is the
//! behaviour the function is graded on.
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

use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

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

/// How long a gate that was asked to stop is given to do it before the whole
/// group is told to stop now.
///
/// A gate's command is usually a runner of some kind — a test harness, a build
/// tool, a shell that started a compiler server — and the runners worth having
/// clean up after themselves: an exclusive lock left held and a fixture left half
/// written make the *next* run fail for a reason that has nothing to do with the
/// task it was given. Two seconds is what that cleanup costs a run that has
/// already failed, and it is a bound rather than an invitation: no gate gets to
/// set its own budget by being slow to die.
const TERM_GRACE: Duration = Duration::from_secs(2);

/// How long a gate's pipes are still listened to once its group has been killed.
///
/// SIGKILL cannot be caught, so after it the gate and everything the gate
/// spawned are gone, and the only thing that can still hold a pipe open is a
/// process that left the group on purpose — `setsid`, which is how a daemon
/// detaches. That process is outside this supervisor's reach by definition, so
/// the wait is bounded rather than endless: a supervisor blocked on a process it
/// does not own has stopped supervising, and output that did arrive is worth more
/// than output a stranger may never write.
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
/// [`Gate::timeout_secs`] is a budget, not a hint. When it runs out the gate's
/// *process group* is sent SIGTERM, and SIGKILL once `TERM_GRACE` has passed:
/// the group rather than the process, because a gate that can spawn a helper can
/// orphan one, and a timeout that stopped only the command it started leaves the
/// lock file held and the port bound for whoever runs this gate next. The run is
/// reported with [`GateResult::timed_out`] set and whatever output had been read
/// by then — which is why the result keeps an exit code, a signal and a timeout
/// flag as three separate facts (ADR-0036).
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
    let mut command = Command::new(program);
    command
        .args(arguments)
        .current_dir(&directory)
        .envs(&gate.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut command);
    let mut child = command.spawn().map_err(|failure| Error::Gate {
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
        if watch.done_listening() {
            break;
        }
    }

    // Nothing left to read, and the process is still the run's to wait for. The
    // budget still governs that wait: a gate that closed its own pipes and kept
    // working is no less subject to its timeout than one that never wrote.
    loop {
        watch.poll()?;
        watch.enforce_budget(gate.timeout_secs)?;
        if watch.status.is_some() || watch.done_listening() {
            break;
        }
        thread::sleep(CHUNK_POLL);
    }
    // A reader still copying bytes nobody asked it to stop holding a pipe for
    // has nowhere left to copy them to, so it ends with the pipe rather than
    // with this run.
    drop(chunks);

    // Nothing is waited for any more, so the timeout is about to be reported.
    // The escalation in `Watch::enforce_budget` only happens while this run is
    // still waiting for something, and a child that neither writes nor listens
    // for signals waits for nothing — it would still be running when the run
    // said it was finished. The group is signalled once here so that it isn't.
    watch.ensure_group_stopped()?;

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

/// The directory a gate's command runs in: the one it names itself, and `root`
/// when it names none.
///
/// Joining is the whole rule, because [`Path::join`] replaces its base when what
/// it is given is absolute: a directory written as `/srv/build` arrives used
/// exactly as written, and one written as `nested` arrives resolved below `root`.
fn working_directory(gate: &Gate, root: &Path) -> PathBuf {
    match &gate.working_dir {
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

/// What has been done to a gate that overran its budget.
#[derive(Debug, Clone, Copy)]
enum Response {
    /// Nothing: the gate is still inside its budget, and it holds its own pipes.
    Idle,
    /// The gate's process group was sent SIGTERM at the instant it carries.
    Terminated(Instant),
    /// The gate's process group was sent SIGKILL at the instant it carries.
    Killed(Instant),
}

impl Response {
    /// When to stop listening to the gate's pipes, or `None` while there is still
    /// a reason to listen.
    ///
    /// A signalled group gets a deadline rather than a hearing that runs until
    /// somebody else decides it is over: `TERM_GRACE` is the gate's chance to
    /// write what it writes on the way out, `POST_KILL_GRACE` is what is left for
    /// those words to arrive, and a stranger that left the group cannot extend
    /// either of them.
    fn listen_deadline(&self) -> Option<Instant> {
        match self {
            Self::Idle => None,
            Self::Terminated(since) => Some(*since + TERM_GRACE + POST_KILL_GRACE),
            Self::Killed(since) => Some(*since + POST_KILL_GRACE),
        }
    }
}

/// The child process, and the three things worth watching about it while it runs.
struct Watch {
    /// The gate's own process.
    child: Child,
    /// The process group the gate leads, which is what an overrun budget signals.
    /// [`own_process_group`] made the gate the leader of it, so this is the gate's
    /// own id.
    group: u32,
    /// When it was spawned, which is what both the budget and the duration
    /// measure from.
    started: Instant,
    /// What it exited with, once it has said so.
    status: Option<ExitStatus>,
    /// Whether its budget ran out.
    timed_out: bool,
    /// What has been done to stop it, and when each step was taken.
    response: Response,
}

impl Watch {
    /// Watch `child`, timing everything from `started`.
    fn new(child: Child, started: Instant) -> Self {
        let group = child.id();
        Self {
            child,
            group,
            started,
            status: None,
            timed_out: false,
            response: Response::Idle,
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

    /// Spend the budget: record that it is spent, and take the gate's group down
    /// in two steps.
    ///
    /// SIGTERM first, because a gate that stops when it is asked leaves nothing
    /// behind to clean up; SIGKILL once `TERM_GRACE` has passed, because a budget
    /// that can be ignored is not a budget. Both go to the group, so whatever the
    /// gate spawned goes down with it rather than inheriting the mess.
    ///
    /// The ladder runs on the clock rather than on whether the gate's own process
    /// has reported a status, because the gate's death is not the group's death: a
    /// descendant that deafens itself to SIGTERM keeps the group, and its id,
    /// alive after the leader is gone, and the group is what this supervisor is
    /// answerable for. A group that has emptied in the meantime answers `ESRCH`,
    /// which [`signal_group`] reads as the outcome it was sent for.
    ///
    /// `timed_out` is set whether or not there was anything left to signal. A gate
    /// that returned inside the instant between the deadline and the signal still
    /// ran past its budget, and `timed_out` is the fact a response is chosen from.
    fn enforce_budget(&mut self, budget_secs: u64) -> Result<()> {
        if self.started.elapsed() < Duration::from_secs(budget_secs) {
            return Ok(());
        }
        self.timed_out = true;
        let now = Instant::now();
        match self.response {
            Response::Idle => {
                signal_group(self.group, Signal::SIGTERM)?;
                self.response = Response::Terminated(now);
            }
            Response::Terminated(since) if now >= since + TERM_GRACE => {
                signal_group(self.group, Signal::SIGKILL)?;
                self.response = Response::Killed(now);
            }
            Response::Terminated(_) | Response::Killed(_) => {}
        }
        Ok(())
    }

    /// Signal the group with SIGKILL if this run timed out and is about to report
    /// itself finished.
    ///
    /// The escalation in [`Watch::enforce_budget`] only happens while something is
    /// still being waited for, and a child that writes nothing and heeds no
    /// signals is waited for by nobody: without this, that child is still running
    /// when the timeout is reported, which is the exact outcome this task exists
    /// to make impossible. An empty group answers `ESRCH` and this treats it as
    /// success, so the cost of the guarantee on the ordinary path is one syscall.
    fn ensure_group_stopped(&mut self) -> Result<()> {
        if self.timed_out {
            signal_group(self.group, Signal::SIGKILL)?;
            self.response = Response::Killed(Instant::now());
        }
        Ok(())
    }

    /// Whether the run is done listening to the gate's pipes.
    fn done_listening(&self) -> bool {
        self.response
            .listen_deadline()
            .is_some_and(|until| Instant::now() >= until)
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

/// Put `command` in a process group of which it will be the leader.
///
/// `0` asks the kernel for the child's own id as its group id, and the spawn does
/// that before the child runs a single instruction: there is no window in which
/// the gate is still in this process's group, so there is no window in which a
/// timeout cannot reach it. Reaching it afterwards is `killpg`, and `killpg` names
/// a group — which is why the group is arranged here rather than with a `setpgid`
/// after the spawn, which would leave that window open and could miss a gate that
/// died inside it.
///
/// The gate's children join the group by inheritance, which is the point: one
/// signal covers everything the gate spawned, without this process keeping a list
/// of pids it can neither trust nor re-query. A child that calls `setsid` leaves
/// the group, and nothing in this design can reach it — that is what detaching
/// means. ADR-0038 records the trade.
fn own_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;

    command.process_group(0);
}

/// Send `signal` to every process in the group led by `group`.
///
/// A group that has emptied answers `ESRCH`, and that is read as the outcome the
/// signal was sent for rather than as a failure: a supervisor that reported an
/// error because it could not kill something already dead would be reporting on
/// its own bookkeeping instead of on the run. Anything else — a group this process
/// has no leave to signal — is a real refusal and travels as one.
fn signal_group(group: u32, signal: Signal) -> Result<()> {
    match killpg(Pid::from_raw(group.cast_signed()), signal) {
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(refusal) => Err(Error::from(std::io::Error::from(refusal))),
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

/// What a test run reported, read out of the run's own words.
///
/// VISION.md §8 asks for common test output formats to become structured
/// results while the raw output is retained. The raw bytes are retained where
/// they were captured — [`GateResult::stdout`] and [`GateResult::stderr`] hold
/// them whole — and this is the parsed half beside them.
///
/// It holds counts and names rather than a verdict, deliberately. Whether a
/// suite is *green* is a decision over counts and over the run that produced
/// them: a gate killed by its timeout and a gate whose crate never compiled
/// both have output with no failures in it, and only the code that ran the gate
/// holds the facts that tell those apart ([`GateResult::timed_out`], the exit
/// status). Reading counts is this type's whole job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TestSummary {
    /// Tests reported as run and passed, summed over every test binary the run
    /// contained: one `cargo test` writes one line per binary.
    pub passed: u32,
    /// Tests reported as failed. Zero here means the reported output holds no
    /// failure — not that the run finished or even started, which is not this
    /// type's to claim.
    pub failed: u32,
    /// Tests reported as skipped, by an attribute or a filter. Counted rather
    /// than listed: an ignored test is not a result anybody acts on.
    pub ignored: u32,
    /// The names the run itself listed under `failures:`, in the order it wrote
    /// them. Names rather than messages, because a name is what
    /// [`GateKind::Targeted`] re-runs, and it is the one part of that block
    /// that survives a change of assertion library.
    pub failures: Vec<String>,
}

/// The line a cargo test run answers with — one per test binary, and the only
/// line whose counts are believed.
const CARGO_RESULT: &str = "test result: ";

/// The word that opens a block listing the tests that refused.
const CARGO_FAILURES: &str = "failures:";

/// The line each test binary's report opens with, as `running 5 tests`.
const CARGO_RUNNING: &str = "running ";

/// The three counts of one `test result:` line, before they are summed.
struct CargoRun {
    passed: u32,
    failed: u32,
    ignored: u32,
}

/// Read one cargo test run into a [`TestSummary`], or answer `None` when the
/// bytes do not hold cargo's answer.
///
/// The line that carries the verdict is the one cargo writes per test binary:
///
/// ```text
/// test result: FAILED. 1 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s
/// ```
///
/// Everything else in the output is either cargo saying what it is doing or a
/// test's own words, and five rules follow from reading only that line.
///
/// - **No such line, no summary.** A crate that never compiled, a run cut by
///   its gate timeout, an empty transcript and another tool's output all reach
///   this function, and each of them would be read by a hopeful parser as
///   `0 failed` — the one answer more wrong than no answer, because it is the
///   answer that lets a task be called done. `None` says the run has to be read
///   by hand, which is what the retained output is for.
/// - **The line has to be cargo's own, at column zero.** `cargo nextest` quotes
///   libtest's report back at four spaces of indent while spelling its own
///   summary differently, and summing the quotation would report a fraction of
///   the run.
/// - **The three counts are read by their labels**, in order, and a count that
///   arrives where another label belongs is refused rather than adopted.
///   Fields cargo added after those three (`measured`, `filtered out`, the
///   duration) are left unread, so a line that grows another field still parses.
/// - **Every binary that opened has to have answered.** A test binary writes
///   its `running N tests` line before its results and its count after them, so
///   a transcript that opened a binary and never wrote its count was cut short
///   — by a gate's timeout, by a signal, by a crash. Summing the counts that do
///   exist there reports a fraction of the run as the whole of it, which is why
///   the header is counted and an unanswered one refuses the run.
/// - **A `failures:` block has to agree with the count beside it.** The block
///   cargo writes twice — once as headings full of panic text, once as the
///   summary's own list of names — is where the names come from, and a list
///   that holds a different number of names than the line counted is a list a
///   caller must not send to remediation. A test that prints `failures:` and
///   indented lines of its own is indistinguishable from the real thing here,
///   and the honest answer to that output is no answer.
///
/// Counts are summed across the binaries of one run, since the runner's verdict
/// is over the whole run; a run whose binaries together overflow a `u32` of
/// tests is refused on the same ground as any other output that cannot be
/// reported honestly.
#[must_use]
pub fn parse_cargo(output: &str) -> Option<TestSummary> {
    let mut summary = TestSummary {
        passed: 0,
        failed: 0,
        ignored: 0,
        failures: Vec::new(),
    };
    let mut listed: Vec<String> = Vec::new();
    let mut listing = false;
    let mut answered = false;
    let mut opened: u32 = 0;

    for line in output.lines() {
        if let Some(run) = cargo_run(line) {
            if listed.len() != usize::try_from(run.failed).ok()? {
                return None;
            }
            summary.passed = summary.passed.checked_add(run.passed)?;
            summary.failed = summary.failed.checked_add(run.failed)?;
            summary.ignored = summary.ignored.checked_add(run.ignored)?;
            summary.failures.append(&mut listed);
            opened = opened.saturating_sub(1);
            listing = false;
            answered = true;
            continue;
        }
        if cargo_header(line) {
            opened = opened.saturating_add(1);
        } else if line == CARGO_FAILURES {
            listing = true;
        } else if listing {
            let name = line.trim();
            if name.is_empty() || line.trim_start() == line {
                listing = false;
            } else {
                listed.push(name.to_owned());
            }
        }
    }

    // A binary that opened and never wrote its count means the run was cut
    // short. What arrived is then a fraction of the run, and a fraction handed
    // back as the whole of it is the wrong summary this function exists to
    // refuse. One count with no header above it is still cargo's own count.
    (answered && opened == 0).then_some(summary)
}

/// Whether `line` is the one libtest opens a test binary with: `running 5
/// tests`, `running 1 test`, `running 0 tests`. The count is required to be a
/// number so that a test printing the words is not read as a second binary.
fn cargo_header(line: &str) -> bool {
    let Some(rest) = line.strip_prefix(CARGO_RUNNING) else {
        return false;
    };
    let count = match rest
        .strip_suffix(" tests")
        .or_else(|| rest.strip_suffix(" test"))
    {
        Some(count) if !count.is_empty() => count,
        _ => return false,
    };
    count.chars().all(|digit| digit.is_ascii_digit())
}

/// One `test result:` line, at column zero, as its three counts — or `None`
/// when the line is not one, including when its verdict contradicts its counts.
fn cargo_run(line: &str) -> Option<CargoRun> {
    let (verdict, counts) = line.strip_prefix(CARGO_RESULT)?.split_once(". ")?;
    let refused = match verdict {
        "ok" => false,
        "FAILED" => true,
        _ => return None,
    };
    let mut fields = counts.split(';');
    let passed = cargo_count(fields.next()?, "passed")?;
    let failed = cargo_count(fields.next()?, "failed")?;
    let ignored = cargo_count(fields.next()?, "ignored")?;
    if refused != (failed > 0) {
        return None;
    }
    Some(CargoRun {
        passed,
        failed,
        ignored,
    })
}

/// One labelled count of a `test result:` line, as `2 passed`. The label is
/// checked rather than skipped: three bare numbers are three numbers that can
/// be read in the wrong order, and the order is the difference between a suite
/// that is green and one that is not.
fn cargo_count(field: &str, label: &str) -> Option<u32> {
    let (value, seen) = field.trim().split_once(' ')?;
    if seen != label {
        return None;
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        Gate, GateKind, GateResult, Pid, Profile, Signal, TestSummary, parse_cargo, profile_from,
        run_gate, run_gate_streaming,
    };
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

    /// Whether `pid` is still executing, as the kernel's own process table says.
    ///
    /// A zombie counts as gone: it has stopped executing, and what a timed-out
    /// gate must not leave behind is something that goes on running.
    fn is_running(pid: i32) -> bool {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        // The state is the field after the last `)`. The field in front of it is
        // the command name in parentheses, which may itself hold a space and a
        // parenthesis, so counting fields from the front reads the wrong thing.
        match stat.rfind(')') {
            Some(close) => !stat[close + 1..].trim_start().starts_with('Z'),
            None => true,
        }
    }

    /// Wait up to `limit` for `pid` to stop running, and report whether it did.
    ///
    /// A signal is not synchronous: the kernel takes the process down on the
    /// next tick it is given. Waiting is what keeps a real assertion from
    /// failing on a scheduler that had not gotten there yet.
    fn wait_until_gone(pid: i32, limit: Duration) -> bool {
        let deadline = Instant::now() + limit;
        while is_running(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        !is_running(pid)
    }

    /// The pid a gate wrote to `pid_file`, which every fixture here makes the pid
    /// of the child the test is watching.
    fn pid_written_to(pid_file: &Path) -> i32 {
        let written = std::fs::read_to_string(pid_file).unwrap_or_else(|failure| {
            panic!(
                "the gate was to write its child's pid to `{}`: {failure}",
                pid_file.display()
            )
        });
        written.trim().parse().unwrap_or_else(|failure| {
            panic!(
                "`{}` should hold a pid, it holds {written:?}: {failure}",
                pid_file.display()
            )
        })
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
            Some(15),
            "the signal that ended it is what the wait reported, and it is the asking rather \
             than the forcing: a gate that stops when it is stopped with SIGTERM is never sent \
             SIGKILL, which is what the grace is for"
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
            "the stranger is in the gate's own process group, so the signal that ends the gate \
             ends it too, and the pipe it was holding closes instead of the run waiting out its \
             six seconds: {result:?}"
        );
    }

    #[test]
    fn a_gate_that_closes_its_pipes_and_keeps_working_runs_out_its_budget() {
        let gate = gate_within_budget("exec sleep 30 >&- 2>&-", 1);
        let started = Instant::now();
        let result = run_gate(&gate, Path::new("/"), None)
            .expect("a gate that stopped writing is still a gate that has to stop");
        let waited = started.elapsed();

        assert!(
            result.stdout.is_empty() && result.stderr.is_empty(),
            "both pipes were closed before a byte was written: {result:?}"
        );
        assert!(
            result.timed_out,
            "closing the pipes is not finishing, and the budget still governs the wait: \
             waited {waited:?}"
        );
        assert!(
            !result.passed,
            "a command killed at its budget satisfied nothing"
        );
        assert_eq!(result.exit_code, None);
        assert_eq!(
            result.signal,
            Some(15),
            "the run ended because its budget ran out and the group was told to stop"
        );
        assert!(
            result.duration_ms >= 900 && waited < Duration::from_secs(10),
            "the run was not left waiting for a command that had nothing left to write: \
             {waited:?} for a budget of 1s"
        );
    }

    #[test]
    fn a_timed_out_gate_leaves_no_surviving_children() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid file");
        let pid_file = scratch.path().join("grandchild");
        // A gate that starts a child of its own, says who it was, and then runs
        // past its budget. `sleep` is the grandchild: it holds both pipes open,
        // so a supervisor that only stops the gate's own process notices.
        let gate = gate_within_budget(
            &format!(
                "sleep 40 & echo $! > '{}' ; echo started; exec sleep 40",
                pid_file.display()
            ),
            1,
        );

        let result = run_gate(&gate, scratch.path(), None)
            .expect("a gate that spawns a child is still a gate that runs");

        assert!(result.timed_out, "the gate ran past its budget");
        assert_eq!(
            result.stdout, "started\n",
            "the child was on foot before the budget ran out: {result:?}"
        );
        let grandchild = pid_written_to(&pid_file);
        assert!(
            wait_until_gone(grandchild, Duration::from_secs(5)),
            "a timed-out gate left its grandchild pid {grandchild} running for the remaining \
             40 seconds: whatever file it has open, lock it holds or port it listens on is \
             still held, and the next run of this gate inherits it"
        );
    }

    #[test]
    fn a_surviving_child_that_ignores_sigterm_is_killed_before_the_timeout_is_reported() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid file");
        let pid_file = scratch.path().join("grandchild");
        // The hard case: a child that deafens itself to SIGTERM, writes nothing,
        // and holds neither pipe, so nothing about it keeps this run listening.
        // Its parent does answer SIGTERM, so the group is empty of the gate by the
        // time the run is ready to report.
        let gate = gate_within_budget(
            &format!(
                "(trap '' TERM; exec sleep 40) >/dev/null 2>&1 & echo $! > '{}' ; exec sleep 40",
                pid_file.display()
            ),
            1,
        );

        let result = run_gate(&gate, scratch.path(), None)
            .expect("a gate whose child will not be asked twice is still a gate that runs");

        assert!(result.timed_out, "the gate ran past its budget");
        let stubborn = pid_written_to(&pid_file);
        assert!(
            wait_until_gone(stubborn, Duration::from_secs(5)),
            "pid {stubborn} ignored SIGTERM and was still running when the timeout was \
             reported: a run that reports itself finished while the gate's tree still lives is \
             how a supervisor leaks a process every attempt"
        );
    }

    #[test]
    fn a_gate_that_ignores_sigterm_is_killed_once_its_grace_has_passed() {
        // Ignored rather than caught, so the `sleep` inherits the ignoring and the
        // whole gate stays put through the first signal.
        let gate = gate_within_budget("trap '' TERM; echo ready; sleep 40", 1);
        let started = Instant::now();
        let result = run_gate(&gate, Path::new("/"), None)
            .expect("a gate that will not stop on being asked is still reported");
        let waited = started.elapsed();

        assert!(result.timed_out, "the gate ran past its budget");
        assert_eq!(
            result.stdout, "ready\n",
            "what the gate wrote before it stopped cooperating is kept: {result:?}"
        );
        assert_eq!(
            result.signal,
            Some(9),
            "it was asked to stop with SIGTERM and was not, so the group was sent SIGKILL"
        );
        assert!(
            result.duration_ms >= 2_500,
            "the grace is real and not merely written down: this gate was given its two \
             seconds to clean up after itself and was escalated after {}ms",
            result.duration_ms
        );
        assert!(
            waited < Duration::from_secs(10),
            "a gate that ignores the first signal is still not allowed to run on: {waited:?} \
             for a budget of 1s"
        );
    }

    #[test]
    fn a_descendant_that_left_the_group_cannot_hold_a_timed_out_gate_open() {
        // `setsid` puts the sleep in a session of its own, which is how a daemon
        // detaches. It is out of reach of a group signal and it keeps the inherited
        // stdout pipe open for twelve seconds, so the only thing left that can end
        // this run is the bound on how long a killed gate's pipes are listened to —
        // which is why it sleeps far longer than the bound allows and the assertion
        // below is well inside it.
        let gate = gate_within_budget("setsid sleep 12 & echo early; exec sleep 30", 1);
        let started = Instant::now();
        let result = run_gate(&gate, Path::new("/"), None)
            .expect("a stranger holding a pipe is not a reason a gate cannot be reported");
        let waited = started.elapsed();

        assert!(result.timed_out, "the gate ran past its budget");
        assert_eq!(
            result.stdout, "early\n",
            "the run stops listening to a pipe a stranger is holding and reports what did \
             arrive: {result:?}"
        );
        assert!(
            result.duration_ms >= 4_000 && waited < Duration::from_secs(9),
            "the grace was spent before the run gave up on the pipe, and the bound still held: \
             {waited:?} against a budget of 1s and a stranger that sleeps 12s"
        );
    }

    #[test]
    fn a_gate_that_finished_of_its_own_accord_leaves_its_group_alone() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the pid file");
        let pid_file = scratch.path().join("helper");
        // The other side of the ladder, and the half that is easy to get wrong: a
        // gate that ran to its own end but left a helper behind — a compiler
        // server, a build cache warmer. Nothing overran its budget, so nothing
        // gives this supervisor leave to signal anything, and a supervisor that
        // killed the group of a gate that *passed* would be destroying live work on
        // the one path where it was never asked to intervene.
        // The helper is pointed at the bit bucket because a helper that held the
        // gate's pipe open would be the stranger the test above already covers;
        // here it is only the group that is being measured.
        let gate = gate_within_budget(
            &format!(
                "sleep 40 >/dev/null 2>&1 & echo $! > '{}' ; echo done",
                pid_file.display()
            ),
            30,
        );

        let result = run_gate(&gate, scratch.path(), None)
            .expect("a gate that finishes with a helper alive is still a gate that ran");

        assert!(
            result.passed,
            "the gate exited 0 of its own accord: {result:?}"
        );
        assert!(!result.timed_out, "it finished well inside its budget");
        assert_eq!(result.stdout, "done\n", "a passed gate keeps what it wrote");
        let helper = pid_written_to(&pid_file);
        // Waited rather than looked at once, because a signal that was sent is not
        // visibly gone a microsecond later: a run that had signalled this group
        // would still show a live pid here, and the check would be reading its own
        // timing rather than the behaviour.
        assert!(
            !wait_until_gone(helper, Duration::from_millis(300)),
            "pid {helper} outlived a gate that finished by itself, and this run has no claim \
             on it: signalling the group of a gate that did not overrun its budget is not what \
             a timeout is for"
        );

        nix::sys::signal::kill(Pid::from_raw(helper), Signal::SIGKILL).unwrap_or_else(|failure| {
            panic!("the helper {helper} belongs to this test, which has to put it down: {failure}")
        });
        assert!(
            wait_until_gone(helper, Duration::from_secs(5)),
            "the helper {helper} was killed by the test that started it and did not stop"
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

    // ---- cargo's own words, captured from real runs -------------------------
    //
    // Every constant below is the verbatim output of one command over a scratch
    // crate, kept as cargo wrote it, because a parser written against a fixture
    // of what somebody remembered cargo says parses what somebody remembered.

    /// Real `cargo test` stdout over a scratch crate whose suite is green: 4 unit tests (2 pass, 2 ignore), then a passing doctest.
    const CARGO_GREEN_TWO_BINARIES: &str = r"
running 4 tests
test tests::slow_on_purpose ... ignored
test tests::talks_to_the_registry ... ignored, needs a network, which the sandbox has none of
test tests::adds_two_positive_numbers ... ok
test tests::adds_zero ... ok

test result: ok. 2 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 1 test
test src/lib.rs - documented (line 8) ... ok

test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

all doctests ran in 0.48s; merged doctests compilation took 0.47s
";

    /// The stderr half of that green run: cargo's own progress lines, which carry no counts at all.
    const CARGOS_OWN_PROGRESS_LINES: &str = r"   Compiling pass v0.1.0 (/tmp/fxgen/pass)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.31s
     Running unittests src/lib.rs (target/debug/deps/pass-8edbe19993689f76)
   Doc-tests pass
";

    /// Real `cargo test` stdout over a scratch crate whose suite is red: 1 pass, 3 failures with their detail, 1 ignored. Every byte is what cargo wrote, blank closing line included.
    const CARGO_RED_ONE_BINARY: &str = r"
running 5 tests
test another::panics_with_a_note ... FAILED
test tests::adds_two_positive_numbers ... ok
test tests::reads_the_index ... ignored, needs a database
test tests::doubles_the_value_it_is_given ... FAILED
test tests::prints_what_it_saw ... FAILED

failures:

---- another::panics_with_a_note stdout ----

thread 'another::panics_with_a_note' (1251409) panicked at src/lib.rs:43:9:
a test that reports its own reason
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- tests::doubles_the_value_it_is_given stdout ----

thread 'tests::doubles_the_value_it_is_given' (1251411) panicked at src/lib.rs:22:9:
assertion `left == right` failed
  left: 9
 right: 8

---- tests::prints_what_it_saw stdout ----
stdout the failure block has to carry
a second line of it

thread 'tests::prints_what_it_saw' (1251412) panicked at src/lib.rs:29:9:
assertion `left == right` failed
  left: 21
 right: 20


failures:
    another::panics_with_a_note
    tests::doubles_the_value_it_is_given
    tests::prints_what_it_saw

test result: FAILED. 1 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

    /// The stderr half of that red run, where cargo names the binary that refused.
    const CARGO_RED_PROGRESS_LINES: &str = r"   Compiling fail v0.1.0 (/tmp/fxgen/fail)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.36s
     Running unittests src/lib.rs (target/debug/deps/fail-de1da06020e4c23c)
error: test failed, to rerun pass `--lib`
";

    /// The same red suite run with `--nocapture`, so libtest writes no detail blocks at all and leaves only the summary's list to read.
    const CARGO_RED_WITHOUT_CAPTURE: &str = r"
running 5 tests
test tests::reads_the_index ... ignored, needs a database
stdout the failure block has to carry
a second line of it
test tests::adds_two_positive_numbers ... ok
test another::panics_with_a_note ... FAILED
test tests::doubles_the_value_it_is_given ... FAILED
test tests::prints_what_it_saw ... FAILED

failures:

failures:
    another::panics_with_a_note
    tests::doubles_the_value_it_is_given
    tests::prints_what_it_saw

test result: FAILED. 1 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

    /// The same red suite run with `cargo test -q`, where the status lines collapse to progress noise and everything else is the shape the summary is read from.
    const CARGO_RED_QUIET: &str = r"
running 5 tests
i 1/5
tests::doubles_the_value_it_is_given --- FAILED
. 3/5
another::panics_with_a_note --- FAILED
tests::prints_what_it_saw --- FAILED

failures:

---- tests::doubles_the_value_it_is_given stdout ----

thread 'tests::doubles_the_value_it_is_given' (1256479) panicked at src/lib.rs:22:9:
assertion `left == right` failed
  left: 9
 right: 8

---- another::panics_with_a_note stdout ----

thread 'another::panics_with_a_note' (1256477) panicked at src/lib.rs:43:9:
a test that reports its own reason
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- tests::prints_what_it_saw stdout ----
stdout the failure block has to carry
a second line of it

thread 'tests::prints_what_it_saw' (1256480) panicked at src/lib.rs:29:9:
assertion `left == right` failed
  left: 21
 right: 20


failures:
    another::panics_with_a_note
    tests::doubles_the_value_it_is_given
    tests::prints_what_it_saw

test result: FAILED. 1 passed; 3 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

    /// Real `cargo test` stdout where the unit tests pass and a doctest refuses: the failing name is a file, a line and a sentence, and cargo appends its own prose after the result line.
    const CARGO_RED_DOCTEST: &str = r"
running 4 tests
test tests::slow_on_purpose ... ignored
test tests::talks_to_the_registry ... ignored, needs a network, which the sandbox has none of
test tests::adds_two_positive_numbers ... ok
test tests::adds_zero ... ok

test result: ok. 2 passed; 0 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 1 test
test src/lib.rs - documented (line 8) ... FAILED

failures:

---- src/lib.rs - documented (line 8) stdout ----
error[E0433]: cannot find module or crate `fixtures_pass` in this scope
  --> src/lib.rs:10:12
   |
10 | assert_eq!(fixtures_pass::add(2, 2), 4);
   |            ^^^^^^^^^^^^^ use of unresolved module or unlinked crate `fixtures_pass`
   |
   = help: if you wanted to use a crate named `fixtures_pass`, use `cargo add fixtures_pass` to add it to your `Cargo.toml`

error: aborting due to 1 previous error

For more information about this error, try `rustc --explain E0433`.
Couldn't compile the test.

failures:
    src/lib.rs - documented (line 8)

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.08s

all doctests ran in 0.15s; merged doctests compilation took 0.07s
";

    /// Real `cargo test` stdout for a crate with no tests at all: two binaries, neither with anything to run.
    const CARGO_NO_TESTS: &str = r"
running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

    /// Real `cargo test` stdout where the library binary is green and an integration binary refuses.
    const CARGO_TWO_BINARIES_ONE_RED: &str = r"
running 3 tests
test tests::doubles ... ok
test tests::slow ... ignored
test tests::halves_round_trip ... ok

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s


running 2 tests
test integration_checks_the_public_shape ... ok
test integration_is_the_one_that_refuses ... FAILED

failures:

---- integration_is_the_one_that_refuses stdout ----

thread 'integration_is_the_one_that_refuses' (1251556) panicked at tests/integration.rs:10:5:
assertion `left == right` failed: someone changed the contract
  left: 12
 right: 13
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    integration_is_the_one_that_refuses

test result: FAILED. 1 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

";

    /// Real `cargo test` stderr for a crate that never compiled: cargo's own words, and no test ever ran.
    const CARGO_COMPILE_FAILURE: &str = r#"   Compiling fail v0.1.0 (/tmp/fxgen/fail)
error[E0308]: mismatched types
  --> src/lib.rs:43:42
   |
43 |         if let Err(e) = Ok::<(), String>(Err("a test that returns Err".to_owned())) {
   |                         ---------------- ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ expected `()`, found `Result<_, String>`
   |                         |
   |                         arguments to this enum variant are incorrect
   |
   = note: expected unit type `()`
                   found enum `Result<_, String>`
help: the type constructed contains `Result<_, String>` due to the type of the argument passed
  --> src/lib.rs:43:25
   |
43 |         if let Err(e) = Ok::<(), String>(Err("a test that returns Err".to_owned())) {
   |                         ^^^^^^^^^^^^^^^^^-----------------------------------------^
   |                                          |
   |                                          this argument influences the type of `Ok`
note: tuple variant defined here
  --> library/core/src/result.rs:561:4

For more information about this error, try `rustc --explain E0308`.
error: could not compile `fail` (lib test) due to 1 previous error
"#;

    /// A tail of a real `cargo nextest run` over the same red suite. It quotes libtest's words indented four spaces and spells its own summary differently, so neither line is one cargo wrote.
    const NESTEST_RED: &str = r"
    running 1 test
    test tests::doubles_the_value_it_is_given ... FAILED

    failures:

    failures:
        tests::doubles_the_value_it_is_given

    test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 4 filtered out; finished in 0.00s

  stderr ───

    thread 'tests::doubles_the_value_it_is_given' (1251670) panicked at src/lib.rs:22:9:
    assertion `left == right` failed
      left: 9
     right: 8
    note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

        PASS [   0.013s] (4/4) fail tests::adds_two_positive_numbers
────────────
     Summary [   0.014s] 4 tests run: 1 passed, 3 failed, 1 skipped
        FAIL [   0.007s] (1/4) fail another::panics_with_a_note
        FAIL [   0.008s] (2/4) fail tests::prints_what_it_saw
        FAIL [   0.008s] (3/4) fail tests::doubles_the_value_it_is_given
error: test run failed
";

    /// What one cargo run reported, read out of a fixture of its real output.
    fn cargo_summary(output: &str) -> TestSummary {
        parse_cargo(output)
            .unwrap_or_else(|| panic!("output cargo wrote was refused as unrecognised:\n{output}"))
    }

    /// Asserts `output` is refused, with the reason a reader can act on.
    fn refused(output: &str, why: &str) {
        assert_eq!(parse_cargo(output), None, "{why}\noutput was:\n{output}");
    }

    /// The `failures` half of a [`TestSummary`], spelled as a list of names.
    fn failing_names(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn a_green_run_becomes_counts_and_an_empty_list_of_failures() {
        assert_eq!(
            cargo_summary(CARGO_GREEN_TWO_BINARIES),
            TestSummary {
                passed: 3,
                failed: 0,
                ignored: 2,
                failures: Vec::new(),
            },
            "cargo wrote one result line per test binary (2 passed then 1 passed, 2 ignored then \
             0), and the summary is their sum rather than the last line alone"
        );
    }

    #[test]
    fn a_red_run_names_the_tests_cargo_listed_as_failures() {
        assert_eq!(
            cargo_summary(CARGO_RED_ONE_BINARY),
            TestSummary {
                passed: 1,
                failed: 3,
                ignored: 1,
                failures: failing_names(&[
                    "another::panics_with_a_note",
                    "tests::doubles_the_value_it_is_given",
                    "tests::prints_what_it_saw",
                ]),
            },
            "the three names are the ones the summary's own `failures:` block lists, in the order \
             cargo wrote them, and the panic text between the two blocks is not a name"
        );
    }

    #[test]
    fn a_red_run_that_captured_nothing_still_lists_its_failures() {
        assert_eq!(
            cargo_summary(CARGO_RED_WITHOUT_CAPTURE),
            TestSummary {
                passed: 1,
                failed: 3,
                ignored: 1,
                failures: failing_names(&[
                    "another::panics_with_a_note",
                    "tests::doubles_the_value_it_is_given",
                    "tests::prints_what_it_saw",
                ]),
            },
            "run with `--nocapture` cargo writes no `---- name stdout ----` blocks at all, so it \
             is the summary's list of names that has to be read, not the detail headers"
        );
    }

    #[test]
    fn a_quiet_run_reports_the_same_summary_as_a_noisy_one() {
        assert_eq!(
            cargo_summary(CARGO_RED_QUIET),
            cargo_summary(CARGO_RED_ONE_BINARY),
            "`-q` collapses the status lines to progress dots and lists the failures in a \
             different order, so the summary is cargo's own counts and names, not the shape of \
             the progress above them"
        );
    }

    #[test]
    fn a_failing_doctest_is_named_by_the_file_and_line_cargo_gives_it() {
        assert_eq!(
            cargo_summary(CARGO_RED_DOCTEST),
            TestSummary {
                passed: 2,
                failed: 1,
                ignored: 2,
                failures: failing_names(&["src/lib.rs - documented (line 8)"]),
            },
            "a doctest has no path::through::a::module to be named by, and its name carries \
             spaces and parentheses; cargo's `all doctests ran in` line after the result is not \
             a second result to add"
        );
    }

    #[test]
    fn a_run_with_nothing_to_run_is_a_zero_summary_rather_than_no_summary() {
        assert_eq!(
            cargo_summary(CARGO_NO_TESTS),
            TestSummary {
                passed: 0,
                failed: 0,
                ignored: 0,
                failures: Vec::new(),
            },
            "a suite that holds no tests did run and reported nothing wrong: that is a summary of \
             zeros, which a caller can act on, and not an absence that looks like a parse failure"
        );
    }

    #[test]
    fn every_test_binary_in_one_run_adds_to_the_one_summary() {
        assert_eq!(
            cargo_summary(CARGO_TWO_BINARIES_ONE_RED),
            TestSummary {
                passed: 3,
                failed: 1,
                ignored: 1,
                failures: failing_names(&["integration_is_the_one_that_refuses"]),
            },
            "one `cargo test` runs several binaries, and the runner's verdict is over all of them: \
             stopping at the first result line would report a green suite over a red run"
        );
    }

    #[test]
    fn cargoes_own_progress_lines_are_read_past_rather_than_counted() {
        let merged = format!("{CARGOS_OWN_PROGRESS_LINES}{CARGO_GREEN_TWO_BINARIES}");
        assert_eq!(
            cargo_summary(&merged),
            cargo_summary(CARGO_GREEN_TWO_BINARIES),
            "the stderr half of a run is cargo saying what it is doing, and it is what a caller \
             that merged both pipes hands over"
        );
        let red_merged = format!("{CARGO_RED_ONE_BINARY}{CARGO_RED_PROGRESS_LINES}");
        assert_eq!(
            cargo_summary(&red_merged),
            cargo_summary(CARGO_RED_ONE_BINARY),
            "cargo's closing `error: test failed, to rerun pass` names the binary that refused; \
             it changes nothing about the counts"
        );
    }

    #[test]
    fn output_that_reports_no_test_result_is_refused_rather_than_read_as_a_green_suite() {
        refused("", "a run that wrote nothing has reported nothing");
        refused(
            CARGO_COMPILE_FAILURE,
            "this is cargo's own stderr for a crate that never compiled, and the run it belongs to \
             never ran a single test; reading it as 0 passed and 0 failed would hand a broken \
             build back as a green suite, which is the wrongest answer this function can give",
        );
        refused(
            "running 0 tests\n\nall doctests ran in 0.00s\n",
            "a line cargo writes on the way to a result is not the result itself",
        );
        refused(
            "error: no such command: `tset`\n",
            "cargo's words about a mistyped subcommand carry no counts to read",
        );
    }

    #[test]
    fn a_run_killed_before_it_reported_is_refused_rather_than_left_as_a_partial_summary() {
        let truncated = CARGO_RED_ONE_BINARY
            .split("test result:")
            .next()
            .expect("the red fixture holds a result line to cut before");
        refused(
            truncated,
            "this is the red run cut where a gate's timeout would cut it: three tests had already \
             been reported FAILED and none had written a count, and a summary of what happened to \
             be printed would say the run was smaller than it was",
        );
    }

    #[test]
    fn a_run_cut_short_between_two_binaries_is_refused_rather_than_partially_summed() {
        let half = CARGO_TWO_BINARIES_ONE_RED
            .rsplit_once("test result:")
            .expect("the fixture holds a second binary to cut before")
            .0;
        refused(
            half,
            "the library binary had already written its `test result: ok. 2 passed; 0 failed; \
             1 ignored`, and the integration binary had opened with `running 2 tests` before the \
             run stopped: summing the one answer that arrived would report a suite of 3 tests \
             with none failing over a run of 5 with one refusing, which is the wrong verdict and \
             not merely an incomplete one",
        );
    }

    #[test]
    fn another_tools_summary_that_quotes_cargo_is_not_cargos_own() {
        refused(
            NESTEST_RED,
            "nextest echoes each test's libtest output indented four spaces under its own \
             `Summary [ 0.014s] 4 tests run: 1 passed, 3 failed, 1 skipped`; summing the quoted \
             `test result:` line would report 1 failed test for a run of 4 rather than refuse, \
             and a nextest gate is a gate whose format has to be read on its own terms",
        );
    }

    #[test]
    fn a_result_line_that_is_not_the_shape_cargo_writes_is_refused() {
        for line in [
            "test result: ok.\n",
            "test result: ok. 2 passed;\n",
            "test result: ok. 2 passed; 0 failed;\n",
            "test result: ok. two passed; 0 failed; 0 ignored;\n",
            "test result: ok. -1 passed; 0 failed; 0 ignored;\n",
            "test result: ok. 0 failed; 2 passed; 0 ignored;\n",
            "test result: Weird. 2 passed; 0 failed; 0 ignored;\n",
            "  test result: ok. 2 passed; 0 failed; 0 ignored;\n",
        ] {
            refused(
                line,
                "the three counts are read by their own labels, in order, from a word cargo \
                 either wrote or did not; anything else is a line this reader cannot answer for",
            );
        }
    }

    #[test]
    fn the_fields_a_summary_does_not_read_are_not_demanded() {
        assert_eq!(
            cargo_summary("test result: ok. 2 passed; 0 failed; 1 ignored;\n"),
            TestSummary {
                passed: 2,
                failed: 0,
                ignored: 1,
                failures: Vec::new(),
            },
            "cargo has grown fields on this line before and will again; the three counts this \
             type holds are read by their labels and everything after them is left alone"
        );
    }

    #[test]
    fn a_verdict_that_contradicts_its_own_counts_is_refused() {
        refused(
            "test result: ok. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.00s\n",
            "`ok` written beside two failures is not a verdict that can be reported as either",
        );
        refused(
            "test result: FAILED. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.00s\n",
            "a run that counted no failure did not refuse, whatever word stood in its verdict",
        );
    }

    #[test]
    fn a_failures_block_that_disagrees_with_the_count_is_refused() {
        let understated =
            CARGO_RED_ONE_BINARY.replace("1 passed; 3 failed;", "1 passed; 4 failed;");
        assert_ne!(
            understated, CARGO_RED_ONE_BINARY,
            "the fixture has to hold the count this edit changes"
        );
        refused(
            &understated,
            "a run that counts four failing tests and lists three has a list that cannot be \
             trusted, and the list is the half a caller sends to remediation",
        );
        refused(
            "running 1 test\ntest quiet ... ok\n\nfailures:\n    quiet\n\
             test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; \
             finished in 0.00s\n",
            "a name listed under a run that counted no failure is not a failing test; a test that \
             prints `failures:` and indented lines of its own is indistinguishable from the real \
            thing here, and inventing a failure would send the wrong tests to be fixed",
        );
    }

    #[test]
    fn a_list_of_failing_names_ends_at_the_first_line_that_is_not_indented() {
        let interrupted = CARGO_RED_ONE_BINARY.replace(
            "    tests::prints_what_it_saw\n\ntest result: FAILED.",
            "    tests::prints_what_it_saw\nerror: test failed, to rerun pass `--lib`\n\
             test result: FAILED.",
        );
        assert_ne!(
            interrupted, CARGO_RED_ONE_BINARY,
            "the fixture has to hold the list this edit closes with cargo's own prose"
        );
        assert_eq!(
            cargo_summary(&interrupted),
            cargo_summary(CARGO_RED_ONE_BINARY),
            "cargo's own words stand at column zero, and a caller that merges the two pipes can \
             hand one over in the middle of the list with no blank line to close it: the names \
             are the indented lines and nothing else, because reading that prose as a fourth name \
             would break the count beside the list and send a thing that is not a test to be fixed"
        );
    }

    #[test]
    fn a_running_line_without_a_count_opens_no_test_binary() {
        let strayed = format!("running  tests\n{CARGO_GREEN_TWO_BINARIES}");
        assert_eq!(
            cargo_summary(&strayed),
            cargo_summary(CARGO_GREEN_TWO_BINARIES),
            "cargo always writes a number in that line, so the number is what makes it a binary \
             opening; a line that says the words without one is somebody else's chatter, and \
             chatter must not be able to refuse a run that answered in full, which is the same \
             reason a quoted result line is read as nothing rather than as a count"
        );
    }
}
