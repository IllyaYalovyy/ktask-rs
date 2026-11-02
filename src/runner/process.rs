//! A small worker guardian: stdin is a lifeline, not a terminal. Losing the supervisor
//! stops the entire child process group before releasing its workspace/scratch leases.
use crate::{
    application::Service,
    runner::callback::{Capability, Server},
    scratch::Scratch,
    storage::{Database, Error, Result},
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

#[derive(Clone, Serialize, Deserialize)]
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
    /// Artifact admission is owned by the guardian before it deletes scratch.
    #[serde(default)]
    pub artifact_target: Option<ArtifactTarget>,
    pub worker_lock: PathBuf,
    pub callback: Option<Capability>,
    /// Set for read-only turns: the provider runs inside it.
    #[serde(default)]
    pub sandbox: Option<super::sandbox::Sandbox>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ArtifactTarget {
    pub state: PathBuf,
    pub project: PathBuf,
    pub scratch_root: PathBuf,
    pub attempt: i64,
    pub token: String,
    pub relative_prefix: String,
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
            Ok(Message::Artifact { .. }) => {
                return Err(Error::Conflict(
                    "Unexpected artifact from read-only proposal".into(),
                ));
            }
            Ok(Message::Error(error)) => {
                if let Err(Error::Conflict(reason)) = worker.join()
                    && reason.starts_with("Resource limit:")
                {
                    return Err(Error::Conflict(reason));
                }
                return Err(Error::Conflict(error));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                worker.join()?;
                return Err(Error::Conflict("Remediation worker disconnected".into()));
            }
        }
    }
}
#[derive(Serialize, Deserialize)]
pub enum Message {
    Output { stream: String, bytes: Vec<u8> },
    Artifact { name: String, relative_path: String },
    Exit { code: i32 },
    Error(String),
}

/// A guardian may finish cleanup while its supervisor is not draining stdout.
/// This status survives a full pipe or a partial final protocol frame.
pub enum WorkerResult {
    Delivered,
    ResourceUndelivered(ResourceKind),
}

#[derive(Clone, Copy)]
pub enum ResourceKind {
    Scratch,
    Artifact,
    Delivery,
}
impl ResourceKind {
    fn from_reason(reason: &str) -> Self {
        if reason.contains("managed scratch exceeded") {
            Self::Scratch
        } else if reason.contains("artifact evidence incomplete") {
            Self::Artifact
        } else {
            Self::Delivery
        }
    }

    pub const fn exit_code(self) -> u8 {
        match self {
            Self::Scratch => 75,
            Self::Artifact => 76,
            Self::Delivery => 77,
        }
    }

    fn from_exit_code(code: Option<i32>) -> Option<Self> {
        match code {
            Some(75) => Some(Self::Scratch),
            Some(76) => Some(Self::Artifact),
            Some(77) => Some(Self::Delivery),
            _ => None,
        }
    }

    fn incomplete_reason(self) -> &'static str {
        match self {
            Self::Scratch => {
                "Resource limit: managed scratch exceeded; worker stopped and captured evidence retained, but guardian output delivery was incomplete. Raise the scratch limit or free capacity, then resume for fresh verification"
            }
            Self::Artifact => {
                "Resource limit: artifact evidence incomplete; worker stopped and captured evidence retained, but guardian output delivery was incomplete. Raise the artifact limit or free capacity, then resume for fresh verification"
            }
            Self::Delivery => {
                "Resource limit: guardian output delivery incomplete; worker stopped and captured evidence retained. Restore delivery capacity, then resume for fresh verification"
            }
        }
    }
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
fn scratch_error(path: &std::path::Path) -> io::Result<Option<String>> {
    let bytes = crate::resource::limit("KTASK_SCRATCH_BYTES", 4 * 1024 * 1024 * 1024);
    let entries = crate::resource::limit("KTASK_SCRATCH_ENTRIES", 200_000);
    Ok(crate::resource::scratch_exceeded(path, bytes, entries)?.then(|| format!("Resource limit: managed scratch exceeded {bytes} bytes or {entries} entries; work and captured evidence retained. Raise the limit or free capacity, then resume")))
}
pub fn worker() -> Result<WorkerResult> {
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
    let artifact_dir = scratch.path().join("artifacts");
    if spec.artifact_target.is_some() {
        std::fs::create_dir(&artifact_dir)?;
    }
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
    if spec.artifact_target.is_some() {
        command.env("KTASK_ARTIFACT_DIR", &artifact_dir);
    } else {
        command.env_remove("KTASK_ARTIFACT_DIR");
    }
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
    // Keep the process monitor independent of a supervisor stalled on its pipe.
    // This queue bounds pending delivery instead of spooling it to disk.
    let (outbound, pending) = mpsc::sync_channel(32);
    let (delivery_done, delivered) = mpsc::channel();
    thread::spawn(move || {
        let result = pending.into_iter().try_for_each(|message| send(&message));
        let _ = delivery_done.send(result);
    });
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
    let mut quota_error = None;
    let mut last_scan = Instant::now();
    let mut pending_message = None;
    let mut blocked_since = None;
    while !disconnected || code.is_none() || pending_message.is_some() {
        if quota_error.is_none() && last_scan.elapsed() >= Duration::from_millis(250) {
            last_scan = Instant::now();
            if let Some(error) = scratch_error(scratch.path())? {
                quota_error = Some(error);
                child.stop()?;
                code.get_or_insert(130);
            }
        }
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
        if let Some(message) = pending_message.take()
            && quota_error.is_none()
        {
            match outbound.try_send(message) {
                Ok(()) => blocked_since = None,
                Err(mpsc::TrySendError::Full(message)) => {
                    pending_message = Some(message);
                    if blocked_since.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_secs(2)
                    {
                        quota_error = Some("Resource limit: guardian output delivery stalled; captured evidence retained. Resume after restoring delivery capacity".into());
                        pending_message = None;
                        child.stop()?;
                        code.get_or_insert(130);
                    }
                }
                Err(mpsc::TrySendError::Disconnected(_)) => {
                    stop.store(true, Ordering::Relaxed);
                }
            }
        }
        if disconnected || pending_message.is_some() {
            thread::sleep(Duration::from_millis(25));
            continue;
        }
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(message) => {
                if quota_error.is_none() {
                    pending_message = Some(message);
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    // Pipes may close before the child finishes. The loop above keeps sampling
    // scratch until the process group has stopped, even without more output.
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
    if quota_error.is_none() {
        quota_error = scratch_error(scratch.path())?;
    }
    let artifact_error = spec
        .artifact_target
        .as_ref()
        .map(|target| archive_artifacts(&artifact_dir, target, &outbound))
        .transpose()
        .err();
    drop(callback);
    scratch.close()?;
    lease.set_len(0)?;
    lease.sync_all()?;
    if let Some(error) = quota_error.or_else(|| artifact_error.map(|error| {
        let detail = match error { Error::Conflict(message) => message, other => other.to_string() };
        if detail.contains(" limit") || detail.contains("No space left on device") || detail.contains("Disk quota exceeded") {
            format!("Resource limit: artifact evidence incomplete: {detail}; work and captured evidence retained. Raise the limit or free capacity, then resume")
        } else {
            format!("Artifact evidence incomplete: {detail}; work and captured evidence retained")
        }
    })) {
        let resource = ResourceKind::from_reason(&error);
        return Ok(finish_delivery(outbound, delivered, Message::Error(error), resource));
    }
    Ok(finish_delivery(
        outbound,
        delivered,
        Message::Exit {
            code: code.unwrap_or(130),
        },
        ResourceKind::Delivery,
    ))
}

fn finish_delivery(
    outbound: mpsc::SyncSender<Message>,
    delivered: mpsc::Receiver<io::Result<()>>,
    final_message: Message,
    resource: ResourceKind,
) -> WorkerResult {
    if enqueue_final(&outbound, final_message).is_err() {
        return WorkerResult::ResourceUndelivered(resource);
    }
    drop(outbound);
    if wait_for_delivery(delivered).is_ok() {
        WorkerResult::Delivered
    } else {
        WorkerResult::ResourceUndelivered(resource)
    }
}

fn wait_for_delivery(delivered: mpsc::Receiver<io::Result<()>>) -> io::Result<()> {
    delivered
        .recv_timeout(Duration::from_secs(2))
        .map_err(|error| match error {
            mpsc::RecvTimeoutError::Timeout => {
                io::Error::new(io::ErrorKind::TimedOut, "guardian delivery stalled")
            }
            mpsc::RecvTimeoutError::Disconnected => {
                io::Error::new(io::ErrorKind::BrokenPipe, "guardian delivery disconnected")
            }
        })?
}

fn enqueue_final(outbound: &mpsc::SyncSender<Message>, message: Message) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut message = message;
    loop {
        match outbound.try_send(message) {
            Ok(()) => return Ok(()),
            Err(mpsc::TrySendError::Disconnected(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "guardian delivery disconnected",
                ));
            }
            Err(mpsc::TrySendError::Full(pending)) => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "guardian delivery stalled",
                    ));
                }
                message = pending;
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// The guardian owns the scratch lease until every selected file is in state storage.
/// Only regular, flat files are accepted; a worker cannot redirect the copy with a symlink.
fn archive_artifacts(
    source: &std::path::Path,
    target: &ArtifactTarget,
    outbound: &mpsc::SyncSender<Message>,
) -> Result<()> {
    let service = Service::open(Database::open_in(&target.state)?, &target.project, None)?;
    let tree = super::workspace::Workspace::fingerprint_at(&target.project, &target.scratch_root)?;
    let scan_limit = crate::resource::limit("KTASK_ARTIFACT_SCAN_ENTRIES", 256) as usize;
    let mut entries = Vec::new();
    let mut scan_overflow = false;
    for entry in std::fs::read_dir(source)? {
        if entries.len() == scan_limit {
            scan_overflow = true;
            break;
        }
        entries.push(entry?);
    }
    entries.sort_by_key(|entry| entry.file_name());
    let mut errors = Vec::new();
    if scan_overflow {
        errors.push(format!("Artifact directory exceeds the {scan_limit}-entry scan limit; remaining evidence was not archived"));
    }
    for (index, entry) in entries.iter().enumerate() {
        let name = match entry.file_name().into_string() {
            Ok(name) => name,
            Err(_) => {
                if errors.len() < 5 {
                    errors.push("Artifact name must be UTF-8".to_owned());
                }
                continue;
            }
        };
        let stored_name = format!("file-{}-{index}", std::process::id());
        let relative = format!("{}/{stored_name}", target.relative_prefix);
        match service.publish_artifact(
            target.attempt,
            &target.token,
            &name,
            &entry.path(),
            &relative,
            &tree,
        ) {
            Ok(()) => {
                // Publication is already durable. A disconnected supervisor must
                // not prevent later siblings from being archived.
                if outbound
                    .try_send(Message::Artifact {
                        name,
                        relative_path: stored_name,
                    })
                    .is_err()
                    && errors.len() < 5
                {
                    errors.push("Resource limit: artifact notification delivery stalled; archived evidence retained".into());
                }
            }
            Err(error) => {
                if errors.len() < 5 {
                    errors.push(match error {
                        Error::Conflict(message) => message,
                        other => other.to_string(),
                    });
                }
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(Error::Conflict(errors.join("; ")))
    }
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
        // The reader keeps draining the pipe while the supervisor waits on a busy shared
        // database, which under many concurrent projects can block a commit for seconds. The
        // bound (1,024 chunks, each at most one 4 KiB read plus framing) keeps memory finite; a supervisor that truly
        // stops consuming still fills it and the guardian's two-second stall rule applies.
        let (tx, messages) = mpsc::sync_channel(1024);
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
        require_exit(status)
    }
}
fn require_exit(status: std::process::ExitStatus) -> Result<()> {
    if let Some(resource) = ResourceKind::from_exit_code(status.code()) {
        Err(Error::Conflict(resource.incomplete_reason().into()))
    } else if status.success() {
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
    fn final_enqueue_and_delivery_failures_keep_resource_outcome() {
        let (full, _receiver) = mpsc::sync_channel(1);
        full.send(Message::Output {
            stream: "stdout".into(),
            bytes: vec![0],
        })
        .unwrap();
        let (_finished, delivered) = mpsc::channel();
        assert!(matches!(
            finish_delivery(
                full,
                delivered,
                Message::Error("scratch".into()),
                ResourceKind::Scratch
            ),
            WorkerResult::ResourceUndelivered(ResourceKind::Scratch)
        ));

        let (queued, _receiver) = mpsc::sync_channel(1);
        let (_finished, delivered) = mpsc::channel();
        assert!(matches!(
            finish_delivery(
                queued,
                delivered,
                Message::Error("artifact".into()),
                ResourceKind::Artifact
            ),
            WorkerResult::ResourceUndelivered(ResourceKind::Artifact)
        ));
    }

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
