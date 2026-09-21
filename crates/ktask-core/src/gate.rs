//! Gate definitions and verification profiles.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::{Bus, Error, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;

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

/// Summary of test results parsed from cargo output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestSummary {
    /// Number of passed tests.
    pub passed: u32,
    /// Number of failed tests.
    pub failed: u32,
    /// Number of ignored tests.
    pub ignored: u32,
    /// List of test names that failed.
    pub failures: Vec<String>,
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

fn kill_process_group(child_pid: u32) {
    let pgid = Pid::from_raw(i32::try_from(child_pid).unwrap_or(1));
    let _ = kill(pgid, Signal::SIGTERM);
    thread::sleep(Duration::from_millis(100));
    let _ = kill(pgid, Signal::SIGKILL);
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
        .stderr(Stdio::piped())
        .process_group(0);

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

    // Store the child PID to kill the process group on timeout
    let child_pid = child.id();

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
        // Timeout occurred, kill the entire process group
        kill_process_group(child_pid);
        // Wait for child to exit
        if let Ok(mut child) = child_arc.lock() {
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

/// Parse cargo test output to extract test results summary.
///
/// Reads the `test result:` line from cargo output and extracts the counts.
/// Also parses the failures block to collect failed test names.
///
/// # Arguments
///
/// * `output` - The complete stdout from a cargo test run
///
/// # Returns
///
/// Returns `Some(TestSummary)` if the output contains a valid `test result:` line,
/// or `None` if the output format is unrecognized or parsing fails.
#[must_use]
pub fn parse_cargo(output: &str) -> Option<TestSummary> {
    let mut passed = 0u32;
    let mut failed = 0u32;
    let mut ignored = 0u32;
    let mut failures = Vec::new();

    // Find the test result line and extract counts
    for line in output.lines() {
        if let Some(result_part) = line.split("test result:").nth(1) {
            // Parse: "test result: ok. 306 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.55s"
            // or: "test result: FAILED. 3 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s"
            for segment in result_part.split(';') {
                let segment = segment.trim();
                if segment.contains("passed") {
                    // Extract the last number in the segment before "passed"
                    passed = segment
                        .split_whitespace()
                        .rev()
                        .nth(1)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                } else if segment.contains("failed") {
                    // Extract the last number in the segment before "failed"
                    failed = segment
                        .split_whitespace()
                        .rev()
                        .nth(1)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                } else if segment.contains("ignored") {
                    // Extract the last number in the segment before "ignored"
                    ignored = segment
                        .split_whitespace()
                        .rev()
                        .nth(1)
                        .and_then(|s| s.parse::<u32>().ok())
                        .unwrap_or(0);
                }
            }
            break;
        }
    }

    // Parse failures section if present
    let mut in_failures = false;
    for line in output.lines() {
        if line.trim() == "failures:" {
            in_failures = true;
            continue;
        }

        if in_failures {
            // Failure block starts with "---- test_name stdout ----"
            if line.starts_with("----") && line.ends_with("----") {
                // Extract test name from "---- module::test_name stdout ----"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 3 {
                    // parts[0] = "----", parts[1..n-1] = test name, parts[n-1] = "----"
                    if let Some(test_names) = parts.get(1..parts.len().saturating_sub(1)) {
                        let test_name = test_names.join(" ");
                        failures.push(test_name);
                    }
                }
            }

            // Stop parsing failures when we hit the summary at the end
            if line.contains("test result:") && line != "test result:" {
                break;
            }
        }
    }

    // Return None if we didn't find a test result line
    if !output.contains("test result:") {
        return None;
    }

    Some(TestSummary {
        passed,
        failed,
        ignored,
        failures,
    })
}

/// Run the completion set of gates in deterministic order.
///
/// Executes format, lint, build, verify, and privacy gates in that order.
/// Stops at the first failure (except verify which always runs).
/// Returns all results that were actually executed.
///
/// # Arguments
///
/// * `profile` - The verification profile containing gates to run
/// * `root` - The working directory for gate execution
/// * `base_sha` - The base commit SHA (used for logging/journaling)
/// * `bus` - Optional event bus for publishing gate events
///
/// # Returns
///
/// A vector of gate results in execution order.
/// Includes results only for gates that were actually run.
///
/// # Errors
///
/// Returns an error if a gate cannot be spawned (e.g., command not found).
/// A timeout, nonzero exit code, or signal do not produce an error; they are
/// captured in the [`GateResult`].
pub fn run_completion_set(
    profile: &Profile,
    root: &Path,
    _base_sha: &str,
    bus: Option<&Bus>,
) -> Result<Vec<GateResult>> {
    let mandatory_gates = [GateKind::Format, GateKind::Lint, GateKind::Build];
    let verify_gate = GateKind::Verify;
    let optional_gates = [GateKind::Privacy];

    let mut results = Vec::new();
    let mut should_continue = true;

    for gate_kind in &mandatory_gates {
        if let Some(gate) = profile.get(*gate_kind) {
            if !should_continue {
                break;
            }

            let result = run_gate(gate, root, bus)?;
            if !result.passed {
                should_continue = false;
            }
            results.push(result);
        }
    }

    if let Some(gate) = profile.get(verify_gate) {
        let result = run_gate(gate, root, bus)?;
        results.push(result);
    }

    if should_continue {
        for gate_kind in &optional_gates {
            if let Some(gate) = profile.get(*gate_kind) {
                let result = run_gate(gate, root, bus)?;
                results.push(result);
            }
        }
    }

    Ok(results)
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
    fn run_gate_kills_process_group_on_timeout() {
        let gate = Gate {
            kind: GateKind::Build,
            command: vec![
                "sh".to_string(),
                "-c".to_string(),
                "sleep 30 & sleep 30".to_string(),
            ],
            timeout_secs: 1,
            working_dir: None,
            env: BTreeMap::new(),
        };

        let result = run_gate(&gate, Path::new("."), None)
            .expect("run_gate should return a result even on timeout");

        assert_eq!(result.kind, GateKind::Build);
        assert!(!result.passed);
        assert!(result.timed_out);

        thread::sleep(Duration::from_millis(1500));

        let ps_output = Command::new("pgrep").arg("-f").arg("sleep 30").output();

        match ps_output {
            Ok(output) => {
                assert!(
                    output.stdout.is_empty(),
                    "no sleep processes should survive timeout"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // pgrep not found, skip the check
            }
            Err(e) => {
                panic!("unexpected error checking for sleep processes: {e}");
            }
        }
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

    #[test]
    fn parse_cargo_with_all_passing_tests() {
        let output = r"
running 306 tests

test result: ok. 306 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.55s
";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 306);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.ignored, 0);
        assert!(summary.failures.is_empty());
    }

    #[test]
    fn parse_cargo_with_some_failures() {
        let output = r"
running 10 tests

test result: FAILED. 8 passed; 2 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.23s

failures:

---- module::test_one stdout ----
thread 'module::test_one' panicked at 'assertion failed'

---- module::test_two stdout ----
thread 'module::test_two' panicked at 'expected value'

";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 8);
        assert_eq!(summary.failed, 2);
        assert_eq!(summary.ignored, 0);
        assert_eq!(summary.failures.len(), 2);
        assert!(
            summary
                .failures
                .contains(&"module::test_one stdout".to_string())
        );
        assert!(
            summary
                .failures
                .contains(&"module::test_two stdout".to_string())
        );
    }

    #[test]
    fn parse_cargo_with_ignored_tests() {
        let output = r"
running 50 tests

test result: ok. 45 passed; 0 failed; 5 ignored; 0 measured; 0 filtered out; finished in 5.00s
";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 45);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.ignored, 5);
        assert!(summary.failures.is_empty());
    }

    #[test]
    fn parse_cargo_with_empty_output() {
        let output = "";
        let summary = parse_cargo(output);
        assert!(summary.is_none());
    }

    #[test]
    fn parse_cargo_with_unrecognized_output() {
        let output = "some random output without test result line";
        let summary = parse_cargo(output);
        assert!(summary.is_none());
    }

    #[test]
    fn parse_cargo_with_zero_tests() {
        let output = r"
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 0);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.ignored, 0);
        assert!(summary.failures.is_empty());
    }

    #[test]
    fn parse_cargo_extracts_all_failure_names() {
        let output = r"
running 5 tests

test result: FAILED. 2 passed; 3 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.50s

failures:

---- tests::alpha stdout ----
panic message

---- tests::beta stdout ----
panic message

---- tests::gamma stdout ----
panic message

";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.failed, 3);
        assert_eq!(summary.failures.len(), 3);
        assert!(
            summary
                .failures
                .contains(&"tests::alpha stdout".to_string())
        );
        assert!(summary.failures.contains(&"tests::beta stdout".to_string()));
        assert!(
            summary
                .failures
                .contains(&"tests::gamma stdout".to_string())
        );
    }

    #[test]
    fn parse_cargo_with_real_nextest_output() {
        let output = r"
     Running unittests src/lib.rs (target/debug/deps/ktask_core-abc123)

running 306 tests

test result: ok. 306 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 30.55s
";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 306);
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn parse_cargo_handles_whitespace_variations() {
        let output = r"test result: ok.   100 passed;   0  failed;   2 ignored; 0 measured; 0 filtered out; finished in 1.00s";
        let summary = parse_cargo(output);
        assert!(summary.is_some());
        let summary = summary.unwrap();
        assert_eq!(summary.passed, 100);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.ignored, 2);
    }

    #[test]
    fn run_completion_set_runs_gates_in_order() {
        let mut config = Config::default();
        config.format_command = Some(vec!["echo".to_string(), "format".to_string()]);
        config.lint_command = Some(vec!["echo".to_string(), "lint".to_string()]);
        config.build_command = Some(vec!["echo".to_string(), "build".to_string()]);
        config.verify_command = Some(vec!["echo".to_string(), "verify".to_string()]);
        config.privacy_command = Some(vec!["echo".to_string(), "privacy".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        let results =
            run_completion_set(&profile, Path::new("."), "abc123", None).expect("should succeed");

        assert_eq!(results.len(), 5);
        assert_eq!(results[0].kind, GateKind::Format);
        assert_eq!(results[1].kind, GateKind::Lint);
        assert_eq!(results[2].kind, GateKind::Build);
        assert_eq!(results[3].kind, GateKind::Verify);
        assert_eq!(results[4].kind, GateKind::Privacy);
    }

    #[test]
    fn run_completion_set_stops_at_first_failure_before_verify() {
        let mut config = Config::default();
        config.format_command = Some(vec!["echo".to_string(), "format".to_string()]);
        config.lint_command = Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit 1".to_string(),
        ]);
        config.build_command = Some(vec!["echo".to_string(), "build".to_string()]);
        config.verify_command = Some(vec!["echo".to_string(), "verify".to_string()]);
        config.privacy_command = Some(vec!["echo".to_string(), "privacy".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        let results =
            run_completion_set(&profile, Path::new("."), "abc123", None).expect("should succeed");

        assert_eq!(results.len(), 3);
        assert_eq!(results[0].kind, GateKind::Format);
        assert!(results[0].passed);
        assert_eq!(results[1].kind, GateKind::Lint);
        assert!(!results[1].passed);
        assert_eq!(results[2].kind, GateKind::Verify);
        assert!(results[2].passed);
    }

    #[test]
    fn run_completion_set_runs_verify_even_if_earlier_gates_fail() {
        let mut config = Config::default();
        config.format_command = Some(vec![
            "sh".to_string(),
            "-c".to_string(),
            "exit 1".to_string(),
        ]);
        config.lint_command = Some(vec!["echo".to_string(), "lint".to_string()]);
        config.build_command = Some(vec!["echo".to_string(), "build".to_string()]);
        config.verify_command = Some(vec!["echo".to_string(), "verify".to_string()]);
        config.privacy_command = Some(vec!["echo".to_string(), "privacy".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        let results =
            run_completion_set(&profile, Path::new("."), "abc123", None).expect("should succeed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].kind, GateKind::Format);
        assert!(!results[0].passed);
        assert_eq!(results[1].kind, GateKind::Verify);
        assert!(results[1].passed);
    }

    #[test]
    fn run_completion_set_skips_missing_gates() {
        let mut config = Config::default();
        config.format_command = Some(vec!["echo".to_string(), "format".to_string()]);
        config.verify_command = Some(vec!["echo".to_string(), "verify".to_string()]);

        let profile = profile_from(&config).expect("profile_from should succeed");
        let results =
            run_completion_set(&profile, Path::new("."), "abc123", None).expect("should succeed");

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].kind, GateKind::Format);
        assert_eq!(results[1].kind, GateKind::Verify);
    }
}
