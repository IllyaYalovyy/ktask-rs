//! A small worker guardian: stdin is a lifeline, not a terminal. Losing the supervisor
//! stops the entire child process group before releasing its workspace/scratch leases.
use crate::{
    runner::callback::{Capability, Server},
    scratch::Scratch,
    storage::{Error, Result},
};
use nix::{
    sys::signal::{Signal, kill, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions, TryLockError},
    io::{self, BufRead, Read, Write},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Serialize, Deserialize)]
pub struct Invocation {
    pub executable: PathBuf,
    pub args: Vec<String>,
    /// Provider settings for this turn only, such as a read-only permission policy.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub prompt: String,
    pub schema: Option<serde_json::Value>,
    pub scratch_root: PathBuf,
    pub worker_lock: PathBuf,
    pub callback: Option<Capability>,
    /// Set for read-only turns: the provider runs inside it.
    #[serde(default)]
    pub sandbox: Option<super::sandbox::Sandbox>,
}

/// Use the isolated guardian so descendants cannot retain pipes or outlive cancellation.
pub fn capture_readonly(
    spec: &Invocation,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    let mut worker = Worker::spawn(&std::env::current_exe()?, spec)?;
    let started = std::time::Instant::now();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        if cancelled.load(Ordering::Relaxed) || started.elapsed() >= timeout {
            worker.join()?;
            return Err(Error::Conflict("Remediation cancelled or timed out".into()));
        }
        match worker.messages.recv_timeout(Duration::from_millis(25)) {
            Ok(Message::Output { stream, bytes }) => {
                let output = if stream == "stdout" {
                    &mut stdout
                } else {
                    &mut stderr
                };
                if output.len() + bytes.len() > 8 * 1024 * 1024 {
                    worker.join()?;
                    return Err(Error::Conflict("Remediation response exceeds limit".into()));
                }
                output.extend(bytes);
            }
            Ok(Message::Exit { code }) => {
                worker.join()?;
                return Ok((code, stdout, stderr));
            }
            Ok(Message::Error(error)) => return Err(Error::Conflict(error)),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(Error::Conflict("Remediation worker disconnected".into()));
            }
        }
    }
}
#[derive(Serialize, Deserialize)]
pub enum Message {
    Output { stream: String, bytes: Vec<u8> },
    Exit { code: i32 },
    Error(String),
}
/// An owner's exclusive lock. Dropping it unlocks explicitly: a child forked by another
/// thread shares the lock until it execs, and closing our descriptor alone would leave
/// the lock held for that moment.
pub struct Lease(File);

impl std::ops::Deref for Lease {
    type Target = File;
    fn deref(&self) -> &File {
        &self.0
    }
}

impl std::ops::DerefMut for Lease {
    fn deref_mut(&mut self) -> &mut File {
        &mut self.0
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

/// Takes ownership: an exclusive lock held for the owner's lifetime. Status checks
/// ([`inspect`]) hold a shared lock for an instant, so contention from them is retried;
/// only an exclusive holder is another owner.
pub fn lock(path: &std::path::Path) -> Result<Lease> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(Lease(file)),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        match file.try_lock_shared() {
            Ok(()) => file.unlock()?,
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Conflict(
                    "Workspace is already owned by a runner/worker".into(),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(Error::Conflict(format!(
                "A ktask status check held {} for 10s; retry",
                path.display()
            )));
        }
        thread::sleep(Duration::from_millis(1));
    }
}
/// Read-only ownership check: None while an owner holds the lock, else `read`'s result.
/// The shared lock never blocks another check, and keeps owners out while `read` runs.
pub fn inspect<T>(file: &File, read: impl FnOnce(&File) -> io::Result<T>) -> io::Result<Option<T>> {
    match file.try_lock_shared() {
        Ok(()) => {
            let result = read(file);
            file.unlock()?;
            result.map(Some)
        }
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(error)) => Err(error),
    }
}
pub fn stopped() -> io::Result<Arc<AtomicBool>> {
    let flag = Arc::new(AtomicBool::new(false));
    for signal in [
        signal_hook::consts::SIGINT,
        signal_hook::consts::SIGTERM,
        signal_hook::consts::SIGHUP,
    ] {
        signal_hook::flag::register(signal, Arc::clone(&flag))?;
    }
    Ok(flag)
}
struct Group(Child);
impl Group {
    fn stop(&mut self) -> io::Result<()> {
        // Also kill background descendants after the direct child has exited.
        let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
        self.0.wait()?;
        reap_descendants()?;
        Ok(())
    }
}
fn reap_descendants() -> io::Result<()> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    // Subreaper adoption also catches a child that starts its own process group/session.
    // /proc reports OUR live children; never kill a stored/reused PID from a database.
    let children = format!("/proc/self/task/{}/children", std::process::id());
    loop {
        for pid in std::fs::read_to_string(&children)?
            .split_whitespace()
            .filter_map(|s| s.parse::<i32>().ok())
        {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }
        match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
            Err(nix::errno::Errno::ECHILD) => return Ok(()),
            Err(nix::errno::Errno::EINTR) => {}
            Err(error) => return Err(io::Error::other(error)),
            Ok(WaitStatus::StillAlive) => thread::sleep(Duration::from_millis(10)),
            Ok(_) => {}
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}
fn send(message: &Message) -> io::Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, message)?;
    writeln!(stdout)?;
    stdout.flush()
}
fn reader(
    stream: &'static str,
    mut input: impl Read + Send + 'static,
    tx: mpsc::SyncSender<Message>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut bytes = [0; 4096];
        loop {
            match input.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    if tx
                        .send(Message::Output {
                            stream: stream.into(),
                            bytes: bytes[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Message::Error(e.to_string()));
                    break;
                }
            }
        }
    })
}
pub fn worker() -> Result<()> {
    nix::sys::prctl::set_child_subreaper(true).map_err(io::Error::other)?;
    let stop = stopped()?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut line = String::new();
    input.by_ref().take(32 * 1024 * 1024).read_line(&mut line)?;
    let spec: Invocation = serde_json::from_str(&line).map_err(io::Error::other)?;
    drop(input);
    let mut lease = lock(&spec.worker_lock)?;
    if lease.metadata()?.len() != 0 {
        let mut marker = Vec::new();
        lease.read_to_end(&mut marker)?;
        stop_orphaned_workers(&marker)?;
        lease.set_len(0)?;
        io::Seek::rewind(&mut *lease)?;
    }
    let scratch = Scratch::new(&spec.scratch_root)?;
    // If this guardian dies without cleanup, the next lease holder finds and stops its
    // workers by this scratch directory, which every worker inherits as TMPDIR.
    writeln!(&mut *lease, "guardian {}", std::process::id())?;
    lease.write_all(b"scratch ")?;
    lease.write_all(scratch.path().as_os_str().as_encoded_bytes())?;
    lease.write_all(b"\n")?;
    lease.sync_all()?;
    let callback = spec
        .callback
        .map(|capability| Server::start(scratch.path(), capability))
        .transpose()?;
    let mut command = match &spec.sandbox {
        Some(sandbox) => sandbox.command(&spec.executable, scratch.path()),
        None => Command::new(&spec.executable),
    };
    command
        .args(&spec.args)
        .envs(spec.env.iter().map(|(key, value)| (key, value)))
        .current_dir(&spec.cwd)
        .env("TMPDIR", scratch.path())
        .env("KTASK_WORKER_CALLBACK", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    if let Some(callback) = &callback {
        command.env("KTASK_CALLBACK_DIR", callback.path());
    } else {
        command.env_remove("KTASK_CALLBACK_DIR");
    }
    if let Some(schema) = &spec.schema {
        let path = scratch.path().join("response-schema.json");
        std::fs::write(&path, serde_json::to_vec(schema).map_err(io::Error::other)?)?;
        command.arg("--output-schema").arg(path);
    }
    let mut child = Group(command.spawn()?);
    let mut child_input = child.0.stdin.take().expect("piped stdin");
    let writer = thread::spawn(move || {
        let _ = child_input.write_all(spec.prompt.as_bytes());
    });
    let (tx, rx) = mpsc::sync_channel(32);
    let stdout = reader(
        "stdout",
        child.0.stdout.take().expect("piped stdout"),
        tx.clone(),
    );
    let stderr = reader("stderr", child.0.stderr.take().expect("piped stderr"), tx);
    let parent_gone = Arc::clone(&stop);
    thread::spawn(move || {
        let mut byte = [0];
        // Supervisor never writes again. EOF or any control byte requests cancellation.
        let _ = io::stdin().read(&mut byte);
        parent_gone.store(true, Ordering::Relaxed);
    });
    let mut code = None;
    let mut disconnected = false;
    while !disconnected {
        if stop.load(Ordering::Relaxed) {
            child.stop()?;
            code.get_or_insert(130);
        }
        if code.is_none()
            && let Some(status) = child.0.try_wait()?
        {
            code = Some(status.code().unwrap_or(130));
            child.stop()?;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(message) => {
                if send(&message).is_err() {
                    stop.store(true, Ordering::Relaxed);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    // Pipes may close before the child finishes. Never mistake EOF for completion.
    while code.is_none() {
        if stop.load(Ordering::Relaxed) {
            child.stop()?;
            code = Some(130);
        } else if let Some(status) = child.0.try_wait()? {
            code = Some(status.code().unwrap_or(130));
        } else {
            thread::sleep(Duration::from_millis(25));
        }
    }
    child.stop()?;
    writer
        .join()
        .map_err(|_| io::Error::other("stdin writer panicked"))?;
    stdout
        .join()
        .map_err(|_| io::Error::other("stdout reader panicked"))?;
    stderr
        .join()
        .map_err(|_| io::Error::other("stderr reader panicked"))?;
    drop(callback);
    scratch.close()?;
    lease.set_len(0)?;
    lease.sync_all()?;
    send(&Message::Exit {
        code: code.unwrap_or(130),
    })?;
    Ok(())
}

pub struct Worker {
    child: Child,
    pub messages: mpsc::Receiver<Message>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Worker {
    pub fn spawn(executable: &std::path::Path, spec: &Invocation) -> Result<Self> {
        let spawned = Command::new(executable)
            .arg("__worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = spawned?;
        let mut input = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, messages) = mpsc::sync_channel(32);
        let reader = thread::spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                let message = line
                    .map_err(|e| e.to_string())
                    .and_then(|line| serde_json::from_str(&line).map_err(|e| e.to_string()));
                if tx.send(message.unwrap_or_else(Message::Error)).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            messages,
            reader: Some(reader),
        };
        // Retain stdin in Child as the lifeline; Drop closes it before wait.
        let sent = serde_json::to_writer(&mut input, spec)
            .map_err(io::Error::other)
            .and_then(|_| writeln!(input))
            .and_then(|_| input.flush());
        worker.child.stdin = Some(input);
        sent?;
        Ok(worker)
    }
    pub fn stop(&mut self) {
        self.child.stdin.take();
    }
    pub fn join(&mut self) -> Result<()> {
        self.stop();
        let status = loop {
            while self.messages.try_recv().is_ok() {}
            if let Some(status) = self.child.try_wait()? {
                break status;
            }
            thread::sleep(Duration::from_millis(10));
        };
        if let Some(reader) = self.reader.take() {
            while !reader.is_finished() {
                while self.messages.try_recv().is_ok() {}
                thread::sleep(Duration::from_millis(5));
            }
            reader
                .join()
                .map_err(|_| io::Error::other("worker reader panicked"))?;
        }
        require_exit(status.success())
    }
}
fn require_exit(success: bool) -> Result<()> {
    if success {
        Ok(())
    } else {
        Err(Error::Conflict(
            "Worker guardian failed; work preserved, inspect runner log before retry".into(),
        ))
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// Stop the workers of a guardian that died without cleaning up. They are found by the
/// scratch directory its marker names (every worker's TMPDIR), never by a stored PID.
/// The caller holds the worker lock, so no live guardian owns them.
pub fn stop_orphaned_workers(marker: &[u8]) -> Result<usize> {
    use std::os::unix::ffi::OsStrExt;
    let Some(scratch) = marker
        .split(|&b| b == b'\n')
        .find_map(|line| line.strip_prefix(b"scratch "))
        .filter(|path| !path.is_empty())
    else {
        return Ok(0);
    };
    let wanted = [b"TMPDIR=".as_slice(), scratch].concat();
    let mut stopped = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let mut found = 0;
        for entry in std::fs::read_dir("/proc")? {
            let entry = entry?;
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<i32>().ok())
            else {
                continue;
            };
            let Ok(environ) = std::fs::read(entry.path().join("environ")) else {
                continue;
            };
            if environ
                .split(|&b| b == 0)
                .any(|var| var == wanted.as_slice())
                && pid != std::process::id() as i32
            {
                found += 1;
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
        if found == 0 {
            let _ = std::fs::remove_dir_all(std::ffi::OsStr::from_bytes(scratch));
            return Ok(stopped);
        }
        stopped = stopped.max(found);
        if Instant::now() >= deadline {
            return Err(Error::Conflict(format!(
                "{found} worker processes of a crashed run did not stop; stop them, then try again"
            )));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_checks_never_block_owners_or_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ktask-runner.lock");
        let check = File::create(&path).unwrap();
        let other_check = File::open(&path).unwrap();
        // A check in progress neither blocks another check nor fails an arriving owner.
        check.try_lock_shared().unwrap();
        assert_eq!(inspect(&other_check, |_| Ok(())).unwrap(), Some(()));
        let (arriving, arrived) = mpsc::channel();
        let owner = thread::spawn({
            let path = path.clone();
            move || {
                arriving.send(()).unwrap();
                lock(&path)
            }
        });
        arrived.recv().unwrap();
        // Give the owner time to meet the check; its result does not depend on this delay.
        thread::sleep(Duration::from_millis(50));
        check.unlock().unwrap();
        let owner = owner
            .join()
            .unwrap()
            .expect("owner waits out a status check");
        // A live owner is reported to checks and refused to a second owner.
        assert_eq!(inspect(&check, |_| Ok(())).unwrap(), None);
        assert!(matches!(lock(&path), Err(Error::Conflict(_))));
        drop(owner);
        assert_eq!(inspect(&check, |_| Ok(())).unwrap(), Some(()));
    }
}
