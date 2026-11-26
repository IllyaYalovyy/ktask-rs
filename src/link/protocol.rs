//! What crosses a link: one JSON message per line, bounded in size. The same framing serves
//! the hub's local readers. SSH supplies authentication and encryption; nothing here does.
use crate::application::overview::Summary;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    io::{self, Read, Write},
    os::{
        fd::AsRawFd,
        unix::net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Raised whenever a message changes. Parent and child must agree on it and on the version.
pub(crate) const PROTOCOL: u64 = 2;
pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The largest message either side accepts.
pub(crate) const MAX_MESSAGE: usize = 4 << 20;
/// How often a child says it is there; the parent answers each one.
pub(crate) const HEARTBEAT: Duration = Duration::from_secs(5);
/// A side that has heard nothing for this long drops the connection.
pub(crate) const SILENCE: Duration = Duration::from_secs(20);
/// Commands one child runs at once; a foreground run is one for as long as it takes.
pub(crate) const MAX_COMMANDS: usize = 32;
/// The most output one forwarded command may return.
pub(crate) const MAX_OUTPUT: usize = 64 << 20;
/// The output one message carries; JSON escaping stays within `MAX_MESSAGE`.
pub(crate) const CHUNK: usize = 256 << 10;

const MAX_NAME: usize = 256;
const MAX_ROOT: usize = 4096;
const MAX_TITLE: usize = 512;
const MAX_VERSION: usize = 64;
const MAX_ID: usize = 64;

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum FromChild {
    Hello(Hello),
    /// Every project of the child, replacing what the parent had.
    Projects {
        projects: Vec<Summary>,
    },
    /// A piece of the answer to the parent's command `id`.
    Reply {
        id: u64,
        part: Part,
    },
    Heartbeat,
}

/// The answer to a forwarded command, in order; every kind but output ends it.
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Part {
    Stdout(String),
    Stderr(String),
    /// The command ended with this exit status.
    Exit(u8),
    /// The command was killed, so what it changed is unknown.
    Interrupted,
    /// The command was not started.
    Refused(String),
}

impl Part {
    pub fn last(&self) -> bool {
        !matches!(self, Self::Stdout(_) | Self::Stderr(_))
    }
}

#[derive(Serialize, Deserialize, Debug)]
pub(crate) struct Hello {
    pub protocol: u64,
    pub version: String,
    /// The name the parent lists this child's projects under.
    pub host: String,
    /// Tells two computers that chose the same name apart.
    #[serde(default)]
    pub id: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum FromParent {
    Welcome {
        protocol: u64,
        version: String,
    },
    Reject {
        reason: String,
        /// The two sides run different versions or protocols.
        #[serde(default)]
        incompatible: bool,
    },
    /// A ktask command line for the child to run as its own. The child decides whether it
    /// accepts it; the parent's word is not enough.
    Command {
        id: u64,
        args: Vec<String>,
    },
    Heartbeat,
}

/// The first message of a child. Read without requiring this protocol's shape, so that a
/// child of another version is told so instead of being dropped as malformed.
pub(crate) fn hello(line: &[u8]) -> Result<Hello, String> {
    let value: serde_json::Value =
        serde_json::from_slice(line).map_err(|error| format!("malformed message: {error}"))?;
    if value["type"] != "hello" {
        return Err("malformed message: expected hello".into());
    }
    let mut hello: Hello =
        serde_json::from_value(value).map_err(|error| format!("malformed message: {error}"))?;
    check_host(&hello.host)?;
    if hello.version.len() > MAX_VERSION || hello.id.len() > MAX_ID {
        return Err("malformed message: hello field too long".into());
    }
    hello.version = clip(&hello.version, MAX_VERSION);
    Ok(hello)
}

/// Why `host` and this build cannot work together, if they cannot.
pub(crate) fn mismatch(protocol: u64, version: &str, parent: bool) -> Option<String> {
    if protocol == PROTOCOL && version == VERSION {
        return None;
    }
    let (theirs, ours) = (
        format!("ktask {version} (protocol {protocol})"),
        format!("ktask {VERSION} (protocol {PROTOCOL})"),
    );
    let (parent, child) = if parent {
        (theirs, ours)
    } else {
        (ours, theirs)
    };
    Some(format!(
        "incompatible: the parent runs {parent}, the child {child}"
    ))
}

/// A host name is shown in every listing and names a file, so its characters are limited.
pub(crate) fn check_host(host: &str) -> Result<(), String> {
    let mut characters = host.chars();
    let valid = host.len() <= 64
        && characters
            .next()
            .is_some_and(|first| first.is_ascii_alphanumeric())
        && characters.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !host.eq_ignore_ascii_case("local");
    valid.then_some(()).ok_or_else(|| {
        format!(
            "invalid host name {:?}: use up to 64 letters, digits, dots, dashes or underscores, and not \"local\"",
            clip(host, 80)
        )
    })
}

/// Text from another computer or bound for one: no control characters, bounded length.
fn clip(text: &str, max: usize) -> String {
    text.chars()
        .take(max)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Fits a child's own projects into the limits its parent enforces.
pub(crate) fn fit(projects: &mut [Summary]) {
    for project in projects {
        project.name = clip(&project.name, MAX_NAME);
        project.root = clip(&project.root, MAX_ROOT);
        if let Some(current) = &mut project.current {
            current.title = clip(&current.title, MAX_TITLE);
        }
    }
}

/// Refuses a project list that is out of bounds, then removes control characters from it.
pub(crate) fn accept(projects: &mut [Summary]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for project in projects.iter() {
        let long = |text: &str, max| text.chars().count() > max;
        if project.id <= 0
            || !ids.insert(project.id)
            || project.queued < 0
            || project.finished < 0
            || long(&project.name, MAX_NAME)
            || long(&project.root, MAX_ROOT)
            || project
                .current
                .as_ref()
                .is_some_and(|task| task.id <= 0 || long(&task.title, MAX_TITLE))
        {
            return Err("malformed message: project out of bounds".into());
        }
    }
    fit(projects);
    Ok(())
}

pub(crate) fn send(to: &mut impl Write, message: &impl Serialize) -> io::Result<()> {
    let mut line = serde_json::to_vec(message).map_err(io::Error::other)?;
    if line.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "message is larger than the link allows",
        ));
    }
    line.push(b'\n');
    to.write_all(&line)?;
    to.flush()
}

/// Newline-delimited messages. A read that times out keeps what it already has.
pub(crate) struct Lines<R> {
    source: R,
    buffer: Vec<u8>,
    scanned: usize,
    limit: usize,
}

impl<R: Read> Lines<R> {
    pub fn new(source: R, limit: usize) -> Self {
        Self {
            source,
            buffer: Vec::new(),
            scanned: 0,
            limit,
        }
    }

    /// The next line without its newline; None at the end of the stream.
    pub fn next_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        self.next_line_by(None)
    }

    /// Like `next_line`, giving up at `deadline` even while bytes keep trickling in. A
    /// source that can fall silent also needs a read timeout.
    pub fn next_line_by(&mut self, deadline: Option<Instant>) -> io::Result<Option<Vec<u8>>> {
        let oversized = || io::Error::new(io::ErrorKind::InvalidData, "oversized message");
        let mut chunk = [0; 16 * 1024];
        loop {
            if let Some(found) = self.buffer[self.scanned..].iter().position(|b| *b == b'\n') {
                let end = self.scanned + found;
                if end > self.limit {
                    return Err(oversized());
                }
                let mut line: Vec<u8> = self.buffer.drain(..=end).collect();
                line.pop();
                self.scanned = 0;
                return Ok(Some(line));
            }
            self.scanned = self.buffer.len();
            if self.buffer.len() > self.limit {
                return Err(oversized());
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(io::ErrorKind::TimedOut.into());
            }
            match self.source.read(&mut chunk) {
                Ok(0) => return Ok(None),
                Ok(count) => self.buffer.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
    }
}

/// Whether a socket read gave up waiting rather than failed.
pub(crate) fn timed_out(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Unix socket addresses are limited to about 100 bytes, which a runtime directory can
/// exceed; the directory is therefore addressed through an open descriptor.
fn address(directory: &Path, name: &str) -> io::Result<(File, PathBuf)> {
    let directory = File::open(directory)?;
    let path = format!("/proc/self/fd/{}/{name}", directory.as_raw_fd());
    Ok((directory, path.into()))
}

pub(crate) fn connect(directory: &Path, name: &str) -> io::Result<UnixStream> {
    let (_directory, path) = address(directory, name)?;
    UnixStream::connect(path)
}

/// Binds `name`, replacing the socket of a process that is gone. The caller holds the lock
/// that proves no other owner is alive.
pub(crate) fn listen(directory: &Path, name: &str) -> io::Result<UnixListener> {
    let (_directory, path) = address(directory, name)?;
    match std::fs::remove_file(&path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_are_split_and_bounded() {
        let mut lines = Lines::new(&b"one\ntwo\npartial"[..], 8);
        assert_eq!(lines.next_line().unwrap().unwrap(), b"one");
        assert_eq!(lines.next_line().unwrap().unwrap(), b"two");
        assert!(lines.next_line().unwrap().is_none());
        for oversized in [&b"123456789\n"[..], &b"1234567890123"[..]] {
            let error = Lines::new(oversized, 8).next_line().unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
    }

    #[test]
    fn a_trickled_line_does_not_outlast_its_deadline() {
        struct Trickle;
        impl Read for Trickle {
            fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
                buffer[0] = b'x';
                Ok(1)
            }
        }
        let deadline = Instant::now() + Duration::from_millis(20);
        let error = Lines::new(Trickle, usize::MAX)
            .next_line_by(Some(deadline))
            .unwrap_err();
        assert!(timed_out(&error));
    }

    #[test]
    fn host_names_are_limited_and_local_is_reserved() {
        for host in ["devbox", "build-2.lan", "a_b"] {
            check_host(host).unwrap();
        }
        let long = "a".repeat(65);
        for host in ["", "local", "LOCAL", "-x", ".hidden", "a b", "a/b", &long] {
            assert!(check_host(host).is_err(), "{host:?}");
        }
    }

    #[test]
    fn a_hello_of_another_protocol_is_read_and_reported() {
        let hello = hello(br#"{"type":"hello","protocol":99,"version":"9.0.0","host":"new"}"#)
            .expect("readable");
        let reason = mismatch(hello.protocol, &hello.version, false).unwrap();
        assert!(reason.contains("the child ktask 9.0.0 (protocol 99)"));
        assert!(mismatch(PROTOCOL, VERSION, false).is_none());
        assert!(mismatch(PROTOCOL, "0.0.0", true).is_some());
    }
}
