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
//! as a budget rather than advice.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

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

/// How long a killed gate's pipes are still read before the run gives up on
/// them and reports what it has.
///
/// Killing the immediate child does not close a pipe a descendant of it
/// still holds open; reaping every descendant is `T042`'s process-group
/// kill. Until that lands, this bounds the wait so a lingering grandchild
/// can never hang the run.
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

    let mut child = Command::new(program)
        .args(args)
        .current_dir(&dir)
        .envs(&gate.env)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| Error::Gate {
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
            // Best-effort: the process may have exited in the instant
            // between the deadline and this signal, which `wait` below
            // reports on its own terms either way.
            let _ = child.kill();
            kill_deadline = Some(Instant::now() + KILL_GRACE);
        }
        if kill_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
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
        fn a_timed_out_gate_is_killed_with_sigkill() {
            let root = tempfile::tempdir().expect("tempdir");
            let gate = shell_gate(GateKind::Verify, "sleep 5", 1);

            let result = run_gate(&gate, root.path(), None).expect("run");

            assert!(result.timed_out);
            assert_eq!(result.signal, Some(9));
            assert_eq!(result.exit_code, None);
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
}
