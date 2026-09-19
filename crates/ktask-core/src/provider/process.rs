//! Process execution with streaming output and timeout management.
//!
//! Provides the `run_streaming` function for executing commands in isolated process groups
//! with support for idle and hard timeouts, stdin writing, and streamed output capture.

use crate::{Bus, Error, Outcome, Result};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

/// Execute a command with streaming output, stdin support, and dual timeouts.
///
/// Spawns the command in its own process group, optionally writing stdin,
/// and reads stdout/stderr on separate threads. Implements two timeouts:
/// - hard_timeout: absolute time limit from spawn
/// - idle_timeout: resets on every output chunk arrival
///
/// Kills the entire process group on timeout and returns the collected output.
///
/// # Arguments
///
/// * `cmd` - The command to execute, configured with arguments
/// * `stdin_data` - Optional stdin to write to the process
/// * `idle_timeout` - Time since last output before killing
/// * `hard_timeout` - Absolute time limit from spawn
/// * `bus` - Optional event bus for publishing output chunks
///
/// # Returns
///
/// An `Outcome` with exit code, stdout, stderr, and optional usage/session info.
/// On timeout, returns the partial output collected so far with exit code -1.
///
/// # Errors
///
/// Returns an error if the command cannot be spawned or if a critical I/O
/// operation fails. Timeout and non-zero exit codes do not produce errors.
pub fn run_streaming(
    cmd: &mut Command,
    stdin_data: Option<&str>,
    idle_timeout: Duration,
    hard_timeout: Duration,
    bus: Option<&Bus>,
) -> Result<Outcome> {
    let start = Instant::now();
    let _ = bus; // Published to bus if provided (for future use)

    // Configure the command for streaming I/O with isolated process group
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(if stdin_data.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .process_group(0);

    // Spawn the process
    let mut child = cmd.spawn().map_err(|e| Error::Provider {
        provider: "process".to_string(),
        detail: format!("Failed to spawn process: {e}"),
    })?;

    // Write stdin if provided
    if let Some(data) = stdin_data {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(data.as_bytes());
        }
    }

    // Extract stdout and stderr pipes
    let stdout_pipe = child.stdout.take().ok_or_else(|| Error::Provider {
        provider: "process".to_string(),
        detail: "Could not open stdout pipe".to_string(),
    })?;

    let stderr_pipe = child.stderr.take().ok_or_else(|| Error::Provider {
        provider: "process".to_string(),
        detail: "Could not open stderr pipe".to_string(),
    })?;

    // Shared buffers for output
    let stdout_buf = Arc::new(Mutex::new(String::new()));
    let stderr_buf = Arc::new(Mutex::new(String::new()));

    // Track last output time for idle timeout
    let last_output = Arc::new(Mutex::new(Instant::now()));

    // Reader thread for stdout
    let stdout_buf_clone = Arc::clone(&stdout_buf);
    let last_output_clone = Arc::clone(&last_output);
    let stdout_handle = thread::spawn(move || {
        use std::io::Read;
        let mut reader = stdout_pipe;
        let mut buf = [0; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(slice) = buf.get(..n) {
                        if let Ok(s) = std::str::from_utf8(slice) {
                            if let Ok(mut output) = stdout_buf_clone.lock() {
                                output.push_str(s);
                            }
                            // Reset idle timeout on output
                            if let Ok(mut last) = last_output_clone.lock() {
                                *last = Instant::now();
                            }
                        }
                    }
                }
            }
        }
    });

    // Reader thread for stderr
    let stderr_buf_clone = Arc::clone(&stderr_buf);
    let last_output_clone = Arc::clone(&last_output);
    let stderr_handle = thread::spawn(move || {
        use std::io::Read;
        let mut reader = stderr_pipe;
        let mut buf = [0; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Some(slice) = buf.get(..n) {
                        if let Ok(s) = std::str::from_utf8(slice) {
                            if let Ok(mut output) = stderr_buf_clone.lock() {
                                output.push_str(s);
                            }
                            // Reset idle timeout on output
                            if let Ok(mut last) = last_output_clone.lock() {
                                *last = Instant::now();
                            }
                        }
                    }
                }
            }
        }
    });

    // Store child PID for process group killing
    let child_pid = child.id();

    // Monitor for timeout in a separate thread
    let last_output_clone = Arc::clone(&last_output);
    let (tx_timeout, rx_timeout) = mpsc::channel::<()>();
    let timeout_thread = thread::spawn(move || {
        loop {
            let total_elapsed = start.elapsed();
            if total_elapsed >= hard_timeout {
                let _ = tx_timeout.send(());
                break;
            }

            if let Ok(last) = last_output_clone.lock() {
                if last.elapsed() >= idle_timeout {
                    let _ = tx_timeout.send(());
                    break;
                }
            }

            thread::sleep(Duration::from_millis(100));
        }
    });

    // Wait for child to exit or timeout
    let (tx_wait, rx_wait) = mpsc::channel();
    let wait_thread = thread::spawn(move || {
        let status = child.wait();
        let _ = tx_wait.send(status);
    });

    // Wait for either child exit or timeout
    let mut timed_out = false;
    let mut exit_status = None;
    loop {
        // Check timeout
        if rx_timeout.try_recv().is_ok() {
            timed_out = true;
            kill_process_group(child_pid);
            break;
        }

        // Check child exit
        if let Ok(status) = rx_wait.try_recv() {
            exit_status = Some(status);
            break;
        }

        thread::sleep(Duration::from_millis(100));
    }

    // Wait for threads to finish
    let _ = wait_thread.join();
    let _ = timeout_thread.join();

    // Wait for reader threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Collect output
    let stdout = stdout_buf.lock().map(|s| s.clone()).unwrap_or_default();
    let stderr = stderr_buf.lock().map(|s| s.clone()).unwrap_or_default();

    // Extract exit code
    let exit_code = if timed_out {
        -1
    } else {
        match exit_status {
            Some(Ok(status)) => status.code().unwrap_or(-1),
            _ => -1,
        }
    };

    Ok(Outcome {
        exit_code,
        stdout,
        stderr,
        usage: None,
        session_id: None,
    })
}

/// Kill an entire process group.
///
/// Sends SIGTERM followed by SIGKILL to ensure the process group is terminated.
fn kill_process_group(child_pid: u32) {
    let pgid = Pid::from_raw(i32::try_from(child_pid).unwrap_or(1));
    let _ = kill(pgid, Signal::SIGTERM);
    thread::sleep(Duration::from_millis(100));
    let _ = kill(pgid, Signal::SIGKILL);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_streaming_successful_command() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo 'hello' && echo 'world' >&2");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("hello"));
        assert!(result.stderr.contains("world"));
    }

    #[test]
    fn run_streaming_with_stdin() {
        let mut cmd = Command::new("cat");

        let result = run_streaming(
            &mut cmd,
            Some("test input"),
            Duration::from_secs(5),
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("test input"));
    }

    #[test]
    fn run_streaming_idle_timeout_kills_process_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 1 & sleep 1");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_millis(200),
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should return result");

        assert_eq!(result.exit_code, -1); // Timeout exit code
    }

    #[test]
    fn run_streaming_hard_timeout_kills_process_group() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 1 & sleep 1");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(10),
            Duration::from_millis(200),
            None,
        )
        .expect("run_streaming should return result");

        assert_eq!(result.exit_code, -1); // Timeout exit code
    }

    #[test]
    fn run_streaming_idle_timer_resets_on_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("for i in 1 2 3; do echo $i; sleep 0.2; done");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(1), // idle timeout longer than gaps
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.contains("1"));
        assert!(result.stdout.contains("2"));
        assert!(result.stdout.contains("3"));
    }

    #[test]
    fn run_streaming_captures_nonzero_exit() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 42");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should succeed");

        assert_eq!(result.exit_code, 42);
    }

    #[test]
    fn run_streaming_empty_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("true");

        let result = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(10),
            None,
        )
        .expect("run_streaming should succeed");

        assert_eq!(result.exit_code, 0);
        assert!(result.stdout.is_empty());
        assert!(result.stderr.is_empty());
    }
}
