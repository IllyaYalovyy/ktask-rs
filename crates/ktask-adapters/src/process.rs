//! Starts real subprocesses: feeds stdin, captures stdout and stderr, and kills a command and
//! every process it started when it runs past its time limit — or when this process is asked
//! to stop while one is running.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;

use ktask_core::{CommandSpec, Commands, CommandsError, Exit, Output};
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// Commands, by starting real subprocesses, each in its own process group so that the whole
/// group can be killed at once.
#[derive(Debug, Clone, Copy)]
pub struct ProcessCommands;

/// What ended the wait for the child: it exited on its own, or this process was asked to
/// stop while it was still running.
enum Awaited {
    /// The child exited; this is [`std::process::Child::wait`]'s own result.
    Exited(std::io::Result<ExitStatus>),
    /// SIGTERM, SIGINT or SIGHUP arrived at this process.
    AskedToStop,
}

impl Commands for ProcessCommands {
    fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
        let fail =
            |cause: String| CommandsError::new(format!("cannot run {}: {cause}", spec.program));

        let mut child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A new process group, of which this child is the leader, so that a timeout can
            // kill it and every process it started together, however deep the descent.
            .process_group(0)
            .spawn()
            .map_err(|e| fail(e.to_string()))?;
        let pgid = i32::try_from(child.id()).unwrap_or(i32::MAX);

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| fail("the child has no standard input".to_owned()))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| fail("the child has no standard output".to_owned()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| fail("the child has no standard error".to_owned()))?;

        let input = spec.stdin.clone();
        let writer = thread::spawn(move || {
            let _ = stdin.write_all(&input);
        });
        let stdout_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let stderr_reader = thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });

        let mut signals = Signals::new([SIGTERM, SIGINT, SIGHUP])
            .map_err(|e| fail(format!("cannot watch for a termination signal: {e}")))?;
        let handle = signals.handle();

        let (sender, receiver) = mpsc::channel();
        let waiter_sender = sender.clone();
        let waiter = thread::spawn(move || {
            let status = child.wait();
            let _ = waiter_sender.send(Awaited::Exited(status));
        });
        let signal_watcher = thread::spawn(move || {
            if signals.forever().next().is_some() {
                let _ = sender.send(Awaited::AskedToStop);
            }
        });

        let status = match receiver.recv_timeout(spec.timeout) {
            Ok(Awaited::Exited(status)) => status,
            Ok(Awaited::AskedToStop) => {
                // The whole group, not just the child itself, so nothing it started is left
                // behind. A failure here means it is already gone.
                let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
                // Waited for, so it is reaped rather than left a zombie, but otherwise
                // ignored: nothing about this attempt is recorded. The next run finds its
                // task still `running` and accounts for the interruption. There is
                // deliberately no further cleanup past this point — the process is ending
                // regardless of what called it.
                let _ = receiver.recv();
                std::process::exit(1);
            }
            Err(RecvTimeoutError::Timeout) => {
                // The whole group, not just the child itself, so nothing it started is left
                // behind. A failure here means it is already gone.
                let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
                match receiver.recv() {
                    Ok(Awaited::Exited(status)) => status,
                    Ok(Awaited::AskedToStop) => std::process::exit(1),
                    Err(_) => {
                        return Err(fail("the wait thread stopped without a result".to_owned()));
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(fail("the wait thread stopped without a result".to_owned()));
            }
        };
        handle.close();
        let _ = signal_watcher.join();
        let _ = waiter.join();

        let stdout = stdout_reader
            .join()
            .map_err(|_| fail("the standard output reader panicked".to_owned()))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| fail("the standard error reader panicked".to_owned()))?;
        let _ = writer.join();

        let status: ExitStatus = status.map_err(|e| fail(format!("cannot wait for it: {e}")))?;
        let exit = match status.code() {
            Some(code) => Exit::Code(code),
            None => Exit::Killed,
        };
        Ok(Output {
            stdout,
            stderr,
            exit,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use tempfile::TempDir;

    use super::*;

    fn spec(args: Vec<&str>, dir: &Path, stdin: &[u8], timeout: Duration) -> CommandSpec {
        CommandSpec {
            program: "bash".to_owned(),
            args: args.into_iter().map(str::to_owned).collect(),
            dir: dir.to_owned(),
            stdin: stdin.to_vec(),
            timeout,
        }
    }

    #[test]
    fn stdout_stderr_and_the_exit_code_are_captured() {
        let dir = TempDir::new().unwrap();
        let output = ProcessCommands
            .run(&spec(
                vec!["-c", "echo out; echo err >&2; exit 7"],
                dir.path(),
                b"",
                Duration::from_secs(5),
            ))
            .unwrap();
        assert_eq!(output.stdout, b"out\n");
        assert_eq!(output.stderr, b"err\n");
        assert_eq!(output.exit, Exit::Code(7));
    }

    #[test]
    fn standard_input_is_fed_to_the_command_and_then_closed() {
        let dir = TempDir::new().unwrap();
        let output = ProcessCommands
            .run(&spec(
                vec!["-c", "cat"],
                dir.path(),
                b"hello",
                Duration::from_secs(5),
            ))
            .unwrap();
        assert_eq!(output.stdout, b"hello");
        assert_eq!(output.exit, Exit::Code(0));
    }

    #[test]
    fn the_command_starts_in_the_given_directory() {
        let dir = TempDir::new().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let output = ProcessCommands
            .run(&spec(
                vec!["-c", "pwd"],
                &canonical,
                b"",
                Duration::from_secs(5),
            ))
            .unwrap();
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim_end(),
            canonical.display().to_string()
        );
    }

    #[test]
    fn a_command_that_cannot_be_started_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let error = ProcessCommands
            .run(&CommandSpec {
                program: "there-is-no-such-program".to_owned(),
                args: vec![],
                dir: dir.path().to_owned(),
                stdin: vec![],
                timeout: Duration::from_secs(5),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("there-is-no-such-program"), "{error}");
    }

    #[test]
    fn a_command_past_its_time_limit_is_killed_along_with_every_process_it_started() {
        let dir = TempDir::new().unwrap();
        let pid_file = dir.path().join("grandchild.pid");
        let script = format!("sleep 30 & echo $! > {}; sleep 30", pid_file.display());
        let started = Instant::now();
        let output = ProcessCommands
            .run(&spec(
                vec!["-c", &script],
                dir.path(),
                b"",
                Duration::from_millis(200),
            ))
            .unwrap();
        assert_eq!(output.exit, Exit::Killed);
        // A generous ceiling: proves the wait ended with the kill, not with the full sleep.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );

        let pid: i32 = wait_for_file(&pid_file).trim().parse().unwrap();
        wait_until_not_running(pid);
    }

    #[test]
    fn a_command_that_finishes_within_its_time_limit_is_not_killed() {
        let dir = TempDir::new().unwrap();
        let output = ProcessCommands
            .run(&spec(
                vec!["-c", "echo quick"],
                dir.path(),
                b"",
                Duration::from_secs(5),
            ))
            .unwrap();
        assert_eq!(output.stdout, b"quick\n");
        assert_eq!(output.exit, Exit::Code(0));
    }

    /// Waits until `path` exists and is non-empty, for up to a few seconds.
    fn wait_for_file(path: &Path) -> String {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(content) = std::fs::read_to_string(path)
                && !content.trim().is_empty()
            {
                return content;
            }
            assert!(
                Instant::now() < deadline,
                "{} was never written",
                path.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits, for up to a few seconds, until process `pid` is no longer running: gone, or a
    /// zombie waiting for its new parent to reap it. A killed process's entry under `/proc`
    /// can briefly outlive the signal that ended it, so existence alone is not enough.
    fn wait_until_not_running(pid: i32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Err(_) => return,
                Ok(stat) => {
                    let state = stat
                        .split(')')
                        .next_back()
                        .and_then(|rest| rest.split_whitespace().next());
                    if state == Some("Z") {
                        return;
                    }
                }
            }
            assert!(Instant::now() < deadline, "process {pid} is still running");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
