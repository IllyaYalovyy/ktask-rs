//! Remote link: a parent and children as separate instances on one machine. A script named
//! `ssh` on each child's PATH stands in for SSH: it runs the requested command as the
//! parent instance, exactly what sshd would start there.
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{Definition, Kind, Service},
    storage::Database,
};
use serde_json::{Value, json};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Instant,
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The link protocol of this build.
pub(super) const PROTOCOL: u64 = 2;

fn definition(title: &str) -> Definition {
    Definition {
        title: title.into(),
        goal: "Be seen from the parent".into(),
        criteria: vec!["Listed on the parent".into()],
        kind: Kind::Ai,
        category: None,
        lenses: None,
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: Some(600_000),
        stage_timeout_ms: None,
    }
}

/// One computer: its own home and XDG roots.
pub(super) struct Host {
    pub(super) dir: PathBuf,
}

impl Host {
    pub(super) fn new(root: &Path, name: &str) -> Result<Self> {
        let dir = root.join(name);
        fs::create_dir_all(dir.join("bin"))?;
        Ok(Self { dir })
    }

    /// A child whose `ssh` reaches `parent` as me@parent, unless `unreachable` exists.
    pub(super) fn child(root: &Path, name: &str, parent: &Host) -> Result<Self> {
        let child = Self::new(root, name)?;
        let parent = parent.dir.display();
        child.transport(&format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/ssh-arguments"
if [ -e "$HOME/unreachable" ]; then
    echo "ssh: connect to host parent port 22: Connection refused" >&2
    exit 255
fi
while [ "$1" != -- ]; do shift; done
[ "$2" = me@parent ] && [ "$3" = ktask-dev ] || exit 255
shift 3
exec env -i HOME='{parent}' XDG_CONFIG_HOME='{parent}/config' XDG_STATE_HOME='{parent}/state' \
    XDG_CACHE_HOME='{parent}/cache' XDG_DATA_HOME='{parent}/data' \
    XDG_RUNTIME_DIR='{parent}/runtime' PATH=/usr/bin:/bin '{DEV}' "$@"
"#
        ))?;
        child.ok(&["link", "parent", "me@parent", "--name", name])?;
        Ok(child)
    }

    pub(super) fn transport(&self, script: &str) -> Result {
        support::write_executable(&self.dir.join("bin/ssh"), script)
    }

    pub(super) fn command(&self) -> Command {
        let mut command = Command::new(DEV);
        let dir = &self.dir;
        command
            .env_clear()
            .env("HOME", dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .env("XDG_CACHE_HOME", dir.join("cache"))
            .env("XDG_DATA_HOME", dir.join("data"))
            .env("XDG_RUNTIME_DIR", dir.join("runtime"))
            .env("TMPDIR", dir)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", dir.join("bin").display()),
            )
            .current_dir(dir);
        command
    }

    /// Runs a command expecting `status`; returns stdout.
    pub(super) fn run(&self, args: &[&str], status: i32) -> Result<String> {
        let output = self.command().args(args).output()?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(output.status.code(), Some(status), "{args:?}\n{text}");
        Ok(String::from_utf8(output.stdout)?)
    }

    pub(super) fn ok(&self, args: &[&str]) -> Result<Value> {
        let mut with_json = vec!["--json"];
        with_json.extend(args);
        let reply: Value = serde_json::from_str(&self.run(&with_json, 0)?)?;
        Ok(reply["data"].clone())
    }

    pub(super) fn status(&self) -> Result<Value> {
        self.ok(&["link", "status"])
    }

    pub(super) fn projects(&self) -> Result<Vec<Value>> {
        Ok(self.ok(&["projects"])?["projects"]
            .as_array()
            .ok_or("projects array")?
            .clone())
    }

    pub(super) fn project(&self, name: &str) -> Result<Service> {
        let folder = self.dir.join(name);
        fs::create_dir_all(&folder)?;
        Ok(Service::open(
            Database::open_in(&self.dir.join("state/ktask-dev"))?,
            &folder,
            None,
        )?)
    }

    pub(super) fn link_run(&self) -> Result<Linked> {
        let log = self.dir.join("link.log");
        let process = self
            .command()
            .args(["link", "run"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(fs::File::options().create(true).append(true).open(&log)?)
            .spawn()?;
        Ok(Linked { process, log })
    }

    /// Until the listing is as wanted.
    pub(super) fn listing(
        &self,
        what: &str,
        wanted: impl Fn(&[Value]) -> bool,
    ) -> Result<Vec<Value>> {
        support::eventually(what, || {
            let projects = self.projects()?;
            Ok(wanted(&projects).then_some(projects))
        })
    }

    pub(super) fn until_status(
        &self,
        what: &str,
        wanted: impl Fn(&Value) -> bool,
    ) -> Result<Value> {
        support::eventually(what, || {
            let status = self.status()?;
            Ok(wanted(&status).then_some(status))
        })
    }
}

/// A `ktask link run`, stopped with SIGTERM as systemd would.
pub(super) struct Linked {
    pub(super) process: Child,
    log: PathBuf,
}

impl Linked {
    fn alive(&mut self) -> Result<bool> {
        Ok(self.process.try_wait()?.is_none())
    }

    fn exited(&mut self) -> Result {
        let status = support::eventually("link run exits", || Ok(self.process.try_wait()?))?;
        assert!(
            status.success(),
            "{status:?}\n{}",
            fs::read_to_string(&self.log)?
        );
        Ok(())
    }

    pub(super) fn stop(mut self) -> Result {
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(self.process.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )?;
        self.exited()
    }
}

impl Drop for Linked {
    fn drop(&mut self) {
        if matches!(self.process.try_wait(), Ok(None)) {
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }
}

/// `name@host` of each listed project, in listing order.
fn names(projects: &[Value]) -> Vec<String> {
    projects
        .iter()
        .map(|p| {
            format!(
                "{}@{}",
                p["name"].as_str().unwrap_or(""),
                p["host"].as_str().unwrap_or("local")
            )
        })
        .collect()
}

fn find<'a>(projects: &'a [Value], name: &str, host: &str) -> &'a Value {
    projects
        .iter()
        .find(|p| p["name"] == name && p["host"] == host)
        .unwrap_or_else(|| panic!("{name}@{host} in {projects:#?}"))
}

pub(super) fn all_online(projects: &[Value], count: usize) -> bool {
    projects.len() == count && projects.iter().all(|p| p["online"] == true)
}

#[test]
fn an_unconfigured_instance_is_unchanged_and_a_parent_is_saved_and_cleared() -> Result {
    let sandbox = support::sandbox()?;
    let host = Host::new(sandbox.path(), "solo")?;

    assert_eq!(host.run(&["projects"], 0)?, "No projects.\n");
    assert_eq!(host.ok(&["projects"])?, json!({"projects":[],"count":0}));
    let none = host.status()?;
    assert_eq!(
        none,
        json!({"role":"none","running":false,"peer":null,"name":null,"connected":false,
            "incompatible":false,"last_contact_ms":null,"last_error":null,"peers":[]})
    );
    let text = host.run(&["link", "status"], 0)?;
    println!("STATUS UNCONFIGURED\n{text}");
    assert!(text.contains("role\tnone\npeer\t-\n") && text.contains("connected\tno\n"));
    assert!(
        !host.dir.join("state").exists() && !host.dir.join("runtime").exists(),
        "reading created storage"
    );

    host.project("only")?;
    let listed = host.run(&["projects"], 0)?;
    // The line of today: no host column without a linked host.
    assert_eq!(
        listed,
        format!("1\tidle\t0\tonly\t{}\t-\n", host.dir.join("only").display())
    );
    let projects = host.ok(&["projects"])?;
    assert!(projects.get("hosts").is_none(), "{projects}");
    assert_eq!(
        projects["projects"][0],
        json!({"id":1,"name":"only","root":host.dir.join("only"),"state":"idle",
            "runner_alive":false,"queued":0,"finished":0,"current":null,
            "host":null,"online":true})
    );

    let saved = host.ok(&["link", "parent", "me@parent", "--name", "devbox"])?;
    assert_eq!(saved, json!({"parent":"me@parent","name":"devbox"}));
    let child = host.status()?;
    assert_eq!(child["role"], "child");
    assert_eq!(child["peer"], "me@parent");
    assert_eq!(child["name"], "devbox");
    assert_eq!(child["connected"], false);
    assert_eq!(child["running"], false);
    assert_eq!(child["control"], true);
    assert!(child["last_contact_ms"].is_null() && child["last_error"].is_null());
    let text = host.run(&["link", "status"], 0)?;
    println!("STATUS CONFIGURED CHILD\n{text}");
    for line in [
        "role\tchild",
        "peer\tme@parent",
        "control\ton",
        "connected\tno",
        "last contact\tnever",
        "last error\t-",
    ] {
        assert!(text.contains(line), "{line}\n{text}");
    }
    // The name is kept when only the parent changes.
    assert_eq!(
        host.ok(&["link", "parent", "other"])?["name"],
        json!("devbox")
    );

    host.run(&["link", "parent", "--", "-oProxyCommand=true"], 4)?;
    host.run(&["link", "parent", "me@parent", "--name", "local"], 4)?;
    host.run(&["link", "parent"], 2)?;
    host.run(&["--project", ".", "link", "status"], 2)?;
    assert_eq!(
        host.ok(&["link", "parent", "--clear"])?,
        json!({"parent":null,"cleared":"other"})
    );
    assert_eq!(host.status()?["role"], "none");
    assert_eq!(
        host.run(&["link", "parent", "--clear"], 0)?,
        "No parent was configured\n"
    );
    host.run(&["link", "forget", "nobody"], 3)?;
    Ok(())
}

/// A child with no hub to reach, or no network at all, records why and keeps running.
fn waits_for_the_parent(devbox: &Host) -> Result<Linked> {
    // Without a hub on the parent the child records why and keeps trying.
    let mut first = devbox.link_run()?;
    let waiting = devbox.until_status("no hub yet", |s| {
        s["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("no hub is running on the parent"))
    })?;
    println!("STATUS WITHOUT A HUB\n{waiting:#}");
    assert_eq!(waiting["connected"], false);
    assert_eq!(waiting["running"], true);
    // Nor does an unreachable parent end it.
    fs::write(devbox.dir.join("unreachable"), "")?;
    devbox.until_status("unreachable", |s| {
        s["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("Connection refused") && e.contains("255"))
    })?;
    assert!(first.alive()?);
    devbox.run(&["link", "run"], 4)?;
    fs::remove_file(devbox.dir.join("unreachable"))?;

    Ok(first)
}

fn flat_list_of_both_children(parent: &Host, devbox: &Host) -> Result {
    let listed = parent.listing("both children listed", |p| all_online(p, 5))?;
    println!("LINKED JSON\n{listed:#?}");
    assert_eq!(
        names(&listed),
        [
            "alpha@devbox",
            "beta@devbox",
            "beta@local",
            "gamma@laptop",
            "zeta@local"
        ]
    );
    assert!(listed[2]["host"].is_null());
    // The same data the child lists itself, plus where it is.
    let mut own = devbox
        .projects()?
        .into_iter()
        .find(|p| p["name"] == "beta")
        .ok_or("the child's beta")?;
    assert!(own["host"].is_null() && own["online"] == true);
    own["host"] = json!("devbox");
    assert_eq!(*find(&listed, "beta", "devbox"), own);
    assert_eq!(find(&listed, "beta", "devbox")["state"], "needs_owner");
    assert_eq!(
        find(&listed, "beta", "devbox")["current"]["state"],
        "needs_approval"
    );
    let text = parent.run(&["projects"], 0)?;
    println!("LINKED TEXT\n{text}");
    let hosts: Vec<_> = text
        .lines()
        .map(|line| line.rsplit('\t').next().unwrap_or(""))
        .collect();
    assert_eq!(hosts, ["devbox", "devbox", "local", "laptop", "local"]);
    assert!(text.contains("#1 Needs approval · Approve the remote plan\tdevbox"));

    let child = devbox.status()?;
    println!("CHILD STATUS\n{child:#}");
    assert_eq!(child["connected"], true);
    assert!(child["last_error"].is_null() && child["last_contact_ms"].is_i64());
    let status = parent.status()?;
    println!("PARENT STATUS\n{status:#}");
    assert_eq!(status["role"], "parent");
    assert_eq!(status["running"], true);
    assert_eq!(status["peers"].as_array().map(Vec::len), Some(2));
    println!("{}", parent.run(&["link", "status"], 0)?);
    parent.run(&["link", "parent", "someone@else"], 4)?;

    Ok(())
}

fn changes_and_heartbeats_arrive(parent: &Host, devbox: &Host, alpha: &mut Service) -> Result {
    // A change on a child reaches the parent without anything being asked of the child.
    let started = Instant::now();
    alpha.create(&[definition("Write the remote guide")], None, true)?;
    let changed = parent.listing("live change", |p| find(p, "alpha", "devbox")["queued"] == 1)?;
    println!("CHANGE ARRIVED AFTER {:?}", started.elapsed());
    assert_eq!(
        find(&changed, "alpha", "devbox")["current"]["title"],
        "Write the remote guide"
    );
    // Heartbeats keep the contact fresh while nothing changes.
    let contact = |status: &Value| status["last_contact_ms"].as_i64().unwrap_or(0);
    let before = contact(&devbox.status()?);
    devbox.until_status("heartbeat", |s| contact(s) > before)?;

    Ok(())
}

fn a_stopped_child_is_listed_offline(parent: &Host, devbox: &Host, first: Linked) -> Result {
    first.stop()?;
    let offline = parent.listing("devbox offline", |p| {
        p.len() == 5 && find(p, "alpha", "devbox")["online"] == false
    })?;
    println!("ONE CHILD STOPPED\n{offline:#?}");
    assert_eq!(find(&offline, "alpha", "devbox")["queued"], 1);
    assert!(find(&offline, "alpha", "devbox")["last_contact_ms"].is_i64());
    assert_eq!(find(&offline, "gamma", "laptop")["online"], true);
    let text = parent.run(&["projects"], 0)?;
    println!("ONE CHILD STOPPED TEXT\n{text}");
    assert!(
        text.lines()
            .any(|l| l.contains("\toffline\t1\talpha\t") && l.contains("\tdevbox\tlast contact "))
    );
    assert_eq!(devbox.status()?["connected"], false);

    Ok(())
}

/// A disconnected host is forgotten only on request; a connected one not at all.
fn forgets_only_a_disconnected_host(parent: &Host, first: Linked) -> Result {
    parent.run(&["link", "forget", "devbox"], 4)?;
    first.stop()?;
    parent.listing("offline again", |p| {
        find(p, "alpha", "devbox")["online"] == false
    })?;
    assert_eq!(
        parent.ok(&["link", "forget", "devbox"])?,
        json!({"forgotten":"devbox"})
    );
    assert_eq!(
        names(&parent.projects()?),
        ["beta@local", "gamma@laptop", "zeta@local"]
    );
    parent.run(&["link", "forget", "devbox"], 3)?;

    Ok(())
}

/// Clearing the parent ends the child's link run, and shutdown ends the hub.
fn ends_links(parent: &Host, laptop: &Host, mut second: Linked, mut hub: Linked) -> Result {
    laptop.ok(&["link", "parent", "--clear"])?;
    second.exited()?;
    let shutdown = parent.ok(&["shutdown"])?;
    println!("SHUTDOWN\n{shutdown:#}");
    assert_eq!(shutdown["links"], json!(["ktask-dev"]));
    hub.exited()?;
    // Without a hub the record can still be read and forgotten.
    assert_eq!(
        find(&parent.projects()?, "gamma", "laptop")["online"],
        false
    );
    parent.ok(&["link", "forget", "laptop"])?;
    assert_eq!(parent.run(&["projects"], 0)?.lines().count(), 2);

    Ok(())
}

#[test]
fn children_appear_change_live_go_offline_and_return() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let parent = Host::new(root, "parent")?;
    parent.project("zeta")?;
    parent.project("beta")?;
    let devbox = Host::child(root, "devbox", &parent)?;
    let mut alpha = devbox.project("alpha")?;
    devbox
        .project("beta")?
        .create(&[definition("Approve the remote plan")], None, false)?;
    let laptop = Host::child(root, "laptop", &parent)?;
    laptop.project("gamma")?;

    let first = waits_for_the_parent(&devbox)?;
    let hub = parent.link_run()?;
    let second = laptop.link_run()?;
    flat_list_of_both_children(&parent, &devbox)?;
    changes_and_heartbeats_arrive(&parent, &devbox, &mut alpha)?;
    a_stopped_child_is_listed_offline(&parent, &devbox, first)?;

    // The parent's own restart keeps the record; meanwhile everything remote is offline.
    hub.stop()?;
    let without = parent.projects()?;
    assert_eq!(without.len(), 5);
    assert_eq!(find(&without, "gamma", "laptop")["online"], false);
    laptop.until_status("laptop notices", |s| s["connected"] == false)?;
    let hub = parent.link_run()?;
    let first = devbox.link_run()?;
    parent.listing("both return", |p| all_online(p, 5))?;

    forgets_only_a_disconnected_host(&parent, first)?;
    ends_links(&parent, &laptop, second, hub)?;

    let dialed = fs::read_to_string(devbox.dir.join("ssh-arguments"))?;
    let dialed = dialed.lines().next().unwrap_or("");
    println!("SSH ARGUMENTS\n{dialed}");
    assert!(dialed.contains("-o BatchMode=yes"));
    assert!(dialed.ends_with("-- me@parent ktask-dev link accept"));
    assert!(!dialed.contains("StrictHostKeyChecking"));
    Ok(())
}

/// A raw connection through `link accept`, as a child of any version would make it.
pub(super) struct Raw {
    pub(super) process: Child,
    replies: BufReader<std::process::ChildStdout>,
}

impl Raw {
    pub(super) fn open(parent: &Host) -> Result<Self> {
        let mut process = parent
            .command()
            .args(["link", "accept"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let replies = BufReader::new(process.stdout.take().ok_or("stdout")?);
        Ok(Self { process, replies })
    }

    /// The parent may close while a large message is still being written.
    pub(super) fn write(&mut self, data: &[u8]) {
        if let Some(input) = &mut self.process.stdin {
            let _ = input.write_all(data).and_then(|()| input.flush());
        }
    }

    pub(super) fn say(&mut self, message: Value) {
        self.write(format!("{message}\n").as_bytes());
    }

    pub(super) fn reply(&mut self) -> Result<Value> {
        let mut line = String::new();
        self.replies.read_line(&mut line)?;
        Ok(serde_json::from_str(&line)?)
    }

    /// The parent closes the connection, which ends `link accept`.
    pub(super) fn closed(mut self) -> Result {
        let mut rest = String::new();
        self.replies.read_to_string(&mut rest)?;
        self.process.wait()?;
        Ok(())
    }
}

pub(super) fn hello(host: &str, protocol: u64, version: &str) -> Value {
    json!({"type":"hello","protocol":protocol,"version":version,"host":host,"id":"raw"})
}

fn peer<'a>(status: &'a Value, host: &str) -> Option<&'a Value> {
    status["peers"]
        .as_array()?
        .iter()
        .find(|peer| peer["host"] == host)
}

fn incompatible_children_are_listed_as_such(parent: &Host) -> Result {
    for (host, protocol, version) in [("older", PROTOCOL, "0.0.1"), ("newer", 99, VERSION)] {
        let mut raw = Raw::open(parent)?;
        raw.say(hello(host, protocol, version));
        let reply = raw.reply()?;
        println!("REPLY TO {host}\n{reply}");
        assert_eq!(reply["type"], "reject");
        assert_eq!(reply["incompatible"], true);
        let reason = reply["reason"].as_str().ok_or("reason")?;
        assert!(reason.contains(&format!(
            "the parent runs ktask {VERSION} (protocol {PROTOCOL})"
        )));
        assert!(reason.contains(&format!("the child ktask {version} (protocol {protocol})")));
        raw.closed()?;
        let status = parent.status()?;
        let listed = peer(&status, host).ok_or("incompatible peer listed")?;
        assert_eq!(listed["incompatible"], true);
        assert_eq!(listed["connected"], false);
        assert!(
            listed["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("incompatible"))
        );
    }
    let reply = parent.ok(&["projects"])?;
    println!("WITH INCOMPATIBLE HOSTS\n{reply:#}");
    assert_eq!(reply["count"], 2, "an incompatible host adds no project");
    assert_eq!(reply["hosts"].as_array().map(Vec::len), Some(3));
    let text = parent.run(&["projects"], 0)?;
    println!("{text}");
    assert!(
        text.lines()
            .any(|l| l.starts_with("-\tincompatible\t") && l.contains("\tolder\tlast contact "))
    );

    Ok(())
}

fn nothing_is_recorded_before_a_valid_hello(parent: &Host) -> Result {
    // Before a valid hello nothing is recorded.
    for opening in [
        "not json",
        r#"{"type":"projects","projects":[]}"#,
        r#"{"type":"hello","protocol":1,"version":"x","host":"local","id":"raw"}"#,
        r#"{"type":"hello","protocol":1,"version":"x","host":"../../escape","id":"raw"}"#,
    ] {
        let mut raw = Raw::open(parent)?;
        raw.write(format!("{opening}\n").as_bytes());
        let reply = raw.reply()?;
        assert_eq!(reply["type"], "reject", "{opening}");
        raw.closed()?;
    }
    assert_eq!(parent.status()?["peers"].as_array().map(Vec::len), Some(3));

    Ok(())
}

fn a_bad_message_closes_only_its_connection(parent: &Host) -> Result {
    let project = |id: i64| json!({"id":id,"name":"raw-project","root":"/raw","state":"idle","runner_alive":false,"queued":0,"finished":0,"current":null});
    let oversized = vec![b'x'; (4 << 20) + 65_536];
    for (case, bad, error) in [
        ("garbage", b"{nonsense\n".to_vec(), "malformed message"),
        (
            "bounds",
            format!("{}\n", json!({"type":"projects","projects":[project(0)]})).into_bytes(),
            "malformed message",
        ),
        ("oversized", oversized, "oversized message"),
    ] {
        let mut raw = Raw::open(parent)?;
        raw.say(hello(case, PROTOCOL, VERSION));
        assert_eq!(raw.reply()?["type"], "welcome");
        raw.say(json!({"type":"projects","projects":[project(7)]}));
        parent.listing("raw project", |p| {
            p.iter().any(|p| p["host"] == case && p["online"] == true)
        })?;
        raw.write(&bad);
        raw.closed()?;
        let status = parent.until_status("closed", |s| {
            peer(s, case).is_some_and(|p| p["connected"] == false)
        })?;
        let closed = peer(&status, case).ok_or("peer")?;
        println!("{case}: {closed}");
        assert!(
            closed["last_error"]
                .as_str()
                .is_some_and(|e| e.contains(error))
        );
        // Only that connection: the other child is still there.
        let projects = parent.projects()?;
        assert_eq!(find(&projects, "steady", "good")["online"], true);
        assert_eq!(find(&projects, "raw-project", case)["online"], false);
        parent.ok(&["link", "forget", case])?;
    }

    Ok(())
}

/// The child's side: a parent of another version, a refusal, garbage.
fn the_child_reports_a_bad_parent(root: &Path) -> Result {
    // The child's side of a mismatch: a parent of another version.
    let odd = Host::new(root, "odd")?;
    odd.transport(
        "#!/bin/sh\necho '{\"type\":\"welcome\",\"protocol\":1,\"version\":\"9.9.9\"}'\nexec cat >/dev/null\n",
    )?;
    odd.ok(&["link", "parent", "me@parent", "--name", "odd"])?;
    let mut odd_run = odd.link_run()?;
    let status = odd.until_status("child reports the mismatch", |s| s["incompatible"] == true)?;
    println!("CHILD SIDE MISMATCH\n{status:#}");
    assert_eq!(status["connected"], false);
    assert!(
        status["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("the parent runs ktask 9.9.9"))
    );
    assert!(odd_run.alive()?);
    assert!(
        odd.run(&["link", "status"], 0)?
            .contains("incompatible\tyes")
    );
    // And a refusal, or a malformed or oversized message from the parent, only ends that
    // connection.
    for (script, error) in [
        (
            r#"echo '{"type":"reject","reason":"incompatible: refused","incompatible":true}'"#,
            "incompatible: refused",
        ),
        ("echo nonsense", "malformed message from the parent"),
        ("head -c 5000000 /dev/zero", "oversized message"),
    ] {
        odd.transport(&format!("#!/bin/sh\n{script}\nexec cat >/dev/null\n"))?;
        odd.until_status(error, |s| {
            s["last_error"].as_str().is_some_and(|e| e.contains(error))
        })?;
        assert!(odd_run.alive()?);
    }
    odd_run.stop()
}

#[test]
fn mismatched_and_bad_connections_are_reported_and_harm_no_other() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let parent = Host::new(root, "parent")?;
    parent.project("home")?;
    let good = Host::child(root, "good", &parent)?;
    good.project("steady")?;
    let hub = parent.link_run()?;
    let linked = good.link_run()?;
    parent.listing("good child", |p| all_online(p, 2))?;

    incompatible_children_are_listed_as_such(&parent)?;
    nothing_is_recorded_before_a_valid_hello(&parent)?;
    a_bad_message_closes_only_its_connection(&parent)?;

    // A second computer cannot take the name of a connected one.
    let mut raw = Raw::open(&parent)?;
    raw.say(hello("good", PROTOCOL, VERSION));
    let reply = raw.reply()?;
    assert!(
        reply["reason"]
            .as_str()
            .is_some_and(|r| r.contains("--name")),
        "{reply}"
    );
    raw.closed()?;
    assert_eq!(find(&parent.projects()?, "steady", "good")["online"], true);

    the_child_reports_a_bad_parent(root)?;
    linked.stop()?;
    hub.stop()
}

/// The list row of project `name` on `host`, without the details pane beside it.
fn row<'a>(screen: &'a str, name: &str, host: &str) -> &'a str {
    screen
        .lines()
        .filter_map(|line| line.split("││").next())
        .find(|row| {
            let rest = row.split_once(&format!(" {name} ")).map(|(_, rest)| rest);
            row.starts_with('│')
                && rest.and_then(|rest| rest.split_whitespace().next()) == Some(host)
        })
        .unwrap_or("")
}

/// Prints a screen into the check transcript and, in a supervised run, keeps its picture.
fn record(screen: &str, name: &str) -> Result {
    println!("{name}\n{screen}");
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(screen, &Path::new(&directory).join(format!("{name}.png")))?;
    }
    Ok(())
}

/// The details pane beside the list, its wrapped lines joined.
fn details(screen: &str) -> String {
    screen
        .lines()
        .filter_map(|line| line.split_once("││").map(|(_, pane)| pane))
        .map(|pane| pane.trim_end_matches('│').trim())
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn projects_screen_lists_local_and_remote_projects_by_keyboard_and_mouse() -> Result {
    for (columns, rows) in [(110, 30), (70, 22)] {
        for mouse in [false, true] {
            let sandbox = support::sandbox()?;
            let root = sandbox.path();
            let parent = Host::new(root, "parent")?;
            parent.project("alpha")?;
            parent.project("shared")?;
            let devbox = Host::child(root, "devbox", &parent)?;
            devbox.project("shared")?.create(
                &[definition("Approve the remote plan")],
                None,
                false,
            )?;
            let mut beta = devbox.project("beta")?;
            let laptop = Host::child(root, "laptop", &parent)?;
            laptop.project("gamma")?;
            let hub = parent.link_run()?;
            let first = devbox.link_run()?;
            let second = laptop.link_run()?;
            parent.listing("linked", |p| all_online(p, 5))?;

            let mut t =
                Terminal::open_project(DEV, &parent.dir, &parent.dir.join("alpha"), columns, rows)?;
            let header = t.screen("header counts remote projects", |s| s.contains("/ alpha"))?;
            if columns > 100 {
                assert!(
                    header.contains("Projects: 1 need you · 0 running"),
                    "{header}"
                );
            }
            if mouse {
                t.click("0 Projects")?;
            } else {
                t.send("0")?;
            }
            let listed = t.screen("flat list", |s| {
                s.contains("Projects · 5 registered · 0 running · 1 need you")
                    && !row(s, "gamma", "laptop").is_empty()
            })?;
            let case = format!("link-{columns}x{rows}-mouse-{mouse}");
            record(&listed, &format!("{case}-1-flat-list"))?;
            let order = [
                ("alpha", "local", "Idle"),
                ("beta", "devbox", "Idle"),
                ("gamma", "laptop", "Idle"),
                ("shared", "devbox", "Needs approval"),
                ("shared", "local", "Idle"),
            ];
            let mut last = 0;
            for (name, host, state) in order {
                let line = row(&listed, name, host);
                assert!(line.contains(state), "{name}@{host}: {line:?}\n{listed}");
                let at = listed.find(line).ok_or("row position")?;
                assert!(at > last, "{name}@{host} out of order\n{listed}");
                last = at;
            }

            // Select the remote project that needs its owner.
            if mouse {
                t.click("shared devbox")?;
            } else {
                t.send("jjj")?;
            }
            let selected = t.screen("remote selected", |s| {
                s.contains("Projects · 4 / 5") && s.contains("Project · linked")
            })?;
            record(&selected, &format!("{case}-2-remote-selected"))?;
            let pane = details(&selected);
            assert!(pane.contains("Host: devbox · online"), "{selected}");
            assert!(pane.contains("State: Needs approval"), "{selected}");
            assert!(pane.contains("#1 on devbox · Approve"), "{selected}");
            assert!(pane.contains("Queue: 1 unfinished"), "{selected}");
            // It opens like a local one; moving and deleting it stay on its host.
            assert!(selected.contains("[Enter Open]"), "{selected}");
            for absent in ["[m Move]", "[D Delete]", "[f Fix]"] {
                assert!(!selected.contains(absent), "{absent}\n{selected}");
            }
            if !mouse {
                t.send("m")?;
                t.screen("move is local-only", |s| {
                    s.contains("Local-only: this project is on devbox")
                })?;
            }

            first.stop()?;
            let offline = t.screen("devbox offline", |s| {
                row(s, "beta", "devbox").contains("Offline")
                    && row(s, "shared", "devbox").contains("Offline")
                    && s.contains("last contact")
                    && s.contains("0 need you")
            })?;
            record(&offline, &format!("{case}-3-offline"))?;
            assert!(row(&offline, "gamma", "laptop").contains("Idle"));
            let pane = details(&offline);
            assert!(pane.contains("Host: devbox · offline"), "{offline}");
            assert!(pane.contains("last contact "), "{offline}");
            // Never opened here, so it waits for its host.
            if mouse {
                t.click("Host: devbox")?;
            } else {
                t.send("\r")?;
            }
            let refused = t.screen("waits for its host", |s| {
                s.contains("devbox is offline · shared opens")
            })?;
            assert!(refused.contains("/ alpha"), "{refused}");

            let first = devbox.link_run()?;
            beta.create(&[definition("Work while away")], None, true)?;
            let back = t.screen("devbox returns with its change", |s| {
                row(s, "beta", "devbox").contains("Ready") && s.contains("1 need you")
            })?;
            record(&back, &format!("{case}-4-returned"))?;

            // A local project still opens.
            if mouse {
                t.click("shared local")?;
            } else {
                t.send("G")?;
            }
            t.screen("local selected", |s| s.contains("Projects · 5 / 5"))?;
            if mouse {
                t.click("Enter Open")?;
            } else {
                t.send("\r")?;
            }
            let opened = t.screen("the local project opens", |s| s.contains("/ shared "))?;
            record(&opened, &format!("{case}-5-local-opened"))?;
            t.send("q")?;
            t.finish()?;
            first.stop()?;
            second.stop()?;
            hub.stop()?;
        }
    }
    Ok(())
}

/// A parent listing two incompatible hosts: `kept` reported a project before its protocol
/// changed, `older` never did.
fn incompatible_hosts(parent: &Host) -> Result {
    // The hub may still be starting; until then link accept refuses.
    let mut linked = support::eventually("the hub welcomes", || {
        let mut raw = Raw::open(parent)?;
        raw.say(hello("kept", PROTOCOL, VERSION));
        if raw.reply()?["type"] == "welcome" {
            return Ok(Some(raw));
        }
        raw.closed()?;
        Ok(None)
    })?;
    // It would need its owner, were its host linked.
    linked.say(
        json!({"type":"projects","projects":[{"id":7,"name":"beta","root":"/srv/beta",
        "state":"needs_owner","runner_alive":false,"queued":1,"finished":0,
        "current":{"id":3,"title":"Approve it","kind":"ai","state":"needs_approval"}}]}),
    );
    parent.listing("kept project", |p| p.iter().any(|p| p["host"] == "kept"))?;
    drop(linked.process.stdin.take());
    linked.closed()?;
    for (host, protocol, version) in [("kept", 99, VERSION), ("older", PROTOCOL, "0.0.1")] {
        let mut raw = Raw::open(parent)?;
        raw.say(hello(host, protocol, version));
        assert_eq!(raw.reply()?["incompatible"], true, "{host}");
        raw.closed()?;
    }
    Ok(())
}

/// The details of a selected incompatible row: read-only, and why the host cannot link.
fn assert_incompatible_details(screen: &str, host: &str, child: &str) {
    let pane = details(screen);
    assert!(
        pane.contains(&format!("Host: {host} · incompatible")),
        "{screen}"
    );
    assert!(pane.contains("last contact "), "{screen}");
    let reason =
        format!("the parent runs ktask {VERSION} (protocol {PROTOCOL}), the child {child}");
    assert!(pane.contains(&reason), "{reason}\n{screen}");
    for absent in ["[Enter Open]", "[m Move]", "[D Delete]", "[f Fix]"] {
        assert!(!screen.contains(absent), "{absent}\n{screen}");
    }
}

#[test]
fn projects_screen_lists_incompatible_hosts_by_keyboard_and_mouse() -> Result {
    for (columns, rows) in [(110, 30), (70, 22)] {
        for mouse in [false, true] {
            let sandbox = support::sandbox()?;
            let parent = Host::new(sandbox.path(), "parent")?;
            parent.project("alpha")?;
            let hub = parent.link_run()?;
            incompatible_hosts(&parent)?;

            let mut t =
                Terminal::open_project(DEV, &parent.dir, &parent.dir.join("alpha"), columns, rows)?;
            t.screen("alpha opens", |s| s.contains("/ alpha"))?;
            if mouse {
                t.click("0 Projects")?;
            } else {
                t.send("0")?;
            }
            // The host without projects is no project, and neither host counts as work.
            let listed = t.screen("incompatible hosts listed", |s| {
                s.contains("Projects · 2 registered · 0 running · 0 need you")
                    && row(s, "beta", "kept").contains("Incompatible")
            })?;
            let case = format!("link-incompatible-{columns}x{rows}-mouse-{mouse}");
            record(&listed, &format!("{case}-1-list"))?;
            assert!(
                row(&listed, "(no projects)", "older").contains("Incompatible"),
                "{listed}"
            );

            for (step, name, host, child) in [
                (2, "beta", "kept", format!("ktask {VERSION} (protocol 99)")),
                (
                    4,
                    "(no projects)",
                    "older",
                    format!("ktask 0.0.1 (protocol {PROTOCOL})"),
                ),
            ] {
                if mouse {
                    t.click(name)?;
                } else {
                    // Sorted by name: the placeholder, then alpha (open), then beta.
                    t.send(if host == "kept" { "j" } else { "g" })?;
                }
                let selected = t.screen(host, |s| {
                    s.contains("Project · read-only") && s.contains(&format!("Host: {host}"))
                })?;
                record(&selected, &format!("{case}-{step}-{host}-selected"))?;
                assert_incompatible_details(&selected, host, &child);

                if mouse {
                    t.click(&format!("Host: {host}"))?;
                } else {
                    t.send("\r")?;
                }
                let refused = t.screen("read-only", |s| {
                    s.contains(&format!("Read-only here: this project is on {host}"))
                })?;
                record(&refused, &format!("{case}-{}-{host}-refused", step + 1))?;
                assert!(refused.contains("/ alpha"), "{refused}");
            }
            t.send("q")?;
            t.finish()?;
            hub.stop()?;
        }
    }
    Ok(())
}
