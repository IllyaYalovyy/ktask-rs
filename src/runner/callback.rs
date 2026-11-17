//! Restricted worker requests cross the workspace sandbox through owned TMPDIR files.
//! The guardian, outside the Codex sandbox, applies follow-up and dependency requests.
use crate::{
    application::{Actor, ActorKind, Definition, Kind, Service, dependency::Requirement},
    runner::android_transport::{Endpoint, MAX_RESPONSE, Output},
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
    pub environment_fingerprint: String,
    pub environment_path: String,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    FollowUp {
        token: String,
        title: String,
        goal: String,
        criteria: Vec<String>,
    },
    Dependency {
        token: String,
        requirement: Requirement,
    },
    Android {
        token: String,
        operation: String,
        args: Vec<String>,
        cwd: PathBuf,
    },
}

#[derive(Serialize, Deserialize)]
struct Response {
    id: Option<i64>,
    error: Option<String>,
    #[serde(default)]
    output: Option<Output>,
}

/// The CLI writes only beneath the worker's sandbox-owned TMPDIR, never SQLite.
pub fn file(token: &str, title: &str, goal: &str, criteria: &[String]) -> Result<i64> {
    send(Request::FollowUp {
        token: token.into(),
        title: title.into(),
        goal: goal.into(),
        criteria: criteria.to_vec(),
    })
}

pub fn dependency(token: &str, requirement: &Requirement) -> Result<i64> {
    send(Request::Dependency {
        token: token.into(),
        requirement: requirement.clone(),
    })
}

pub fn android(token: &str, operation: &str, args: &[String]) -> Result<Output> {
    let cwd = std::env::current_dir()?;
    let reply = exchange(
        Request::Android {
            token: token.into(),
            operation: operation.into(),
            args: args.to_vec(),
            cwd,
        },
        Duration::from_secs(120),
    )?;
    reply.output.ok_or_else(|| {
        Error::Conflict(
            reply
                .error
                .unwrap_or_else(|| "Android command returned no result".into()),
        )
    })
}

fn send(request: Request) -> Result<i64> {
    let reply = exchange(request, Duration::from_secs(15))?;
    reply.id.ok_or_else(|| {
        Error::Conflict(
            reply
                .error
                .unwrap_or_else(|| "Worker callback returned no receipt".into()),
        )
    })
}

fn exchange(request: Request, timeout: Duration) -> Result<Response> {
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
    let bytes = serde_json::to_vec(&request).map_err(std::io::Error::other)?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(Error::Conflict("Worker callback exceeds 64 KiB".into()));
    }
    let mut output = new_file(&temporary)?;
    output.write_all(&bytes)?;
    output.sync_all()?;
    drop(output);
    fs::rename(&temporary, &ready)?;
    let deadline = Instant::now() + timeout;
    loop {
        match File::open(&response) {
            Ok(mut file) => {
                let mut bytes = Vec::new();
                (&mut file).take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
                let _ = fs::remove_file(&response);
                if bytes.len() as u64 > MAX_RESPONSE {
                    return Err(Error::Conflict(
                        "Worker callback response exceeds capacity".into(),
                    ));
                }
                let reply: Response = serde_json::from_slice(&bytes).map_err(|error| {
                    Error::Conflict(format!("Invalid callback receipt: {error}"))
                })?;
                return Ok(reply);
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
    pub fn start(
        scratch: &Path,
        capability: Capability,
        android: Option<Endpoint>,
    ) -> Result<Self> {
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
                            serve(&entry.path(), &capability, android.as_ref(), &stopping);
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

fn serve(ready: &Path, capability: &Capability, android: Option<&Endpoint>, stop: &AtomicBool) {
    let processing = ready.with_extension("processing");
    if fs::rename(ready, &processing).is_err() {
        return;
    }
    let writing = ready.with_extension("writing");
    let response = ready.with_extension("response");
    // Reserve the response inode before changing state; a worker cannot replace
    // the file with a symlink and make the guardian write outside scratch.
    if let Ok(mut output) = new_file(&writing) {
        let result = apply(&processing, capability, android, stop);
        let reply = match result {
            Ok(reply) => reply,
            Err(error) => Response {
                id: None,
                error: Some(error.to_string()),
                output: None,
            },
        };
        if serde_json::to_writer(&mut output, &reply).is_ok() && output.sync_all().is_ok() {
            let _ = fs::rename(&writing, response);
        }
    }
    let _ = fs::remove_file(processing);
}

fn apply(
    path: &Path,
    capability: &Capability,
    android: Option<&Endpoint>,
    stop: &AtomicBool,
) -> Result<Response> {
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
    let token = match &request {
        Request::FollowUp { token, .. }
        | Request::Dependency { token, .. }
        | Request::Android { token, .. } => token,
    };
    if *token != capability.token {
        return Err(Error::Conflict(
            "Worker callback token does not match this attempt".into(),
        ));
    }
    let db = Database::open_in(&capability.state)?;
    let mut service = Service::find(db, &capability.project)?
        .ok_or_else(|| Error::Conflict("Worker callback project is not registered".into()))?;
    service.set_actor(Actor {
        kind: ActorKind::Ai,
        name: "execution worker".into(),
    })?;
    match request {
        Request::FollowUp {
            token,
            title,
            goal,
            criteria,
        } => {
            let definition = Definition {
                title,
                goal,
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria,
                models: service.defaults()?,
                attempt_limit: Some(3),
                timeout_ms: None,
                stage_timeout_ms: None,
            };
            Ok(Response {
                id: Some(service.file_follow_up(
                    &token,
                    capability.task,
                    capability.attempt,
                    &definition,
                )?),
                error: None,
                output: None,
            })
        }
        Request::Dependency { token, requirement } => Ok(Response {
            id: Some(service.request_live_requirement(
                capability.task,
                capability.attempt,
                &token,
                &requirement,
                &capability.environment_fingerprint,
                &capability.environment_path,
            )?),
            error: None,
            output: None,
        }),
        Request::Android {
            token,
            operation,
            args,
            cwd,
        } => {
            let endpoint = android.ok_or_else(|| {
                Error::Conflict("No owned Android transport for this attempt".into())
            })?;
            service.authorize_android_transport(
                capability.task,
                capability.attempt,
                &token,
                endpoint.sdk(),
            )?;
            Ok(Response {
                id: None,
                error: None,
                output: Some(endpoint.exchange(&operation, &args, &cwd, stop)?),
            })
        }
    }
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
                environment_fingerprint: "unbound".into(),
                environment_path: std::env::var("PATH").unwrap_or_default(),
            },
            None,
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
