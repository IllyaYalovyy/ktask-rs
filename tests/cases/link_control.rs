//! Task commands run from a parent on a child instance (`--host`), with the same SSH stand-in
//! as the link tests. A second, unlinked computer set up like the child runs the same
//! commands locally, so what the parent shows is compared with what a local run prints.
use super::link::{Host, Linked, PROTOCOL, Raw, all_online, hello};
use crate::support::{self, Result};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    fs,
    path::PathBuf,
    process::{Command, Stdio},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// A stand-in for Codex. `$HOME/agent` holds how it behaves: `hold` never ends a turn and
/// leaves `$HOME/holding`, `ask` asks a question; otherwise it does the work and every
/// judgment passes.
const AGENT: &str = r##"#!/bin/sh
model=unknown
while [ "$#" -gt 0 ]; do case "$1" in -m) shift; model="$1";; esac; shift; done
prompt=$(cat)
printf '{"type":"thread.started","thread_id":"s-%s-%s"}\n' "$model" "$$"
mode=$(cat "$HOME/agent" 2>/dev/null)
if [ "$mode" = hold ]; then touch "$HOME/holding"; sleep 30; fi
if [ "$mode" = ask ] && [ "$model" = execution ]; then
 printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}"}}' '{"type":"turn.completed","usage":{}}'
 exit 0
fi
if [ "$model" = execution ]; then printf 'verified %s\n' "$$" >> result.txt; fi
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"outcome\":\"complete\",\"summary\":\"Verified result\",\"question\":null,\"criteria\":[{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected result.txt\"}]}"}}' '{"type":"turn.completed","usage":{}}'
"##;

/// What a command ended with.
#[derive(Debug, PartialEq, Eq)]
struct Ended {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn end(mut command: Command) -> Result<Ended> {
    let output = command.stdin(Stdio::null()).output()?;
    Ok(Ended {
        status: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

/// `ktask args` on this computer.
fn local(host: &Host, args: &[&str]) -> Result<Ended> {
    let mut command = host.command();
    command.args(args);
    end(command)
}

/// `ktask --host devbox --project web args` on the parent.
fn remote(parent: &Host, args: &[&str]) -> Result<Ended> {
    let mut all = vec!["--host", "devbox", "--project", "web"];
    all.extend(args);
    local(parent, &all)
}

fn git(folder: &PathBuf, args: &[&str]) -> Result {
    let output = Command::new("git")
        .arg("-C")
        .arg(folder)
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(())
}

/// A project `web` on `host` that can run tasks with the stand-in agent.
fn web(host: &Host) -> Result {
    support::write_executable(&host.dir.join("bin/codex"), AGENT)?;
    let folder = host.dir.join("web");
    fs::create_dir(&folder)?;
    fs::write(folder.join("README.md"), "A project\n")?;
    git(&folder, &["init", "-q"])?;
    git(&folder, &["config", "user.name", "Project Author"])?;
    git(&folder, &["config", "user.email", "project@example.com"])?;
    git(&folder, &["add", "README.md"])?;
    git(&folder, &["commit", "-qm", "baseline"])?;
    let on = |args: &[&str]| -> Result {
        let mut all = vec!["--project", "web"];
        all.extend(args);
        let ended = local(host, &all)?;
        // init saves first; exit 4 only lists what is still missing.
        assert!(matches!(ended.status, Some(0 | 4)), "{args:?} {ended:?}");
        Ok(())
    };
    on(&["check-set", "build", "--command", "test -f result.txt"])?;
    for role in ["execution", "review", "validation", "remediation"] {
        on(&[
            "model-default",
            role,
            "--provider",
            "codex",
            "--model",
            role,
        ])?;
    }
    on(&["init", "--check-before-task", "off"])
}

fn agent(host: &Host, mode: &str) -> Result {
    Ok(fs::write(host.dir.join("agent"), mode)?)
}

/// Until an agent turn on `host` is under way and will not end by itself.
fn holding(host: &Host) -> Result {
    support::eventually("the agent holds its turn", || {
        Ok(fs::remove_file(host.dir.join("holding")).ok())
    })
}

/// A parent, its child `devbox` and an unlinked `mirror` set up like the child.
struct World {
    parent: Host,
    devbox: Host,
    mirror: Host,
    hub: Linked,
    child: Linked,
    _sandbox: support::Sandbox,
}

impl World {
    fn new() -> Result<Self> {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        let parent = Host::new(root, "parent")?;
        let devbox = Host::child(root, "devbox", &parent)?;
        let mirror = Host::new(root, "mirror")?;
        web(&devbox)?;
        web(&mirror)?;
        let hub = parent.link_run()?;
        let child = devbox.link_run()?;
        parent.listing("devbox linked", |p| all_online(p, 1))?;
        Ok(Self {
            parent,
            devbox,
            mirror,
            hub,
            child,
            _sandbox: sandbox,
        })
    }

    fn stop(self) -> Result {
        self.child.stop()?;
        self.hub.stop()
    }

    /// Runs `args` from the parent on the child, and locally on the mirror; both must end
    /// the same way, with the same output.
    fn same(&self, status: i32, args: &[&str]) -> Result<Ended> {
        let there = remote(&self.parent, args)?;
        let mut here = vec!["--project", "web"];
        here.extend(args);
        let here = local(&self.mirror, &here)?;
        println!("REMOTE {args:?}\n{there:?}");
        assert_eq!(there.status, Some(status), "{args:?}\n{there:?}");
        assert_eq!(there, here, "{args:?}");
        Ok(there)
    }

    /// A read of the child's own project: from the parent it prints what the child prints.
    fn reads(&self, args: &[&str]) -> Result<String> {
        let mut here = vec!["--project", "web"];
        here.extend(args);
        let here = local(&self.devbox, &here)?;
        let there = remote(&self.parent, args)?;
        println!("REMOTE {args:?}\n{there:?}");
        assert_eq!(there, here, "{args:?}");
        Ok(there.stdout)
    }

    /// A command from the parent expected to succeed; its JSON data.
    fn ok(&self, args: &[&str]) -> Result<Value> {
        let mut all = vec!["--json"];
        all.extend(args);
        let ended = remote(&self.parent, &all)?;
        println!("REMOTE {args:?}\n{ended:?}");
        assert_eq!(ended.status, Some(0), "{args:?}\n{ended:?}");
        Ok(serde_json::from_str::<Value>(&ended.stdout)?["data"].clone())
    }

    fn database(&self) -> Result<Connection> {
        Ok(Connection::open(
            self.devbox.dir.join("state/ktask-dev/ktask.sqlite3"),
        )?)
    }

    /// One column of a query on the child's records.
    fn column(&self, sql: &str) -> Result<Vec<String>> {
        let database = self.database()?;
        let mut statement = database.prepare(sql)?;
        let rows = statement
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    fn requests(&self) -> Result<Vec<String>> {
        self.column("SELECT message FROM events WHERE kind='remote.request' ORDER BY id")
    }

    /// Until task `id` of `host`'s project is in `state`.
    fn until(&self, host: &Host, id: &str, state: &str) -> Result {
        support::eventually(&format!("task {id} {state}"), || {
            let ended = local(host, &["--json", "--project", "web", "show", id])?;
            let shown: Value = serde_json::from_str(&ended.stdout)?;
            Ok((shown["data"]["state"] == state).then_some(()))
        })
    }

    /// Until no runner of either project is alive.
    fn idle(&self) -> Result {
        for host in [&self.devbox, &self.mirror] {
            support::eventually("runner exits", || {
                Ok((host.projects()?[0]["runner_alive"] == false).then_some(()))
            })?;
        }
        Ok(())
    }
}

const NEW_TASK: [&str; 7] = [
    "add",
    "--title",
    "Write the result",
    "--goal",
    "Leave result.txt",
    "--criterion",
    "result.txt exists",
];

fn add<'a>(extra: &[&'a str]) -> Vec<&'a str> {
    NEW_TASK.iter().chain(extra).copied().collect()
}

#[test]
fn task_changes_from_the_parent_match_local_ones_including_version_guards() -> Result {
    let world = World::new()?;

    world.same(0, &add(&[]))?;
    let mut json = add(&["--approval", "draft"]);
    json.insert(0, "--json");
    world.same(0, &json)?;
    world.same(0, &add(&["--approval", "draft"]))?;
    world.same(0, &add(&[]))?;

    // The version guard is the child's own: a stale number changes nothing.
    for args in [
        &["approve", "2", "--if-version", "9"][..],
        &["reject", "3", "--reason", "no", "--if-version", "9"],
        &["edit", "1", "--title", "Stale", "--if-version", "9"],
        &["stop", "1", "--if-version", "9"],
        &["restart", "1", "--if-version", "9"],
        &["done", "1", "--if-version", "9"],
        &["cancel", "4", "--if-version", "9"],
        &["answer", "1", "--text", "x", "--if-version", "9"],
    ] {
        let stale = world.same(4, args)?;
        assert!(stale.stderr.contains("not 9") || stale.stderr.contains("version"));
    }
    world.same(4, &["--json", "approve", "2", "--if-version", "9"])?;

    world.same(0, &["approve", "2", "--if-version", "1"])?;
    world.same(0, &["--json", "reject", "3", "--reason", "Not yet"])?;
    world.same(
        0,
        &["edit", "1", "--title", "Better title", "--if-version", "1"],
    )?;
    world.same(4, &["edit", "1", "--title", "Late", "--if-version", "1"])?;
    world.same(0, &["--json", "cancel", "4", "--reason", "Not needed"])?;
    world.same(0, &["done", "2", "--reason", "Done by hand"])?;
    // Refusals of the state machine, missing tasks and usage errors are the local ones.
    world.same(4, &["cancel", "4"])?;
    world.same(4, &["answer", "1", "--text", "Nothing was asked"])?;
    world.same(3, &["--json", "show", "99"])?;
    world.same(3, &["approve", "99"])?;
    world.same(2, &["edit", "1"])?;
    world.same(2, &["--actor-kind", "ai", "approve", "1"])?;
    world.same(0, &["pause"])?;
    world.same(0, &["--json", "pause"])?;

    for args in [
        &["list"][..],
        &["--json", "list", "--scope", "all"],
        &["list", "--search", "better"],
        &["current"],
        &["--json", "current"],
        &["show", "1"],
        &["--json", "show", "2"],
        &["events"],
        &["--json", "events", "--after", "2", "--limit", "3"],
        &["stats"],
        &["--json", "stats"],
    ] {
        assert!(!world.reads(args)?.is_empty(), "{args:?}");
    }
    // The project's ID names it as well as its name does.
    let by_id = local(
        &world.parent,
        &["--host", "devbox", "--project", "1", "list"],
    )?;
    assert_eq!(by_id.stdout, world.reads(&["list"])?);

    // A command line and an answer larger than one message arrive whole.
    let long = "é".repeat(60_000);
    let criteria: Vec<&str> = std::iter::repeat_n(["--criterion", long.as_str()], 4)
        .flatten()
        .collect();
    world.same(0, &add(&criteria))?;
    let shown = world.reads(&["--json", "show", "5"])?;
    assert!(shown.len() > 900_000, "{}", shown.len());

    for (args, status, message) in [
        (&["--project", "web", "list"][..], 0, ""),
        (&["--project", "nowhere", "list"], 3, "no project with"),
        (&["--project", "web", "init"], 2, "never accepted remotely"),
        (&["list"], 2, "--host requires --project"),
        (
            &["--project", "web", "--project-name", "x", "list"],
            2,
            "Only list",
        ),
    ] {
        let mut all = vec!["--host", "devbox"];
        all.extend(args);
        let ended = local(&world.parent, &all)?;
        assert_eq!(ended.status, Some(status), "{all:?} {ended:?}");
        assert!(ended.stderr.contains(message), "{all:?} {ended:?}");
    }
    // The parent has no copy of anything: it never opened a database of its own.
    assert!(
        !world
            .parent
            .dir
            .join("state/ktask-dev/ktask.sqlite3")
            .exists()
    );
    world.stop()
}

#[test]
fn run_pause_stop_restart_and_answer_drive_the_childs_runner() -> Result {
    let world = World::new()?;
    world.same(0, &add(&[]))?;
    for host in [&world.devbox, &world.mirror] {
        agent(host, "hold")?;
    }

    world.same(0, &["--json", "run", "--background"])?;
    for host in [&world.devbox, &world.mirror] {
        holding(host)?;
        world.until(host, "1", "running")?;
    }
    world.same(0, &["pause"])?;
    world.until(&world.devbox, "1", "paused")?;
    world.idle()?;
    world.same(0, &["stop", "1", "--reason", "Start over"])?;
    world.until(&world.devbox, "1", "pending")?;

    world.same(0, &["run", "--background"])?;
    for host in [&world.devbox, &world.mirror] {
        holding(host)?;
        // The held turn ends only when restart stops it; the fresh one does the work.
        agent(host, "work")?;
    }
    let restarted = world.same(0, &["restart", "1"])?;
    assert!(restarted.stdout.starts_with("restarted: #1\n"));
    world.until(&world.devbox, "1", "done")?;
    world.idle()?;

    // A foreground run blocks until the owner is needed, as it does locally.
    for host in [&world.devbox, &world.mirror] {
        agent(host, "ask")?;
    }
    world.same(0, &add(&[]))?;
    world.same(0, &["--json", "run", "--one"])?;
    world.until(&world.devbox, "2", "needs_input")?;
    world.same(4, &["run"])?;
    for host in [&world.devbox, &world.mirror] {
        agent(host, "work")?;
    }
    world.same(0, &["answer", "2", "--text", "Option B please"])?;
    for host in [&world.devbox, &world.mirror] {
        world.until(host, "2", "done")?;
    }
    world.idle()?;
    world.reads(&["--json", "show", "2"])?;

    let requests = world.requests()?;
    println!("REQUESTS {requests:#?}");
    for expected in [
        "run requested by human owner via me@parent",
        "pause requested by human owner via me@parent",
        "stop #1 requested by human owner via me@parent",
        "restart #1 requested by human owner via me@parent",
        "answer #2 requested by human owner via me@parent",
    ] {
        assert!(requests.iter().any(|r| r == expected), "{expected}");
    }
    assert_eq!(
        world.column("SELECT answered_by FROM questions")?,
        ["owner via me@parent"]
    );
    world.stop()
}

#[test]
fn changes_record_who_acted_and_the_parent_they_came_from() -> Result {
    let world = World::new()?;
    let ai = ["--actor-kind", "ai", "--actor", "claude-code"];
    let by_ai = |args: &[&str]| -> Result<Value> {
        let mut all = ai.to_vec();
        all.extend(args);
        world.ok(&all)
    };
    let named = ["--actor", "Dana"];

    by_ai(&add(&["--approval", "draft"]))?;
    let mut human = named.to_vec();
    human.extend(add(&["--approval", "draft"]));
    world.ok(&human)?;
    world.ok(&add(&["--approval", "draft"]))?;
    world.ok(&add(&[]))?;
    by_ai(&["approve", "1"])?;
    world.ok(&["--actor", "Dana", "approve", "2"])?;
    by_ai(&["reject", "3", "--reason", "Too vague"])?;
    by_ai(&["cancel", "4", "--reason", "Dropped"])?;
    world.ok(&["done", "2"])?;
    by_ai(&["edit", "1", "--title", "Edited from afar"])?;

    // An AI-issued approval is recorded as AI, never as human.
    assert_eq!(
        world.column(
            "SELECT task_id || ' ' || decided_by_kind || ' ' || decided_by FROM task_revisions WHERE decided_by IS NOT NULL ORDER BY task_id, revision"
        )?,
        [
            "1 ai claude-code via me@parent",
            "1 ai claude-code via me@parent",
            "2 human Dana via me@parent",
            "3 ai claude-code via me@parent",
            "4 human owner via me@parent",
        ]
    );
    let shown = world.ok(&["show", "1"])?;
    let authors: Vec<_> = shown["revisions"]
        .as_array()
        .ok_or("revisions")?
        .iter()
        .map(|revision| revision["actor"].to_string())
        .collect();
    assert_eq!(authors.len(), 2, "{shown}");
    assert!(
        authors
            .iter()
            .all(|a| a.contains("claude-code via me@parent") && !a.to_lowercase().contains("human")),
        "{authors:?}"
    );
    let closed = |id: &str| -> Result<String> {
        Ok(world.ok(&["show", id])?["lifecycle_events"].to_string())
    };
    assert!(closed("2")?.contains("human: owner via me@parent"));
    assert!(closed("4")?.contains("ai: claude-code via me@parent"));
    assert_eq!(
        world.requests()?[4..6],
        [
            "approve #1 requested by ai claude-code via me@parent",
            "approve #2 requested by human Dana via me@parent",
        ]
    );
    // Reading leaves no record.
    let before = world.requests()?.len();
    world.ok(&["list"])?;
    world.ok(&["show", "1"])?;
    assert_eq!(world.requests()?.len(), before);
    world.stop()
}

/// Every changing command, against a task that would accept most of them.
const CHANGES: [&[&str]; 11] = [
    &["add", "--title", "t", "--goal", "g", "--criterion", "c"],
    &["edit", "1", "--title", "Changed"],
    &["approve", "1"],
    &["reject", "1", "--reason", "No"],
    &["run"],
    &["pause"],
    &["stop", "1"],
    &["restart", "1"],
    &["done", "1"],
    &["cancel", "1"],
    &["answer", "1", "--text", "Yes"],
];

#[test]
fn control_off_makes_the_link_read_only_until_turned_on() -> Result {
    let world = World::new()?;
    world.ok(&add(&["--approval", "draft"]))?;
    assert_eq!(world.devbox.status()?["control"], true);
    assert_eq!(
        world.devbox.ok(&["link", "control", "off"])?,
        json!({"control":false,"parent":"me@parent"})
    );
    assert_eq!(world.devbox.status()?["control"], false);
    assert!(
        world
            .devbox
            .run(&["link", "status"], 0)?
            .contains("control\toff")
    );
    // Naming the parent again does not give control back.
    world.devbox.ok(&["link", "parent", "me@parent"])?;
    assert_eq!(world.devbox.status()?["control"], false);

    let before = (world.reads(&["--json", "show", "1"])?, world.requests()?);
    for args in CHANGES {
        let mut all = vec!["--json"];
        all.extend(args);
        let refused = remote(&world.parent, &all)?;
        println!("READ ONLY {args:?}\n{refused:?}");
        assert_eq!(refused.status, Some(4), "{args:?} {refused:?}");
        assert!(refused.stdout.is_empty());
        let error: Value = serde_json::from_str(&refused.stderr)?;
        assert_eq!(error["error"]["code"], "read_only", "{args:?}");
    }
    // The child's own CLI is not limited.
    for args in [
        &["list"][..],
        &["current"],
        &["show", "1"],
        &["events"],
        &["stats"],
    ] {
        world.reads(args)?;
    }
    assert_eq!(
        (world.reads(&["--json", "show", "1"])?, world.requests()?),
        before
    );
    let own = local(&world.devbox, &["--project", "web", "approve", "1"])?;
    assert_eq!(own.status, Some(0), "{own:?}");

    let on = world.devbox.run(&["link", "control", "on"], 0)?;
    assert!(on.starts_with("Control on: me@parent"), "{on}");
    // The running link needs no restart.
    world.ok(&["cancel", "1"])?;
    world.parent.run(&["link", "control"], 4)?;
    world.stop()
}

/// What a stand-in parent that sends whatever it likes was told for each command.
fn answers(devbox: &Host, count: usize) -> Result<Vec<(u64, String)>> {
    support::eventually("every command answered", || {
        let said = fs::read_to_string(devbox.dir.join("replies")).unwrap_or_default();
        let mut ended = Vec::new();
        let mut text = std::collections::BTreeMap::<u64, String>::new();
        for line in said.lines() {
            let Ok(message) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let (Some(id), part) = (message["id"].as_u64(), &message["part"]) else {
                continue;
            };
            for stream in ["stdout", "stderr"] {
                if let Some(piece) = part[stream].as_str() {
                    text.entry(id).or_default().push_str(piece);
                }
            }
            assert!(part["refused"].is_null(), "{message}");
            if let Some(status) = part["exit"].as_u64() {
                ended.push((
                    id,
                    format!("{status} {}", text.remove(&id).unwrap_or_default()),
                ));
            }
        }
        ended.sort();
        Ok((ended.len() >= count).then_some(ended))
    })
}

#[test]
fn a_child_itself_refuses_what_no_parent_may_send() -> Result {
    let sandbox = support::sandbox()?;
    let devbox = Host::new(sandbox.path(), "devbox")?;
    web(&devbox)?;
    devbox.ok(&["link", "parent", "me@parent", "--name", "devbox"])?;
    let refused: Vec<Vec<&str>> = vec![
        vec!["project-delete", "1", "--confirm", "web"],
        vec!["project-move", "1", "/tmp"],
        vec!["shutdown"],
        vec!["pause", "--all"],
        vec!["--project", "web", "tool", "list"],
        vec!["--project", "web", "tool", "provision-mode", "automatic"],
        vec![
            "--project",
            "web",
            "tool",
            "provision-install",
            "android",
            "1",
        ],
        vec![
            "--project",
            "web",
            "tool",
            "provision-consent",
            "android",
            "1",
        ],
        vec!["--project", "web", "init"],
        vec![
            "--project",
            "web",
            "check-set",
            "build",
            "--command",
            "true",
        ],
        vec!["link", "control", "on"],
        vec!["link", "parent", "--clear"],
        vec!["evidence", "/etc/hostname"],
        vec!["--paths"],
        vec!["--project", "web", "run", "--notify"],
        // Naming another parent in the command line does not replace the real one.
        vec!["--from-parent", "me@parent", "--project", "web", "list"],
    ];
    let commands: String = refused
        .iter()
        .chain(std::iter::once(&vec!["--project", "web", "list"]))
        .enumerate()
        .map(|(id, args)| format!("{}\n", json!({"type":"command","id":id,"args":args})))
        .collect();
    fs::write(devbox.dir.join("commands"), commands)?;
    devbox.transport(&format!(
        "#!/bin/sh\necho '{{\"type\":\"welcome\",\"protocol\":{PROTOCOL},\"version\":\"{VERSION}\"}}'\ncat \"$HOME/commands\"\n# Descriptor 3 keeps the channel to the child open.\nexec 3>&1\nexec cat >>\"$HOME/replies\"\n"
    ))?;
    let before = devbox.ok(&["projects"])?;
    let linked = devbox.link_run()?;
    let answered = answers(&devbox, refused.len() + 1)?;
    linked.stop()?;
    for (id, answer) in &answered[..refused.len()] {
        let args = &refused[*id as usize];
        println!("REFUSED {args:?}\n{answer}");
        assert!(answer.starts_with("2 usage: "), "{args:?} {answer}");
    }
    assert_eq!(answered.last().map(|a| a.1.as_str()), Some("0 No tasks.\n"));
    assert_eq!(devbox.ok(&["projects"])?, before);
    assert_eq!(devbox.status()?["peer"], "me@parent");

    // Only the configured parent's commands are admitted.
    let from = |parent: &str| -> Result<Ended> {
        local(
            &devbox,
            &["--from-parent", parent, "--project", "web", "list"],
        )
    };
    assert_eq!(from("me@parent")?.status, Some(0));
    let other = from("someone@else")?;
    assert_eq!(other.status, Some(4), "{other:?}");
    assert!(other.stderr.contains("only from the parent"), "{other:?}");
    devbox.ok(&["link", "parent", "new@parent"])?;
    assert_eq!(from("me@parent")?.status, Some(4));
    devbox.ok(&["link", "parent", "--clear"])?;
    assert_eq!(from("new@parent")?.status, Some(4));
    Ok(())
}

fn failed(ended: &Ended, status: i32, code: &str) -> String {
    assert_eq!(ended.status, Some(status), "{ended:?}");
    assert!(ended.stdout.is_empty(), "{ended:?}");
    let error: Value = serde_json::from_str(&ended.stderr).expect("a JSON error");
    assert_eq!(error["error"]["code"], code, "{ended:?}");
    error["error"]["message"].as_str().unwrap_or("").to_owned()
}

/// A child that takes a command and goes away before answering it.
fn linked_raw(parent: &Host) -> Result<Raw> {
    support::eventually("the hub welcomes", || {
        let mut raw = Raw::open(parent)?;
        raw.say(hello("raw", PROTOCOL, VERSION));
        if raw.reply()?["type"] == "welcome" {
            return Ok(Some(raw));
        }
        raw.closed()?;
        Ok(None)
    })
}

/// Sends a change to `raw` and returns the waiting parent and what the child was sent.
fn send_to_raw(parent: &Host, raw: &mut Raw) -> Result<(std::process::Child, Value)> {
    let waiting = parent
        .command()
        .args(["--json", "--host", "raw", "--project", "1", "approve", "1"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let sent = raw.reply()?;
    Ok((waiting, sent))
}

fn ended(waiting: std::process::Child) -> Result<Ended> {
    let output = waiting.wait_with_output()?;
    Ok(Ended {
        status: output.status.code(),
        stdout: String::from_utf8(output.stdout)?,
        stderr: String::from_utf8(output.stderr)?,
    })
}

#[test]
fn offline_incompatible_and_lost_hosts_fail_distinctly_and_nothing_is_retried() -> Result {
    let sandbox = support::sandbox()?;
    let parent = Host::new(sandbox.path(), "parent")?;
    let to = |host: &str| -> Result<Ended> {
        local(
            &parent,
            &["--json", "--host", host, "--project", "1", "approve", "1"],
        )
    };
    failed(&to("nobody")?, 3, "not_found");
    // A worker cannot reach another host any more than it can change this one.
    let mut worker = parent.command();
    worker.env("KTASK_WORKER_CALLBACK", "1").args([
        "--json",
        "--host",
        "nobody",
        "--project",
        "1",
        "approve",
        "1",
    ]);
    assert!(failed(&end(worker)?, 2, "usage").starts_with("Workers may only"));

    let hub = parent.link_run()?;
    let mut raw = linked_raw(&parent)?;
    let (waiting, sent) = send_to_raw(&parent, &mut raw)?;
    assert_eq!(sent["type"], "command");
    assert_eq!(
        sent["args"],
        json!(["--json", "--host", "raw", "--project", "1", "approve", "1"])
    );
    drop(raw.process.stdin.take());
    let lost = failed(&ended(waiting)?, 6, "outcome_unknown");
    println!("LOST {lost}");
    assert!(lost.contains("outcome is unknown") && lost.contains("nothing is retried"));
    raw.closed()?;

    // The child returns and is not sent the command again.
    let mut raw = linked_raw(&parent)?;
    raw.say(json!({"type":"heartbeat"}));
    assert_eq!(raw.reply()?, json!({"type":"heartbeat"}));

    // The parent's own hub going away leaves the outcome just as unknown.
    let (waiting, _) = send_to_raw(&parent, &mut raw)?;
    hub.stop()?;
    failed(&ended(waiting)?, 6, "outcome_unknown");
    raw.closed()?;

    // Without a hub, and with a hub but a child that left, the host is offline.
    let offline = failed(&to("raw")?, 5, "host_offline");
    println!("OFFLINE {offline}");
    assert!(offline.contains("raw is offline") && offline.contains("approve was not sent"));
    let hub = parent.link_run()?;
    linked_raw(&parent).and_then(|mut raw| {
        drop(raw.process.stdin.take());
        raw.closed()
    })?;
    parent.until_status("raw gone", |s| s["connected"] == false)?;
    failed(&to("raw")?, 5, "host_offline");

    let mut old = Raw::open(&parent)?;
    old.say(hello("older", PROTOCOL, "0.0.1"));
    assert_eq!(old.reply()?["incompatible"], true);
    old.closed()?;
    let incompatible = failed(&to("older")?, 5, "host_incompatible");
    println!("INCOMPATIBLE {incompatible}");
    assert!(incompatible.contains("ktask 0.0.1"), "{incompatible}");
    let text = local(&parent, &["--host", "older", "--project", "1", "list"])?;
    assert_eq!(text.status, Some(5), "{text:?}");
    assert!(
        text.stderr.starts_with("host_incompatible: older"),
        "{text:?}"
    );
    hub.stop()
}

#[test]
fn a_run_whose_connection_is_lost_goes_on_once_on_the_child() -> Result {
    let mut world = World::new()?;
    world.ok(&add(&[]))?;
    agent(&world.devbox, "hold")?;
    let waiting = world
        .parent
        .command()
        .args(["--json", "--host", "devbox", "--project", "web", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    holding(&world.devbox)?;
    // The link's process dies outright, as with a lost network or a crash.
    world.child.process.kill()?;
    world.child.process.wait()?;
    failed(&ended(waiting)?, 6, "outcome_unknown");

    world.child = world.devbox.link_run()?;
    world.parent.listing("devbox back", |p| all_online(p, 1))?;
    // The run was not interrupted, and was started only once.
    let shown = world.ok(&["show", "1"])?;
    assert_eq!(shown["state"], "running", "{shown}");
    assert_eq!(shown["attempts"].as_array().map(Vec::len), Some(1));
    world.ok(&["pause"])?;
    world.until(&world.devbox, "1", "paused")?;
    world.idle()?;
    assert_eq!(
        world
            .requests()?
            .iter()
            .filter(|r| r.starts_with("run "))
            .count(),
        1
    );
    world.stop()
}
