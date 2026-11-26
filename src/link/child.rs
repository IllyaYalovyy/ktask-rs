//! The child side: dial the parent over SSH, report this instance's projects, run the
//! commands it sends, and keep the connection up. Only this connection, to the configured
//! parent, ever carries a command.
use super::{
    Config, Files,
    execute::Executor,
    protocol::{
        self, FromChild, FromParent, HEARTBEAT, Hello, Lines, MAX_MESSAGE, PROTOCOL, SILENCE,
        VERSION,
    },
};
use crate::{
    application::{now_ms, overview::Summary},
    instance::Paths,
    storage::{Database, Result},
};
use serde::{Deserialize, Serialize};
use std::{
    io,
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError},
    thread,
    time::{Duration, Instant},
};

/// How often the projects are read; a change reaches the parent within this and the send.
const SNAPSHOT: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(100);
const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(30);

/// The last known state of the connection, for `ktask link status`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(super) struct Status {
    pub peer: String,
    pub connected: bool,
    pub incompatible: bool,
    pub last_contact_ms: Option<i64>,
    pub last_error: Option<String>,
}

enum End {
    Stopped,
    Reconfigured,
    Failed {
        error: String,
        incompatible: bool,
        welcomed: bool,
    },
}

struct Link<'a> {
    files: &'a Files,
    paths: &'a Paths,
    /// The remote executable: the parent's instance of the same name.
    program: &'a str,
    stopped: &'a dyn Fn() -> bool,
    config: Config,
    status: Status,
    database: Option<Database>,
    executor: Executor,
}

/// Stays linked to the configured parent until stopped or the parent is cleared. An
/// unreachable or refusing parent only delays the next attempt.
pub(super) fn run(
    files: &Files,
    paths: &Paths,
    program: &str,
    config: Config,
    stopped: &dyn Fn() -> bool,
) -> Result<()> {
    let status = previous_status(files, &config);
    let mut link = Link {
        files,
        paths,
        program,
        stopped,
        config,
        status,
        database: None,
        executor: Executor::new(paths.executable.clone(), files.state.clone()),
    };
    let mut delay = FIRST_RETRY;
    let mut logged = String::new();
    link.record();
    note!(
        "ktask link: dialing {} as {}",
        link.config.parent,
        link.config.name
    );
    while !stopped() {
        match super::load(files) {
            Ok(Some(config)) if !config.same_link(&link.config) => {
                link.status = previous_status(files, &config);
                link.config = config;
                delay = FIRST_RETRY;
                logged.clear();
                link.record();
            }
            Ok(Some(_)) => {}
            Ok(None) => {
                note!("ktask link: the parent was cleared; nothing to link");
                return Ok(());
            }
            Err(error) => note!("ktask link: {error}"),
        }
        match link.session() {
            End::Stopped => break,
            End::Reconfigured => continue,
            End::Failed {
                error,
                incompatible,
                welcomed,
            } => {
                if welcomed {
                    delay = FIRST_RETRY;
                    logged.clear();
                }
                // One line per distinct failure: a long outage must not fill the journal.
                if logged != error {
                    note!("ktask link: {error}");
                    logged.clone_from(&error);
                }
                link.status.connected = false;
                link.status.incompatible = incompatible;
                link.status.last_error = Some(error);
                link.record();
                link.wait(delay);
                delay = next_delay(delay);
            }
        }
    }
    link.status.connected = false;
    link.record();
    Ok(())
}

/// Doubles the wait after each failure, up to a bound.
fn next_delay(delay: Duration) -> Duration {
    (delay * 2).min(LAST_RETRY)
}

fn previous_status(files: &Files, config: &Config) -> Status {
    let mut status = super::read_json::<Status>(&files.status())
        .ok()
        .flatten()
        .filter(|status| status.peer == config.parent)
        .unwrap_or_default();
    status.peer.clone_from(&config.parent);
    status.connected = false;
    status
}

impl Link<'_> {
    fn record(&self) {
        if let Err(error) = super::write_json(&self.files.status(), &self.status) {
            note!("ktask link: cannot record the status: {error}");
        }
    }

    fn reconfigured(&self) -> bool {
        super::load(self.files)
            .is_ok_and(|config| !config.is_some_and(|config| config.same_link(&self.config)))
    }

    fn wait(&self, delay: Duration) {
        let until = Instant::now() + delay;
        while Instant::now() < until && !(self.stopped)() && !self.reconfigured() {
            thread::sleep(TICK);
        }
    }

    fn projects(&mut self) -> Result<Vec<Summary>> {
        if self.database.is_none() {
            self.database = Database::open_existing(self.paths)?;
        }
        let Some(database) = &self.database else {
            return Ok(Vec::new());
        };
        let listed = crate::runner::registration::listed(self.paths, database);
        if listed.is_err() {
            self.database = None;
        }
        let mut projects: Vec<_> = listed?
            .iter()
            .map(|(project, alive)| Summary::of(project, *alive))
            .collect();
        protocol::fit(&mut projects);
        Ok(projects)
    }

    /// One connection, from dialing until it ends.
    fn session(&mut self) -> End {
        let mut welcomed = false;
        let failed = |error: String, incompatible, welcomed| End::Failed {
            error,
            incompatible,
            welcomed,
        };
        let mut ssh = match Ssh::dial(&self.config.parent, self.program) {
            Ok(ssh) => ssh,
            Err(error) => return failed(format!("cannot start ssh: {error}"), false, false),
        };
        // A failed write shows up as the end of the stream, which says why.
        let _ = ssh.send(&FromChild::Hello(Hello {
            protocol: PROTOCOL,
            version: VERSION.into(),
            host: self.config.name.clone(),
            id: self.config.id.clone(),
        }));
        let mut heard = Instant::now();
        let mut beat = Instant::now();
        let mut polled: Option<Instant> = None;
        let mut checked = Instant::now();
        let mut sent: Option<Vec<Summary>> = None;
        // Answers of this connection's commands; one that outlasts it is dropped.
        let (answers, answered) = mpsc::channel();
        loop {
            if (self.stopped)() {
                return End::Stopped;
            }
            if checked.elapsed() >= SNAPSHOT {
                checked = Instant::now();
                if self.reconfigured() {
                    return End::Reconfigured;
                }
            }
            // One piece at a time, so that a long answer starves neither side.
            let wait = match answered.try_recv() {
                Ok((id, part)) => {
                    // A broken pipe shows up as the end of the stream.
                    let _ = ssh.send(&FromChild::Reply { id, part });
                    Duration::ZERO
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => TICK,
            };
            match ssh.lines.recv_timeout(wait) {
                Ok(Ok(line)) => {
                    heard = Instant::now();
                    match serde_json::from_slice(&line) {
                        Ok(FromParent::Welcome { protocol, version }) => {
                            if let Some(reason) = protocol::mismatch(protocol, &version, true) {
                                return failed(reason, true, welcomed);
                            }
                            welcomed = true;
                            note!("ktask link: connected to {}", self.config.parent);
                            self.status.connected = true;
                            self.status.incompatible = false;
                            self.status.last_error = None;
                        }
                        Ok(FromParent::Reject {
                            reason,
                            incompatible,
                        }) => return failed(reason, incompatible, welcomed),
                        Ok(FromParent::Command { id, args }) if welcomed => {
                            self.executor.start(&self.config.parent, id, args, &answers);
                        }
                        Ok(FromParent::Command { .. }) => {
                            let error = "the parent sent a command before its welcome";
                            return failed(error.into(), false, welcomed);
                        }
                        Ok(FromParent::Heartbeat) => {}
                        Err(error) => {
                            return failed(
                                format!("malformed message from the parent: {error}"),
                                false,
                                welcomed,
                            );
                        }
                    }
                    self.status.last_contact_ms = Some(now_ms());
                    self.record();
                }
                Ok(Err(error)) => return failed(error.to_string(), false, welcomed),
                Err(RecvTimeoutError::Timeout) if heard.elapsed() < SILENCE => {}
                Err(RecvTimeoutError::Timeout) => {
                    let error = format!("the parent has not answered for {}s", SILENCE.as_secs());
                    return failed(error, false, welcomed);
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return failed(ssh.ended(), false, welcomed);
                }
            }
            if !welcomed {
                continue;
            }
            if polled.is_none_or(|at| at.elapsed() >= SNAPSHOT) {
                polled = Some(Instant::now());
                let projects = match self.projects() {
                    Ok(projects) => projects,
                    Err(error) => return failed(error.to_string(), false, welcomed),
                };
                if sent.as_ref() != Some(&projects) {
                    let message = FromChild::Projects {
                        projects: projects.clone(),
                    };
                    // Only a list too large to send fails here; a broken pipe shows up
                    // as the end of the stream.
                    if let Err(error) = ssh.send(&message)
                        && error.kind() == io::ErrorKind::InvalidData
                    {
                        return failed(error.to_string(), false, welcomed);
                    }
                    sent = Some(projects);
                }
            }
            if beat.elapsed() >= HEARTBEAT {
                beat = Instant::now();
                let _ = ssh.send(&FromChild::Heartbeat);
            }
        }
    }
}

/// The ssh process of one connection; dropping it ends the connection.
struct Ssh {
    process: Child,
    lines: Receiver<io::Result<Vec<u8>>>,
    /// The last line ssh or the remote shell wrote to stderr, once that stream ends.
    complaint: Receiver<String>,
}

impl Ssh {
    /// Authentication and host verification are the user's own SSH keys and known_hosts;
    /// BatchMode makes anything that would need a person fail instead of waiting.
    fn dial(destination: &str, program: &str) -> io::Result<Self> {
        let mut command = Command::new("ssh");
        command.args(["-T", "-x", "-a"]);
        for option in [
            "BatchMode=yes",
            "ClearAllForwardings=yes",
            "PermitLocalCommand=no",
            // A shared master connection would outlive this process.
            "ControlMaster=no",
            "ControlPath=none",
            "ConnectTimeout=10",
            "ServerAliveInterval=5",
            "ServerAliveCountMax=3",
        ] {
            command.args(["-o", option]);
        }
        let mut process = command
            .arg("--")
            .arg(destination)
            .args([program, "link", "accept"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let output = process.stdout.take();
        let errors = process.stderr.take();
        let (send, lines) = mpsc::sync_channel(8);
        thread::spawn(move || {
            let Some(output) = output else { return };
            let mut reader = Lines::new(output, MAX_MESSAGE);
            while let Some(line) = reader.next_line().transpose() {
                let last = line.is_err();
                if send.send(line).is_err() || last {
                    break;
                }
            }
        });
        let (say, complaint) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let Some(errors) = errors else { return };
            let mut reader = Lines::new(errors, 64 * 1024);
            let mut last = String::new();
            while let Ok(Some(line)) = reader.next_line() {
                let line = String::from_utf8_lossy(&line).trim().to_owned();
                if !line.is_empty() {
                    last = line;
                }
            }
            let _ = say.send(last);
        });
        Ok(Self {
            process,
            lines,
            complaint,
        })
    }

    fn send(&mut self, message: &FromChild) -> io::Result<()> {
        match &mut self.process.stdin {
            Some(input) => protocol::send(input, message),
            None => Err(io::ErrorKind::BrokenPipe.into()),
        }
    }

    /// Why the stream ended.
    fn ended(&mut self) -> String {
        let complaint = self
            .complaint
            .recv_timeout(Duration::from_secs(1))
            .unwrap_or_default();
        let deadline = Instant::now() + Duration::from_secs(1);
        let status = loop {
            match self.process.try_wait() {
                Ok(None) if Instant::now() < deadline => thread::sleep(TICK),
                Ok(status) => break status,
                Err(_) => break None,
            }
        };
        let code = status
            .and_then(|status| status.code())
            .filter(|code| *code != 0)
            .map_or_else(String::new, |code| format!(" (ssh exit status {code})"));
        let complaint: String = complaint.chars().take(400).collect();
        if complaint.is_empty() {
            format!("connection closed{code}")
        } else {
            format!("connection closed{code}: {complaint}")
        }
    }
}

impl Drop for Ssh {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retries_back_off_to_a_bound() {
        let delays: Vec<_> = std::iter::successors(Some(FIRST_RETRY), |d| Some(next_delay(*d)))
            .take(8)
            .map(|delay| delay.as_secs())
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
    }
}
