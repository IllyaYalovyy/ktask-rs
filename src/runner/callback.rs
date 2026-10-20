//! Restricted worker requests cross the workspace sandbox through owned TMPDIR files.
//! The guardian, outside the Codex sandbox, applies only the follow-up command.
use crate::{
    application::{Definition, Kind, Service},
    storage::{Database, Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

const MAX_REQUEST: u64 = 65_536;
static REQUEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
pub struct Capability {
    pub project: PathBuf,
    pub state: PathBuf,
    pub task: i64,
    pub attempt: i64,
    pub token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    title: String,
    goal: String,
    criteria: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct Response {
    id: Option<i64>,
    error: Option<String>,
}

/// The CLI writes only beneath the worker's sandbox-owned TMPDIR, never SQLite.
pub fn file(token: &str, title: &str, goal: &str, criteria: &[String]) -> Result<i64> {
    let root = std::env::var_os("KTASK_CALLBACK_DIR")
        .map(PathBuf::from)
        .ok_or_else(|| Error::Conflict("No active worker callback endpoint".into()))?;
    let name = format!(
        "request-{}-{}",
        std::process::id(),
        REQUEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let temporary = root.join(format!("{name}.tmp"));
    let ready = root.join(format!("{name}.ready"));
    let response = root.join(format!("{name}.response"));
    let request = Request {
        token: token.into(),
        title: title.into(),
        goal: goal.into(),
        criteria: criteria.to_vec(),
    };
    let bytes = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(Error::Conflict("Worker callback exceeds 64 KiB".into()));
    }
    let mut output = new_file(&temporary)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temporary, &ready)?;
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match fs::read(&response) {
            Ok(bytes) => {
                let _ = fs::remove_file(&response);
                let reply: Response = serde_json::from_slice(&bytes).map_err(|error| {
                    Error::Conflict(format!("Invalid callback receipt: {error}"))
                })?;
                return reply.id.ok_or_else(|| {
                    Error::Conflict(
                        reply
                            .error
                            .unwrap_or_else(|| "Worker callback returned no receipt".into()),
                    )
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(Error::Conflict("Worker callback response timed out".into()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

fn new_file(path: &Path) -> std::io::Result<std::fs::File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

pub struct Server {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
    path: PathBuf,
}

impl Server {
    pub fn start(scratch: &Path, capability: Capability) -> Result<Self> {
        let path = scratch.join("callback");
        fs::create_dir(&path)?;
        let directory_fd = File::open(&path)?;
        // Resolve every request through this descriptor. Replacing the writable
        // directory path cannot redirect guardian filesystem operations.
        let directory = PathBuf::from(format!("/proc/self/fd/{}", directory_fd.as_raw_fd()));
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            let _directory_fd = directory_fd;
            while !stopping.load(Ordering::Relaxed) {
                if let Ok(entries) = fs::read_dir(&directory) {
                    for entry in entries.flatten() {
                        if entry.path().extension().is_some_and(|ext| ext == "ready") {
                            serve(&entry.path(), &capability);
                        }
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        Ok(Self {
            stop,
            thread: Some(thread),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(ready: &Path, capability: &Capability) {
    let processing = ready.with_extension("processing");
    if fs::rename(ready, &processing).is_err() {
        return;
    }
    let writing = ready.with_extension("writing");
    let response = ready.with_extension("response");
    // Reserve the response inode before changing state; a worker cannot replace
    // the file with a symlink and make the guardian write outside scratch.
    if let Ok(mut output) = new_file(&writing) {
        let result = apply(&processing, capability);
        let reply = match result {
            Ok(id) => Response {
                id: Some(id),
                error: None,
            },
            Err(error) => Response {
                id: None,
                error: Some(error.to_string()),
            },
        };
        if serde_json::to_writer(&mut output, &reply).is_ok() && output.sync_all().is_ok() {
            let _ = fs::rename(&writing, response);
        }
    }
    let _ = fs::remove_file(processing);
}

fn apply(path: &Path, capability: &Capability) -> Result<i64> {
    let mut input = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !input.metadata()?.is_file() {
        return Err(Error::Conflict(
            "Worker callback must be a regular file".into(),
        ));
    }
    let mut bytes = Vec::new();
    (&mut input).take(MAX_REQUEST + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(Error::Conflict("Worker callback exceeds 64 KiB".into()));
    }
    let request: Request = serde_json::from_slice(&bytes)
        .map_err(|error| Error::Conflict(format!("Invalid worker callback: {error}")))?;
    if request.token != capability.token {
        return Err(Error::Conflict(
            "Worker callback token does not match this attempt".into(),
        ));
    }
    let db = Database::open_in(&capability.state)?;
    let mut service = Service::find(db, &capability.project)?
        .ok_or_else(|| Error::Conflict("Worker callback project is not registered".into()))?;
    let definition = Definition {
        title: request.title,
        goal: request.goal,
        kind: Kind::Ai,
        criteria: request.criteria,
        models: service.defaults()?,
        attempt_limit: Some(3),
        timeout_ms: None,
    };
    service.file_follow_up(
        &request.token,
        capability.task,
        capability.attempt,
        &definition,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, process::Command};

    #[test]
    fn guardian_stays_in_pinned_directory_and_fifo_cannot_block_cleanup() -> Result<()> {
        let root = tempfile::tempdir_in(std::env::temp_dir())?;
        let scratch = root.path().join("scratch");
        fs::create_dir(&scratch)?;
        let server = Server::start(
            &scratch,
            Capability {
                project: root.path().to_owned(),
                state: root.path().join("state"),
                task: 1,
                attempt: 1,
                token: "unused".into(),
            },
        )?;
        let replacement = root.path().join("outside");
        fs::create_dir(&replacement)?;
        let original = scratch.join("callback");
        let moved = scratch.join("callback-moved");
        fs::rename(&original, &moved)?;
        symlink(&replacement, &original)?;
        fs::write(replacement.join("fake.ready"), "{}")?;
        thread::sleep(Duration::from_millis(100));
        assert!(!replacement.join("fake.response").exists());
        assert!(!replacement.join("fake.processing").exists());

        let fifo = moved.join("hang.ready");
        let status = Command::new("mkfifo").arg(&fifo).status()?;
        assert!(status.success());
        let response = moved.join("hang.response");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !response.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let reply: Response =
            serde_json::from_slice(&fs::read(response)?).map_err(std::io::Error::other)?;
        assert!(reply.error.unwrap_or_default().contains("regular file"));
        let started = Instant::now();
        drop(server);
        assert!(started.elapsed() < Duration::from_secs(1));
        println!(
            "Guardian stayed in its pinned directory; FIFO was rejected and cleanup completed"
        );
        Ok(())
    }
}
