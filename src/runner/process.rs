//! A small worker guardian: stdin is a lifeline, not a terminal. Losing the supervisor
//! stops the entire child process group before releasing its workspace/scratch leases.
use crate::{
    application::Service,
    resource::{Breach, Limit},
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
    #[serde(default)]
    pub unset_env: Vec<String>,
    #[serde(default)]
    pub scratch_limits: Option<(u64, u64)>,
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

impl Invocation {
    /// A plain command under the guardian: no prompt, schema, artifacts, callback or sandbox.
    pub fn new(
        executable: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        scratch_root: PathBuf,
        worker_lock: PathBuf,
    ) -> Self {
        Self {
            executable,
            args,
            env: Vec::new(),
            unset_env: Vec::new(),
            scratch_limits: None,
            cwd,
            prompt: String::new(),
            schema: None,
            scratch_root,
            artifact_target: None,
            worker_lock,
            callback: None,
            sandbox: None,
        }
    }
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
/// `supervisor` is the supervisor's recorded executable, which runs the guardian.
pub fn capture_readonly(
    supervisor: &std::path::Path,
    spec: &Invocation,
    cancelled: &AtomicBool,
    timeout: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    let mut worker = Worker::spawn(supervisor, spec)?;
    let started = std::time::Instant::now();
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        if cancelled.load(Ordering::Relaxed) || started.elapsed() >= timeout {
            worker.join()?;
            return Err(Error::Conflict("Remediation cancelled or timed out".into()));
        }
        match worker.receive(Duration::from_millis(25))? {
            Received::Idle => {}
            Received::Output { stream, bytes } => {
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
            Received::Exited(code) => return Ok((code, stdout, stderr)),
            Received::Artifact => {
                return Err(Error::Conflict(
                    "Unexpected artifact from read-only proposal".into(),
                ));
            }
            Received::Failed(error) => return Err(worker.failure(error)),
            Received::Disconnected => {
                return Err(Error::Conflict("Remediation worker disconnected".into()));
            }
        }
    }
}

/// A local prerequisite probe under the same guardian and descendant cleanup as workers.
/// The deadline covers the command, its descendants and output collection.
pub fn capture_probe(
    supervisor: &std::path::Path,
    spec: &Invocation,
    timeout: Duration,
) -> Result<(i32, Vec<u8>, Vec<u8>)> {
    const LIMIT: usize = 4096;
    let mut worker = Worker::spawn(supervisor, spec)?;
    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        if Instant::now() >= deadline {
            worker.join()?;
            return Err(Error::Conflict(format!(
                "Dependency probe timed out after {} seconds",
                timeout.as_secs()
            )));
        }
        match worker.receive(Duration::from_millis(20))? {
            Received::Idle => {}
            Received::Output { stream, bytes } => {
                let output = if stream == "stdout" {
                    &mut stdout
                } else {
                    &mut stderr
                };
                if output.len().saturating_add(bytes.len()) > LIMIT {
                    worker.join()?;
                    return Err(Error::Conflict(
                        "Dependency probe output exceeds 4 KiB".into(),
                    ));
                }
                output.extend(bytes);
            }
            Received::Exited(code) => return Ok((code, stdout, stderr)),
            Received::Artifact => {
                worker.join()?;
                return Err(Error::Conflict(
                    "Unexpected dependency probe artifact".into(),
                ));
            }
            Received::Failed(error) => return Err(worker.failure(error)),
            Received::Disconnected => {
                return Err(Error::Conflict(
                    "Dependency probe guardian disconnected".into(),
                ));
            }
        }
    }
}
#[derive(Serialize, Deserialize)]
pub enum Message {
    Output {
        stream: String,
        bytes: Vec<u8>,
    },
    Artifact {
        name: String,
        relative_path: String,
    },
    Exit {
        code: i32,
    },
    Error(String),
    /// A storage limit stopped the worker; the scratch summary was taken before cleanup.
    Breach(Breach),
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
                "Resource limit: managed scratch exceeded; worker stopped and its scratch deleted, and captured evidence retained, but guardian output delivery was incomplete. Raise the scratch limit, then resume for fresh verification"
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

/// Before reclaiming provisioning staging, stop a previous guardian's orphaned
/// descendants using the same worker marker used for ordinary task recovery.
pub(crate) fn recover_orphaned(path: &std::path::Path) -> Result<()> {
    let mut lease = lock(path)?;
    let mut marker = Vec::new();
    lease.read_to_end(&mut marker)?;
    if !marker.is_empty() {
        stop_orphaned_workers(&marker)?;
        lease.set_len(0)?;
        lease.sync_all()?;
    }
    Ok(())
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
/// The scratch limits in force: those the supervisor passed, else the configured ones.
fn scratch_limits(passed: Option<(u64, u64)>) -> (u64, u64) {
    passed.unwrap_or_else(crate::resource::configured_scratch)
}

fn scratch_exceeded(path: &std::path::Path, passed: Option<(u64, u64)>) -> io::Result<bool> {
    let (bytes, entries) = scratch_limits(passed);
    crate::resource::scratch_exceeded(path, bytes, entries)
}

/// What filled a breached scratch directory. The worker is stopped; nothing is deleted yet.
fn scratch_breach(path: &std::path::Path, passed: Option<(u64, u64)>) -> Breach {
    let (byte_limit, entry_limit) = scratch_limits(passed);
    let summary = crate::resource::summarize(path);
    let (limit, configured, measured) = if summary.bytes > byte_limit {
        (Limit::ScratchBytes, byte_limit, summary.bytes)
    } else {
        (Limit::ScratchEntries, entry_limit, summary.entries)
    };
    Breach {
        limit,
        configured,
        measured,
        cause: summary.cause(),
        summary: summary.render(),
        reason: format!(
            "Resource limit: managed scratch exceeded {byte_limit} bytes or {entry_limit} entries; the guardian stopped the worker and deleted its scratch, and work and captured evidence are retained. Raise the scratch limit, then resume"
        ),
    }
}

fn android_runtime_error(root: Option<&std::path::Path>) -> io::Result<Option<String>> {
    let Some(root) = root else {
        return Ok(None);
    };
    let bytes = crate::resource::limit("KTASK_ANDROID_RUNTIME_BYTES", 8 * 1024 * 1024 * 1024);
    let entries = crate::resource::limit("KTASK_ANDROID_RUNTIME_ENTRIES", 200_000);
    Ok(crate::resource::scratch_exceeded(root, bytes, entries)?
        .then(|| format!("Resource limit: isolated Android runtime exceeded {bytes} bytes or {entries} entries; emulator and adb descendants stopped")))
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
    let android = if spec.callback.is_some() {
        super::android_transport::Transport::start(
            &std::env::current_exe()?,
            &spec.cwd,
            scratch.path(),
            &spec.env,
        )?
    } else {
        None
    };
    let callback_token = spec
        .callback
        .as_ref()
        .map(|capability| capability.token.clone());
    let callback = spec
        .callback
        .map(|capability| {
            Server::start(
                scratch.path(),
                capability,
                android.as_ref().map(|transport| transport.endpoint()),
            )
        })
        .transpose()?;
    let mut command = match &spec.sandbox {
        Some(sandbox) => {
            let mut sandbox = sandbox.clone();
            if let Some(android) = &android {
                sandbox.hide(android.hidden_path())?;
            }
            sandbox.command(&spec.executable, scratch.path())
        }
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
    for key in &spec.unset_env {
        command.env_remove(key);
    }
    if spec.artifact_target.is_some() {
        command.env("KTASK_ARTIFACT_DIR", &artifact_dir);
    } else {
        command.env_remove("KTASK_ARTIFACT_DIR");
    }
    if let Some(callback) = &callback {
        command.env("KTASK_CALLBACK_DIR", callback.path());
        if let Some(token) = &callback_token {
            command.env("KTASK_ANDROID_TOKEN", token);
        }
    } else {
        command.env_remove("KTASK_CALLBACK_DIR");
        command.env_remove("KTASK_ANDROID_TOKEN");
    }
    if let Some(schema) = &spec.schema {
        let path = scratch.path().join("response-schema.json");
        std::fs::write(&path, serde_json::to_vec(schema).map_err(io::Error::other)?)?;
        command.arg("--output-schema").arg(path);
    }
    let mut child = Group(command.spawn()?);
    let mut child_input = child.0.stdin.take().expect("piped stdin");
    let android_runtime_root = spec
        .env
        .iter()
        .find(|(key, _)| key == "KTASK_ANDROID_RUNTIME_ROOT")
        .map(|(_, path)| std::path::PathBuf::from(path));
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
    let mut quota = None;
    let mut last_scan = Instant::now();
    let mut pending_message = None;
    let mut blocked_since = None;
    while !disconnected || code.is_none() || pending_message.is_some() {
        if quota.is_none() && last_scan.elapsed() >= Duration::from_millis(250) {
            last_scan = Instant::now();
            quota = capacity(
                scratch.path(),
                spec.scratch_limits,
                android_runtime_root.as_deref(),
            )?;
            if quota.is_some() {
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
            && quota.is_none()
        {
            match outbound.try_send(message) {
                Ok(()) => blocked_since = None,
                Err(mpsc::TrySendError::Full(message)) => {
                    pending_message = Some(message);
                    if blocked_since.get_or_insert_with(Instant::now).elapsed()
                        >= Duration::from_secs(2)
                    {
                        quota = Some(Quota::Reason("Resource limit: guardian output delivery stalled; captured evidence retained. Resume after restoring delivery capacity".into()));
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
                if quota.is_none() {
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
    if quota.is_none() {
        quota = capacity(
            scratch.path(),
            spec.scratch_limits,
            android_runtime_root.as_deref(),
        )?;
    }
    let artifact_error = spec
        .artifact_target
        .as_ref()
        .map(|target| archive_artifacts(&artifact_dir, target, &outbound))
        .transpose()
        .err();
    // The scratch is summarized while it still exists: a breach is diagnosed from this.
    let final_error = final_error(quota, scratch.path(), spec.scratch_limits, artifact_error);
    drop(callback);
    drop(android);
    scratch.close()?;
    lease.set_len(0)?;
    lease.sync_all()?;
    if let Some(error) = final_error {
        let resource = error.kind();
        return Ok(finish_delivery(
            outbound,
            delivered,
            error.message(),
            resource,
        ));
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

/// The capacity limit a worker has exceeded, if any.
fn capacity(
    scratch: &std::path::Path,
    limits: Option<(u64, u64)>,
    android_runtime: Option<&std::path::Path>,
) -> io::Result<Option<Quota>> {
    if scratch_exceeded(scratch, limits)? {
        return Ok(Some(Quota::Scratch));
    }
    Ok(android_runtime_error(android_runtime)?.map(Quota::Reason))
}

/// What the guardian reports instead of an exit: a quota stop first, else an archive failure.
fn final_error(
    quota: Option<Quota>,
    scratch: &std::path::Path,
    limits: Option<(u64, u64)>,
    artifacts: Option<Error>,
) -> Option<Final> {
    match quota {
        Some(Quota::Scratch) => Some(Final::Breach(scratch_breach(scratch, limits))),
        Some(Quota::Reason(reason)) => Some(Final::Text(reason)),
        None => artifacts.map(|error| artifact_failure(error, scratch)),
    }
}

/// Why the guardian stopped a worker for capacity.
enum Quota {
    Scratch,
    Reason(String),
}

/// The guardian's last message when the work did not simply exit.
enum Final {
    Text(String),
    Breach(Breach),
}

impl Final {
    fn kind(&self) -> ResourceKind {
        match self {
            Self::Text(reason) => ResourceKind::from_reason(reason),
            Self::Breach(breach) if breach.limit.is_scratch() => ResourceKind::Scratch,
            Self::Breach(_) => ResourceKind::Artifact,
        }
    }

    fn message(self) -> Message {
        match self {
            Self::Text(reason) => Message::Error(reason),
            Self::Breach(breach) => Message::Breach(breach),
        }
    }
}

/// An artifact archive error as the guardian reports it. A limit breach also carries what
/// filled the scratch it is about to delete.
fn artifact_failure(error: Error, scratch: &std::path::Path) -> Final {
    let detail = match &error {
        Error::Conflict(message) => message.clone(),
        Error::Breach(breach) => breach.reason.clone(),
        other => other.to_string(),
    };
    let limited = detail.contains(" limit")
        || detail.contains("No space left on device")
        || detail.contains("Disk quota exceeded");
    if !limited {
        return Final::Text(format!(
            "Artifact evidence incomplete: {detail}; work and captured evidence retained"
        ));
    }
    let reason = format!(
        "Resource limit: artifact evidence incomplete: {detail}; work and captured evidence retained. Raise the limit or free capacity, then resume"
    );
    match error {
        Error::Breach(breach) => {
            let summary = crate::resource::summarize(scratch);
            Final::Breach(Breach {
                reason,
                summary: summary.render(),
                ..*breach
            })
        }
        _ => Final::Text(reason),
    }
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
    let mut breach = None;
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
                    errors.push(match &error {
                        Error::Conflict(message) => message.clone(),
                        Error::Breach(breach) => breach.reason.clone(),
                        other => other.to_string(),
                    });
                }
                if let Error::Breach(first) = error {
                    breach.get_or_insert(first);
                }
            }
        }
    }
    let reason = errors.join("; ");
    match (errors.is_empty(), breach) {
        (true, _) => Ok(()),
        (false, Some(breach)) => Err(Error::Breach(Box::new(Breach { reason, ..*breach }))),
        (false, None) => Err(Error::Conflict(reason)),
    }
}

pub struct Worker {
    child: Child,
    messages: mpsc::Receiver<Message>,
    /// A message read ahead while collecting a batch of output.
    pending: Option<Message>,
    reader: Option<thread::JoinHandle<()>>,
    /// The limit breach the guardian reported, for [`Self::failure`].
    breach: Option<Breach>,
}

/// One step of a guardian's message stream, as [`Worker::receive`] reports it.
pub enum Received {
    /// Nothing arrived within the wait.
    Idle,
    Output {
        stream: String,
        bytes: Vec<u8>,
    },
    /// The guardian archived an evidence file.
    Artifact,
    /// The worker exited with this code; the guardian has been joined.
    Exited(i32),
    /// The guardian reported a protocol or I/O error; it has not been joined yet.
    Failed(String),
    /// The guardian closed its stream without an exit; it has been joined.
    Disconnected,
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
            pending: None,
            reader: Some(reader),
            breach: None,
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

    /// The worker message pump: waits up to `wait` for the next guardian message. Final
    /// messages join the guardian first, so its cleanup has finished when they return.
    pub fn receive(&mut self, wait: Duration) -> Result<Received> {
        let message = match self.pending.take() {
            Some(message) => Ok(message),
            None => self.messages.recv_timeout(wait),
        };
        Ok(match message {
            Ok(Message::Output { stream, bytes }) => Received::Output { stream, bytes },
            Ok(Message::Artifact { .. }) => Received::Artifact,
            Ok(Message::Exit { code }) => {
                self.join()?;
                Received::Exited(code)
            }
            Ok(Message::Error(message)) => Received::Failed(message),
            Ok(Message::Breach(breach)) => {
                let reason = breach.reason.clone();
                self.breach = Some(breach);
                Received::Failed(reason)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Received::Idle,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.join()?;
                Received::Disconnected
            }
        })
    }

    /// Output already queued behind the last message, without waiting. Any other message
    /// is kept for the next [`Self::receive`].
    pub fn queued_output(&mut self) -> Option<(String, Vec<u8>)> {
        if self.pending.is_some() {
            return None;
        }
        match self.messages.try_recv() {
            Ok(Message::Output { stream, bytes }) => Some((stream, bytes)),
            Ok(other) => {
                self.pending = Some(other);
                None
            }
            Err(_) => None,
        }
    }

    /// The error for a guardian that reported `message`: a partial final frame is a protocol
    /// error, but the guardian's exit status still carries a resource pause after cleanup.
    pub fn failure(&mut self, message: String) -> Error {
        let breach = self.breach.take();
        match self.join() {
            Err(error) if super::resource_limit(&error).is_some() => error,
            _ => breach.map_or(Error::Conflict(message), |breach| {
                Error::Breach(Box::new(breach))
            }),
        }
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
