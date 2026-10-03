//! Starts real subprocesses: feeds stdin, captures stdout and stderr, and kills a command and
//! every process it started when it runs past its time limit, when this process is asked to
//! stop (`SIGINT`, `SIGTERM`, `SIGHUP`) while one is running, or — even when this process is
//! killed outright, with no chance to run any code of its own — the moment the kernel notices
//! it is gone, through [`exec_tied_to_parent`] and [`kill_group_if_orphaned`].

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use ktask_core::{CommandSpec, Commands, CommandsError, Exit, Output};
use nix::fcntl::OFlag;
use nix::sys::prctl;
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getppid, pipe2};
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM};
use signal_hook::iterator::Signals;

/// Commands, by starting real subprocesses, each in its own process group so that the whole
/// group can be killed at once. Every child's `PATH` gets the running `ktask-rs` binary's own
/// directory prepended, ahead of whatever is already there, so a command that calls back into
/// `ktask-rs` by name — a provider's script reporting an attempt's outcome, say — reaches the
/// one that is running, not some other one found first on an inherited `PATH`.
#[derive(Debug, Clone, Copy)]
pub struct ProcessCommands;

/// The `argv[1]` that tells the `ktask-rs` binary to become [`exec_tied_to_parent`] instead
/// of running its usual command line: `ktask-rs <MARKER> <parent-pid> <program> [args...]`.
/// Every command [`ProcessCommands`] starts goes through this first, so the program it really
/// wants never outlives this process even when it is killed outright. Never typed by a
/// person — chosen so it collides with nothing a real command line would ever pass as its
/// first argument — so `ktask-cli`'s `main` checks for it before its normal argument parsing
/// even starts, and the rest of the CLI's grammar and `--help` never mention it.
pub const EXEC_TIED_TO_PARENT_MARKER: &str = "__ktask-rs-exec-tied-to-parent__";

/// Sets this process's parent-death signal to `SIGKILL`, tying its life to `parent_pid` at
/// the kernel level, then execs `program` with `args`, replacing this process entirely — the
/// far side of [`EXEC_TIED_TO_PARENT_MARKER`], run by `ktask-cli`'s `main`. Returns only when
/// something failed — the death signal could not be set, `parent_pid` is no longer this
/// process's parent (it already died in the narrow window before this ran, so nothing should
/// run unsupervised on its behalf), or `program` could not be started — since a successful
/// exec never returns.
///
/// Unlike [`std::os::unix::process::CommandExt::pre_exec`], this needs no `unsafe`: it runs
/// ordinary, safe syscalls in a process of its own that has already fully exec'd into
/// `ktask-rs`, rather than inside a fork of a process that might have more than one thread.
#[must_use]
pub fn exec_tied_to_parent(parent_pid: u32, program: &str, args: &[String]) -> io::Error {
    let expected_parent = Pid::from_raw(i32::try_from(parent_pid).unwrap_or(i32::MAX));
    if let Err(errno) = prctl::set_pdeathsig(Signal::SIGKILL) {
        return io::Error::from(errno);
    }
    if getppid() != expected_parent {
        return io::Error::other(
            "the process that started this one is already gone; refusing to run unsupervised",
        );
    }
    Command::new(program).args(args).exec()
}

/// The `argv[1]` that tells the `ktask-rs` binary to become [`kill_group_if_orphaned`]:
/// `ktask-rs <MARKER> <pgid>`. [`EXEC_TIED_TO_PARENT_MARKER`] only ties the single process a
/// command starts to [`ProcessCommands`]'s own death — whatever that process goes on to
/// start of its own, in the foreground or the background, is never tied to anything and
/// survives it. This is [`ProcessCommands`]'s guard against that: never typed by a person,
/// for the same reason [`EXEC_TIED_TO_PARENT_MARKER`] is not.
pub const KILL_GROUP_IF_ORPHANED_MARKER: &str = "__ktask-rs-kill-group-if-orphaned__";

/// Blocks until every process holding the write end of the pipe behind its own standard
/// input has closed its copy, then kills process group `pgid` — the far side of
/// [`KILL_GROUP_IF_ORPHANED_MARKER`], run by `ktask-cli`'s `main`. [`ProcessCommands`] keeps
/// one such copy open for as long as it is around to kill `pgid` itself when the command it
/// started ends; if it stops for any reason before that — even a `SIGKILL` it never had a
/// chance to react to — the kernel closes its copy the same as any other file descriptor,
/// this notices, and `pgid` — everything the command started, however deep — goes with it.
pub fn kill_group_if_orphaned(pgid: i32) {
    let _ = io::copy(&mut io::stdin(), &mut io::sink());
    let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
}

/// Keeps a command's whole process group from outliving [`ProcessCommands::run`], even when
/// it is killed outright: the write end of a pipe, held open here for as long as `run_spawned`
/// is still around to end the command itself, and the [`KILL_GROUP_IF_ORPHANED_MARKER`]
/// process watching its read end.
struct OrphanGuard {
    write_end: File,
    watcher: std::process::Child,
}

impl OrphanGuard {
    /// Spawns the watcher for `pgid`. `exe` must be the real `ktask-rs` binary — the one
    /// [`ProcessCommands::run`] already found for its own command — so it recognises
    /// [`KILL_GROUP_IF_ORPHANED_MARKER`].
    fn spawn(
        exe: &std::path::Path,
        pgid: u32,
        fail: &impl Fn(String) -> CommandsError,
    ) -> Result<Self, CommandsError> {
        // `O_CLOEXEC` on both ends: neither must ever reach the command's own process tree,
        // or a copy it holds would keep the write end open no matter what becomes of this
        // one, and the watcher would then wait forever for a close that never comes.
        let (read_end, write_end) = pipe2(OFlag::O_CLOEXEC).map_err(|e| {
            fail(format!(
                "cannot open a pipe to guard its process group: {e}"
            ))
        })?;
        let watcher = Command::new(exe)
            .arg(KILL_GROUP_IF_ORPHANED_MARKER)
            .arg(pgid.to_string())
            .stdin(Stdio::from(File::from(read_end)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| fail(format!("cannot start its process group's guard: {e}")))?;
        Ok(Self {
            write_end: File::from(write_end),
            watcher,
        })
    }

    /// Tells the watcher its job is over — [`run_spawned`] has already ended the command's
    /// process group itself, or never needed to — and waits for it to go, so it never
    /// outlives its one command as a zombie through a long `run`'s many attempts.
    fn stand_down(mut self) {
        let _ = self.watcher.kill();
        let _ = self.watcher.wait();
        drop(self.write_end);
    }
}

/// The current process's `PATH`, with the directory of the running binary put first.
fn path_with_own_binary_first() -> Result<OsString, CommandsError> {
    let exe = std::env::current_exe()
        .map_err(|e| CommandsError::new(format!("cannot find the running binary: {e}")))?;
    let dir = exe.parent().ok_or_else(|| {
        CommandsError::new(format!(
            "the running binary {} has no parent directory",
            exe.display()
        ))
    })?;
    let mut dirs = vec![dir.to_path_buf()];
    if let Some(existing) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&existing));
    }
    std::env::join_paths(dirs).map_err(|e| CommandsError::new(format!("cannot build PATH: {e}")))
}

/// What ended the wait for the child: it exited on its own, or this process was asked to
/// stop while it was still running.
enum Awaited {
    /// The child exited; this is [`std::process::Child::wait`]'s own result.
    Exited(io::Result<ExitStatus>),
    /// SIGTERM, SIGINT or SIGHUP arrived at this process.
    AskedToStop,
}

/// How the child's wait ended, before it is turned into an [`Exit`].
enum Ended {
    /// It exited on its own, or was reaped after the timeout killed it.
    Exited(io::Result<ExitStatus>),
    /// It ran past its time limit and was killed along with everything it started.
    TimedOut,
    /// This process was asked to stop while it was still running, and it was killed along
    /// with everything it started.
    Interrupted,
}

impl Commands for ProcessCommands {
    fn run(&self, spec: &CommandSpec) -> Result<Output, CommandsError> {
        let fail =
            |cause: String| CommandsError::new(format!("cannot run {}: {cause}", spec.program));

        let path = path_with_own_binary_first()?;
        let exe = std::env::current_exe()
            .map_err(|e| fail(format!("cannot find the running binary: {e}")))?;
        // The immediate child is `ktask-rs` itself, told to become `exec_tied_to_parent`: it
        // ties itself to this process before exec'ing into `spec.program`, so that program
        // never outlives this one, even when this one is killed outright. Since `exec`
        // replaces the process image without forking again, this adds no real process to the
        // tree: the pid spawned here is the pid `spec.program` itself ends up running as.
        let child = Command::new(&exe)
            .arg(EXEC_TIED_TO_PARENT_MARKER)
            .arg(std::process::id().to_string())
            .arg(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.dir)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A new process group, of which this child is the leader, so that a timeout can
            // kill it and every process it started together, however deep the descent.
            .process_group(0)
            .spawn()
            .map_err(|e| fail(e.to_string()))?;
        // Guards against everything `child` itself goes on to start — `EXEC_TIED_TO_PARENT_MARKER`
        // alone only ties `child` to this process; a background job or a later command in its
        // own script is tied to nothing and would survive this process being killed outright.
        let guard = OrphanGuard::spawn(&exe, child.id(), &fail)?;
        run_spawned(child, spec, fail, Some(guard))
    }
}

/// Waits for `child` (already spawned as the leader of its own process group) to finish or
/// run past `spec.timeout`, feeding `spec.stdin` and capturing its output; kills the whole
/// process group on a timeout, or when this process is asked to stop while `child` is still
/// running. `guard`, when given, is stood down the moment that outcome is known, its own
/// watcher process now the last line of defence against this process itself being killed
/// outright before it gets here. Shared by [`ProcessCommands::run`] and this module's own
/// tests, which spawn `child` directly against `bash` rather than through
/// [`EXEC_TIED_TO_PARENT_MARKER`] and pass no `guard`, since neither depends on how `child`
/// came to exist — only [`ProcessCommands::run`] itself needs the real, marker-aware
/// `ktask-rs` binary both rely on, and that is proven through the real binary instead, in
/// `ktask-cli`'s own end-to-end tests.
fn run_spawned(
    mut child: std::process::Child,
    spec: &CommandSpec,
    fail: impl Fn(String) -> CommandsError,
    guard: Option<OrphanGuard>,
) -> Result<Output, CommandsError> {
    let pgid = i32::try_from(child.id()).unwrap_or(i32::MAX);
    let (stdin, stdout, stderr) = take_pipes(&mut child, &fail)?;
    let (writer, stdout_reader, stderr_reader) = spawn_io_threads(
        stdin,
        stdout,
        stderr,
        spec.stdin.clone(),
        spec.output_path.as_deref(),
    )?;

    let ended = wait_for_child(child, pgid, spec.timeout, &fail)?;
    // The command has ended, one way or another, with this process very much still able to
    // run code of its own: its watcher, if it has one, is no longer needed to do this job in
    // its place.
    if let Some(guard) = guard {
        guard.stand_down();
    }

    let stdout = stdout_reader
        .join()
        .map_err(|_| fail("the standard output reader panicked".to_owned()))?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| fail("the standard error reader panicked".to_owned()))?;
    let _ = writer.join();

    Ok(Output {
        stdout,
        stderr,
        exit: exit_from(ended, &fail)?,
    })
}

/// Takes `child`'s standard input, output and error out of it, so they can be handed to their
/// own threads.
fn take_pipes(
    child: &mut std::process::Child,
    fail: &impl Fn(String) -> CommandsError,
) -> Result<
    (
        std::process::ChildStdin,
        std::process::ChildStdout,
        std::process::ChildStderr,
    ),
    CommandsError,
> {
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| fail("the child has no standard input".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| fail("the child has no standard output".to_owned()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| fail("the child has no standard error".to_owned()))?;
    Ok((stdin, stdout, stderr))
}

/// Starts feeding `input` to `stdin` and draining `stdout`/`stderr` on their own threads, so
/// none of the three can block the other two, or the wait for the child to end.
type IoThreads = (
    thread::JoinHandle<()>,
    thread::JoinHandle<Vec<u8>>,
    thread::JoinHandle<Vec<u8>>,
);

fn spawn_io_threads(
    mut stdin: std::process::ChildStdin,
    mut stdout: std::process::ChildStdout,
    mut stderr: std::process::ChildStderr,
    input: Vec<u8>,
    output_path: Option<&std::path::Path>,
) -> Result<IoThreads, CommandsError> {
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = open_output(output_path)?;
    let error_output = output
        .as_ref()
        .map(File::try_clone)
        .transpose()
        .map_err(|e| {
            CommandsError::new(format!("cannot open the attempt output for streaming: {e}"))
        })?;
    let stdout_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = read_and_copy(&mut stdout, &mut buf, output);
        buf
    });
    let stderr_reader = thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = read_and_copy(&mut stderr, &mut buf, error_output);
        buf
    });
    Ok((writer, stdout_reader, stderr_reader))
}

/// Opens an append-only attempt log before its provider starts writing. Each output reader owns
/// a cloned descriptor, so stdout and stderr are both recorded immediately without either one
/// blocking the other; `O_APPEND` keeps every individual write whole.
fn open_output(path: Option<&std::path::Path>) -> Result<Option<File>, CommandsError> {
    let Some(path) = path else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CommandsError::new(format!(
                "cannot create attempt output directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Some)
        .map_err(|e| {
            CommandsError::new(format!(
                "cannot open attempt output {}: {e}",
                path.display()
            ))
        })
}

/// Drains one stream in small chunks, retaining the exact bytes for the caller while appending
/// each chunk to the live attempt log before asking the process for more.
fn read_and_copy(
    reader: &mut impl Read,
    captured: &mut Vec<u8>,
    mut output: Option<File>,
) -> io::Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(());
        }
        let Some(bytes) = chunk.get(..count) else {
            return Err(io::Error::other(
                "reader returned a count beyond its buffer",
            ));
        };
        captured.extend_from_slice(bytes);
        if let Some(file) = &mut output {
            file.write_all(bytes)?;
            file.flush()?;
        }
    }
}

/// Waits for `child`, in process group `pgid`, to exit or run past `timeout`, or for this
/// process to be asked to stop while it still runs — killing the whole group in either of the
/// last two cases.
/// Kills process group `pgid` outright — the whole group, not just its leader, so nothing it
/// started is left behind. A failure here means it is already gone.
fn kill_group(pgid: i32) {
    let _ = kill(Pid::from_raw(-pgid), Signal::SIGKILL);
}

/// What `receiver` reports before `timeout` passes — killing process group `pgid` and waiting
/// for the report either once it is asked to stop, or once `timeout` passes with nothing yet.
fn resolve_ended(
    receiver: &mpsc::Receiver<Awaited>,
    timeout: Duration,
    pgid: i32,
    fail: &impl Fn(String) -> CommandsError,
) -> Result<Ended, CommandsError> {
    match receiver.recv_timeout(timeout) {
        Ok(Awaited::Exited(status)) => Ok(Ended::Exited(status)),
        Ok(Awaited::AskedToStop) => {
            kill_group(pgid);
            // Waited for, so it is reaped rather than left a zombie; the status itself is of
            // no interest, since this attempt is ending `Interrupted` regardless of it.
            let _ = receiver.recv();
            Ok(Ended::Interrupted)
        }
        Err(RecvTimeoutError::Timeout) => {
            kill_group(pgid);
            match receiver.recv() {
                Ok(Awaited::Exited(_) | Awaited::AskedToStop) => Ok(Ended::TimedOut),
                Err(_) => Err(fail("the wait thread stopped without a result".to_owned())),
            }
        }
        Err(RecvTimeoutError::Disconnected) => {
            Err(fail("the wait thread stopped without a result".to_owned()))
        }
    }
}

fn wait_for_child(
    mut child: std::process::Child,
    pgid: i32,
    timeout: Duration,
    fail: &impl Fn(String) -> CommandsError,
) -> Result<Ended, CommandsError> {
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

    let ended = resolve_ended(&receiver, timeout, pgid, fail);
    handle.close();
    let _ = signal_watcher.join();
    let _ = waiter.join();
    ended
}

/// The [`Exit`] `ended` means, once the child's real exit status, when it has one, has been
/// read.
fn exit_from(ended: Ended, fail: &impl Fn(String) -> CommandsError) -> Result<Exit, CommandsError> {
    Ok(match ended {
        Ended::Exited(status) => {
            let status: ExitStatus =
                status.map_err(|e| fail(format!("cannot wait for it: {e}")))?;
            match status.code() {
                Some(code) => Exit::Code(code),
                None => Exit::Killed,
            }
        }
        Ended::TimedOut => Exit::Killed,
        Ended::Interrupted => Exit::Interrupted,
    })
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
            output_path: None,
        }
    }

    /// Spawns `spec.program` directly, bypassing `EXEC_TIED_TO_PARENT_MARKER`: these tests
    /// exercise the `PATH` handling, waiting, capturing, timeout and signal handling
    /// `path_with_own_binary_first` and `run_spawned` share with [`ProcessCommands::run`]
    /// against `bash` directly, since none of that depends on how the child came to exist —
    /// only [`ProcessCommands::run`] itself needs the real `ktask-rs` binary the tie-through
    /// relies on, and that is proven through the real binary instead, in `ktask-cli`'s own
    /// end-to-end tests.
    fn run_directly(spec: &CommandSpec) -> Result<Output, CommandsError> {
        let fail =
            |cause: String| CommandsError::new(format!("cannot run {}: {cause}", spec.program));
        let path = path_with_own_binary_first()?;
        let child = Command::new(&spec.program)
            .args(&spec.args)
            .current_dir(&spec.dir)
            .env("PATH", path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .spawn()
            .map_err(|e| fail(e.to_string()))?;
        run_spawned(child, spec, fail, None)
    }

    #[test]
    fn stdout_stderr_and_the_exit_code_are_captured() {
        let dir = TempDir::new().unwrap();
        let output = run_directly(&spec(
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
        let output = run_directly(&spec(
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
    fn the_running_binarys_own_directory_is_put_first_on_the_childs_path() {
        let dir = TempDir::new().unwrap();
        let output = run_directly(&spec(
            vec!["-c", "echo $PATH"],
            dir.path(),
            b"",
            Duration::from_secs(5),
        ))
        .unwrap();
        let path = String::from_utf8(output.stdout).unwrap();
        let own_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let first = std::env::split_paths(path.trim_end()).next().unwrap();
        assert_eq!(first, own_dir, "{path}");
    }

    #[test]
    fn the_process_kept_its_own_path_after_the_running_binarys_directory() {
        let dir = TempDir::new().unwrap();
        let output = run_directly(&spec(
            vec!["-c", "echo $PATH"],
            dir.path(),
            b"",
            Duration::from_secs(5),
        ))
        .unwrap();
        let path = String::from_utf8(output.stdout).unwrap();
        let own_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let mut entries = std::env::split_paths(path.trim_end());
        assert_eq!(entries.next().unwrap(), own_dir, "{path}");
        let rest: Vec<_> = entries.collect();
        let previous: Vec<_> = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect())
            .unwrap_or_default();
        assert_eq!(rest, previous, "{path}");
    }

    #[test]
    fn the_command_starts_in_the_given_directory() {
        let dir = TempDir::new().unwrap();
        let canonical = std::fs::canonicalize(dir.path()).unwrap();
        let output = run_directly(&spec(
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
        let error = run_directly(&CommandSpec {
            program: "there-is-no-such-program".to_owned(),
            args: vec![],
            dir: dir.path().to_owned(),
            stdin: vec![],
            timeout: Duration::from_secs(5),
            output_path: None,
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
        let output = run_directly(&spec(
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
        let output = run_directly(&spec(
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
            thread::park_timeout(Duration::from_millis(10));
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
            thread::park_timeout(Duration::from_millis(20));
        }
    }
}
