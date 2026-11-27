//! Remote link: a child instance keeps one SSH connection to a parent instance, which lists
//! the child's projects beside its own and sends task commands for the child to run.
//!
//! The child runs `ssh PARENT ktask link accept`, so authentication and encryption are the
//! hosts' SSH keys and `known_hosts`. On the parent `link accept` only pipes that channel
//! into the hub, which `link run` serves on sockets below the runtime directory. `screen`
//! is what the parent's TUI reads and sends over it.
mod child;
mod execute;
pub(crate) mod fleet;
mod hub;
mod protocol;
pub(crate) mod screen;

pub(crate) use hub::{Called, Forgotten, HostRecord};

use crate::{
    instance::{Instance, Paths},
    scratch::open_lock,
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::{self, File, TryLockError},
    io::{self, Read, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::atomic::Ordering,
    thread,
    time::{Duration, Instant},
};

/// Where one instance keeps its link.
struct Files {
    /// The configured parent; absent on every other instance.
    config: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
}

impl Files {
    fn new(paths: &Paths) -> Self {
        Self {
            config: paths.config.join("link.json"),
            state: paths.state.join("link"),
            runtime: paths.runtime.join("link"),
        }
    }
    fn hosts(&self) -> PathBuf {
        self.state.join("hosts")
    }
    /// `host` passed `protocol::check_host`, so it names a file in the directory.
    fn record(&self, host: &str) -> PathBuf {
        self.hosts().join(format!("{host}.json"))
    }
    fn status(&self) -> PathBuf {
        self.state.join("status.json")
    }
    /// Held by `link run`, which writes its role into it.
    fn lock(&self) -> PathBuf {
        self.runtime.join("run.lock")
    }
    /// Asks `link run` to end.
    fn stop(&self) -> PathBuf {
        self.runtime.join("stop")
    }
}

/// Replaces a file in one step, so a reader sees the old or the new contents.
fn write_json(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing parent directory"))?;
    fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(&serde_json::to_vec(value).map_err(io::Error::other)?)?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let data = match fs::read(path) {
        Ok(data) => data,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| Error::Conflict(format!("Invalid {}: {error}", path.display())))
}

/// The parent this instance dials.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Config {
    /// An SSH destination such as user@host or a Host alias of the SSH configuration.
    pub parent: String,
    /// The name the parent lists this computer's projects under.
    pub name: String,
    id: String,
    /// Whether the parent may change anything here, or only read.
    #[serde(default = "allowed")]
    pub control: bool,
}

const fn allowed() -> bool {
    true
}

impl Config {
    /// Whether a connection made for `other` still serves this configuration.
    fn same_link(&self, other: &Self) -> bool {
        (&self.parent, &self.name, &self.id) == (&other.parent, &other.name, &other.id)
    }
}

/// The configured parent, if this instance is a child.
pub(crate) fn config(paths: &Paths) -> Result<Option<Config>> {
    load(&Files::new(paths))
}

/// Shows, or sets, whether the configured parent may change anything here; None without a
/// parent.
pub(crate) fn control(paths: &Paths, allow: Option<bool>) -> Result<Option<Config>> {
    let files = Files::new(paths);
    let Some(mut config) = load(&files)? else {
        return Ok(None);
    };
    if let Some(allow) = allow {
        config.control = allow;
        write_json(&files.config, &config)?;
    }
    Ok(Some(config))
}

fn load(files: &Files) -> Result<Option<Config>> {
    read_json(&files.config)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    Hub,
    Child,
}

impl Role {
    const fn name(self) -> &'static str {
        match self {
            Self::Hub => "parent",
            Self::Child => "child",
        }
    }
}

/// What the live `link run` of this instance serves as, if one runs.
fn running(files: &Files) -> Result<Option<Role>> {
    let lock = match File::open(files.lock()) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let free = crate::runner::process::inspect(&lock, |_| Ok(()))?.is_some();
    if free {
        return Ok(None);
    }
    let mut role = String::new();
    (&lock).take(16).read_to_string(&mut role)?;
    Ok(Some(if role == Role::Child.name() {
        Role::Child
    } else {
        Role::Hub
    }))
}

fn destination(text: &str) -> Result<()> {
    let valid = !text.is_empty()
        && text.len() <= 255
        // A leading dash would be read by ssh as an option.
        && !text.starts_with('-')
        && !text.chars().any(|c| c.is_whitespace() || c.is_control());
    valid.then_some(()).ok_or_else(|| {
        Error::Conflict("The parent must be an SSH destination such as user@host".into())
    })
}

/// Saves the parent this instance reports to. A running `link run` picks it up.
pub(crate) fn configure(paths: &Paths, parent: &str, name: Option<&str>) -> Result<Config> {
    destination(parent)?;
    let files = Files::new(paths);
    if running(&files)? == Some(Role::Hub) {
        return Err(Error::Conflict(
            "This instance's ktask link run serves children as a parent; stop it first".into(),
        ));
    }
    let previous = load(&files)?;
    let name = match (name, &previous) {
        (Some(name), _) => name.to_owned(),
        (None, Some(previous)) => previous.name.clone(),
        (None, None) => fs::read_to_string("/proc/sys/kernel/hostname")?
            .trim()
            .to_owned(),
    };
    protocol::check_host(&name)
        .map_err(|error| Error::Conflict(format!("{error}; pass --name NAME")))?;
    // Naming the parent again never turns control back on by itself.
    let control = previous.as_ref().is_none_or(|previous| previous.control);
    let id = match previous {
        Some(previous) => previous.id,
        None => {
            let mut random = [0u8; 16];
            File::open("/dev/urandom")?.read_exact(&mut random)?;
            random.iter().map(|byte| format!("{byte:02x}")).collect()
        }
    };
    let config = Config {
        parent: parent.to_owned(),
        name,
        id,
        control,
    };
    write_json(&files.config, &config)?;
    Ok(config)
}

/// Removes the configured parent; returns the one there was.
pub(crate) fn clear(paths: &Paths) -> Result<Option<Config>> {
    let files = Files::new(paths);
    let previous = load(&files)?;
    for file in [&files.config, &files.status()] {
        match fs::remove_file(file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
            _ => {}
        }
    }
    Ok(previous)
}

/// `ktask link run`: dial the configured parent, or serve children when there is none.
pub(crate) fn run(instance: Instance, paths: &Paths) -> Result<()> {
    let files = Files::new(paths);
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&files.runtime)?;
    let lock = open_lock(&files.lock())?;
    // A status check holds the lock shared for an instant.
    let mut attempts = 0;
    loop {
        match lock.try_lock() {
            Ok(()) => break,
            Err(TryLockError::WouldBlock) if attempts < 4 => {
                attempts += 1;
                thread::sleep(Duration::from_millis(50));
            }
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Conflict(
                    "ktask link run is already running for this instance".into(),
                ));
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
    let config = load(&files)?;
    let role = if config.is_some() {
        Role::Child
    } else {
        Role::Hub
    };
    lock.set_len(0)?;
    (&lock).write_all(role.name().as_bytes())?;
    match fs::remove_file(files.stop()) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let signalled = crate::runner::process::stopped()?;
    let request = files.stop();
    let stopped = move || signalled.load(Ordering::Relaxed) || request.exists();
    match config {
        Some(config) => child::run(&files, paths, instance.name(), config, &stopped),
        None => hub::serve(files, &stopped),
    }
}

/// Ends a running `link run`, for `ktask shutdown`; true when one was running.
pub(crate) fn shutdown(paths: &Paths, deadline: Instant) -> Result<bool> {
    let files = Files::new(paths);
    if running(&files)?.is_none() {
        return Ok(false);
    }
    fs::write(files.stop(), [])?;
    while running(&files)?.is_some() {
        if Instant::now() >= deadline {
            return Err(Error::Conflict(
                "Timed out waiting for ktask link run to stop".into(),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
    Ok(true)
}

/// Runs a task command on a linked host, once.
pub(crate) fn call(paths: &Paths, host: &str, args: Vec<String>) -> Result<Called> {
    let files = Files::new(paths);
    let hosts = match hub::hosts(&files) {
        Ok(hosts) => hosts,
        // A hub that does not answer reaches no host.
        Err(Error::Conflict(why)) => return Ok(Called::Offline(why)),
        Err(error) => return Err(error),
    };
    let Some(record) = hosts.into_iter().find(|record| record.host == host) else {
        return Ok(Called::Unknown);
    };
    // An incompatible host is never connected, so this names the better reason first.
    if record.incompatible {
        return Ok(Called::Incompatible(record.last_error.unwrap_or_default()));
    }
    if !record.online {
        return Ok(Called::Offline(format!(
            "last contact {}",
            crate::clock::format_local(record.last_contact_ms, crate::application::now_ms())
        )));
    }
    // A hub started before an upgrade still links the children of its own version.
    if record.protocol != protocol::PROTOCOL || record.version != protocol::VERSION {
        return Ok(Called::Incompatible(format!(
            "it runs ktask {} and this is ktask {}; restart ktask link run here",
            record.version,
            protocol::VERSION
        )));
    }
    hub::call(&files, host, args)
}

pub(crate) fn accept(paths: &Paths) -> Result<()> {
    hub::relay(&Files::new(paths))
}

/// Every child this instance has heard from; empty unless it has been a parent.
pub(crate) fn hosts(paths: &Paths) -> Result<Vec<HostRecord>> {
    hub::hosts(&Files::new(paths))
}

pub(crate) fn forget(paths: &Paths, host: &str) -> Result<Forgotten> {
    hub::forget(&Files::new(paths), host)
}

/// One child as the parent reports it.
#[derive(Debug, Serialize)]
pub(crate) struct Peer {
    pub host: String,
    pub connected: bool,
    pub incompatible: bool,
    pub version: String,
    pub last_contact_ms: i64,
    pub last_error: Option<String>,
}

/// `ktask link status`.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    /// none, child or parent.
    pub role: &'static str,
    /// Whether `link run` is alive in that role.
    pub running: bool,
    /// A child's configured parent and the name it gives itself.
    pub peer: Option<String>,
    pub name: Option<String>,
    /// Whether a child's parent may change tasks there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control: Option<bool>,
    pub connected: bool,
    pub incompatible: bool,
    pub last_contact_ms: Option<i64>,
    pub last_error: Option<String>,
    /// A parent's children.
    pub peers: Vec<Peer>,
}

pub(crate) fn report(paths: &Paths) -> Result<Report> {
    let files = Files::new(paths);
    let running = running(&files)?;
    if let Some(config) = load(&files)? {
        let status = read_json::<child::Status>(&files.status())?
            .filter(|status| status.peer == config.parent)
            .unwrap_or_default();
        let running = running == Some(Role::Child);
        return Ok(Report {
            role: Role::Child.name(),
            running,
            peer: Some(config.parent),
            name: Some(config.name),
            control: Some(config.control),
            connected: running && status.connected,
            incompatible: status.incompatible,
            last_contact_ms: status.last_contact_ms,
            last_error: status.last_error,
            peers: Vec::new(),
        });
    }
    let peers: Vec<_> = hosts(paths)?
        .into_iter()
        .map(|record| Peer {
            host: record.host,
            connected: record.online,
            incompatible: record.incompatible,
            version: record.version,
            last_contact_ms: record.last_contact_ms,
            last_error: record.last_error,
        })
        .collect();
    let running = running == Some(Role::Hub);
    Ok(Report {
        role: if running || !peers.is_empty() {
            Role::Hub.name()
        } else {
            "none"
        },
        running,
        peer: None,
        name: None,
        control: None,
        connected: peers.iter().any(|peer| peer.connected),
        incompatible: peers.iter().any(|peer| peer.incompatible),
        last_contact_ms: peers.iter().map(|peer| peer.last_contact_ms).max(),
        last_error: None,
        peers,
    })
}

#[cfg(test)]
mod tests {
    use super::destination;

    #[test]
    fn a_destination_cannot_pass_options_to_ssh() {
        for valid in ["me@parent", "parent", "ssh://me@parent:2222"] {
            destination(valid).unwrap();
        }
        for invalid in ["", "-oProxyCommand=touch x", "me@parent -p 1", "a\nb"] {
            assert!(destination(invalid).is_err(), "{invalid:?}");
        }
    }
}
