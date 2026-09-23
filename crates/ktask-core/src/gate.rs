//! `GateKind`, `Gate` and `Profile`: the runner's mechanical quality gates as
//! typed configuration, matching `docs/DESIGN.md`'s `gate.rs` pseudocode.
//!
//! A gate is a command the runner executes out-of-band from the agent,
//! per VISION.md §8. Every gate has its own timeout, working directory and
//! environment; a project's full set of gates is its [`Profile`]. Only
//! [`GateKind::Verify`] — "the mandatory, complete local suite. Not
//! optional, not skippable by config in strict mode" — is required to be
//! present; [`Profile::load`] rejects a profile that omits it.
//!
//! [`run_gate`] executes one gate as a subprocess: never through a shell,
//! stdin closed, both output pipes read on their own threads so a gate that
//! fills one cannot stall on the other, and [`Gate::timeout_secs`] enforced
//! as a budget rather than advice. The child runs as the leader of its own
//! process group, so a timeout signals every descendant it spawned —
//! `SIGTERM`, then `SIGKILL` after a grace period — not just the immediate
//! child.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::{Bus, Error, Result, Stream};

/// Which mechanical quality gate a [`Gate`] configures.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum GateKind {
    /// Proves the project was green before the task started.
    Baseline,
    /// Fast edit-loop verification during the run.
    Targeted,
    /// The mandatory, complete local suite. Not optional, not skippable by
    /// config in strict mode.
    Verify,
    /// Static analysis / linting.
    Lint,
    /// Source formatting check.
    Format,
    /// Compiles or builds the project.
    Build,
    /// Scans staged files, tracked files and the outgoing commit range for
    /// forbidden paths and content patterns.
    Privacy,
}

/// One mechanical quality gate: the command the runner executes, and how.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    /// Which gate this is.
    pub kind: GateKind,
    /// The command to run, as an argv. Never shell-interpreted.
    pub command: Vec<String>,
    /// Wall-clock limit for this gate, in seconds.
    pub timeout_secs: u64,
    /// The directory the command runs in. `None` runs it at the project
    /// root.
    pub working_dir: Option<PathBuf>,
    /// Environment variables set for the command, beyond whatever the
    /// runner's own process environment already provides.
    pub env: BTreeMap<String, String>,
}

/// A project's verification profile: every gate the runner may execute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Profile {
    /// The gates this profile defines.
    pub gates: Vec<Gate>,
}

impl Profile {
    /// Returns the gate of the given kind, if this profile defines one.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|gate| gate.kind == kind)
    }

    /// Parses `text` as a TOML verification profile.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if `text` is not valid TOML for a
    /// `Profile`, or if the resulting profile has no [`GateKind::Verify`]
    /// gate: VISION.md §8 makes that gate mandatory, so its absence is a
    /// configuration error caught at load time rather than a profile that
    /// silently never verifies.
    pub fn load(text: &str) -> Result<Profile> {
        let profile: Profile = toml::from_str(text).map_err(|err| Error::Config {
            key: "profile".to_string(),
            detail: err.to_string(),
        })?;
        if profile.get(GateKind::Verify).is_none() {
            return Err(Error::Config {
                key: "gates".to_string(),
                detail: "a verification profile must define the mandatory Verify gate".to_string(),
            });
        }
        Ok(profile)
    }
}

/// The outcome of running one [`Gate`], matching `docs/DESIGN.md`'s
/// `GateFinished` event payload (`result: GateResult`).
///
/// `exit_code` and `signal` are independent of `timed_out`: a gate the
/// runner killed for exceeding [`Gate::timeout_secs`] sets `timed_out` and
/// still records whatever exit status the killed process reported, so a
/// timeout is never confused with an ordinary non-zero exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    /// Which gate produced this result.
    pub kind: GateKind,
    /// Whether the gate's command exited successfully.
    pub passed: bool,
    /// The command's exit code, if it exited normally.
    pub exit_code: Option<i32>,
    /// The signal that terminated the command, if it was killed by one.
    pub signal: Option<i32>,
    /// Wall-clock time the gate took to run, in milliseconds.
    pub duration_ms: u64,
    /// The command's captured standard output.
    pub stdout: String,
    /// The command's captured standard error.
    pub stderr: String,
    /// Whether the runner killed the command for exceeding its timeout.
    pub timed_out: bool,
}

/// A cargo test run's outcome, parsed from its human-readable console output
/// by [`parse_cargo`] (VISION.md §8: "Common test output formats ... are
/// parsed into structured results while raw output is retained").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSummary {
    /// Tests that passed, summed across every `test result:` line in the
    /// output — a multi-binary run (unit tests plus doc-tests, or a
    /// workspace of crates) emits more than one.
    pub passed: u32,
    /// Tests that failed, summed the same way.
    pub failed: u32,
    /// Tests skipped with `#[ignore]`, summed the same way.
    pub ignored: u32,
    /// The names of the failing tests, read from the `failures:` summary
    /// list cargo prints just above the final `test result:` line.
    pub failures: Vec<String>,
}

/// The literal line cargo prints to introduce the final `failures:` summary
/// list, just above the `test result:` line — distinct from the `failures:`
/// line that introduces the per-test `---- name stdout ----` output dump
/// earlier in the same run, which [`parse_failures`] must not mistake for
/// it.
const FAILURES_HEADER: &str = "\nfailures:\n";

/// Parses `output` — the combined stdout/stderr of a `cargo test` (or
/// `cargo nextest run`) invocation — into a [`TestSummary`].
///
/// Every `test result:` line contributes its passed/failed/ignored counts;
/// they are summed rather than taken from the first or last, since a single
/// invocation commonly prints more than one (unit tests plus doc-tests, or
/// one line per crate in a workspace). The failing test names come from the
/// summary `failures:` list cargo prints immediately before the last
/// `test result:` line — not the earlier `failures:` line that introduces
/// each failing test's captured output, which has no fixed-indent name list
/// directly beneath it.
///
/// Returns `None` when `output` contains no `test result:` line at all —
/// a compile error, an empty string, or any other output this function does
/// not recognize — rather than guess at a summary from a run that never
/// produced one.
#[must_use]
pub fn parse_cargo(output: &str) -> Option<TestSummary> {
    // Matches one `test result: ok. 2 passed; 0 failed; 1 ignored; ...` (or
    // `FAILED.`) line. Cargo prints one per test binary it ran, so a run
    // with doc-tests or a workspace of crates emits several, each captured
    // and summed below. Compiled fresh per call rather than cached, since
    // this pattern is not on any hot path — a gate runs once, not once per
    // line.
    let test_result_line =
        Regex::new(r"(?m)^test result: (?:ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored;")
            .ok()?;

    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut ignored = 0u32;
    let mut found_any = false;

    for caps in test_result_line.captures_iter(output) {
        found_any = true;
        passed += caps.get(1)?.as_str().parse::<u32>().ok()?;
        failed += caps.get(2)?.as_str().parse::<u32>().ok()?;
        ignored += caps.get(3)?.as_str().parse::<u32>().ok()?;
    }

    if !found_any {
        return None;
    }

    Some(TestSummary {
        passed,
        failed,
        ignored,
        failures: parse_failures(output),
    })
}

/// Reads the failing test names out of the last `failures:` summary list in
/// `output`, or an empty vec when there is none (a run with no failures).
fn parse_failures(output: &str) -> Vec<String> {
    let Some(start) = output.rfind(FAILURES_HEADER) else {
        return Vec::new();
    };
    output[start + FAILURES_HEADER.len()..]
        .lines()
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .map(str::to_string)
        .collect()
}

/// Builds a [`Profile`] from `config`'s gate command fields (VISION.md §8):
/// each `Some` `*_command` field becomes a [`Gate`] of the corresponding
/// kind, sharing `config.gate_timeout_secs` as its timeout and running at
/// the project root with no extra environment. A `None` field is simply
/// absent from the profile.
///
/// `config.flake_command` has no corresponding `GateKind` yet — VISION.md
/// §14 places flaky-test wiring in v0.2/backlog scope — so it is not turned
/// into a gate here.
///
/// # Errors
///
/// Returns [`Error::Config`] if `config.verify_command` is `None`: VISION.md
/// §8 makes the Verify gate mandatory, so its absence is a configuration
/// error caught here rather than a profile that silently never verifies.
pub fn profile_from(config: &Config) -> Result<Profile> {
    let commands: [(GateKind, &Option<Vec<String>>); 7] = [
        (GateKind::Baseline, &config.baseline_command),
        (GateKind::Targeted, &config.targeted_test_command),
        (GateKind::Verify, &config.verify_command),
        (GateKind::Lint, &config.lint_command),
        (GateKind::Format, &config.format_command),
        (GateKind::Build, &config.build_command),
        (GateKind::Privacy, &config.privacy_command),
    ];

    let gates = commands
        .into_iter()
        .filter_map(|(kind, command)| {
            command.as_ref().map(|command| Gate {
                kind,
                command: command.clone(),
                timeout_secs: config.gate_timeout_secs,
                working_dir: None,
                env: BTreeMap::new(),
            })
        })
        .collect::<Vec<_>>();

    let profile = Profile { gates };
    if profile.get(GateKind::Verify).is_none() {
        return Err(Error::Config {
            key: "verify_command".to_string(),
            detail: "a verification profile must define the mandatory verify_command".to_string(),
        });
    }
    Ok(profile)
}

/// How often the collector wakes to check the clock and drain whatever
/// output has arrived, against a budget `docs/DESIGN.md` measures in whole
/// seconds — four orders of magnitude coarser than this.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a timed-out gate's process group is given to exit after
/// `SIGTERM` before this escalates to `SIGKILL`, and how long a killed
/// gate's pipes are still read before the run gives up on them and reports
/// what it has.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// This gate's kind, formatted the way [`Error::Gate`] names it.
fn gate_name(kind: GateKind) -> String {
    format!("{kind:?}")
}

/// The directory `gate`'s command runs in: its own [`Gate::working_dir`],
/// resolved under `root` when it is relative, or `root` itself when the
/// gate names none.
fn working_dir(gate: &Gate, root: &Path) -> PathBuf {
    match &gate.working_dir {
        Some(dir) if dir.is_absolute() => dir.clone(),
        Some(dir) => root.join(dir),
        None => root.to_path_buf(),
    }
}

/// One line of a gate's output, tagged with the pipe it arrived on.
struct Chunk {
    stream: Stream,
    text: String,
}

/// Reads `pipe` a line at a time on its own thread, sending each line to
/// `tx` as it arrives. Ends when the pipe closes or `tx`'s receiver is gone.
///
/// A line is read with [`BufRead::read_until`] rather than
/// [`BufRead::lines`] so a final line with no trailing newline is still
/// delivered, and bytes that are not valid UTF-8 become the replacement
/// character rather than a dropped line or a panic.
fn spawn_reader(pipe: impl Read + Send + 'static, stream: Stream, tx: Sender<Chunk>) {
    thread::spawn(move || {
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            let Ok(read) = reader.read_until(b'\n', &mut line) else {
                break;
            };
            if read == 0 {
                break;
            }
            let text = String::from_utf8_lossy(&line).into_owned();
            if tx.send(Chunk { stream, text }).is_err() {
                break;
            }
        }
    });
}

/// The signal that ended `status`, when a signal ended it — distinct from a
/// process that ran to its own exit, which is why [`GateResult`] holds the
/// two facts separately.
#[cfg(unix)]
fn terminating_signal(status: ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt as _;
    status.signal()
}

/// No platform-reported signal outside Unix.
#[cfg(not(unix))]
fn terminating_signal(_status: ExitStatus) -> Option<i32> {
    None
}

/// Puts the spawned child in a new process group led by itself, so that
/// every descendant it forks — a shell's pipeline, a backgrounded worker —
/// shares one group id and can be signalled together at timeout.
///
/// `process_group(0)` is the safe replacement for the `pre_exec` +
/// `setpgid` pattern this would otherwise need: it runs `setpgid(0, 0)` in
/// the child after `fork` and before `exec` without any `unsafe` in this
/// crate, which `unsafe_code = "forbid"` does not allow lifting.
#[cfg(unix)]
fn new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// No process groups outside Unix; the child is signalled alone.
#[cfg(not(unix))]
fn new_process_group(_command: &mut Command) {}

/// Which signal [`kill_group`] should send.
#[derive(Debug, Clone, Copy)]
enum GroupSignal {
    /// Ask the group to exit; a well-behaved process can catch this and
    /// clean up.
    Terminate,
    /// End the group unconditionally, for a process that ignored or
    /// outlived [`GroupSignal::Terminate`].
    Kill,
}

/// Signals every process in `child`'s process group — not just `child`
/// itself — since [`new_process_group`] made `child`'s pid its own group
/// id. A gate that spawned a grandchild (a shell pipeline, a backgrounded
/// worker) has no other process reachable from here that can reap it.
///
/// Outside Unix there is no process group to reach, and this is a no-op:
/// `Child::kill` needs `&mut Child`, unavailable through this shared
/// reference, so [`run_gate`] cannot fall back to killing just the
/// immediate child from here either.
fn kill_group(child: &std::process::Child, signal: GroupSignal) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        let pgid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
        let signal = match signal {
            GroupSignal::Terminate => Signal::SIGTERM,
            GroupSignal::Kill => Signal::SIGKILL,
        };
        // Best-effort: the group may already be gone, which is the goal,
        // not an error.
        let _ = signal::killpg(pgid, signal);
    }
    #[cfg(not(unix))]
    {
        let _ = (child, signal);
    }
}

/// Runs `gate`'s command as a subprocess rooted at `root`, enforcing its
/// [`Gate::timeout_secs`] and returning a record of what happened.
///
/// The command is spawned from its own argv — never through a shell — with
/// [`Gate::env`] laid over this process's own environment and stdin closed,
/// since a runner-executed gate has nobody to prompt. Both output pipes are
/// read on their own threads, so a gate that fills one cannot stall because
/// nothing is emptying the other.
///
/// `bus` is accepted for the caller that will one day stream a gate's
/// output live to a frontend, and is not published to yet: `EventKind` has
/// no variant that can carry a chunk of gate output (its module doc
/// reserves `GateStarted`/`GateFinished` for the task that defines their
/// payload), and [`Bus::publish`] is documented as [`crate::Recorder`]'s
/// alone to call. See `docs/adr/0001-defer-gate-output-on-the-bus.md`.
///
/// # Errors
///
/// Returns [`Error::Gate`] when `gate.command` is empty, or when the
/// process could not be spawned — a nonexistent program or working
/// directory. A gate that spawns and exits non-zero, or times out, is not
/// an error: it is a [`GateResult`] with `passed: false`.
pub fn run_gate(gate: &Gate, root: &Path, bus: Option<&Bus>) -> Result<GateResult> {
    // Nowhere honest to publish to yet; see the doc comment above.
    let _ = bus;

    let (program, args) = gate.command.split_first().ok_or_else(|| Error::Gate {
        kind: gate_name(gate.kind),
        detail: "the gate has no command to run".to_string(),
    })?;
    let dir = working_dir(gate, root);

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&dir)
        .envs(&gate.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    new_process_group(&mut command);
    let mut child = command.spawn().map_err(|err| Error::Gate {
        kind: gate_name(gate.kind),
        detail: format!("could not start `{program}` in `{}`: {err}", dir.display()),
    })?;
    let started = Instant::now();

    let stdout = child.stdout.take().ok_or_else(|| Error::Gate {
        kind: gate_name(gate.kind),
        detail: "the spawned process's stdout was not piped".to_string(),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| Error::Gate {
        kind: gate_name(gate.kind),
        detail: "the spawned process's stderr was not piped".to_string(),
    })?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, Stream::Stdout, tx.clone());
    spawn_reader(stderr, Stream::Stderr, tx);

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let budget = Duration::from_secs(gate.timeout_secs);
    let mut timed_out = false;
    let mut kill_deadline = None;

    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => match chunk.stream {
                Stream::Stdout => stdout_buf.push_str(&chunk.text),
                Stream::Stderr => stderr_buf.push_str(&chunk.text),
            },
            // Both readers reached the end of their pipe: everything the
            // gate wrote has already been handed over and kept.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !timed_out && started.elapsed() >= budget {
            timed_out = true;
            // Best-effort: the group may have exited in the instant
            // between the deadline and this signal, which `wait` below
            // reports on its own terms either way.
            kill_group(&child, GroupSignal::Terminate);
            kill_deadline = Some(Instant::now() + KILL_GRACE);
        }
        if kill_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            // The group ignored or outlived the grace period; end it
            // unconditionally.
            kill_group(&child, GroupSignal::Kill);
            break;
        }
    }

    let status = child.wait().map_err(|err| Error::Gate {
        kind: gate_name(gate.kind),
        detail: format!("could not wait for `{program}`: {err}"),
    })?;

    Ok(GateResult {
        kind: gate.kind,
        passed: status.success() && !timed_out,
        exit_code: status.code(),
        signal: terminating_signal(status),
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        stdout: stdout_buf,
        stderr: stderr_buf,
        timed_out,
    })
}

/// The fixed order [`run_completion_set`] runs gates in — VISION.md §8's
/// mechanical quality gates, in the sequence that fails fast: formatting and
/// linting are cheap and catch the most common mistakes, a build proves the
/// tree compiles before the expensive [`GateKind::Verify`] suite runs, and
/// the privacy scan runs last since VISION.md §11 has it inspect "the full
/// outgoing commit range" of a tree that, by then, is known to build and
/// pass.
///
/// [`GateKind::Baseline`] and [`GateKind::Targeted`] are deliberately absent:
/// VISION.md §8 scopes them to "before the task started" and "during the
/// run" respectively, not to the completion decision this function makes.
const COMPLETION_ORDER: [GateKind; 5] = [
    GateKind::Format,
    GateKind::Lint,
    GateKind::Build,
    GateKind::Verify,
    GateKind::Privacy,
];

/// The environment variable [`run_completion_set`] sets, on every gate it
/// runs, to the `base_sha` it was called with — so a gate command can learn
/// the commit the task is being verified from without that commit being
/// baked into the profile. [`GateKind::Privacy`]'s command is the intended
/// reader: VISION.md §8 has it scan "the full outgoing commit range,"
/// meaning the range from this commit to the candidate tree.
const BASE_SHA_ENV: &str = "KTASK_BASE_SHA";

/// Returns a copy of `gate` with [`BASE_SHA_ENV`] added to its environment,
/// naming `base_sha`. A key `gate.env` already sets is overwritten: `env`
/// belongs to the static profile, `base_sha` is per-run truth, and the
/// latter is what a gate command needs.
fn with_base_sha(gate: &Gate, base_sha: &str) -> Gate {
    let mut gate = gate.clone();
    gate.env
        .insert(BASE_SHA_ENV.to_string(), base_sha.to_string());
    gate
}

/// Runs `gate` once, exactly as [`run_completion_set`] runs each of its
/// gates: rooted at `root` with `base_sha` exported as `KTASK_BASE_SHA`. For
/// a caller that needs a single gate — one the completion set does not
/// include, or one run alone — to see what the runner would have seen.
///
/// # Errors
///
/// Returns whatever [`Error`] [`run_gate`] returns when the gate's command
/// cannot start.
pub fn run_gate_at(
    gate: &Gate,
    root: &Path,
    base_sha: &str,
    bus: Option<&Bus>,
) -> Result<GateResult> {
    run_gate(&with_base_sha(gate, base_sha), root, bus)
}

/// Runs the gates that decide whether a task is done: VISION.md §3
/// invariant 7 ("completion of an executable task requires local
/// verification, clean publication, and fetched remote-mainline equality")
/// and §8's mechanical quality gates.
///
/// Runs, in a fixed order (format, lint, build, verify, privacy), whichever of [`GateKind::Format`],
/// [`GateKind::Lint`], [`GateKind::Build`], [`GateKind::Verify`] and
/// [`GateKind::Privacy`] `profile` defines. A gate `profile` does not define
/// is skipped, not treated as a failure — every kind but `Verify` is
/// optional per [`profile_from`] — but `Verify` itself is checked up front:
/// a `profile` that omits it is a configuration error, the same one
/// [`Profile::load`] reports, caught here rather than by silently skipping
/// the one gate this function exists to guarantee.
///
/// Stops at the first gate whose [`GateResult::passed`] is `false`,
/// returning every result gathered so far — including the failing one —
/// rather than an error: a gate that ran and failed is not an error, it is
/// the answer this function was asked for. If [`run_gate`] itself cannot
/// even start a gate's command (a missing program, a bad working
/// directory), that error is propagated immediately instead, since there is
/// no [`GateResult`] to report for a gate that never ran.
///
/// `base_sha` is exported to every gate as the `KTASK_BASE_SHA` environment
/// variable; `bus` is forwarded unchanged to each [`run_gate`] call.
/// Per `docs/adr/0001-defer-gate-output-on-the-bus.md`, `run_gate` does not
/// publish through `bus` yet — there is no `EventKind` payload for a gate's
/// output or its start/finish, and building one is explicitly out of that
/// ADR's scope (and this function's: its file scope is `gate.rs` alone).
/// See `docs/adr/0002-completion-set-does-not-journal-gate-boundaries.md`
/// for why this function accepts `bus` without using it to record a start
/// or finish event, despite being asked to.
///
/// # Errors
///
/// Returns [`Error::Config`] if `profile` has no [`GateKind::Verify`] gate.
/// Returns whatever [`Error`] the first [`run_gate`] call that cannot start
/// its command returns.
pub fn run_completion_set(
    profile: &Profile,
    root: &Path,
    base_sha: &str,
    bus: Option<&Bus>,
) -> Result<Vec<GateResult>> {
    if profile.get(GateKind::Verify).is_none() {
        return Err(Error::Config {
            key: "gates".to_string(),
            detail: "a verification profile must define the mandatory Verify gate".to_string(),
        });
    }

    let mut results = Vec::new();
    for kind in COMPLETION_ORDER {
        let Some(gate) = profile.get(kind) else {
            continue;
        };
        let result = run_gate_at(gate, root, base_sha, bus)?;
        let passed = result.passed;
        results.push(result);
        if !passed {
            break;
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(kind: GateKind) -> Gate {
        Gate {
            kind,
            command: vec!["true".to_string()],
            timeout_secs: 60,
            working_dir: None,
            env: BTreeMap::new(),
        }
    }

    #[test]
    fn get_returns_the_gate_of_the_requested_kind() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify), gate(GateKind::Lint)],
        };
        assert_eq!(
            profile.get(GateKind::Verify).unwrap().kind,
            GateKind::Verify
        );
        assert_eq!(profile.get(GateKind::Lint).unwrap().kind, GateKind::Lint);
    }

    #[test]
    fn get_returns_none_for_a_kind_the_profile_does_not_define() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify)],
        };
        assert!(profile.get(GateKind::Format).is_none());
    }

    #[test]
    fn a_profile_missing_the_verify_gate_is_a_configuration_error_at_load_time() {
        let profile = Profile {
            gates: vec![gate(GateKind::Lint), gate(GateKind::Build)],
        };
        let text = toml::to_string(&profile).expect("serialize");

        let err = Profile::load(&text).expect_err("must fail without a Verify gate");
        assert!(matches!(&err, Error::Config { key, .. } if key == "gates"));
        assert!(err.to_string().contains("Verify"));
    }

    #[test]
    fn an_empty_profile_is_a_configuration_error() {
        let text = toml::to_string(&Profile::default()).expect("serialize");
        let err = Profile::load(&text).expect_err("must fail without any gates");
        assert!(matches!(&err, Error::Config { key, .. } if key == "gates"));
    }

    #[test]
    fn a_profile_with_a_verify_gate_loads() {
        let profile = Profile {
            gates: vec![gate(GateKind::Verify)],
        };
        let text = toml::to_string(&profile).expect("serialize");
        let loaded = Profile::load(&text).expect("load");
        assert_eq!(loaded, profile);
    }

    #[test]
    fn malformed_toml_is_a_configuration_error() {
        let err = Profile::load("not valid toml =====").expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "profile"));
    }

    #[test]
    fn a_full_profile_round_trips_through_toml() {
        let mut env = BTreeMap::new();
        env.insert("RUST_LOG".to_string(), "warn".to_string());

        let profile = Profile {
            gates: vec![
                Gate {
                    kind: GateKind::Baseline,
                    command: vec!["cargo".to_string(), "check".to_string()],
                    timeout_secs: 1_800,
                    working_dir: Some(PathBuf::from("crates/ktask-core")),
                    env: env.clone(),
                },
                Gate {
                    kind: GateKind::Verify,
                    command: vec![
                        "cargo".to_string(),
                        "nextest".to_string(),
                        "run".to_string(),
                    ],
                    timeout_secs: 1_800,
                    working_dir: None,
                    env: BTreeMap::new(),
                },
                Gate {
                    kind: GateKind::Privacy,
                    command: vec!["ktask-rs".to_string(), "privacy-scan".to_string()],
                    timeout_secs: 120,
                    working_dir: None,
                    env,
                },
            ],
        };

        let text = toml::to_string(&profile).expect("serialize");
        let round_tripped = Profile::load(&text).expect("load");
        assert_eq!(round_tripped, profile);
    }

    fn cmd(word: &str) -> Vec<String> {
        vec![word.to_string()]
    }

    #[test]
    fn profile_from_builds_a_gate_for_every_configured_command() {
        let mut config = Config::default();
        config.baseline_command = Some(cmd("baseline"));
        config.targeted_test_command = Some(cmd("targeted"));
        config.verify_command = Some(cmd("verify"));
        config.lint_command = Some(cmd("lint"));
        config.format_command = Some(cmd("format"));
        config.build_command = Some(cmd("build"));
        config.privacy_command = Some(cmd("privacy"));

        let profile = profile_from(&config).expect("profile");

        let kinds: Vec<GateKind> = profile.gates.iter().map(|gate| gate.kind).collect();
        assert_eq!(
            kinds,
            vec![
                GateKind::Baseline,
                GateKind::Targeted,
                GateKind::Verify,
                GateKind::Lint,
                GateKind::Format,
                GateKind::Build,
                GateKind::Privacy,
            ]
        );
        assert_eq!(profile.get(GateKind::Lint).unwrap().command, cmd("lint"));
    }

    #[test]
    fn profile_from_omits_gates_for_unconfigured_commands() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.gates.len(), 1);
        assert_eq!(profile.gates[0].kind, GateKind::Verify);
    }

    #[test]
    fn profile_from_without_verify_command_is_a_configuration_error() {
        let mut config = Config::default();
        config.lint_command = Some(cmd("lint"));

        let err = profile_from(&config).expect_err("must fail without verify_command");
        assert!(matches!(&err, Error::Config { key, .. } if key == "verify_command"));
        assert!(err.to_string().contains("verify_command"));
    }

    #[test]
    fn profile_from_an_empty_config_is_a_configuration_error() {
        let err = profile_from(&Config::default()).expect_err("must fail");
        assert!(matches!(&err, Error::Config { key, .. } if key == "verify_command"));
    }

    #[test]
    fn profile_from_uses_gate_timeout_secs_from_config() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));
        config.gate_timeout_secs = 42;

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.get(GateKind::Verify).unwrap().timeout_secs, 42);
    }

    #[test]
    fn profile_from_ignores_flake_command_since_no_gate_kind_covers_it() {
        let mut config = Config::default();
        config.verify_command = Some(cmd("verify"));
        config.flake_command = Some(cmd("flake"));

        let profile = profile_from(&config).expect("profile");

        assert_eq!(profile.gates.len(), 1);
        assert_eq!(profile.gates[0].kind, GateKind::Verify);
    }

    fn passing_result() -> GateResult {
        GateResult {
            kind: GateKind::Verify,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 1_234,
            stdout: "all tests passed".to_string(),
            stderr: String::new(),
            timed_out: false,
        }
    }

    #[test]
    fn gate_result_round_trips_through_json_as_an_event_payload() {
        let result = passing_result();

        let json = serde_json::to_string(&result).expect("serialize");
        let back: GateResult = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(back, result);
    }

    #[test]
    fn gate_result_serializes_with_the_documented_fields() {
        let result = passing_result();

        let value = serde_json::to_value(&result).expect("serialize to value");
        assert_eq!(value["kind"], "Verify");
        assert_eq!(value["passed"], true);
        assert_eq!(value["exit_code"], 0);
        assert_eq!(value["signal"], serde_json::Value::Null);
        assert_eq!(value["duration_ms"], 1_234);
        assert_eq!(value["stdout"], "all tests passed");
        assert_eq!(value["stderr"], "");
        assert_eq!(value["timed_out"], false);
    }

    #[test]
    fn a_timed_out_gate_is_distinguishable_from_an_ordinary_failing_exit() {
        let failed = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 500,
            stdout: String::new(),
            stderr: "assertion failed".to_string(),
            timed_out: false,
        };
        let timed_out = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 500,
            stdout: String::new(),
            stderr: "assertion failed".to_string(),
            timed_out: true,
        };

        // Same exit code, same everything else — only `timed_out` differs,
        // and that alone must make the two results unequal and separately
        // identifiable.
        assert_ne!(failed, timed_out);
        assert!(!failed.timed_out);
        assert!(timed_out.timed_out);
    }

    #[test]
    fn a_gate_killed_by_a_signal_has_no_exit_code() {
        let result = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: None,
            signal: Some(9),
            duration_ms: 100,
            stdout: String::new(),
            stderr: String::new(),
            timed_out: true,
        };

        assert_eq!(result.exit_code, None);
        assert_eq!(result.signal, Some(9));
        assert!(result.timed_out);
    }

    mod parse_cargo_tests {
        use super::*;

        /// `cargo test` on a passing crate with one ignored test and an
        /// empty doc-tests binary: two `test result:` lines, both `ok.`.
        const PASSING: &str = "\
   Compiling cargoscratch v0.1.0 (/tmp/cargoscratch)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.51s
     Running unittests src/lib.rs (target/debug/deps/cargoscratch-0c2759547569d219)

running 3 tests
test tests::skipped ... ignored
test tests::another_pass ... ok
test tests::it_adds ... ok

test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s

   Doc-tests cargoscratch

running 0 tests

test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";

        /// `cargo test` on a crate with two failing tests: the per-test
        /// `---- name stdout ----` dump (itself introduced by a `failures:`
        /// line) followed by the summary `failures:` list this function
        /// must read instead.
        const FAILING: &str = "\
   Compiling cargoscratch v0.1.0 (/tmp/cargoscratch)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 0.34s
     Running unittests src/lib.rs (target/debug/deps/cargoscratch-0c2759547569d219)

running 3 tests
test tests::another_pass ... ok
test tests::it_fails_too ... FAILED
test tests::it_adds ... FAILED

failures:

---- tests::it_fails_too stdout ----

thread 'tests::it_fails_too' (3779725) panicked at src/lib.rs:19:9:
assertion `left == right` failed
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- tests::it_adds stdout ----

thread 'tests::it_adds' (3779724) panicked at src/lib.rs:9:9:
assertion `left == right` failed
  left: 4
 right: 5


failures:
    tests::it_adds
    tests::it_fails_too

test result: FAILED. 1 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

error: test failed, to rerun pass `--lib`
";

        /// `cargo test` on a crate that fails to compile: no test binary
        /// ever ran, so there is no `test result:` line at all.
        const COMPILE_ERROR: &str = "\
   Compiling cargoscratch v0.1.0 (/tmp/cargoscratch)
error: expected one of `!` or `::`, found `is`
 --> src/lib.rs:1:6
  |
1 | this is not valid rust at all !!!
  |      ^^ expected one of `!` or `::`

error: could not compile `cargoscratch` (lib) due to 1 previous error
warning: build failed, waiting for other jobs to finish...
error: could not compile `cargoscratch` (lib test) due to 1 previous error
";

        #[test]
        fn a_passing_run_sums_passed_across_every_test_result_line() {
            let summary = parse_cargo(PASSING).expect("recognized cargo output");

            assert_eq!(summary.passed, 2);
            assert_eq!(summary.failed, 0);
            assert_eq!(summary.ignored, 1);
            assert!(summary.failures.is_empty());
        }

        #[test]
        fn a_failing_run_reports_counts_and_the_failing_test_names() {
            let summary = parse_cargo(FAILING).expect("recognized cargo output");

            assert_eq!(summary.passed, 1);
            assert_eq!(summary.failed, 2);
            assert_eq!(summary.ignored, 0);
            assert_eq!(
                summary.failures,
                vec![
                    "tests::it_adds".to_string(),
                    "tests::it_fails_too".to_string()
                ]
            );
        }

        #[test]
        fn a_failing_run_does_not_mistake_the_stdout_dump_header_for_the_summary() {
            let summary = parse_cargo(FAILING).expect("recognized cargo output");

            // The dump's `failures:` line is followed by a blank line, not
            // test names — if that header were parsed instead of the real
            // summary, the failures list would come out empty.
            assert_eq!(summary.failures.len(), 2);
        }

        #[test]
        fn empty_output_is_not_recognized() {
            assert!(parse_cargo("").is_none());
        }

        #[test]
        fn output_with_no_test_result_line_is_not_recognized() {
            assert!(parse_cargo(COMPILE_ERROR).is_none());
        }

        #[test]
        fn unrelated_text_containing_the_word_failures_is_not_recognized() {
            assert!(parse_cargo("failures:\n    something\n\nnot cargo output").is_none());
        }
    }

    mod run_gate {
        use super::*;
        use std::fs;
        use std::time::Instant;

        fn shell_gate(kind: GateKind, script: &str, timeout_secs: u64) -> Gate {
            Gate {
                kind,
                command: vec!["sh".to_string(), "-c".to_string(), script.to_string()],
                timeout_secs,
                working_dir: None,
                env: BTreeMap::new(),
            }
        }

        #[test]
        fn captures_stdout_and_stderr_separately_and_reports_success() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Lint, "echo out; echo err >&2", 10);

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.passed);
            assert_eq!(result.kind, GateKind::Lint);
            assert_eq!(result.exit_code, Some(0));
            assert_eq!(result.signal, None);
            assert!(!result.timed_out);
            assert_eq!(result.stdout, "out\n");
            assert_eq!(result.stderr, "err\n");
        }

        #[test]
        fn a_nonzero_exit_is_not_passed_but_is_not_an_error() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "exit 3", 10);

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(!result.passed);
            assert_eq!(result.exit_code, Some(3));
            assert!(!result.timed_out);
        }

        #[test]
        fn duration_reflects_how_long_the_command_actually_ran() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "sleep 0.2", 10);

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(
                result.duration_ms >= 150,
                "a command that slept 200ms reported only {}ms",
                result.duration_ms
            );
        }

        #[test]
        fn a_nonexistent_program_is_a_clear_error_not_a_panic() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = Gate {
                kind: GateKind::Verify,
                command: vec!["definitely-not-a-real-ktask-test-command".to_string()],
                timeout_secs: 10,
                working_dir: None,
                env: BTreeMap::new(),
            };

            let err = run_gate(&gate, root.path(), None).expect_err("must fail cleanly");

            assert!(matches!(&err, Error::Gate { kind, .. } if kind == "Verify"));
            assert!(
                err.to_string()
                    .contains("definitely-not-a-real-ktask-test-command")
            );
        }

        #[test]
        fn an_empty_command_is_a_clear_error() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = Gate {
                kind: GateKind::Build,
                command: vec![],
                timeout_secs: 10,
                working_dir: None,
                env: BTreeMap::new(),
            };

            let err = run_gate(&gate, root.path(), None).expect_err("must fail");

            assert!(matches!(&err, Error::Gate { kind, .. } if kind == "Build"));
        }

        #[test]
        fn a_nonexistent_working_directory_is_a_clear_error_not_a_panic() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = Gate {
                kind: GateKind::Verify,
                command: vec!["true".to_string()],
                timeout_secs: 10,
                working_dir: Some(PathBuf::from("no-such-subdirectory")),
                env: BTreeMap::new(),
            };

            let err = run_gate(&gate, root.path(), None).expect_err("must fail");

            assert!(matches!(&err, Error::Gate { .. }));
        }

        #[test]
        fn a_slow_gate_is_killed_at_its_timeout_and_keeps_output_produced_before_that() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "echo before; sleep 5; echo after", 1);

            let clock = Instant::now();
            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.timed_out);
            assert!(!result.passed);
            assert!(result.stdout.contains("before"));
            assert!(!result.stdout.contains("after"));
            assert!(
                clock.elapsed() < Duration::from_secs(4),
                "run_gate waited {:?}, which is most of the way to the 5s sleep it was \
                 supposed to kill",
                clock.elapsed()
            );
        }

        #[cfg(unix)]
        #[test]
        fn a_timed_out_gate_is_killed_with_sigterm_first() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "sleep 5", 1);

            let clock = Instant::now();
            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.timed_out);
            assert_eq!(
                result.signal,
                Some(15),
                "expected SIGTERM, not escalation to SIGKILL"
            );
            assert_eq!(result.exit_code, None);
            assert!(
                clock.elapsed() < Duration::from_secs(1) + KILL_GRACE,
                "a process that dies on SIGTERM should not wait out the SIGKILL grace \
                 period: {:?}",
                clock.elapsed()
            );
        }

        #[cfg(unix)]
        #[test]
        fn a_timed_out_gate_that_ignores_sigterm_is_killed_with_sigkill_after_the_grace_period() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "trap '' TERM; sleep 5", 1);

            let clock = Instant::now();
            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.timed_out);
            assert_eq!(result.signal, Some(9));
            assert_eq!(result.exit_code, None);
            assert!(
                clock.elapsed() >= Duration::from_secs(1) + KILL_GRACE,
                "SIGKILL should only follow once the grace period after SIGTERM has \
                 elapsed: {:?}",
                clock.elapsed()
            );
        }

        #[cfg(unix)]
        #[test]
        fn a_timed_out_gate_kills_a_grandchild_in_its_process_group() {
            let root = tempfile::tempdir().expect("tempdir");
            let pid_file = root.path().join("grandchild.pid");
            let gate = shell_gate(
                GateKind::Verify,
                &format!("sleep 30 & echo $! > {} ; wait", pid_file.display()),
                1,
            );

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.timed_out);
            let pid_text =
                fs::read_to_string(&pid_file).expect("grandchild pid was written before timeout");
            let grandchild_pid: u32 = pid_text.trim().parse().expect("pid file holds a pid");

            let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
            let deadline = Instant::now() + Duration::from_secs(2);
            while alive(grandchild_pid) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(20));
            }
            assert!(
                !alive(grandchild_pid),
                "grandchild pid {grandchild_pid} outlived the gate that spawned it"
            );
        }

        #[test]
        fn runs_in_the_gates_configured_working_directory() {
            let root = tempfile::tempdir().expect("tempdir");
            let sub = root.path().join("sub");
            fs::create_dir(&sub).expect("mkdir");
            let gate = Gate {
                kind: GateKind::Verify,
                command: vec!["sh".to_string(), "-c".to_string(), "pwd".to_string()],
                timeout_secs: 10,
                working_dir: Some(PathBuf::from("sub")),
                env: BTreeMap::new(),
            };

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert_eq!(
                result.stdout.trim(),
                sub.canonicalize().expect("canonicalize").to_string_lossy()
            );
        }

        #[test]
        fn passes_the_gates_configured_environment_to_the_command() {
            let root = tempfile::tempdir().expect("tempdir");
            let mut env = BTreeMap::new();
            env.insert("KTASK_GATE_TEST_VAR".to_string(), "hello-gate".to_string());
            let gate = Gate {
                kind: GateKind::Verify,
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf '%s' \"$KTASK_GATE_TEST_VAR\"".to_string(),
                ],
                timeout_secs: 10,
                working_dir: None,
                env,
            };

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert_eq!(result.stdout, "hello-gate");
        }

        #[test]
        fn a_bus_is_accepted_but_nothing_is_published_to_it_yet() {
            let root = tempfile::tempdir().expect("tempdir");
            let bus = Bus::new(8);
            let mut sub = bus.subscribe();
            let gate = shell_gate(GateKind::Lint, "echo out", 10);

            let result = run_gate(&gate, root.path(), Some(&bus)).expect("run");

            assert!(result.passed);
            let (events, dropped) = sub.drain();
            assert!(
                events.is_empty(),
                "no EventKind variant can carry a gate output chunk yet; see the ADR"
            );
            assert_eq!(dropped, 0);
        }
    }

    mod run_completion_set_tests {
        use super::*;
        use std::fs;

        /// A gate that, on success, appends `kind` as its own line to
        /// `marker` (so test order can be read back off disk) and exits
        /// `exit_code`.
        fn marker_gate(kind: GateKind, marker: &Path, exit_code: i32) -> Gate {
            Gate {
                kind,
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    format!(
                        "echo {} >> {} ; exit {}",
                        gate_name(kind),
                        marker.display(),
                        exit_code
                    ),
                ],
                timeout_secs: 10,
                working_dir: None,
                env: BTreeMap::new(),
            }
        }

        fn full_profile(marker: &Path) -> Profile {
            Profile {
                gates: vec![
                    marker_gate(GateKind::Format, marker, 0),
                    marker_gate(GateKind::Lint, marker, 0),
                    marker_gate(GateKind::Build, marker, 0),
                    marker_gate(GateKind::Verify, marker, 0),
                    marker_gate(GateKind::Privacy, marker, 0),
                ],
            }
        }

        fn marker_lines(marker: &Path) -> Vec<String> {
            fs::read_to_string(marker)
                .unwrap_or_default()
                .lines()
                .map(str::to_string)
                .collect()
        }

        #[test]
        fn runs_every_configured_gate_in_format_lint_build_verify_privacy_order() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let profile = full_profile(&marker);

            let results =
                run_completion_set(&profile, root.path(), "base123", None).expect("completion set");

            assert!(results.iter().all(|r| r.passed));
            assert_eq!(
                results.iter().map(|r| r.kind).collect::<Vec<_>>(),
                vec![
                    GateKind::Format,
                    GateKind::Lint,
                    GateKind::Build,
                    GateKind::Verify,
                    GateKind::Privacy,
                ]
            );
            assert_eq!(
                marker_lines(&marker),
                vec![
                    "Format".to_string(),
                    "Lint".to_string(),
                    "Build".to_string(),
                    "Verify".to_string(),
                    "Privacy".to_string(),
                ]
            );
        }

        #[test]
        fn skips_gates_the_profile_does_not_define() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let profile = Profile {
                gates: vec![marker_gate(GateKind::Verify, &marker, 0)],
            };

            let results =
                run_completion_set(&profile, root.path(), "base123", None).expect("completion set");

            assert_eq!(results.len(), 1);
            assert_eq!(results[0].kind, GateKind::Verify);
        }

        #[test]
        fn a_failing_gate_short_circuits_the_gates_that_would_follow_it() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let mut profile = full_profile(&marker);
            profile.gates[1] = marker_gate(GateKind::Lint, &marker, 1); // fails

            let results =
                run_completion_set(&profile, root.path(), "base123", None).expect("completion set");

            // Every result gathered so far is still returned, including the
            // failure itself.
            assert_eq!(
                results.iter().map(|r| r.kind).collect::<Vec<_>>(),
                vec![GateKind::Format, GateKind::Lint]
            );
            assert!(results[0].passed);
            assert!(!results[1].passed);
            // Build, Verify and Privacy never ran: their markers were never
            // written, so the mandatory Verify gate did not silently run
            // after a failure either.
            assert_eq!(
                marker_lines(&marker),
                vec!["Format".to_string(), "Lint".to_string()]
            );
        }

        #[test]
        fn the_mandatory_verify_gate_always_runs_when_every_earlier_gate_passes() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let profile = full_profile(&marker);

            let results =
                run_completion_set(&profile, root.path(), "base123", None).expect("completion set");

            let verify = results
                .iter()
                .find(|r| r.kind == GateKind::Verify)
                .expect("Verify must have run");
            assert!(verify.passed);
        }

        #[test]
        fn a_profile_missing_the_mandatory_verify_gate_is_a_configuration_error() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let profile = Profile {
                gates: vec![marker_gate(GateKind::Lint, &marker, 0)],
            };

            let err = run_completion_set(&profile, root.path(), "base123", None)
                .expect_err("must fail without a Verify gate");

            assert!(matches!(&err, Error::Config { key, .. } if key == "gates"));
            assert!(err.to_string().contains("Verify"));
            assert!(
                marker_lines(&marker).is_empty(),
                "no gate should run once the profile is rejected"
            );
        }

        #[test]
        fn base_sha_is_exported_to_every_gates_environment() {
            let root = tempfile::tempdir().expect("tempdir");
            let profile = Profile {
                gates: vec![Gate {
                    kind: GateKind::Verify,
                    command: vec![
                        "sh".to_string(),
                        "-c".to_string(),
                        "printf '%s' \"$KTASK_BASE_SHA\"".to_string(),
                    ],
                    timeout_secs: 10,
                    working_dir: None,
                    env: BTreeMap::new(),
                }],
            };

            let results = run_completion_set(&profile, root.path(), "deadbeef", None)
                .expect("completion set");

            assert_eq!(results[0].stdout, "deadbeef");
        }

        #[test]
        fn run_gate_at_exports_the_base_sha_to_a_single_gate() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = Gate {
                kind: GateKind::Targeted,
                command: vec![
                    "sh".to_string(),
                    "-c".to_string(),
                    "printf '%s' \"$KTASK_BASE_SHA\"".to_string(),
                ],
                timeout_secs: 10,
                working_dir: None,
                env: BTreeMap::new(),
            };

            let result = run_gate_at(&gate, root.path(), "cafef00d", None).expect("gate runs");

            assert_eq!(result.kind, GateKind::Targeted);
            assert_eq!(result.stdout, "cafef00d");
        }

        #[test]
        fn a_gate_that_cannot_start_is_a_propagated_error_not_a_result() {
            let root = tempfile::tempdir().expect("tempdir");
            let profile = Profile {
                gates: vec![Gate {
                    kind: GateKind::Verify,
                    command: vec!["definitely-not-a-real-ktask-test-command".to_string()],
                    timeout_secs: 10,
                    working_dir: None,
                    env: BTreeMap::new(),
                }],
            };

            let err = run_completion_set(&profile, root.path(), "base123", None)
                .expect_err("must fail cleanly");

            assert!(matches!(&err, Error::Gate { kind, .. } if kind == "Verify"));
        }

        #[test]
        fn a_bus_is_forwarded_but_nothing_is_published_to_it_yet() {
            let root = tempfile::tempdir().expect("tempdir");
            let marker = root.path().join("order.log");
            let profile = Profile {
                gates: vec![marker_gate(GateKind::Verify, &marker, 0)],
            };
            let bus = Bus::new(8);
            let mut sub = bus.subscribe();

            let results = run_completion_set(&profile, root.path(), "base123", Some(&bus))
                .expect("completion set");

            assert!(results[0].passed);
            let (events, dropped) = sub.drain();
            assert!(
                events.is_empty(),
                "no EventKind variant can carry gate start/finish yet; see the ADR"
            );
            assert_eq!(dropped, 0);
        }
    }
}
