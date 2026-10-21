//! A small worker guardian: stdin is a lifeline, not a terminal. Losing the supervisor
//! stops the entire child process group before releasing its workspace/scratch leases.
use crate::{
    application::evidence_pin::GUARDIAN_RESERVED as RESERVED,
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
    fs::{File, OpenOptions},
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
    time::Duration,
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
    #[serde(default)]
    pub evidence_pin_guardian: Option<PathBuf>,
    pub callback: Option<Capability>,
}

// A durable reservation closes the gap between spawning a guardian and its first
// lock acquisition. An abnormal exit after provider launch deliberately stays dirty.
struct EvidenceGuardian {
    lease: File,
    launched: bool,
}
// EvidencePin supplies a /proc/<owner>/fd path to the original guard inode.
// Never manufacture an empty replacement when that descriptor is gone.
fn evidence_lock(path: &std::path::Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Conflict(
            "Evidence guard is not a regular file".into(),
        ));
    }
    file.try_lock()
        .map_err(|e| Error::Conflict(format!("Evidence guardian is already owned: {e}")))?;
    Ok(file)
}

impl EvidenceGuardian {
    fn claim(path: &std::path::Path) -> Result<Self> {
        let mut lease = evidence_lock(path)?;
        let mut marker = Vec::new();
        lease.read_to_end(&mut marker)?;
        if marker != RESERVED {
            return Err(Error::Conflict(
                "Evidence guardian reservation is missing or dirty".into(),
            ));
        }
        lease.set_len(0)?;
        use std::io::{Seek, SeekFrom};
        lease.seek(SeekFrom::Start(0))?;
        lease.write_all(b"guardian running\n")?;
        lease.sync_all()?;
        Ok(Self {
            lease,
            launched: false,
        })
    }
    fn clear(&self) -> Result<()> {
        self.lease.set_len(0)?;
        self.lease.sync_all()?;
        Ok(())
    }
}
impl Drop for EvidenceGuardian {
    fn drop(&mut self) {
        if !self.launched {
            let _ = self.clear();
        }
    }
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
pub fn lock(path: &std::path::Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock().map_err(|e| {
        Error::Conflict(format!(
            "Workspace is already owned by a runner/worker: {e}"
        ))
    })?;
    Ok(file)
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
    let mut evidence_guardian = spec
        .evidence_pin_guardian
        .as_deref()
        .map(EvidenceGuardian::claim)
        .transpose()?;
    let mut lease = lock(&spec.worker_lock)?;
    if lease.metadata()?.len() != 0 {
        return Err(Error::Conflict("Previous worker did not confirm cleanup; inspect processes before clearing its worker-lock marker".into()));
    }
    writeln!(&mut lease, "guardian {}", std::process::id())?;
    lease.sync_all()?;
    let scratch = Scratch::new(&spec.scratch_root)?;
    let callback = spec
        .callback
        .map(|capability| Server::start(scratch.path(), capability))
        .transpose()?;
    let mut command = Command::new(&spec.executable);
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
    if let Some(guardian) = &mut evidence_guardian {
        guardian.launched = true;
    }
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
    if let Some(guardian) = &evidence_guardian {
        guardian.clear()?;
    }
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
        if let Some(path) = &spec.evidence_pin_guardian {
            let mut lease = evidence_lock(path)?;
            if lease.metadata()?.len() != 0 {
                return Err(Error::Conflict(
                    "Evidence guardian marker is already in use".into(),
                ));
            }
            lease.write_all(RESERVED)?;
            lease.sync_all()?;
        }
        let spawned = Command::new(executable)
            .arg("__worker")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn();
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                // spawn failed before any guardian/provider existed.
                if let Some(path) = &spec.evidence_pin_guardian {
                    let lease = evidence_lock(path)?;
                    lease.set_len(0)?;
                    lease.sync_all()?;
                }
                return Err(error.into());
            }
        };
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
