//! Gate definitions and verification profiles.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::{Bus, Error, Result};

/// Mechanical quality gate kinds, as defined in VISION.md section 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum GateKind {
    /// Baseline: prove the project was green before the task started.
    Baseline,
    /// Targeted: fast edit-loop verification during the run.
    Targeted,
    /// Verify: mandatory, complete local suite. Required for all profiles.
    Verify,
    /// Lint: lint command execution.
    Lint,
    /// Format: format command execution.
    Format,
    /// Build: build command execution.
    Build,
    /// Privacy: scan for forbidden paths and content patterns.
    Privacy,
    /// Flake: repeated or randomized execution of affected tests.
    Flake,
}

/// A gate command with its configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Gate {
    /// The gate kind.
    pub kind: GateKind,
    /// Command to execute, as a list of arguments.
    pub command: Vec<String>,
    /// Timeout in seconds.
    pub timeout_secs: u64,
    /// Working directory for execution, if specified.
    pub working_dir: Option<PathBuf>,
    /// Environment variables to set for the gate execution.
    pub env: BTreeMap<String, String>,
}

/// The result of executing a gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GateResult {
    /// The gate kind that was executed.
    pub kind: GateKind,
    /// Whether the gate passed (exit code 0 and not timed out).
    pub passed: bool,
    /// Exit code from the command, if available.
    pub exit_code: Option<i32>,
    /// Signal number that terminated the process, if applicable.
    pub signal: Option<i32>,
    /// Duration of execution in milliseconds.
    pub duration_ms: u64,
    /// Standard output from the gate execution.
    pub stdout: String,
    /// Standard error from the gate execution.
    pub stderr: String,
    /// Whether the gate execution timed out.
    pub timed_out: bool,
}

/// A verification profile containing gates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    /// Gates in this profile.
    pub gates: Vec<Gate>,
}

impl Profile {
    /// Get a gate by its kind.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|g| g.kind == kind)
    }

    /// Validate that the profile has the mandatory Verify gate.
    ///
    /// # Errors
    ///
    /// Returns an error if the Verify gate is missing from the profile.
    pub fn validate(&self) -> Result<()> {
        if self.get(GateKind::Verify).is_none() {
            return Err(Error::Config {
                key: "gates".to_string(),
                detail: "Verify gate is mandatory and must be present in the profile".to_string(),
            });
        }
        Ok(())
    }
}

/// Execute a gate command and capture its output.
///
/// Spawns a subprocess with the gate's command, captures stdout and stderr,
/// enforces the timeout, and publishes output chunks to the bus as they arrive.
/// The output is captured even if the gate times out.
///
/// # Arguments
///
/// * `gate` - The gate command to execute
/// * `root` - The working directory for the gate execution
/// * `bus` - Optional event bus for publishing output events (not yet used for output events)
///
/// # Errors
///
/// Returns an error if the command cannot be spawned (e.g., command not found).
/// A timeout, nonzero exit code, or signal do not produce an error; they are
/// captured in the [`GateResult`].
///
/// # Panics
///
/// Panics if the gate command vector is empty.
pub fn run_gate(gate: &Gate, root: &Path, bus: Option<&Bus>) -> Result<GateResult> {
    let start = Instant::now();
    let _ = bus; // bus parameter not yet used for output events

    // Resolve the command path
    let program = gate.command.first().ok_or_else(|| Error::Gate {
        kind: format!("{:?}", gate.kind),
        detail: "Command vector must not be empty".to_string(),
    })?;
    let args = gate.command.get(1..).unwrap_or_default();

    // Try to spawn the process
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Apply environment variables
    for (key, value) in &gate.env {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().map_err(|e| Error::Gate {
        kind: format!("{:?}", gate.kind),
        detail: format!("Failed to spawn command '{program}': {e}"),
    })?;

    // Extract stdout and stderr pipes
    let stdout = child.stdout.take().ok_or_else(|| Error::Gate {
        kind: format!("{:?}", gate.kind),
        detail: "Could not open stdout pipe".to_string(),
    })?;

    let stderr = child.stderr.take().ok_or_else(|| Error::Gate {
        kind: format!("{:?}", gate.kind),
        detail: "Could not open stderr pipe".to_string(),
    })?;

    // Shared buffers for output
    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));

    // Spawn reader thread for stdout
    let stdout_buf_clone = Arc::clone(&stdout_buf);
    let stdout_handle = thread::spawn(move || {
        let mut reader = stdout;
        let mut buf = [0; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(slice) = buf.get(..n)
                        && let Ok(s) = std::str::from_utf8(slice)
                        && let Ok(mut output) = stdout_buf_clone.lock()
                    {
                        output.push_str(s);
                    }
                }
            }
        }
    });

    // Spawn reader thread for stderr
    let stderr_buf_clone = Arc::clone(&stderr_buf);
    let stderr_handle = thread::spawn(move || {
        let mut reader = stderr;
        let mut buf = [0; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(slice) = buf.get(..n)
                        && let Ok(s) = std::str::from_utf8(slice)
                        && let Ok(mut output) = stderr_buf_clone.lock()
                    {
                        output.push_str(s);
                    }
                }
            }
        }
    });

    // Wrap child in Arc<Mutex> so we can kill it if needed
    let child_arc = Arc::new(Mutex::new(child));
    let child_clone = Arc::clone(&child_arc);

    // Wait for the child with timeout using a separate thread and channel
    let (tx, rx) = mpsc::channel();
    let timeout = Duration::from_secs(gate.timeout_secs);

    thread::spawn(move || {
        if let Ok(mut child) = child_clone.lock() {
            let status = child.wait();
            let _ = tx.send(status);
        }
    });

    let (timed_out, exit_status) = if let Ok(status) = rx.recv_timeout(timeout) {
        (false, Some(status))
    } else {
        // Timeout occurred, try to kill the child process
        if let Ok(mut child) = child_arc.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Try to get the status one more time with a short wait
        (true, rx.try_recv().ok())
    };

    // Wait for reader threads to finish (they should finish when pipes close)
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Collect the output
    let stdout = stdout_buf.lock().map(|s| s.clone()).unwrap_or_default();
    let stderr = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();

    // Extract exit code and signal
    let (exit_code, signal) = match exit_status {
        Some(Ok(status)) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                (status.code(), status.signal())
            }
            #[cfg(not(unix))]
            {
                (status.code(), None)
            }
        }
        _ => (None, None),
    };

    #[allow(clippy::cast_possible_truncation)]
    let duration_ms = start.elapsed().as_millis().min(u128::from(u64::MAX)) as u64;
    let passed = !timed_out && exit_code == Some(0);

    let result = GateResult {
        kind: gate.kind,
        passed,
        exit_code,
        signal,
        duration_ms,
        stdout,
        stderr,
        timed_out,
    };

    Ok(result)
}

/// Build a verification profile from configuration.
///
/// Creates a Profile with gates for each configured gate command in the Config.
/// The `verify_command` is mandatory and must be present in the configuration.
/// All gates use the timeout value from `config.gate_timeout_secs`.
///
/// # Errors
///
/// Returns an error if:
/// - `verify_command` is not configured (mandatory)
pub fn profile_from(config: &Config) -> Result<Profile> {
    if config.verify_command.is_none() {
        return Err(Error::Config {
            key: "verify_command".to_string(),
            detail: "verify_command is mandatory and must be configured".to_string(),
        });
    }

    let mut gates = Vec::new();
    let timeout_secs = u64::from(config.gate_timeout_secs);

    if let Some(cmd) = &config.baseline_command {
        gates.push(Gate {
            kind: GateKind::Baseline,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.targeted_test_command {
        gates.push(Gate {
            kind: GateKind::Targeted,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.verify_command {
        gates.push(Gate {
            kind: GateKind::Verify,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.lint_command {
        gates.push(Gate {
            kind: GateKind::Lint,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.format_command {
        gates.push(Gate {
            kind: GateKind::Format,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.build_command {
        gates.push(Gate {
            kind: GateKind::Build,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.privacy_command {
        gates.push(Gate {
            kind: GateKind::Privacy,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    if let Some(cmd) = &config.flake_command {
        gates.push(Gate {
            kind: GateKind::Flake,
            command: cmd.clone(),
            timeout_secs,
            working_dir: None,
            env: BTreeMap::new(),
        });
    }

    let profile = Profile { gates };
    profile.validate()?;
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_get_returns_gate_by_kind() {
        let gate = Gate {
            kind: GateKind::Verify,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![gate.clone()],
        };
        assert_eq!(profile.get(GateKind::Verify), Some(&gate));
        assert_eq!(profile.get(GateKind::Baseline), None);
    }

    #[test]
    fn profile_validation_requires_verify_gate() {
        let baseline_gate = Gate {
            kind: GateKind::Baseline,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![baseline_gate],
        };
        let result = profile.validate();
        assert!(result.is_err());
        if let Err(Error::Config { key, detail }) = result {
            assert_eq!(key, "gates");
            assert!(detail.contains("Verify"));
        }
    }

    #[test]
    fn profile_validation_passes_with_verify_gate() {
        let verify_gate = Gate {
            kind: GateKind::Verify,
            command: vec!["cargo".to_string(), "test".to_string()],
            timeout_secs: 300,
            working_dir: None,
            env: BTreeMap::new(),
        };
        let profile = Profile {
            gates: vec![verify_gate],
        };
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn gate_kind_roundtrips_through_serde() {
        let kinds = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
            GateKind::Flake,
        ];

        for kind in &kinds {
            let json = serde_json::to_string(kind).expect("serialize");
            let deserialized: GateKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(*kind, deserialized);
        }
    }

    #[test]
    fn profile_roundtrips_through_toml() {
        use toml;

        let profile_toml = r#"
[[gates]]
kind = "Verify"
command = ["cargo", "test"]
timeout_secs = 300
env = {}

[[gates]]
kind = "Lint"
command = ["cargo", "clippy"]
timeout_secs = 60
env = {}
"#;

        let profile: Profile = toml::from_str(profile_toml).expect("deserialize");
        assert_eq!(profile.gates.len(), 2);
        assert!(profile.get(GateKind::Verify).is_some());
        assert!(profile.get(GateKind::Lint).is_some());

        let serialized = toml::to_string(&profile).expect("serialize");
        let deserialized: Profile = toml::from_str(&serialized).expect("deserialize");
        assert_eq!(deserialized.gates.len(), 2);
    }

    #[test]
    fn profile_from_requires_verify_command() {
        let config = Config::default();
        let result = profile_from(&config);
        assert!(result.is_err());
        if let Err(Error::Config { key, detail }) = result {
            assert_eq!(key, "verify_command");
            assert!(detail.contains("mandatory"));
        } else {
            panic!("expected Config error");
        }
    }

    #[test]
    fn profile_from_with_only_verify_command() {
        let mut config = Config::default();
        config.verify_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        let profile = profile_from(&config).expect("profile_from should succeed");
        assert_eq!(profile.gates.len(), 1);
        let verify = profile
            .get(GateKind::Verify)
            .expect("verify gate should exist");
        assert_eq!(
            verify.command,
            vec!["cargo".to_string(), "test".to_string()]
        );
        assert_eq!(verify.timeout_secs, u64::from(config.gate_timeout_secs));
    }

    #[test]
    fn profile_from_includes_configured_gates() {
        let mut config = Config::default();
        config.baseline_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        config.targeted_test_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        config.verify_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        config.lint_command = Some(vec!["cargo".to_string(), "clippy".to_string()]);
        config.format_command = Some(vec!["cargo".to_string(), "fmt".to_string()]);
        config.build_command = Some(vec!["cargo".to_string(), "build".to_string()]);
        config.privacy_command = Some(vec!["cargo".to_string(), "deny".to_string()]);
        config.flake_command = Some(vec!["cargo".to_string(), "nextest".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        assert_eq!(profile.gates.len(), 8);
        assert!(profile.get(GateKind::Baseline).is_some());
        assert!(profile.get(GateKind::Targeted).is_some());
        assert!(profile.get(GateKind::Verify).is_some());
        assert!(profile.get(GateKind::Lint).is_some());
        assert!(profile.get(GateKind::Format).is_some());
        assert!(profile.get(GateKind::Build).is_some());
        assert!(profile.get(GateKind::Privacy).is_some());
        assert!(profile.get(GateKind::Flake).is_some());
    }

    #[test]
    fn profile_from_uses_config_timeout() {
        let mut config = Config::default();
        config.gate_timeout_secs = 3600;
        config.verify_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        config.lint_command = Some(vec!["cargo".to_string(), "clippy".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        let verify = profile
            .get(GateKind::Verify)
            .expect("verify gate should exist");
        let lint = profile.get(GateKind::Lint).expect("lint gate should exist");
        assert_eq!(verify.timeout_secs, 3600);
        assert_eq!(lint.timeout_secs, 3600);
    }

    #[test]
    fn profile_from_omits_unconfigured_gates() {
        let mut config = Config::default();
        config.verify_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        config.lint_command = Some(vec!["cargo".to_string(), "clippy".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        assert_eq!(profile.gates.len(), 2);
        assert!(profile.get(GateKind::Verify).is_some());
        assert!(profile.get(GateKind::Lint).is_some());
        assert!(profile.get(GateKind::Baseline).is_none());
        assert!(profile.get(GateKind::Format).is_none());
    }

    #[test]
    fn profile_from_profile_is_valid() {
        let mut config = Config::default();
        config.verify_command = Some(vec!["cargo".to_string(), "test".to_string()]);
        let profile = profile_from(&config).expect("profile_from should succeed");
        assert!(profile.validate().is_ok());
    }

    #[test]
    fn gate_result_passed_roundtrips_through_json() {
        let result = GateResult {
            kind: GateKind::Verify,
            passed: true,
            exit_code: Some(0),
            signal: None,
            duration_ms: 5000,
            stdout: "test output".to_string(),
            stderr: String::new(),
            timed_out: false,
        };

        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: GateResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn gate_result_failed_with_exit_code_roundtrips_through_json() {
        let result = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 3000,
            stdout: "some output".to_string(),
            stderr: "error output".to_string(),
            timed_out: false,
        };

        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: GateResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn gate_result_timed_out_is_distinguishable_from_exit_code() {
        let timed_out = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: None,
            signal: None,
            duration_ms: 60000,
            stdout: "partial output".to_string(),
            stderr: String::new(),
            timed_out: true,
        };

        let exit_code_failure = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(1),
            signal: None,
            duration_ms: 1000,
            stdout: "output".to_string(),
            stderr: String::new(),
            timed_out: false,
        };

        assert_ne!(timed_out, exit_code_failure);
        assert!(timed_out.timed_out);
        assert!(!exit_code_failure.timed_out);
    }

    #[test]
    fn gate_result_with_signal_roundtrips_through_json() {
        let result = GateResult {
            kind: GateKind::Lint,
            passed: false,
            exit_code: None,
            signal: Some(9),
            duration_ms: 2500,
            stdout: String::new(),
            stderr: "killed".to_string(),
            timed_out: false,
        };

        let json = serde_json::to_string(&result).expect("serialize");
        let deserialized: GateResult = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(result, deserialized);
    }

    #[test]
    fn gate_result_with_all_gate_kinds() {
        let kinds = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
            GateKind::Flake,
        ];

        for kind in &kinds {
            let result = GateResult {
                kind: *kind,
                passed: true,
                exit_code: Some(0),
                signal: None,
                duration_ms: 1000,
                stdout: "output".to_string(),
                stderr: String::new(),
                timed_out: false,
            };

            let json = serde_json::to_string(&result).expect("serialize");
            let deserialized: GateResult = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(result, deserialized);
            assert_eq!(deserialized.kind, *kind);
        }
    }

    #[test]
    fn run_gate_with_successful_command() {
        let gate = Gate {
            kind: GateKind::Verify,
            command: vec!["echo".to_string(), "hello".to_string()],
            timeout_secs: 5,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None).expect("run_gate should succeed");

        assert_eq!(result.kind, GateKind::Verify);
        assert!(result.passed);
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.signal, None);
        assert!(!result.timed_out);
        assert!(result.duration_ms > 0);
        assert!(result.stdout.contains("hello"));
    }

    #[test]
    fn run_gate_with_failing_command() {
        let gate = Gate {
            kind: GateKind::Lint,
            command: vec!["sh".to_string(), "-c".to_string(), "exit 1".to_string()],
            timeout_secs: 5,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None).expect("run_gate should succeed");

        assert_eq!(result.kind, GateKind::Lint);
        assert!(!result.passed);
        assert_eq!(result.exit_code, Some(1));
        assert_eq!(result.signal, None);
        assert!(!result.timed_out);
        assert!(result.duration_ms > 0);
    }

    #[test]
    fn run_gate_with_nonexistent_command() {
        let gate = Gate {
            kind: GateKind::Format,
            command: vec!["nonexistent_command_xyz_abc".to_string()],
            timeout_secs: 5,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None);
        assert!(
            result.is_err(),
            "nonexistent command should return an error"
        );

        if let Err(Error::Gate { kind, detail }) = result {
            assert_eq!(kind, "Format");
            assert!(detail.contains("not found") || detail.contains("No such file"));
        } else {
            panic!("expected Gate error");
        }
    }

    #[test]
    fn run_gate_with_timeout() {
        let gate = Gate {
            kind: GateKind::Build,
            command: vec!["sleep".to_string(), "10".to_string()],
            timeout_secs: 1,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None)
            .expect("run_gate should return a result even on timeout");

        assert_eq!(result.kind, GateKind::Build);
        assert!(!result.passed);
        assert!(result.timed_out);
        assert!(result.duration_ms > 1000);
    }

    #[test]
    fn run_gate_captures_output() {
        let gate = Gate {
            kind: GateKind::Verify,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "echo 'stdout line' && echo 'stderr line' >&2".to_string(),
            ],
            timeout_secs: 5,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None).expect("run_gate should succeed");

        assert!(result.stdout.contains("stdout line"));
        assert!(result.stderr.contains("stderr line"));
    }

    #[test]
    fn run_gate_records_duration() {
        let gate = Gate {
            kind: GateKind::Verify,
            command: vec!["echo".to_string(), "test".to_string()],
            timeout_secs: 5,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None).expect("run_gate should succeed");

        assert!(result.duration_ms > 0);
    }
}
