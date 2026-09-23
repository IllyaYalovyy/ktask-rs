//! [`run_streaming`]: the process-supervision primitive every real provider
//! adapter (Claude, Codex, ...) is built on, per `VISION.md` §12.
//!
//! It spawns a command as the leader of its own process group, writes an
//! optional prompt to its stdin, and reads stdout and stderr on their own
//! threads so a provider that fills one pipe cannot stall on the other. A
//! chunk is published to `bus` the moment it is read, in the order it
//! arrived relative to chunks from the other stream — not batched until
//! exit. Two independent clocks bound the run: `idle_timeout`, which resets
//! on every chunk from either stream, and `hard_timeout`, an absolute cap
//! from the moment the process was spawned. Either firing kills the whole
//! process group, so a provider that forks a helper cannot outlive the
//! timeout that ended it.
//!
//! The process-group and signal mechanics mirror [`crate::gate::run_gate`]'s
//! (`process_group(0)` at spawn, `killpg` at the end), because both solve
//! the same problem: a subprocess this crate does not control the internals
//! of must still be fully reapable on a budget.

use std::io::{self, BufRead, BufReader, Read, Write as _};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::control;
use crate::runner::interrupt_flag;
use crate::{AttemptId, Bus, Error, Event, EventKind, EventSeq, Result, Stream};

use super::Outcome;

/// How often the collector loop wakes up to check the idle and hard
/// timeouts even when no output has arrived.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// How long a killed process's pipes are still drained after the process
/// group has been signalled, before the run gives up on them and reports
/// what it has. Matches [`crate::gate`]'s own grace window.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// How long [`spawn_retrying_busy`] keeps retrying an `ExecutableFileBusy`
/// spawn before giving up and surfacing the error.
const BUSY_RETRY_WINDOW: Duration = Duration::from_millis(200);

/// How long [`spawn_retrying_busy`] waits between retries.
const BUSY_RETRY_INTERVAL: Duration = Duration::from_millis(5);

/// Spawns `cmd`, retrying for up to [`BUSY_RETRY_WINDOW`] if the kernel
/// reports `ExecutableFileBusy` — a transient condition where the target is
/// still open for writing elsewhere (another writer racing the exec, an
/// antivirus scan, a filesystem finishing a delayed close) rather than a
/// real failure to run the command.
fn spawn_retrying_busy(cmd: &mut Command) -> io::Result<Child> {
    let deadline = Instant::now() + BUSY_RETRY_WINDOW;
    loop {
        match cmd.spawn() {
            Err(err)
                if err.kind() == io::ErrorKind::ExecutableFileBusy && Instant::now() < deadline =>
            {
                thread::sleep(BUSY_RETRY_INTERVAL);
            }
            result => return result,
        }
    }
}

/// One chunk of output read from the child's stdout or stderr, tagged with
/// the pipe it arrived on.
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

/// Writes `prompt` to `stdin` on its own thread, then drops it so the child
/// sees EOF. Runs off the main thread because a large prompt could fill the
/// pipe buffer before a provider that has not started reading yet, which
/// would otherwise deadlock a synchronous write against the reader threads
/// this function's caller also depends on.
fn spawn_stdin_writer(mut stdin: std::process::ChildStdin, prompt: String) {
    thread::spawn(move || {
        // Best-effort: a provider that exits before reading its prompt
        // (an early failure, a `--help` invocation) closes its end of the
        // pipe and this write fails; the process's own exit code and
        // stderr already say why, so there is nothing more to report here.
        let _ = stdin.write_all(prompt.as_bytes());
    });
}

/// Puts the spawned child in a new process group led by itself, so every
/// descendant it forks shares one group id and can be signalled together.
#[cfg(unix)]
fn new_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
}

/// No process groups outside Unix; the child is signalled alone.
#[cfg(not(unix))]
fn new_process_group(_command: &mut Command) {}

/// Kills every process in `child`'s process group, not just `child` itself
/// — since [`new_process_group`] made `child`'s pid its own group id, a
/// grandchild it spawned (a shell pipeline, a backgrounded worker) has no
/// other process reachable from here that can reap it.
fn kill_group(child: &mut Child) {
    #[cfg(unix)]
    {
        use nix::sys::signal::{self, Signal};
        use nix::unistd::Pid;

        let pgid = Pid::from_raw(i32::try_from(child.id()).unwrap_or(i32::MAX));
        // Best-effort: the group may already be gone, which is the goal,
        // not an error.
        let _ = signal::killpg(pgid, Signal::SIGKILL);
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

/// Publishes one chunk of provider output to `bus` as an
/// [`EventKind::AgentOutput`], tagged with the next value of `seq`.
///
/// `run_streaming` is not given a task or attempt id — it is the process
/// primitive a provider adapter is built on, invoked before any attempt
/// bookkeeping exists at this layer — so it publishes with `task_id: None`
/// and a placeholder `AttemptId` of `0`, matching the precedent
/// [`crate::provider::dummy::Dummy`] already sets for a chunk with no known
/// attempt.
fn publish_chunk(bus: &Bus, seq: &mut u64, chunk: &Chunk) {
    bus.publish(Event {
        seq: EventSeq::new(*seq),
        ts: time::OffsetDateTime::UNIX_EPOCH,
        task_id: None,
        kind: EventKind::AgentOutput {
            attempt: AttemptId::new(0),
            stream: chunk.stream,
            text: chunk.text.clone(),
        },
    });
    *seq += 1;
}

/// Runs `cmd` as a subprocess, writing `stdin_data` to it if given,
/// streaming its stdout and stderr to `bus` as they arrive, and returns
/// what it produced.
///
/// `cmd` is spawned from its own argv, as the leader of a new process
/// group, with both output pipes read on dedicated threads so a process
/// that fills one cannot stall because nothing is emptying the other.
/// `idle_timeout` resets on every chunk read from either stream;
/// `hard_timeout` is an absolute cap from the moment `cmd` was spawned.
/// Either elapsing kills the whole process group with `SIGKILL` so no
/// descendant survives, and turns this call into an `Err`: a process that
/// had to be killed did not produce a result this function can vouch for,
/// matching [`super::Provider::invoke`]'s own contract that only a
/// completed run is an `Ok` outcome.
///
/// The same loop that checks the two timeouts also checks this process's
/// `SIGINT` flag on every tick: a signal caught while this is in flight
/// kills the process group exactly like a timeout would, well within one
/// poll interval of the signal arriving, rather than leaving it running
/// until the caller's next chance to notice between calls.
///
/// # Errors
///
/// Returns [`Error::Provider`] when `cmd` could not be spawned, when either
/// output pipe was not piped, or when `idle_timeout` or `hard_timeout`
/// elapsed, or this process caught `SIGINT`, and the process group was
/// killed.
pub fn run_streaming(
    cmd: &mut Command,
    stdin_data: Option<&str>,
    idle_timeout: Duration,
    hard_timeout: Duration,
    bus: Option<&Bus>,
) -> Result<Outcome> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let provider_error = |detail: String| Error::Provider {
        provider: program.clone(),
        detail,
    };

    cmd.stdin(if stdin_data.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    new_process_group(cmd);

    let mut child = spawn_retrying_busy(cmd)
        .map_err(|err| provider_error(format!("could not start `{program}`: {err}")))?;
    let started = Instant::now();

    if let Some(prompt) = stdin_data {
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| provider_error("the spawned process's stdin was not piped".into()))?;
        spawn_stdin_writer(stdin, prompt.to_string());
    }
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| provider_error("the spawned process's stdout was not piped".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| provider_error("the spawned process's stderr was not piped".into()))?;

    let (tx, rx) = mpsc::channel();
    spawn_reader(stdout, Stream::Stdout, tx.clone());
    spawn_reader(stderr, Stream::Stderr, tx);

    let mut stdout_buf = String::new();
    let mut stderr_buf = String::new();
    let mut last_activity = started;
    let mut seq: u64 = 1;
    let mut timeout_detail: Option<String> = None;
    let mut kill_deadline = None;

    loop {
        match rx.recv_timeout(POLL_INTERVAL) {
            Ok(chunk) => {
                last_activity = Instant::now();
                match chunk.stream {
                    Stream::Stdout => stdout_buf.push_str(&chunk.text),
                    Stream::Stderr => stderr_buf.push_str(&chunk.text),
                }
                if let Some(bus) = bus {
                    publish_chunk(bus, &mut seq, &chunk);
                }
            }
            // Both readers reached the end of their pipe: everything the
            // process wrote has already been handed over and kept.
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }

        if timeout_detail.is_none() {
            if interrupt_flag().load(Ordering::SeqCst) {
                timeout_detail = Some("interrupted by SIGINT".to_string());
            } else if control::stop_requested() {
                timeout_detail = Some("interrupted by a `ktask-rs` control request".to_string());
            } else if started.elapsed() >= hard_timeout {
                timeout_detail = Some(format!("hard timeout of {hard_timeout:?} exceeded"));
            } else if last_activity.elapsed() >= idle_timeout {
                timeout_detail = Some(format!(
                    "idle timeout of {idle_timeout:?} exceeded with no output"
                ));
            }
            if timeout_detail.is_some() {
                // Best-effort: the group may have exited in the instant
                // between the deadline and this signal, which `wait` below
                // reports on its own terms either way.
                kill_group(&mut child);
                kill_deadline = Some(Instant::now() + KILL_GRACE);
            }
        }
        if kill_deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            break;
        }
    }

    let status = child
        .wait()
        .map_err(|err| provider_error(format!("could not wait for `{program}`: {err}")))?;

    if let Some(detail) = timeout_detail {
        return Err(provider_error(detail));
    }

    Ok(Outcome {
        exit_code: status.code().unwrap_or(-1),
        stdout: stdout_buf,
        stderr: stderr_buf,
        usage: None,
        session_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Project, TaskId};
    use std::fs;
    use std::path::Path;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg(script);
        cmd
    }

    #[test]
    fn a_completed_commands_exit_code_and_output_are_returned() {
        let mut cmd = sh("printf 'out\\n'; printf 'err\\n' 1>&2; exit 3");

        let outcome = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        )
        .expect("run");

        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.stdout, "out\n");
        assert_eq!(outcome.stderr, "err\n");
    }

    #[test]
    fn stdin_data_reaches_the_child_process_stdin() {
        let mut cmd = Command::new("cat");

        let outcome = run_streaming(
            &mut cmd,
            Some("hello from the caller"),
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        )
        .expect("run");

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "hello from the caller");
    }

    #[test]
    fn no_stdin_data_leaves_stdin_closed_rather_than_hanging() {
        // `cat` with a closed stdin reads EOF immediately and exits; if
        // `run_streaming` left stdin open with nothing to write, this would
        // hang until the timeout instead.
        let mut cmd = Command::new("cat");

        let outcome = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        )
        .expect("run");

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "");
    }

    #[test]
    fn stdout_and_stderr_chunks_are_published_to_the_bus_in_arrival_order() {
        // Sleeps between writes force each chunk to be read (and so
        // published) as a distinct event in real arrival order, rather
        // than letting the two pipes' contents race.
        let mut cmd =
            sh("printf 'out1\\n'; sleep 0.1; printf 'err1\\n' 1>&2; sleep 0.1; printf 'out2\\n'");
        let bus = Bus::new(16);
        let mut sub = bus.subscribe();

        let outcome = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            Some(&bus),
        )
        .expect("run");
        assert_eq!(outcome.exit_code, 0);

        let (events, dropped) = sub.drain();
        assert_eq!(dropped, 0);
        let observed: Vec<(Stream, String)> = events
            .into_iter()
            .map(|event| match event.kind {
                EventKind::AgentOutput { stream, text, .. } => (stream, text),
                other => panic!("expected AgentOutput, got {other:?}"),
            })
            .collect();

        assert_eq!(
            observed,
            vec![
                (Stream::Stdout, "out1\n".to_string()),
                (Stream::Stderr, "err1\n".to_string()),
                (Stream::Stdout, "out2\n".to_string()),
            ]
        );
    }

    #[test]
    fn no_bus_means_no_publishing_but_output_is_still_captured() {
        let mut cmd = sh("printf 'quiet\\n'");

        let outcome = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        )
        .expect("run");

        assert_eq!(outcome.stdout, "quiet\n");
    }

    #[test]
    fn idle_watchdog_survives_a_session_that_keeps_talking() {
        // Each tick arrives well inside the idle timeout, but the run as a
        // whole comfortably outlasts it; only a reset on every chunk
        // explains this completing instead of being killed.
        let idle_timeout = Duration::from_millis(300);
        let mut cmd = sh(
            "i=0; while [ $i -lt 5 ]; do printf 'tick%d\\n' \"$i\"; sleep 0.1; \
             i=$((i + 1)); done",
        );

        let outcome = run_streaming(&mut cmd, None, idle_timeout, Duration::from_secs(5), None)
            .expect("a session that keeps talking before every idle deadline is not killed");

        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stdout, "tick0\ntick1\ntick2\ntick3\ntick4\n");
    }

    #[test]
    fn idle_watchdog_kills_a_silent_session() {
        let idle_timeout = Duration::from_millis(150);
        let mut cmd = sh("printf 'once\\n'; sleep 30");

        let started = Instant::now();
        let err = run_streaming(&mut cmd, None, idle_timeout, Duration::from_secs(30), None)
            .expect_err("a session that goes silent past the idle timeout must be killed");
        let elapsed = started.elapsed();

        assert!(
            elapsed >= idle_timeout,
            "not killed before the session had actually been silent for the idle timeout: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "killed promptly rather than left to run to the hard timeout: {elapsed:?}"
        );
        let message = err.to_string();
        assert!(message.contains("idle timeout"), "{message}");
        assert!(
            message.contains(&format!("{idle_timeout:?}")),
            "error reports the silence duration that was exceeded: {message}"
        );
    }

    #[test]
    fn a_command_that_keeps_producing_output_is_still_killed_at_the_hard_timeout() {
        let mut cmd = sh("while true; do printf 'x\\n'; sleep 0.05; done");

        let started = Instant::now();
        let err = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(30),
            Duration::from_millis(150),
            None,
        )
        .expect_err("must be killed for exceeding the hard timeout despite steady output");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed promptly"
        );
        assert!(err.to_string().contains("hard timeout"), "{err}");
    }

    #[test]
    fn no_grandchild_outlives_an_idle_timeout_kill() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pid_file = dir.path().join("grandchild.pid");
        let mut cmd = sh(&format!(
            "sleep 30 & echo $! > {} ; wait",
            pid_file.display()
        ));

        let err = run_streaming(
            &mut cmd,
            None,
            Duration::from_millis(300),
            Duration::from_secs(30),
            None,
        )
        .expect_err("the parent never produces output, so it must idle out");
        assert!(err.to_string().contains("idle timeout"));

        let pid_text =
            fs::read_to_string(&pid_file).expect("grandchild pid was written before the kill");
        let grandchild_pid: u32 = pid_text.trim().parse().expect("pid file holds a pid");

        let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(grandchild_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !alive(grandchild_pid),
            "grandchild pid {grandchild_pid} outlived the process group it was part of"
        );
    }

    #[test]
    fn a_control_request_kills_the_process_group_promptly_and_leaves_no_grandchild() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project {
            root: dir.path().join("root"),
            id: "process-test".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        let task = TaskId::new(1);
        let pid_file = dir.path().join("grandchild.pid");
        let mut cmd = sh(&format!(
            "sleep 30 & echo $! > {} ; printf 'started\\n'; wait",
            pid_file.display()
        ));

        // The operator's `ktask-rs interrupt`, sent from another thread
        // (standing in for another terminal) once the command has started.
        let sender_project = project.clone();
        let sender_pid_file = pid_file.clone();
        let sender = thread::spawn(move || {
            while !sender_pid_file.exists() {
                thread::sleep(Duration::from_millis(5));
            }
            crate::send(&sender_project, crate::Request::Interrupt).expect("send");
        });

        let started = Instant::now();
        let watch = control::watch(&project, task);
        let err = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(30),
            Duration::from_secs(30),
            None,
        )
        .expect_err("a control request must end the run");
        drop(watch);
        sender.join().expect("sender thread");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "killed promptly, not left to a timeout: {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("control request"), "{err}");

        let pid_text = fs::read_to_string(&pid_file).expect("grandchild pid was written");
        let grandchild_pid: u32 = pid_text.trim().parse().expect("pid file holds a pid");
        let alive = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
        let deadline = Instant::now() + Duration::from_secs(3);
        while alive(grandchild_pid) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !alive(grandchild_pid),
            "grandchild pid {grandchild_pid} outlived the request that should have killed its group"
        );
    }

    #[test]
    fn a_request_for_another_task_or_a_pause_does_not_end_the_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project {
            root: dir.path().join("root"),
            id: "process-test".to_string(),
            state_dir: dir.path().to_path_buf(),
        };
        crate::send(&project, crate::Request::Pause).expect("pause");
        crate::send(&project, crate::Request::Cancel(TaskId::new(2))).expect("cancel");
        let mut cmd = sh("sleep 0.2; printf 'finished\\n'");

        let watch = control::watch(&project, TaskId::new(1));
        let outcome = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(5),
            Duration::from_secs(5),
            None,
        )
        .expect("neither request is for this run");
        drop(watch);

        assert_eq!(outcome.stdout, "finished\n");
    }

    #[test]
    fn a_transient_executable_file_busy_is_retried_until_the_writer_closes() {
        // The kernel refuses to exec a file that is still open for writing
        // by anyone (execve(2): ETXTBSY) — deterministic, not a race. That
        // models what a real writer racing this spawn (or a filesystem
        // finishing a delayed close) looks like from here: the first
        // `spawn` must fail, and `spawn_retrying_busy` must succeed once
        // the writer goes away.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("busy.sh");
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write script");
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&path).expect("metadata").permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&path, perms).expect("chmod script");
        }

        let held_open = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open script for writing");
        assert_eq!(
            Command::new(&path).spawn().unwrap_err().kind(),
            io::ErrorKind::ExecutableFileBusy,
            "a plain spawn must fail while the script is still open for writing"
        );

        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            drop(held_open);
        });

        let mut child = spawn_retrying_busy(&mut Command::new(&path))
            .expect("spawn_retrying_busy must retry past the transient busy error");
        let status = child.wait().expect("wait for retried child");
        releaser.join().expect("releaser thread");

        assert!(status.success());
    }

    #[test]
    fn a_nonexistent_program_is_a_provider_error() {
        let mut cmd = Command::new("ktask-process-test-nonexistent-binary");

        let err = run_streaming(
            &mut cmd,
            None,
            Duration::from_secs(1),
            Duration::from_secs(1),
            None,
        )
        .expect_err("spawning a nonexistent program must fail");

        assert!(
            err.to_string()
                .contains("ktask-process-test-nonexistent-binary")
        );
    }
}
