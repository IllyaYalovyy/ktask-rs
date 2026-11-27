//! A linked child's project on the parent's screens: the real binary in a pseudo-terminal,
//! with the SSH stand-in and agent stand-in of the link tests, at both terminal sizes with
//! the keyboard and the mouse.
use super::link::{Host, Linked, all_online};
use super::link_control::{agent, holding, local, web};
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::application::{Definition, Kind, VersionedTask};
use serde_json::Value;
use std::{
    fs, thread,
    time::{Duration, Instant},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const SIZES: [(u16, u16); 2] = [(110, 30), (70, 22)];
const CTRL_S: &str = "\x13";

/// Like the link tests' agent, and a held turn first says what it is doing.
const AGENT: &str = r##"#!/bin/sh
model=unknown
while [ "$#" -gt 0 ]; do case "$1" in -m) shift; model="$1";; esac; shift; done
prompt=$(cat)
printf '{"type":"thread.started","thread_id":"s-%s-%s"}\n' "$model" "$$"
mode=$(cat "$HOME/agent" 2>/dev/null)
if [ "$mode" = hold ]; then
 printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"REMOTE-AGENT-WORKING"}}'
 touch "$HOME/holding"; sleep 60
fi
if [ "$mode" = ask ] && [ "$model" = remediation ]; then
 printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"judgment\":{\"action\":\"escalate\",\"reason\":\"Only the owner knows which content to use\"}}"}}' '{"type":"turn.completed","usage":{}}'
 exit 0
fi
if [ "$mode" = ask ] && [ "$model" = execution ]; then
 printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}"}}' '{"type":"turn.completed","usage":{}}'
 exit 0
fi
if [ "$model" = execution ]; then printf 'verified %s\n' "$$" >> result.txt; fi
printf '%s\n' '{"type":"item.completed","item":{"type":"agent_message","text":"{\"outcome\":\"complete\",\"summary\":\"Verified result\",\"question\":null,\"criteria\":[{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected result.txt\"}]}"}}' '{"type":"turn.completed","usage":{}}'
"##;

/// A parent with its own project `alpha`, and its child `devbox` with project `web`.
struct World {
    parent: Host,
    devbox: Host,
    hub: Linked,
    child: Option<Linked>,
    _sandbox: support::Sandbox,
}

impl World {
    fn new() -> Result<Self> {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        let parent = Host::new(root, "parent")?;
        parent.project("alpha")?;
        let devbox = Host::child(root, "devbox", &parent)?;
        web(&devbox)?;
        support::write_executable(&devbox.dir.join("bin/codex"), AGENT)?;
        let hub = parent.link_run()?;
        let child = devbox.link_run()?;
        parent.listing("devbox linked", |p| all_online(p, 2))?;
        Ok(Self {
            parent,
            devbox,
            hub,
            child: Some(child),
            _sandbox: sandbox,
        })
    }

    /// A command on the child itself, as its owner would run it there.
    fn on_child(&self, args: &[&str]) -> Result<String> {
        let mut all = vec!["--project", "web"];
        all.extend(args);
        let ended = local(&self.devbox, &all)?;
        assert_eq!(ended.status, Some(0), "{args:?} {}", ended.stderr);
        Ok(ended.stdout)
    }

    fn add(&self, title: &str, approval: &str) -> Result {
        self.on_child(&[
            "add",
            "--title",
            title,
            "--goal",
            "Reach it",
            "--criterion",
            "Done",
            "--approval",
            approval,
        ])?;
        Ok(())
    }

    /// The child's own record of a task.
    fn task(&self, id: u64) -> Result<Value> {
        let id = id.to_string();
        let shown = self.on_child(&["--json", "show", &id])?;
        Ok(serde_json::from_str::<Value>(&shown)?["data"].clone())
    }

    fn queue(&self) -> Result<Vec<u64>> {
        let listed = self.on_child(&["--json", "list"])?;
        Ok(serde_json::from_str::<Value>(&listed)?["data"]["tasks"]
            .as_array()
            .ok_or("tasks")?
            .iter()
            .filter_map(|task| task["id"].as_u64())
            .collect())
    }

    fn tui(&self, columns: u16, rows: u16) -> Result<Terminal> {
        let mut t = Terminal::open_project(
            DEV,
            &self.parent.dir,
            &self.parent.dir.join("alpha"),
            columns,
            rows,
        )?;
        // A linked project's screens follow the child's store.
        t.watch(self.devbox.dir.join("state"));
        t.screen("the local project opens", |s| s.contains("/ alpha"))?;
        Ok(t)
    }

    fn go_offline(&mut self) -> Result {
        self.child.take().ok_or("linked")?.stop()
    }

    fn reconnect(&mut self) -> Result {
        self.child = Some(self.devbox.link_run()?);
        Ok(())
    }

    fn stop(mut self) -> Result {
        if let Some(child) = self.child.take() {
            child.stop()?;
        }
        self.hub.stop()
    }
}

/// Keyboard or mouse, doing the same thing.
struct Input {
    t: Terminal,
    mouse: bool,
}

impl Input {
    fn press(&mut self, key: &str, button: &str) -> Result {
        if self.mouse {
            self.t.click(button)
        } else {
            self.t.send(key)
        }
    }

    /// Switches to a screen by its tab or digit.
    fn view(&mut self, digit: u8, name: &str) -> Result {
        self.press(&digit.to_string(), &format!("{digit} {name}"))
    }

    fn wait(&self, what: &str, wanted: impl Fn(&str) -> bool) -> Result<String> {
        // Printed here too: a failure that leaves a run behind is reported over this one.
        self.t
            .screen(what, wanted)
            .inspect_err(|error| println!("{error}"))
    }

    /// Clicks the Queue row holding `title`, not its other mentions on the screen.
    fn click_row(&mut self, title: &str) -> Result {
        self.t.click_after("execution order", title)
    }

    /// Opens the Projects row that shows `host`, from any screen.
    fn open_remote(&mut self) -> Result<String> {
        self.view(0, "Projects")?;
        self.wait("projects listed", |s| {
            s.contains("Projects · 2 registered") && s.contains("devbox")
        })?;
        // Sorted by name: alpha, then web.
        if !self.t.current_screen().contains("Project · linked") {
            self.press("G", "devbox")?;
            self.wait("remote row selected", |s| s.contains("Project · linked"))?;
        }
        self.press("\r", "Enter Open")?;
        self.wait("the remote project", |s| {
            s.contains("web @ devbox") && !s.contains("Projects · 2 registered")
        })
    }

    fn open_local(&mut self) -> Result<String> {
        self.view(0, "Projects")?;
        self.wait("projects listed", |s| s.contains("Projects · 2 / 2"))?;
        self.press("g", "alpha")?;
        self.wait("local row selected", |s| s.contains("Projects · 1 / 2"))?;
        self.press("\r", "Enter Open")?;
        self.wait("the local project", |s| {
            s.contains("/ alpha") && !s.contains("web @ devbox")
        })
    }

    fn quit(mut self) -> Result {
        self.t.send("q")?;
        self.t.finish()
    }
}

/// The list rows, without the details pane beside them.
fn listed(screen: &str) -> impl Iterator<Item = &str> {
    screen
        .lines()
        .filter_map(|line| line.split("││").next())
        .filter(|line| line.starts_with('│') && line.contains('#'))
}

/// The list row holding `title`.
fn row<'a>(screen: &'a str, title: &str) -> &'a str {
    listed(screen)
        .find(|line| line.contains(title))
        .unwrap_or("")
}

/// Open, approve, add, run, follow, pause, offline and reconnect, switching and finishing.
fn operate(columns: u16, rows: u16, mouse: bool) -> Result {
    let case = "remote";
    let mut world = World::new()?;
    world.add("Remote build", "ready")?;
    world.add("Later plan", "draft")?;
    let mut t = Input {
        t: world.tui(columns, rows)?,
        mouse,
    };
    open_and_approve(&mut t, &world, case)?;
    add_remotely(&mut t, &world, case)?;
    run_follow_and_pause(&mut t, &world, case)?;
    lose_and_regain_the_link(&mut t, &mut world, case)?;
    switch_projects(&mut t, case)?;
    return_after_being_away(&mut t, &mut world, case)?;
    local_only_and_finish(&mut t, &world, case)?;
    t.quit()?;
    world.stop()
}

fn open_and_approve(t: &mut Input, world: &World, case: &str) -> Result {
    let opened = t.open_remote()?;
    crate::support::record(&format!("{case}-01-current"), t.mouse, &opened);
    assert!(opened.contains("linked · live"), "{opened}");
    assert!(opened.contains("#1 Remote build"), "{opened}");
    assert!(opened.contains("Opened web on devbox"), "{opened}");

    // Approve the draft from Queue.
    t.view(2, "Queue")?;
    let queue = t.wait("remote queue", |s| {
        row(s, "Later plan").contains("Needs approval")
    })?;
    crate::support::record(&format!("{case}-02-queue"), t.mouse, &queue);
    if t.mouse {
        t.click_row("Later plan")?;
    } else {
        t.t.send("j")?;
    }
    t.wait("draft selected", |s| s.contains("[v Approve]"))?;
    t.press("v", "v Approve")?;
    let approved = t.wait("approved on the child", |s| {
        s.contains("Saved on devbox") && row(s, "Later plan").contains("Pending")
    })?;
    crate::support::record(&format!("{case}-03-approved"), t.mouse, &approved);
    assert_eq!(world.task(2)?["state"], "pending");

    Ok(())
}

/// Adds a task through the same form.
fn add_remotely(t: &mut Input, world: &World, case: &str) -> Result {
    let mouse = t.mouse;
    t.press("a", "a Add")?;
    let form = t.wait("add form", |s| s.contains("Add task"))?;
    assert!(form.contains("on devbox"), "{form}");
    t.t.send("Added remotely")?;
    if mouse {
        t.wait("title typed", |s| s.contains("Added remotely"))?;
        t.t.click_after("Added remotely", "Goal")?;
    } else {
        t.t.send("\t")?;
    }
    t.t.send("From the parent")?;
    if mouse {
        t.wait("goal typed", |s| s.contains("From the parent"))?;
        t.t.click_after("From the parent", "Acceptance check")?;
    } else {
        t.t.send("\t")?;
    }
    t.t.send("Listed there")?;
    let filled = t.wait("filled", |s| s.contains("Listed there"))?;
    crate::support::record(&format!("{case}-04-add-form"), t.mouse, &filled);
    t.press(CTRL_S, "Apply")?;
    // The next read of the host selects the added task.
    let added = t.wait("added on the child", |s| {
        !s.contains("Add task") && s.contains("added #3") && row(s, "Added remotely").contains('›')
    })?;
    crate::support::record(&format!("{case}-05-added"), t.mouse, &added);
    assert_eq!(world.task(3)?["definition"]["goal"], "From the parent");
    assert_eq!(world.task(3)?["definition"]["criteria"][0], "Listed there");

    Ok(())
}

/// Runs the current task and follows the child live: output, details and events.
fn run_follow_and_pause(t: &mut Input, world: &World, case: &str) -> Result {
    agent(&world.devbox, "hold")?;
    t.view(1, "Current")?;
    t.wait("ready to run", |s| {
        s.contains("Task brief") && s.contains("[r Run queue]")
    })?;
    t.press("r", "r Run queue")?;
    let running = t.wait("running with live output", |s| {
        s.contains("0 need you · 1")
            && s.contains("Current #1 · Running")
            && s.contains("REMOTE-AGENT-WORKING")
    })?;
    holding(&world.devbox)?;
    crate::support::record(&format!("{case}-06-running"), t.mouse, &running);

    // Details page the child's evidence; events are the child's.
    t.press("\r", "Enter Details")?;
    let details = t.wait("details from the child", |s| {
        s.contains("Task details / attempts") && s.contains("Output page 1 / 1")
    })?;
    assert!(details.contains("Attempt 1 / 1"), "{details}");
    crate::support::record(&format!("{case}-07-details"), t.mouse, &details);
    t.press("\x1b", "Close")?;
    t.wait("details closed", |s| !s.contains("Task details / attempts"))?;
    t.press("l", "Activity · #1")?;
    let events = t.wait("child events", |s| {
        s.contains("Project events") && s.contains("via me@parent")
    })?;
    crate::support::record(&format!("{case}-08-events"), t.mouse, &events);
    t.press("\x1b", "Close")?;
    t.wait("events closed", |s| !s.contains("Project events"))?;

    t.press("p", "p Pause")?;
    let paused = t.wait("paused on the child", |s| {
        s.contains("0 need you · 0")
            && s.contains("Current #1 · Paused")
            && s.contains("Paused on devbox")
    })?;
    crate::support::record(&format!("{case}-09-paused"), t.mouse, &paused);
    assert_eq!(world.task(1)?["state"], "paused");

    Ok(())
}

/// Offline the screens stay, marked, and every action says why it is refused.
fn lose_and_regain_the_link(t: &mut Input, world: &mut World, case: &str) -> Result {
    world.go_offline()?;
    let offline = t.wait("marked offline", |s| {
        s.contains("OFFLINE · last contact") && s.contains("devbox is offline · last contact")
    })?;
    crate::support::record(&format!("{case}-10-offline"), t.mouse, &offline);
    assert!(offline.contains("#1 Remote build"), "{offline}");
    assert!(offline.contains("REMOTE-AGENT-WORKING"), "{offline}");
    t.press("r", "r Resume")?;
    let refused = t.wait("resume refused", |s| s.contains("actions wait until"))?;
    crate::support::record(&format!("{case}-11-offline-refused"), t.mouse, &refused);
    assert!(!refused.contains("Sending to devbox"), "{refused}");
    t.press("\r", "Enter Details")?;
    t.t.still("details are refused too", |s| {
        s.contains("actions wait until") && !s.contains("Task details / attempts")
    })?;
    t.view(2, "Queue")?;
    let held = t.wait("held queue", |s| {
        !row(s, "Added remotely").is_empty() && s.contains("OFFLINE")
    })?;
    crate::support::record(&format!("{case}-12-offline-queue"), t.mouse, &held);
    // The reason is already on screen, and the form still does not open.
    t.press("a", "a Add")?;
    t.t.still("add is refused", |s| {
        s.contains("actions wait until") && !s.contains("Add task")
    })?;
    // Its screens are kept while another project is open.
    t.open_local()?;
    let away = t.open_remote()?;
    crate::support::record(&format!("{case}-12b-offline-reopened"), t.mouse, &away);
    assert!(away.contains("offline, showing its last state"), "{away}");
    assert!(away.contains("OFFLINE · last contact"), "{away}");
    assert!(!row(&away, "Added remotely").is_empty(), "{away}");

    // It reconnects by itself, with what changed meanwhile.
    world.add("While away", "ready")?;
    world.reconnect()?;
    let back = t.wait("live again", |s| {
        s.contains("linked · live") && !row(s, "While away").is_empty()
    })?;
    crate::support::record(&format!("{case}-13-reconnected"), t.mouse, &back);
    assert!(!back.contains("OFFLINE"), "{back}");

    Ok(())
}

/// Each project keeps its own view, selection and scroll.
fn switch_projects(t: &mut Input, case: &str) -> Result {
    select(t, "Later plan")?;
    t.open_local()?;
    t.view(3, "History")?;
    t.wait("local history", |s| s.contains("closed tasks"))?;
    let returned = t.open_remote()?;
    crate::support::record(&format!("{case}-14-returned-to-remote"), t.mouse, &returned);
    assert!(row(&returned, "Later plan").contains('›'), "{returned}");
    let local_again = t.open_local()?;
    assert!(local_again.contains("closed tasks"), "{local_again}");
    t.open_remote()?;
    Ok(())
}

/// A project left for a while shows what its host is now, not what it was when left.
fn return_after_being_away(t: &mut Input, world: &mut World, case: &str) -> Result {
    // Long enough that the read under way at leaving is old on return.
    let away = Duration::from_secs(4);
    t.open_local()?;
    thread::sleep(away);
    world.add("Added meanwhile", "ready")?;
    t.open_remote()?;
    let news = t.wait("what changed while away", |s| {
        s.contains("linked · live") && !row(s, "Added meanwhile").is_empty()
    })?;
    crate::support::record(&format!("{case}-14b-back-with-news"), t.mouse, &news);

    t.open_local()?;
    world.go_offline()?;
    thread::sleep(away);
    t.open_remote()?;
    let offline = t.wait("went offline while away", |s| {
        s.contains("OFFLINE · last contact")
    })?;
    crate::support::record(&format!("{case}-14c-back-offline"), t.mouse, &offline);
    assert!(!row(&offline, "Added meanwhile").is_empty(), "{offline}");
    t.press("r", "r Resume")?;
    let refused = t.wait("still refused", |s| s.contains("actions wait until"))?;
    assert!(!refused.contains("Sending to devbox"), "{refused}");
    world.reconnect()?;
    let live = t.wait("live once more", |s| {
        s.contains("linked · live") && !s.contains("OFFLINE")
    })?;
    crate::support::record(&format!("{case}-14d-live-again"), t.mouse, &live);
    Ok(())
}

/// What stays on the host is shown as local-only; then the child finishes its queue.
fn local_only_and_finish(t: &mut Input, world: &World, case: &str) -> Result {
    t.view(4, "Config")?;
    let config = t.wait("local-only configuration", |s| {
        s.contains("Local-only: change these on devbox")
    })?;
    crate::support::record(&format!("{case}-15-config"), t.mouse, &config);
    for kept in [
        "Configuration:",
        "Tools and dependency setup",
        "Storage and build caches",
        "Project move and delete",
        "Run mode there",
    ] {
        assert!(config.contains(kept), "{kept}\n{config}");
    }
    t.t.send("k")?;
    t.wait("storage is local-only", |s| {
        s.contains("Local-only: not available")
    })?;
    t.view(0, "Projects")?;
    let projects = t.wait("remote row details", |s| {
        s.contains("Project · linked")
            && s.contains("move, delete and settings are local-only there")
    })?;
    for absent in ["[m Move]", "[D Delete]", "[f Fix]"] {
        assert!(!projects.contains(absent), "{absent}\n{projects}");
    }
    crate::support::record(&format!("{case}-16-projects"), t.mouse, &projects);
    t.t.send("D")?;
    t.wait("delete is local-only", |s| {
        s.contains("Local-only: this project is on devbox")
    })?;

    // Resume to the end: History follows.
    agent(&world.devbox, "work")?;
    t.press("\r", "Enter Open")?;
    t.wait("remote again", |s| !s.contains("Projects · 2 registered"))?;
    t.view(1, "Current")?;
    t.wait("resumable", |s| s.contains("[r Resume]"))?;
    t.press("r", "r Resume")?;
    t.wait("resume started", |s| {
        s.contains("Supervisor started on devbox")
    })?;
    // History keeps the row selected when it opens, so it opens once everything is in it.
    t.wait("every task closed", |s| s.contains("Queue 0 · History 5"))?;
    t.view(3, "History")?;
    let history = t.wait("finished on the child", |s| {
        crate::support::finished(s)
            && s.contains("0 need you · 0")
            && row(s, "Remote build").contains("Done")
    })?;
    crate::support::record(&format!("{case}-17-history"), t.mouse, &history);
    Ok(())
}

#[test]
fn a_remote_project_opens_runs_and_survives_a_lost_link_by_keyboard() -> Result {
    for (columns, rows) in SIZES {
        operate(columns, rows, false)?;
    }
    Ok(())
}

#[test]
fn a_remote_project_opens_runs_and_survives_a_lost_link_by_mouse() -> Result {
    for (columns, rows) in SIZES {
        operate(columns, rows, true)?;
    }
    Ok(())
}

/// Selects the Queue row holding `title`; keyboard moves from the top.
fn select(t: &mut Input, title: &str) -> Result {
    if row(&t.t.current_screen(), title).contains('›') {
        return Ok(());
    }
    if t.mouse {
        t.click_row(title)?;
    } else {
        let screen = t.t.current_screen();
        let rows: Vec<_> = listed(&screen).collect();
        let at = |wanted: &dyn Fn(&str) -> bool| rows.iter().position(|line| wanted(line));
        let from = at(&|line| line.contains('›')).ok_or("no selection")?;
        let to = at(&|line| line.contains(title)).ok_or_else(|| format!("{title}\n{screen}"))?;
        let key = if to > from { "j" } else { "k" };
        t.t.send(&key.repeat(to.abs_diff(from)))?;
    }
    t.wait(title, |s| row(s, title).contains('›'))?;
    Ok(())
}

/// Every other task action through its dialog, and a link the child made read-only.
fn manage(columns: u16, rows: u16, mouse: bool) -> Result {
    let case = "remote-manage";
    let world = World::new()?;
    for (title, approval) in [
        ("Alpha work", "ready"),
        ("Beta draft", "draft"),
        ("Gamma", "ready"),
        ("Delta", "ready"),
        ("Epsilon", "ready"),
    ] {
        world.add(title, approval)?;
    }
    let mut t = Input {
        t: world.tui(columns, rows)?,
        mouse,
    };
    t.open_remote()?;
    t.view(2, "Queue")?;
    t.wait("queue", |s| !row(s, "Epsilon").is_empty())?;

    // Edit: the form opens on the stored revision.
    select(&mut t, "Gamma")?;
    t.press("e", "e Edit")?;
    t.wait("edit form", |s| {
        s.contains("Edit task") && s.contains("Gamma")
    })?;
    t.t.send(" edited")?;
    t.wait("typed", |s| s.contains("Gamma edited"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("edited", |s| {
        !s.contains("Edit task") && !row(s, "Gamma edited").is_empty()
    })?;
    let edited = world.task(3)?;
    assert_eq!(edited["title"], "Gamma edited");
    assert_eq!(edited["definition"]["goal"], "Reach it");
    assert_eq!(edited["definition"]["category"], "code");

    // Move before another.
    select(&mut t, "Epsilon")?;
    t.press("m", "m Move")?;
    t.wait("move form", |s| s.contains("Move pending task"))?;
    t.t.send("3")?;
    t.wait("position", |s| s.contains("before #3"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("moved", |s| {
        !s.contains("Move pending task")
            && s.find("Epsilon")
                .zip(s.find("Gamma edited"))
                .is_some_and(|(e, g)| e < g)
    })?;
    assert_eq!(world.queue()?, [1, 2, 5, 3, 4]);

    // Reject: the child's rules still require a reason.
    select(&mut t, "Beta draft")?;
    t.press("n", "n Reject")?;
    t.wait("reject form", |s| s.contains("Reject pending revision"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("a reason is needed", |s| s.contains("A reason is required"))?;
    t.t.send("Not wanted")?;
    t.wait("reason typed", |s| s.contains("Not wanted"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("rejected", |s| {
        !s.contains("Reject pending revision") && row(s, "Beta draft").is_empty()
    })?;
    assert_eq!(world.task(2)?["state"], "cancelled");

    // Cancel and Mark done.
    select(&mut t, "Delta")?;
    t.press("x", "x Cancel")?;
    t.wait("cancel form", |s| s.contains("Reason (optional)"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("cancelled", |s| {
        !s.contains("Reason (optional)") && row(s, "Delta").is_empty()
    })?;
    assert_eq!(world.task(4)?["state"], "cancelled");
    select(&mut t, "Gamma edited")?;
    t.press("D", "D Mark done")?;
    t.wait("done form", |s| s.contains("Mark done #3"))?;
    t.press(CTRL_S, "Apply")?;
    let done = t.wait("marked done", |s| {
        !s.contains("Mark done #3") && s.contains("on devbox") && row(s, "Gamma edited").is_empty()
    })?;
    crate::support::record(&format!("{case}-1-managed"), t.mouse, &done);
    assert_eq!(world.task(3)?["state"], "done");

    // Answer the worker's question.
    agent(&world.devbox, "ask")?;
    t.view(1, "Current")?;
    t.wait("current", |s| {
        s.contains("Task brief") && s.contains("[r Run queue]")
    })?;
    t.press("r", "r Run queue")?;
    t.wait("a question", |s| s.contains("Current #1 · Needs input"))?;
    agent(&world.devbox, "hold")?;
    t.press("r", "r Answer")?;
    let question = t.wait("answer form", |s| {
        s.contains("1 need you · 0")
            && s.contains("Answer AI question")
            && s.contains("Which content?")
    })?;
    crate::support::record(&format!("{case}-2-question"), t.mouse, &question);
    t.t.send("The short one")?;
    t.wait("answer typed", |s| s.contains("The short one"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("answered and running", |s| {
        !s.contains("Answer AI question") && s.contains("Current #1 · Running")
    })?;
    holding(&world.devbox)?;

    // Restart, then Stop.
    t.press("T", "T Restart")?;
    t.wait("restart form", |s| s.contains("Restart #1"))?;
    t.press(CTRL_S, "Apply")?;
    t.wait("restarted", |s| s.contains("#1 restarted on devbox"))?;
    holding(&world.devbox)?;
    let restarted = t.wait("the fresh attempt", |s| {
        s.contains("0 need you · 1")
            && s.contains("Current #1 · Running")
            && s.contains("Attempt 2")
            && s.contains("REMOTE-AGENT-WORKING")
    })?;
    crate::support::record(&format!("{case}-3-restarted"), t.mouse, &restarted);
    t.press("s", "s Stop")?;
    t.wait("stop form", |s| s.contains("Stop #1"))?;
    t.press(CTRL_S, "Apply")?;
    let stopped = t.wait("stopped", |s| {
        s.contains("0 need you · 0")
            && s.contains("#1 stopped")
            && s.contains("Current #1 · Pending")
    })?;
    crate::support::record(&format!("{case}-4-stopped"), t.mouse, &stopped);
    assert_eq!(world.task(1)?["state"], "pending");

    // Statistics are the child's.
    t.press("S", "S Stats")?;
    t.wait("statistics", |s| {
        s.contains("Project statistics · devbox") && s.contains("Attempts: 2")
    })?;
    t.press("\x1b", "Close")?;
    t.wait("closed", |s| !s.contains("Project statistics"))?;

    // The child can make the link read-only.
    world.on_child_link(&["link", "control", "off"])?;
    t.wait("read-only", |s| s.contains("linked · read-only"))?;
    t.press("r", "r Run queue")?;
    // A narrow header cuts the running count off; wide ones have caught up at zero.
    let refused = t.wait("run refused", |s| {
        !s.contains("need you · 1") && s.contains("is read-only")
    })?;
    crate::support::record(&format!("{case}-5-read-only"), t.mouse, &refused);
    assert_eq!(world.task(1)?["state"], "pending");
    t.quit()?;
    world.stop()
}

impl World {
    fn on_child_link(&self, args: &[&str]) -> Result {
        let ended = local(&self.devbox, args)?;
        assert_eq!(ended.status, Some(0), "{}", ended.stderr);
        Ok(())
    }
}

#[test]
fn remote_task_actions_work_through_their_dialogs() -> Result {
    manage(110, 30, false)?;
    manage(70, 22, true)?;
    manage(70, 22, false)?;
    manage(110, 30, true)
}

#[test]
fn remote_queue_and_history_stay_usable_with_hundreds_of_tasks() -> Result {
    let world = World::new()?;
    {
        let mut project = world.devbox.project("web")?;
        let definitions: Vec<_> = (1..=440)
            .map(|number| Definition {
                title: format!("Bulk {number:03}"),
                goal: "Fill the lists of a linked project with a goal of ordinary length".into(),
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["Listed on the parent".into()],
                models: std::array::from_fn(|_| None),
                attempt_limit: Some(3),
                timeout_ms: None,
                stage_timeout_ms: None,
            })
            .collect();
        let ids = project.create(&definitions, None, true)?;
        for id in &ids[220..] {
            project.cancel(
                VersionedTask {
                    id: *id,
                    version: 1,
                },
                "Seeded history",
            )?;
        }
    }
    for (columns, rows) in SIZES {
        for mouse in [false, true] {
            let case = "remote-bulk";
            let mut t = Input {
                t: world.tui(columns, rows)?,
                mouse,
            };
            t.open_remote()?;
            let started = Instant::now();
            t.view(2, "Queue")?;
            t.wait("long queue", |s| {
                s.contains("Queue 220 · History 220") && !row(s, "Bulk 001").is_empty()
            })?;
            if mouse {
                for _ in 0..4 {
                    t.t.wheel(true, 5, 8)?;
                }
                t.wait("wheel scrolled", |s| row(s, "Bulk 013").contains('›'))?;
            }
            t.t.send("G")?;
            let end = t.wait("queue end", |s| row(s, "Bulk 220").contains('›'))?;
            crate::support::record(&format!("{case}-1-queue-end"), t.mouse, &end);
            t.view(3, "History")?;
            t.wait("history", |s| {
                s.contains("closed tasks") && s.contains("Bulk 440")
            })?;
            t.t.send("G")?;
            let oldest = t.wait("history end", |s| row(s, "Bulk 221").contains('›'))?;
            crate::support::record(&format!("{case}-2-history-end"), t.mouse, &oldest);
            assert!(started.elapsed() < Duration::from_secs(30));
            // Both lists keep their place across a switch to the local project and back.
            t.open_local()?;
            let history = t.open_remote()?;
            assert!(row(&history, "Bulk 221").contains('›'), "{history}");
            t.view(2, "Queue")?;
            t.wait("queue place kept", |s| row(s, "Bulk 220").contains('›'))?;
            t.quit()?;
        }
    }
    // Reading the project leaves no trace in it.
    let events = world.on_child(&["--json", "events", "--limit", "1000"])?;
    assert!(!events.contains("screen requested"), "{events}");
    assert!(fs::exists(world.devbox.dir.join("web/README.md"))?);
    world.stop()
}
