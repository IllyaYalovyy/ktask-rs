//! The parent side: one hub process holds every child connection and records what each child
//! last reported. Children reach it only through `link accept`, on a socket of their own;
//! the local TUI and CLI read it, and send commands to a child, through another.
use super::{
    Files,
    protocol::{
        self, FromChild, FromParent, HEARTBEAT, Hello, Lines, MAX_COMMANDS, MAX_MESSAGE,
        MAX_OUTPUT, PROTOCOL, Part, SILENCE, VERSION,
    },
};
use crate::{
    application::{now_ms, overview::Summary},
    storage::{Error, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read, Write},
    net::Shutdown,
    os::unix::net::{UnixListener, UnixStream},
    path::Path,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

/// Read by the local TUI and CLI.
const READERS: &str = "hub.sock";
/// Reached by `link accept` alone, so that a child can only ever report about itself.
const CHILDREN: &str = "children.sock";
const MAX_HOSTS: usize = 64;
const MAX_CONNECTIONS: usize = 2 * MAX_HOSTS;
const HANDSHAKE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);
const READER_PATIENCE: Duration = Duration::from_secs(2);

/// What the parent knows about one child, kept until the owner forgets it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HostRecord {
    pub host: String,
    id: String,
    pub version: String,
    pub protocol: u64,
    pub online: bool,
    pub incompatible: bool,
    pub last_contact_ms: i64,
    pub last_error: Option<String>,
    pub projects: Vec<Summary>,
}

#[derive(Serialize, Deserialize)]
enum Request {
    List,
    Forget(String),
    /// A ktask command line for the child of this name.
    Run {
        host: String,
        args: Vec<String>,
    },
}

#[derive(Serialize, Deserialize)]
enum Answer {
    Hosts(Vec<HostRecord>),
    Forgotten,
    Unknown,
    Connected,
    Failed(String),
    Offline,
    Incompatible(String),
    /// A piece of a forwarded command's answer.
    Part(Part),
    /// The command has not ended; the hub still waits for it.
    Waiting,
    /// The child's connection ended before the command's answer did.
    Lost,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Forgotten {
    Removed,
    Unknown,
}

/// Writes to one child, shared by its connection and the commands forwarded to it.
type Writer = Arc<Mutex<UnixStream>>;

/// A forwarded command whose answer is not complete.
struct Pending {
    parts: Sender<Part>,
    /// The output it may still send.
    allowance: usize,
}

/// A child's current connection. Dropping it ends every pending command as lost.
struct Live {
    generation: u64,
    stream: UnixStream,
    writer: Writer,
    pending: BTreeMap<u64, Pending>,
}

fn locked(writer: &Writer) -> MutexGuard<'_, UnixStream> {
    writer.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Host {
    record: HostRecord,
    live: Option<Live>,
}

struct Hub {
    files: Files,
    hosts: Mutex<BTreeMap<String, Host>>,
    generations: AtomicU64,
    commands: AtomicU64,
    connections: AtomicUsize,
}

impl Hub {
    fn hosts(&self) -> MutexGuard<'_, BTreeMap<String, Host>> {
        // A panicking connection thread must not stop the others.
        self.hosts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn save(&self, record: &HostRecord) {
        if let Err(error) = super::write_json(&self.files.record(&record.host), record) {
            note!("ktask link: cannot record {}: {error}", record.host);
        }
    }

    fn forget(&self, host: &str) -> Answer {
        let mut hosts = self.hosts();
        match hosts.get(host) {
            None => Answer::Unknown,
            Some(entry) if entry.live.is_some() => Answer::Connected,
            Some(_) => match fs::remove_file(self.files.record(host)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    Answer::Failed(error.to_string())
                }
                _ => {
                    hosts.remove(host);
                    Answer::Forgotten
                }
            },
        }
    }

    /// Registers a greeting child, or returns why it is turned away.
    fn admit(
        &self,
        hello: &Hello,
        stream: &UnixStream,
        writer: &Writer,
    ) -> std::result::Result<u64, (String, bool)> {
        let mut hosts = self.hosts();
        let previous = hosts.get(&hello.host);
        if previous.is_some_and(|host| host.live.is_some() && host.record.id != hello.id) {
            return Err((
                format!(
                    "another computer is connected as {}; choose a name with ktask link parent USER@HOST --name NAME",
                    hello.host
                ),
                false,
            ));
        }
        if previous.is_none() && hosts.len() >= MAX_HOSTS {
            return Err((
                format!("the parent already lists {MAX_HOSTS} hosts; forget one first"),
                false,
            ));
        }
        let copy = stream
            .try_clone()
            .map_err(|error| (error.to_string(), false))?;
        let projects = previous.map_or_else(Vec::new, |host| host.record.projects.clone());
        let reason = protocol::mismatch(hello.protocol, &hello.version, false);
        let record = HostRecord {
            host: hello.host.clone(),
            id: hello.id.clone(),
            version: hello.version.clone(),
            protocol: hello.protocol,
            online: reason.is_none(),
            incompatible: reason.is_some(),
            last_contact_ms: now_ms(),
            last_error: reason.clone(),
            projects,
        };
        self.save(&record);
        let generation = self.generations.fetch_add(1, Ordering::Relaxed);
        let live = reason.is_none().then(|| Live {
            generation,
            stream: copy,
            writer: Arc::clone(writer),
            pending: BTreeMap::new(),
        });
        // A reconnecting child replaces its own half-open connection.
        if let Some(Host {
            live: Some(old), ..
        }) = hosts.insert(hello.host.clone(), Host { record, live })
        {
            let _ = old.stream.shutdown(Shutdown::Both);
        }
        match reason {
            Some(reason) => Err((reason, true)),
            None => Ok(generation),
        }
    }

    /// Applies one message of the child's current connection; false once it was replaced.
    fn apply(&self, host: &str, generation: u64, projects: Option<Vec<Summary>>) -> bool {
        let mut hosts = self.hosts();
        let Some(entry) = hosts.get_mut(host).filter(|entry| {
            entry
                .live
                .as_ref()
                .is_some_and(|live| live.generation == generation)
        }) else {
            return false;
        };
        if let Some(projects) = projects {
            entry.record.projects = projects;
        }
        entry.record.last_contact_ms = now_ms();
        self.save(&entry.record);
        true
    }

    /// Sends a command to a connected child; its answer arrives on the receiver, which
    /// closes without a last part if the connection ends first.
    fn forward(
        &self,
        host: &str,
        args: Vec<String>,
    ) -> std::result::Result<Receiver<Part>, Answer> {
        let id = self.commands.fetch_add(1, Ordering::Relaxed);
        let (parts, answer) = mpsc::channel();
        let writer = {
            let mut hosts = self.hosts();
            let entry = hosts.get_mut(host).ok_or(Answer::Unknown)?;
            if entry.record.incompatible {
                let reason = entry.record.last_error.clone().unwrap_or_default();
                return Err(Answer::Incompatible(reason));
            }
            let live = entry.live.as_mut().ok_or(Answer::Offline)?;
            if live.pending.len() >= MAX_COMMANDS {
                return Err(Answer::Part(Part::Refused(format!(
                    "{host} already has {MAX_COMMANDS} commands waiting"
                ))));
            }
            let allowance = MAX_OUTPUT;
            live.pending.insert(id, Pending { parts, allowance });
            Arc::clone(&live.writer)
        };
        let sent = protocol::send(&mut *locked(&writer), &FromParent::Command { id, args });
        match sent {
            Ok(()) => {}
            // Nothing was written, so the connection is sound and the child heard nothing.
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                self.abandon(host, id);
                return Err(Answer::Part(Part::Refused(
                    "the command line is too long for the link".into(),
                )));
            }
            // Part of it may have arrived: end the connection, which reports it lost.
            Err(_) => {
                let _ = locked(&writer).shutdown(Shutdown::Both);
            }
        }
        Ok(answer)
    }

    fn abandon(&self, host: &str, id: u64) {
        if let Some(live) = self.hosts().get_mut(host).and_then(|e| e.live.as_mut()) {
            live.pending.remove(&id);
        }
    }

    /// Passes a piece of an answer to whoever waits for it. One that is too long is dropped,
    /// which reports it lost.
    fn reply(&self, host: &str, generation: u64, id: u64, part: Part) -> bool {
        let mut hosts = self.hosts();
        let Some(live) = hosts
            .get_mut(host)
            .and_then(|entry| entry.live.as_mut())
            .filter(|live| live.generation == generation)
        else {
            return false;
        };
        let Some(pending) = live.pending.get_mut(&id) else {
            return true;
        };
        let length = match &part {
            Part::Stdout(text) | Part::Stderr(text) => text.len(),
            _ => 0,
        };
        let last = part.last();
        let Some(left) = pending.allowance.checked_sub(length) else {
            live.pending.remove(&id);
            return true;
        };
        pending.allowance = left;
        if pending.parts.send(part).is_err() || last {
            live.pending.remove(&id);
        }
        true
    }

    fn disconnected(&self, host: &str, generation: u64, reason: &str) {
        let mut hosts = self.hosts();
        let Some(entry) = hosts.get_mut(host).filter(|entry| {
            entry
                .live
                .as_ref()
                .is_some_and(|live| live.generation == generation)
        }) else {
            return;
        };
        entry.live = None;
        entry.record.online = false;
        entry.record.last_error = Some(reason.to_owned());
        self.save(&entry.record);
        note!("ktask link: {host} disconnected: {reason}");
    }

    fn stop(&self) {
        for entry in self.hosts().values_mut() {
            let Some(live) = entry.live.take() else {
                continue;
            };
            let _ = live.stream.shutdown(Shutdown::Both);
            entry.record.online = false;
            entry.record.last_error = Some("the parent's hub stopped".into());
            self.save(&entry.record);
        }
    }
}

/// Runs the hub until `stopped`. The caller holds the instance's link lock.
pub(super) fn serve(files: Files, stopped: &dyn Fn() -> bool) -> Result<()> {
    fs::create_dir_all(files.hosts())?;
    let hosts = records(&files)?
        .into_iter()
        .map(|record| {
            let host = record.host.clone();
            (host, Host { record, live: None })
        })
        .collect();
    let readers = protocol::listen(&files.runtime, READERS)?;
    let children = protocol::listen(&files.runtime, CHILDREN)?;
    let runtime = files.runtime.clone();
    let hub = Arc::new(Hub {
        files,
        hosts: Mutex::new(hosts),
        generations: AtomicU64::new(0),
        commands: AtomicU64::new(0),
        connections: AtomicUsize::new(0),
    });
    for host in hub.hosts().values() {
        hub.save(&host.record);
    }
    note!(
        "ktask link: hub waiting for children (socket directory {})",
        runtime.display()
    );
    while !stopped() {
        let reader = incoming(&readers);
        if let Some(stream) = reader {
            let hub = Arc::clone(&hub);
            thread::spawn(move || {
                if let Err(error) = serve_reader(&hub, stream) {
                    note!("ktask link: local reader: {error}");
                }
            });
            continue;
        }
        let Some(stream) = incoming(&children) else {
            thread::sleep(Duration::from_millis(25));
            continue;
        };
        if hub.connections.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            continue;
        }
        hub.connections.fetch_add(1, Ordering::Relaxed);
        let hub = Arc::clone(&hub);
        thread::spawn(move || {
            if let Err(error) = serve_child(&hub, stream) {
                note!("ktask link: connection closed: {error}");
            }
            hub.connections.fetch_sub(1, Ordering::Relaxed);
        });
    }
    hub.stop();
    for name in [READERS, CHILDREN] {
        let _ = fs::remove_file(runtime.join(name));
    }
    Ok(())
}

fn incoming(listener: &UnixListener) -> Option<UnixStream> {
    match listener.accept() {
        Ok((stream, _)) => stream.set_nonblocking(false).ok().map(|()| stream),
        Err(error) => {
            if !protocol::timed_out(&error) {
                note!("ktask link: cannot accept a connection: {error}");
                thread::sleep(Duration::from_secs(1));
            }
            None
        }
    }
}

/// The next message within `patience`, or why the connection is over.
fn receive(
    lines: &mut Lines<UnixStream>,
    patience: Duration,
) -> std::result::Result<Vec<u8>, String> {
    let deadline = Instant::now() + patience;
    loop {
        match lines.next_line_by(Some(deadline)) {
            Ok(Some(line)) => return Ok(line),
            Ok(None) => return Err("connection closed".into()),
            Err(error) if protocol::timed_out(&error) && Instant::now() < deadline => {}
            Err(error) if protocol::timed_out(&error) => {
                return Err(format!("no message for {}s", patience.as_secs()));
            }
            Err(error) => return Err(error.to_string()),
        }
    }
}

fn serve_child(hub: &Hub, stream: UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(POLL))?;
    stream.set_write_timeout(Some(READER_PATIENCE))?;
    let mut output = stream.try_clone()?;
    let mut lines = Lines::new(stream.try_clone()?, MAX_MESSAGE);
    let reject = |output: &mut UnixStream, reason: String, incompatible| {
        note!("ktask link: refused a child: {reason}");
        protocol::send(
            output,
            &FromParent::Reject {
                reason,
                incompatible,
            },
        )
    };
    let greeted = receive(&mut lines, HANDSHAKE).and_then(|line| protocol::hello(&line));
    let hello = match greeted {
        Ok(hello) => hello,
        Err(reason) => return reject(&mut output, reason, false),
    };
    let writer = Arc::new(Mutex::new(output.try_clone()?));
    // Held until the welcome is out, so that no command can overtake it.
    let held = locked(&writer);
    let generation = match hub.admit(&hello, &stream, &writer) {
        Ok(generation) => generation,
        Err((reason, incompatible)) => {
            let reason = format!("{}: {reason}", hello.host);
            return reject(&mut output, reason, incompatible);
        }
    };
    note!("ktask link: {} connected", hello.host);
    let reason = converse(hub, &hello.host, generation, &mut lines, held, &writer);
    hub.disconnected(&hello.host, generation, &reason);
    let _ = stream.shutdown(Shutdown::Both);
    Ok(())
}

/// Serves a welcomed child until the connection ends; returns why it did.
fn converse(
    hub: &Hub,
    host: &str,
    generation: u64,
    lines: &mut Lines<UnixStream>,
    mut output: MutexGuard<'_, UnixStream>,
    writer: &Writer,
) -> String {
    let welcome = FromParent::Welcome {
        protocol: PROTOCOL,
        version: VERSION.into(),
    };
    if let Err(error) = protocol::send(&mut *output, &welcome) {
        return error.to_string();
    }
    drop(output);
    loop {
        let message = receive(lines, SILENCE).and_then(|line| {
            serde_json::from_slice::<FromChild>(&line)
                .map_err(|error| format!("malformed message: {error}"))
        });
        let (projects, answer) = match message {
            Ok(FromChild::Projects { mut projects }) => match protocol::accept(&mut projects) {
                Ok(()) => (Some(projects), false),
                Err(reason) => return reason,
            },
            Ok(FromChild::Heartbeat) => (None, true),
            Ok(FromChild::Reply { id, part }) => {
                if !hub.reply(host, generation, id, part) {
                    return "replaced by a newer connection".into();
                }
                continue;
            }
            Ok(FromChild::Hello(_)) => return "malformed message: second hello".into(),
            Err(reason) => return reason,
        };
        if !hub.apply(host, generation, projects) {
            return "replaced by a newer connection".into();
        }
        if answer && let Err(error) = protocol::send(&mut *locked(writer), &FromParent::Heartbeat) {
            return error.to_string();
        }
    }
}

fn serve_reader(hub: &Hub, stream: UnixStream) -> io::Result<()> {
    stream.set_read_timeout(Some(READER_PATIENCE))?;
    stream.set_write_timeout(Some(READER_PATIENCE))?;
    let mut output = stream.try_clone()?;
    let Some(line) = Lines::new(stream, MAX_MESSAGE).next_line()? else {
        return Ok(());
    };
    let answer = match serde_json::from_slice(&line).map_err(io::Error::other)? {
        Request::Run { host, args } => {
            return match hub.forward(&host, args) {
                Ok(answer) => stream_answer(&mut output, &answer),
                Err(answer) => protocol::send(&mut output, &answer),
            };
        }
        Request::List => Answer::Hosts(
            hub.hosts()
                .values()
                .map(|host| host.record.clone())
                .collect(),
        ),
        Request::Forget(host) => hub.forget(&host),
    };
    // Every host may report up to one full message, more than a child may send.
    let mut line = serde_json::to_vec(&answer).map_err(io::Error::other)?;
    line.push(b'\n');
    output.write_all(&line)
}

/// Writes a forwarded command's answer as it arrives, saying every so often that it is
/// still coming. A reader that left ends only the waiting, never the command.
fn stream_answer(output: &mut UnixStream, parts: &Receiver<Part>) -> io::Result<()> {
    loop {
        let (answer, last) = match parts.recv_timeout(HEARTBEAT) {
            Ok(part) => {
                let last = part.last();
                (Answer::Part(part), last)
            }
            Err(RecvTimeoutError::Timeout) => (Answer::Waiting, false),
            Err(RecvTimeoutError::Disconnected) => (Answer::Lost, true),
        };
        protocol::send(output, &answer)?;
        if last {
            return Ok(());
        }
    }
}

/// How a forwarded command ended, as the parent's CLI sees it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Called {
    /// The command ran on the child; its output and exit status.
    Ran {
        stdout: String,
        stderr: String,
        status: u8,
    },
    /// Nothing was sent: no host of this name has ever linked.
    Unknown,
    /// Nothing was sent: the host is not connected.
    Offline(String),
    /// Nothing was sent: the host runs another ktask.
    Incompatible(String),
    /// The child was reached and did not start it.
    Refused(String),
    /// It was sent and no complete answer came: it may or may not have run.
    Lost,
}

/// Runs a command line on `host` through the running hub, once.
pub(super) fn call(files: &Files, host: &str, args: Vec<String>) -> Result<Called> {
    let Some(mut stream) = connect(files)? else {
        return Ok(Called::Offline("ktask link run is not running here".into()));
    };
    stream.set_write_timeout(Some(READER_PATIENCE))?;
    stream.set_read_timeout(Some(SILENCE))?;
    let request = Request::Run {
        host: host.to_owned(),
        args,
    };
    match protocol::send(&mut stream, &request) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            return Ok(Called::Refused(
                "the command line is too long for the link".into(),
            ));
        }
        // The hub acts only on a whole line, which it did not get.
        Err(error) => return Ok(Called::Offline(format!("the link hub: {error}"))),
    }
    Ok(collect(stream))
}

/// Reads one answer. Anything that cuts it short leaves the outcome unknown.
fn collect(stream: UnixStream) -> Called {
    let mut lines = Lines::new(stream, MAX_MESSAGE);
    let (mut stdout, mut stderr) = (String::new(), String::new());
    loop {
        let Ok(Some(line)) = lines.next_line() else {
            return Called::Lost;
        };
        match serde_json::from_slice(&line) {
            Ok(Answer::Waiting) => {}
            Ok(Answer::Part(Part::Stdout(text))) => stdout.push_str(&text),
            Ok(Answer::Part(Part::Stderr(text))) => stderr.push_str(&text),
            Ok(Answer::Part(Part::Exit(status))) => {
                return Called::Ran {
                    stdout,
                    stderr,
                    status,
                };
            }
            Ok(Answer::Part(Part::Refused(reason))) => return Called::Refused(reason),
            Ok(Answer::Unknown) => return Called::Unknown,
            Ok(Answer::Offline) => return Called::Offline("it is not connected".into()),
            Ok(Answer::Incompatible(reason)) => return Called::Incompatible(reason),
            _ => return Called::Lost,
        }
    }
}

/// A connection to the running hub; None when none runs.
fn connect(files: &Files) -> Result<Option<UnixStream>> {
    match protocol::connect(&files.runtime, READERS) {
        Ok(stream) => Ok(Some(stream)),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error.into()),
    }
}

/// Asks the running hub; None when none runs.
fn ask(files: &Files, request: &Request) -> Result<Option<Answer>> {
    let Some(mut stream) = connect(files)? else {
        return Ok(None);
    };
    let unanswered = |error: String| {
        Error::Conflict(format!(
            "The link hub did not answer ({error}); restart ktask link run"
        ))
    };
    stream.set_read_timeout(Some(READER_PATIENCE))?;
    stream.set_write_timeout(Some(READER_PATIENCE))?;
    protocol::send(&mut stream, request).map_err(|error| unanswered(error.to_string()))?;
    let line = Lines::new(stream, (MAX_HOSTS + 1) * MAX_MESSAGE)
        .next_line()
        .map_err(|error| unanswered(error.to_string()))?
        .ok_or_else(|| unanswered("connection closed".into()))?;
    serde_json::from_slice(&line)
        .map(Some)
        .map_err(|error| unanswered(error.to_string()))
}

/// The records on disk, none of them connected.
fn records(files: &Files) -> Result<Vec<HostRecord>> {
    let entries = match fs::read_dir(files.hosts()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut records = Vec::new();
    for entry in entries {
        let path = entry?.path();
        if let Some(record) = read_record(files, &path) {
            records.push(record);
        }
    }
    records.sort_by(|a, b| a.host.cmp(&b.host));
    Ok(records)
}

/// A record file, ignoring temporary files and anything the hub did not write.
fn read_record(files: &Files, path: &Path) -> Option<HostRecord> {
    let mut data = Vec::new();
    fs::File::open(path)
        .ok()?
        .take(2 * MAX_MESSAGE as u64)
        .read_to_end(&mut data)
        .ok()?;
    let mut record: HostRecord = serde_json::from_slice(&data).ok()?;
    record.online = false;
    (protocol::check_host(&record.host).is_ok() && files.record(&record.host) == path)
        .then_some(record)
}

/// Every host this instance has heard from. Without a running hub they are all offline.
pub(super) fn hosts(files: &Files) -> Result<Vec<HostRecord>> {
    match ask(files, &Request::List)? {
        Some(Answer::Hosts(hosts)) => Ok(hosts),
        Some(_) => Err(Error::Conflict(
            "Unexpected answer from the link hub; restart ktask link run".into(),
        )),
        None => records(files),
    }
}

/// Removes a disconnected host and its listed projects.
pub(super) fn forget(files: &Files, host: &str) -> Result<Forgotten> {
    if protocol::check_host(host).is_err() {
        return Ok(Forgotten::Unknown);
    }
    match ask(files, &Request::Forget(host.to_owned()))? {
        Some(Answer::Forgotten) => Ok(Forgotten::Removed),
        Some(Answer::Unknown) => Ok(Forgotten::Unknown),
        Some(Answer::Connected) => Err(Error::Conflict(format!(
            "{host} is connected; stop ktask link run there (or ktask link parent --clear), then forget it"
        ))),
        Some(Answer::Failed(error)) => Err(Error::Conflict(error)),
        Some(_) => Err(Error::Conflict(
            "Unexpected answer from the link hub; restart ktask link run".into(),
        )),
        None => match fs::remove_file(files.record(host)) {
            Ok(()) => Ok(Forgotten::Removed),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Forgotten::Unknown),
            Err(error) => Err(error.into()),
        },
    }
}

/// What `link accept` does: pipe the SSH channel into the hub, and nothing else.
pub(super) fn relay(files: &Files) -> Result<()> {
    let socket = match protocol::connect(&files.runtime, CHILDREN) {
        Ok(socket) => socket,
        Err(error) => {
            let reason = format!(
                "no hub is running on the parent: start ktask link run there (no socket in {})",
                files.runtime.display()
            );
            protocol::send(
                &mut io::stdout().lock(),
                &FromParent::Reject {
                    reason: reason.clone(),
                    incompatible: false,
                },
            )?;
            return Err(Error::Conflict(format!("{reason}: {error}")));
        }
    };
    let mut upstream = socket.try_clone()?;
    thread::spawn(move || {
        let _ = pipe(&mut io::stdin().lock(), &mut upstream);
        let _ = upstream.shutdown(Shutdown::Write);
    });
    // Either end closing ends the process, and with it the other direction.
    let _ = pipe(&mut &socket, &mut io::stdout().lock());
    Ok(())
}

/// Copies and flushes each chunk as it arrives.
fn pipe(from: &mut impl Read, to: &mut impl Write) -> io::Result<()> {
    let mut chunk = [0; 16 * 1024];
    loop {
        match from.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(count) => {
                to.write_all(&chunk[..count])?;
                to.flush()?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}
