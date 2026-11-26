//! Agent reports through the real binaries with fixture agents: a failed check under a live
//! runner, a paused dependency wait and a healthy idle project, then every way one ends
//! without a report. Each proves that the project folder, its Git state and ktask's task
//! records are what they were.
use crate::support::{self, Result, Terminal};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");

/// The fixture agent for every role. A task role's turn drives a runner into a failed
/// check; an investigator's turn behaves as `$HOME/agent-mode` says.
const AGENT: &str = r##"#!/bin/sh
model=unknown
while [ "$#" -gt 0 ]; do
 case "$1" in -m) shift; model="$1";; esac
 shift
done
prompt=$(cat)
say() { printf '%s\n' "$1"; }
message() {
 escaped=$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n' ' ')
 say "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"$escaped\"}}"
}
done_turn() { say '{"type":"turn.completed","usage":{"input_tokens":120,"cached_input_tokens":20,"output_tokens":30}}'; }
say "{\"type\":\"thread.started\",\"thread_id\":\"$model-$$\"}"
case "$prompt" in "You are ktask's investigator"*) investigating=yes;; *) investigating=no;; esac
if [ "$investigating" = no ]; then
 [ ! -f "$HOME/worker-fails" ] || { done_turn; exit 0; }
 case "$prompt" in *"Fix what Context reports"*)
  touch "$HOME/repairing"
  while [ ! -f "$HOME/release-runner" ]; do sleep 0.05; done;;
 *) printf 'bad\n' > wrong.txt;;
 esac
 message '{"outcome":"complete","summary":"Wrote the file","question":null,"criteria":[{"ordinal":1,"passed":true,"evidence":"wrong.txt"}],"findings":[]}'
 done_turn
 exit 0
fi
printf '%s' "$prompt" > "$HOME/investigator-prompt"
printf '%s\n' "$model" >> "$HOME/investigator-models"
packet=$(printf '%s\n' "$prompt" | sed -n 's/^ktask records, a snapshot taken when this investigation started: //p')
project=$(printf '%s\n' "$prompt" | sed -n 's/^Project folder (your working directory): //p')
rm -rf "$HOME/packet" && cp -R "$packet" "$HOME/packet"
attempted() {
 if (: >> "$1") 2>/dev/null; then say_to=wrote; else say_to=blocked; fi
 printf '%s %s\n' "$2" "$say_to" >> "$HOME/writes"
}
: > "$HOME/writes"
attempted "$project/written-by-agent" project
attempted "$project/.git/written-by-agent" git
attempted "$packet/overview.json" records
attempted "$TMPDIR/scratch-file" scratch
printf '%s' "$TMPDIR" > "$HOME/investigator-tmpdir"
mode=$(cat "$HOME/agent-mode" 2>/dev/null || echo report)
message "Fixture agent read the ktask records"
say '{"type":"item.completed","item":{"type":"command_execution","command":"df -h .","aggregated_output":"fixture disk","exit_code":0}}'
case "$mode" in
 slow) while :; do sleep 0.1; done;;
 descendants) setsid sleep 300 & printf '%s' "$!" > "$HOME/descendant.new"; mv "$HOME/descendant.new" "$HOME/descendant"; while :; do sleep 0.1; done;;
 gated) while [ ! -f "$HOME/release-report" ]; do sleep 0.05; done;;
 fail) say '{"type":"turn.failed","error":{"message":"transport lost by fixture"}}'; exit 2;;
 garbage) message "I could not tell, sorry"; done_turn; exit 0;;
esac
event=$(sed -n 's/^{.*"id":\([0-9]*\).*/\1/p' "$HOME/packet/events.jsonl" | tail -n 1)
reference="{\"kind\":\"command\",\"target\":\"df -h .\",\"detail\":\"The disk has room\"}"
[ -z "$event" ] || reference="{\"kind\":\"event\",\"target\":\"$event\",\"detail\":\"The latest recorded event\"},$reference"
defect=null
[ "$mode" != defect ] || defect='"The fixture supervisor loses check results"'
message "{\"what_happened\":\"Fixture account of what happened.\",\"cause\":\"Fixture cause from the records.\",\"references\":[$reference,{\"kind\":\"attempt\",\"target\":\"987654\",\"detail\":\"An attempt that does not exist\"}],\"fix_steps\":[{\"class\":\"ktask_action\",\"action\":\"Run ktask run\",\"workaround\":false},{\"class\":\"repository\",\"action\":\"Correct the check\",\"workaround\":false},{\"class\":\"configuration\",\"action\":\"Raise the check timeout\",\"workaround\":false},{\"class\":\"host\",\"action\":\"Free disk space\",\"workaround\":false},{\"class\":\"owner\",\"action\":\"Decide the content\",\"workaround\":false}],\"ktask_defect\":$defect}"
done_turn
"##;

struct Fixture {
    sandbox: support::Sandbox,
}

impl Fixture {
    /// A registered project with a check, every task role's model and the operator model.
    fn new() -> Result<Self> {
        let fixture = Self {
            sandbox: support::sandbox()?,
        };
        let project = fixture.project();
        fs::create_dir_all(&project)?;
        support::empty_repository(&project)?;
        // Unlike the storage fixtures, files here are tracked and a change shows in Git.
        fs::write(project.join(".git/info/exclude"), "")?;
        fs::write(project.join("README.md"), "Fixture project\n")?;
        for args in [&["add", "README.md"][..], &["commit", "-qm", "readme"]] {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(&project)
                    .args(args)
                    .status()?
                    .success()
            );
        }
        fs::create_dir_all(fixture.root().join("bin"))?;
        support::write_executable(&fixture.root().join("bin/codex"), AGENT)?;
        fixture.ok(&["check-set", "build", "--command", "test ! -f wrong.txt"])?;
        for role in ["execution", "review", "validation", "remediation"] {
            fixture.ok(&[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ])?;
        }
        fixture.ok(&[
            "model-default",
            "operator",
            "--provider",
            "codex",
            "--model",
            "operator",
        ])?;
        Ok(fixture)
    }

    fn root(&self) -> &Path {
        self.sandbox.path()
    }

    fn project(&self) -> PathBuf {
        self.root().join("project")
    }

    fn command(&self) -> Command {
        let root = self.root();
        let mut command = Command::new(DEV);
        command
            .env_clear()
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env("PATH", format!("{}/bin:/usr/bin:/bin", root.display()))
            .current_dir(root);
        command
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        Ok(self
            .command()
            .arg("--project")
            .arg(self.project())
            .arg("--json")
            .args(args)
            .output()?)
    }

    fn ok(&self, args: &[&str]) -> Result<Value> {
        let output = self.run(args)?;
        assert!(
            output.status.success(),
            "{args:?}: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].take())
    }

    fn refused(&self, args: &[&str], status: i32) -> Result<String> {
        let output = self.run(args)?;
        assert_eq!(output.status.code(), Some(status), "{args:?}");
        Ok(
            serde_json::from_slice::<Value>(&output.stderr)?["error"]["message"]
                .as_str()
                .ok_or("error message")?
                .to_owned(),
        )
    }

    fn text(&self, args: &[&str]) -> Result<String> {
        let output = self
            .command()
            .arg("--project")
            .arg(self.project())
            .args(args)
            .output()?;
        assert!(output.status.success(), "{args:?}: {output:?}");
        Ok(String::from_utf8(output.stdout)?)
    }

    fn mode(&self, mode: &str) -> Result {
        fs::write(self.root().join("agent-mode"), mode)?;
        Ok(())
    }

    fn read(&self, file: &str) -> Result<String> {
        Ok(fs::read_to_string(self.root().join(file))?)
    }

    fn report(&self, args: &[&str]) -> Result<i64> {
        let mut command = vec!["agent", "report"];
        command.extend(args);
        Ok(self.ok(&command)?["id"]
            .as_i64()
            .ok_or("investigation ID")?)
    }

    fn show(&self, id: i64) -> Result<Value> {
        Ok(self.ok(&["agent", "show", &id.to_string()])?["investigation"].take())
    }

    fn ended(&self, id: i64) -> Result<Value> {
        support::eventually(&format!("investigation {id} ends"), || {
            let shown = self.show(id)?;
            Ok((shown["state"] != "running").then_some(shown))
        })
    }

    fn found(&self, id: i64, text: &str) -> Result {
        support::eventually(&format!("investigation {id} reports {text}"), || {
            Ok(self.show(id)?["progress"]
                .to_string()
                .contains(text)
                .then_some(()))
        })
    }

    /// Everything in the project folder, Git metadata included, and every row ktask keeps
    /// that is not an investigation's own or storage accounting, without the runner's clock.
    fn untouched(&self) -> Result<(BTreeMap<PathBuf, Vec<u8>>, String)> {
        fn walk(path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) -> Result {
            for entry in fs::read_dir(path)? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    files.insert(entry.path(), Vec::new());
                    walk(&entry.path(), files)?;
                } else {
                    files.insert(entry.path(), fs::read(entry.path())?);
                }
            }
            Ok(())
        }
        let mut files = BTreeMap::new();
        walk(&self.project(), &mut files)?;
        let db = Connection::open_with_flags(
            self.root().join("state/ktask-dev/ktask.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT GLOB 'sqlite_*' AND name NOT GLOB 'investigation*' AND name NOT GLOB 'instance_*' AND name<>'evidence_sweep' ORDER BY name")?
            .query_map([], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        assert!(tables.iter().any(|table| table == "tasks"));
        let mut rows = String::new();
        for table in tables {
            let mut statement = db.prepare(&format!("SELECT * FROM {table}"))?;
            let names: Vec<String> = statement
                .column_names()
                .iter()
                .map(|name| (*name).into())
                .collect();
            let mut found = statement.query([])?;
            while let Some(row) = found.next()? {
                rows.push_str(&table);
                for (column, name) in names.iter().enumerate() {
                    // A live runner advances this clock of its attempt by itself.
                    if table == "attempts" && name == "active_ms" {
                        continue;
                    }
                    let value = match row.get_ref(column)? {
                        ValueRef::Null => "null".into(),
                        ValueRef::Integer(n) => n.to_string(),
                        ValueRef::Real(n) => n.to_string(),
                        ValueRef::Text(t) | ValueRef::Blob(t) => {
                            String::from_utf8_lossy(t).into_owned()
                        }
                    };
                    rows.push('|');
                    rows.push_str(&value);
                }
                rows.push('\n');
            }
        }
        Ok((files, rows))
    }

    fn assert_sandboxed(&self) -> Result {
        assert_eq!(
            self.read("writes")?,
            "project blocked\ngit blocked\nrecords blocked\nscratch wrote\n"
        );
        let scratch = PathBuf::from(self.read("investigator-tmpdir")?);
        assert!(scratch.starts_with(self.root().join("cache/ktask-dev/workers")));
        assert!(!scratch.exists(), "scratch is cleaned");
        assert!(!self.project().join("written-by-agent").exists());
        Ok(())
    }
}

/// Names what differs rather than printing every file and row.
fn assert_unchanged(
    after: &(BTreeMap<PathBuf, Vec<u8>>, String),
    before: &(BTreeMap<PathBuf, Vec<u8>>, String),
    what: &str,
) {
    let files: Vec<_> = after
        .0
        .keys()
        .chain(before.0.keys())
        .filter(|path| after.0.get(*path) != before.0.get(*path))
        .collect();
    let rows: Vec<_> = after
        .1
        .lines()
        .filter(|line| !before.1.contains(line))
        .chain(before.1.lines().filter(|line| !after.1.contains(line)))
        .collect();
    assert!(
        files.is_empty() && after.1 == before.1,
        "{what} changed: files {files:?} rows {rows:#?}"
    );
}

/// A finished report as stored, printed as text and JSON.
fn assert_finished(fixture: &Fixture, id: i64) -> Result<Value> {
    let shown = fixture.ended(id)?;
    assert_eq!(shown["state"], "finished", "{shown}");
    assert_eq!(shown["model"]["model"], "operator");
    assert_eq!(shown["requester"], "owner");
    assert_eq!(shown["usage"]["input_tokens"], 100);
    assert_eq!(shown["usage"]["cache_read_tokens"], 20);
    assert_eq!(shown["usage"]["output_tokens"], 30);
    assert_eq!(shown["usage"]["cost_microusd"], Value::Null);
    assert!(shown["progress"].to_string().contains("Fixture agent read"));
    assert!(shown["progress"].to_string().contains("df -h ."));
    assert_eq!(
        shown["report"]["what_happened"],
        "Fixture account of what happened."
    );
    let text = fixture.text(&["agent", "show", &id.to_string()])?;
    println!("AGENT SHOW #{id}\n{text}");
    let order = [
        "WHAT HAPPENED",
        "Fixture account of what happened.",
        "CAUSE",
        "Fixture cause from the records.",
        "Shown by:",
        "PROPOSED FIX",
        "1. [ktask action] Run ktask run",
        "2. [repository change] Correct the check",
        "3. [configuration change] Raise the check timeout",
        "4. [host change] Free disk space",
        "5. [owner only] Decide the content",
    ];
    let positions: Vec<_> = order
        .iter()
        .map(|part| text.find(part).unwrap_or_else(|| panic!("{part}: {text}")))
        .collect();
    assert!(positions.is_sorted(), "{text}");
    assert!(text.contains("attempt 987654: An attempt that does not exist [not found"));
    let listed = fixture.ok(&["agent", "list"])?;
    assert_eq!(listed["investigations"][0]["id"], id);
    assert!(
        fixture
            .text(&["agent", "list"])?
            .contains(&format!("#{id} · Finished"))
    );
    Ok(shown)
}

#[test]
fn a_failed_check_is_investigated_beside_a_live_and_then_paused_runner() -> Result {
    let fixture = Fixture::new()?;
    fixture.ok(&[
        "add",
        "--title",
        "Break the check",
        "--goal",
        "Produce a file",
        "--criterion",
        "File exists",
    ])?;
    // The task roles run their own fixture turns; remediation is not reached.
    let log = fs::File::create(fixture.root().join("runner.out"))?;
    let mut runner: Child = fixture
        .command()
        .arg("--project")
        .arg(fixture.project())
        .args(["run", "--one"])
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        .spawn()?;
    support::eventually("the repair turn after the failed check", || {
        Ok(fixture.root().join("repairing").exists().then_some(()))
    })?;
    fixture.ok(&["agent", "list"])?;
    let before = fixture.untouched()?;
    let id = fixture.report(&["Why did the check fail?"])?;
    let shown = assert_finished(&fixture, id)?;
    assert_eq!(shown["question"], "Why did the check fail?");
    assert_unchanged(&fixture.untouched()?, &before, "the live attempt");
    assert!(runner.try_wait()?.is_none(), "the runner still runs");
    fixture.assert_sandboxed()?;
    let overview: Value = serde_json::from_str(&fixture.read("packet/overview.json")?)?;
    assert_eq!(overview["runner_alive"], true);
    assert_eq!(overview["current_task"], 1);
    let steps = fixture.read("packet/task-1/steps.jsonl")?;
    assert!(steps.contains("\"check\":\"build\"") && steps.contains("\"state\":\"failed\""));
    assert!(
        fixture
            .read("packet/events.jsonl")?
            .contains("\"task_id\":1")
    );
    assert!(fixture.read("packet/git.txt")?.contains("wrong.txt"));
    assert!(
        fixture
            .read("packet/configuration.json")?
            .contains("test ! -f wrong.txt")
    );
    assert!(fixture.root().join("packet/attempts.jsonl").exists());
    assert!(shown["report"]["references"][0]["verified"].as_bool() == Some(true));

    fixture.ok(&["pause"])?;
    runner.wait()?;
    let before = fixture.untouched()?;
    let id = fixture.report(&["--task", "1"])?;
    let shown = assert_finished(&fixture, id)?;
    assert_eq!(shown["task_id"], 1);
    assert_eq!(
        fixture.untouched()?,
        before,
        "the paused attempt is as it was"
    );
    assert_eq!(fixture.ok(&["show", "1"])?["state"], "paused");
    assert!(
        fixture
            .read("investigator-prompt")?
            .contains("Task asked about: #1")
    );
    Ok(())
}

#[test]
fn a_failed_task_with_no_runner_is_investigated_without_changes() -> Result {
    let fixture = Fixture::new()?;
    fixture.ok(&[
        "add",
        "--title",
        "Fail it",
        "--goal",
        "Produce a file",
        "--criterion",
        "File exists",
    ])?;
    fs::write(fixture.root().join("worker-fails"), "")?;
    // Every task turn ends without a verdict until the runner gives up.
    let output = fixture.run(&["run", "--one"])?;
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert_eq!(fixture.ok(&["show", "1"])?["status"], "failed");
    fixture.ok(&["agent", "list"])?;
    let before = fixture.untouched()?;
    let id = fixture.report(&[])?;
    assert_finished(&fixture, id)?;
    assert_eq!(fixture.untouched()?, before);
    let overview: Value = serde_json::from_str(&fixture.read("packet/overview.json")?)?;
    assert_eq!(overview["runner_alive"], false);
    assert!(
        fixture
            .read("packet/attempts.jsonl")?
            .contains("\"state\":\"failed\"")
    );
    assert!(
        fixture.root().join("packet/runner.log").exists() || {
            // A foreground run keeps no runner log.
            fixture
                .read("investigator-prompt")?
                .contains("No runner log exists")
        }
    );
    Ok(())
}

#[test]
fn a_dependency_wait_and_a_healthy_idle_project_are_investigated_without_changes() -> Result {
    let fixture = Fixture::new()?;
    fixture.ok(&["agent", "list"])?;
    // Healthy and idle: nothing queued and no runner ever ran.
    let before = fixture.untouched()?;
    let id = fixture.report(&[])?;
    let shown = assert_finished(&fixture, id)?;
    assert_eq!(shown["question"], Value::Null);
    assert_eq!(fixture.untouched()?, before);
    fixture.assert_sandboxed()?;
    let prompt = fixture.read("investigator-prompt")?;
    assert!(prompt.contains("Question: Explain the current state of this project"));
    assert!(prompt.contains("No runner log exists"));
    let overview: Value = serde_json::from_str(&fixture.read("packet/overview.json")?)?;
    assert_eq!(overview["current_task"], Value::Null);
    assert_eq!(overview["runner_alive"], false);

    fixture.ok(&[
        "add",
        "--title",
        "Needs a tool",
        "--goal",
        "Use it",
        "--criterion",
        "Used",
    ])?;
    fixture.ok(&[
        "require",
        "1",
        "--kind",
        "executable",
        "--item",
        "ktask-fixture-missing-tool",
        "--scope",
        "task",
        "--stage",
        "baseline",
        "--reproduction",
        "ktask-fixture-missing-tool: not found",
    ])?;
    fixture.run(&["run", "--one"])?;
    assert_eq!(fixture.ok(&["show", "1"])?["status"], "dependency_wait");
    let before = fixture.untouched()?;
    let id = fixture.report(&[])?;
    assert_finished(&fixture, id)?;
    assert_eq!(fixture.untouched()?, before);
    let dependencies = fixture.read("packet/dependencies.jsonl")?;
    assert!(dependencies.contains("ktask-fixture-missing-tool"));
    assert!(
        dependencies.contains("\"state\":\"waiting\""),
        "{dependencies}"
    );
    assert!(
        fixture
            .read("packet/task-1/task.json")?
            .contains("ktask-fixture-missing-tool")
    );
    Ok(())
}

#[test]
fn an_investigation_that_does_not_finish_is_never_a_report() -> Result {
    let fixture = Fixture::new()?;
    fixture.ok(&["agent", "list"])?;
    let before = fixture.untouched()?;
    stopping_reaps_and_keeps_what_was_found(&fixture)?;
    failed_replies_keep_what_was_found(&fixture)?;
    a_timeout_and_a_crash_keep_what_was_found(&fixture)?;
    fixture.mode("report")?;
    let id = fixture.report(&[])?;
    assert_eq!(fixture.ended(id)?["state"], "finished");
    // Only the limits' own row was added to what ktask keeps.
    let after = fixture.untouched()?;
    assert_eq!(after.0, before.0);
    let others = |rows: &str| {
        rows.lines()
            .filter(|row| !row.starts_with("project_investigation_limits|"))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(others(&after.1), others(&before.1));
    Ok(())
}

fn stopping_reaps_and_keeps_what_was_found(fixture: &Fixture) -> Result {
    fixture.mode("descendants")?;
    let id = fixture.report(&["Is anything stuck?"])?;
    fixture.found(id, "Fixture agent read")?;
    support::eventually("the agent's own process", || {
        Ok(fixture.root().join("descendant").exists().then_some(()))
    })?;
    let descendant: u32 = fixture.read("descendant")?.parse()?;
    let started = support::start_time(descendant)?;
    assert!(started.is_some());
    let refused = fixture.refused(&["agent", "report"], 4)?;
    assert!(
        refused.contains(&format!("Investigation #{id} is already running")),
        "{refused}"
    );
    let running = fixture.text(&["agent", "show", &id.to_string()])?;
    assert!(running.contains("Running") && running.contains("PROGRESS (no report yet)"));
    assert!(!running.contains("WHAT HAPPENED"));
    let stopped = fixture.ok(&["agent", "stop", &id.to_string()])?;
    assert_eq!(stopped["investigation"]["state"], "stopped");
    assert_eq!(stopped["investigation"]["reason"], "Stopped by owner");
    // Stop returns once the whole process tree is reaped.
    assert_ne!(support::start_time(descendant)?, started);
    let shown = fixture.text(&["agent", "show", &id.to_string()])?;
    println!("STOPPED\n{shown}");
    assert!(shown.contains("NOT A FINISHED REPORT: Stopped by owner"));
    assert!(shown.contains("WHAT IT HAD FOUND") && shown.contains("Fixture agent read"));
    assert!(!shown.contains("WHAT HAPPENED"));
    assert!(
        fixture
            .refused(&["agent", "stop", &id.to_string()], 4)?
            .contains("is not running (stopped)")
    );
    fixture.refused(&["agent", "stop", "99"], 3)?;
    fixture.refused(&["agent", "show", "99"], 3)?;
    fixture.refused(&["agent", "report", "--task", "99"], 3)?;
    Ok(())
}

fn failed_replies_keep_what_was_found(fixture: &Fixture) -> Result {
    for (mode, reason) in [
        ("fail", "transport lost by fixture"),
        ("garbage", "not a report"),
    ] {
        fixture.mode(mode)?;
        let id = fixture.report(&[])?;
        let shown = fixture.ended(id)?;
        assert_eq!(shown["state"], "failed", "{mode}: {shown}");
        assert!(
            shown["reason"].as_str().unwrap().contains(reason),
            "{shown}"
        );
        assert_eq!(shown["report"], Value::Null);
        assert!(shown["progress"].to_string().contains("Fixture agent read"));
    }
    Ok(())
}

fn a_timeout_and_a_crash_keep_what_was_found(fixture: &Fixture) -> Result {
    let limits = fixture.ok(&["agent", "limits"])?;
    assert_eq!(limits["limits"]["time_limit_ms"], 900_000);
    assert_eq!(limits["limits"]["cost_limit_microusd"], 2_000_000);
    fixture.ok(&["agent", "limits", "--time-limit-ms", "1500"])?;
    fixture.refused(
        &[
            "agent",
            "limits",
            "--time-limit-ms",
            "2000",
            "--if-version",
            "0",
        ],
        4,
    )?;
    fixture.mode("slow")?;
    let id = fixture.report(&[])?;
    let shown = fixture.ended(id)?;
    assert_eq!(shown["state"], "timed_out", "{shown}");
    assert!(shown["progress"].to_string().contains("Fixture agent read"));
    assert!(
        fixture
            .text(&["agent", "show", &id.to_string()])?
            .contains("NOT A FINISHED REPORT: The time limit")
    );

    // A killed investigator is recorded as failed, with what it had found.
    fixture.ok(&["agent", "limits", "--time-limit-ms", "600000"])?;
    let id = fixture.report(&[])?;
    fixture.found(id, "Fixture agent read")?;
    kill_investigator(fixture)?;
    let shown = fixture.ended(id)?;
    assert_eq!(shown["state"], "failed");
    assert!(
        shown["reason"]
            .as_str()
            .unwrap()
            .contains("ended without recording a result")
    );
    Ok(())
}

/// Ends the investigator process of this fixture as a crash would.
fn kill_investigator(fixture: &Fixture) -> Result {
    let state = fixture.root().join("state/ktask-dev");
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(command) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if command
            .split(|&b| b == 0)
            .any(|arg| arg == b"__investigate")
            && fs::read_link(entry.path().join("cwd")).is_ok_and(|cwd| cwd == state)
        {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            )?;
            return Ok(());
        }
    }
    Err("no investigator process".into())
}

#[test]
fn a_ktask_defect_is_said_and_the_operator_defaults_to_the_remediation_model() -> Result {
    let fixture = Fixture::new()?;
    fixture.mode("defect")?;
    // Without an operator override the remediation model investigates.
    fixture.ok(&["profile", "inherit", "operator"])?;
    let effective = fixture.ok(&["profile", "effective"])?;
    assert!(
        effective.to_string().contains("\"layer\":\"remediation\""),
        "{effective}"
    );
    let id = fixture.report(&[])?;
    let shown = fixture.ended(id)?;
    assert_eq!(shown["state"], "finished", "{shown}");
    assert_eq!(shown["model"]["model"], "remediation");
    assert_eq!(fixture.read("investigator-models")?, "remediation\n");
    let text = fixture.text(&["agent", "show", &id.to_string()])?;
    assert!(text.contains("This is a defect in ktask itself: The fixture supervisor"));
    assert!(text.contains("The fix belongs in ktask"));
    assert!(text.contains("1. [ktask action] (workaround, not a fix) Run ktask run"));
    assert!(text.contains("2. [repository change] Correct the check"));
    assert!(text.contains("3. [configuration change] (workaround, not a fix)"));
    Ok(())
}

#[test]
fn providers_that_report_cost_are_held_to_the_cost_limit() -> Result {
    let fixture = Fixture::new()?;
    support::write_executable(
        &fixture.root().join("bin/opencode"),
        r##"#!/bin/sh
cat > /dev/null
printf '%s\n' '{"type":"text","sessionID":"s","part":{"text":"Looked at the records so far"}}'
printf '%s\n' '{"type":"step_finish","sessionID":"s","part":{"reason":"tool-calls","cost":0.75,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}'
while :; do sleep 0.1; done
"##,
    )?;
    support::write_executable(
        &fixture.root().join("bin/claude"),
        r##"#!/bin/sh
printf '%s\n' "$@" > "$HOME/claude-args"
cat > /dev/null
printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"text","text":"Looked at the records so far"}]}}'
printf '%s\n' '{"type":"result","subtype":"error_max_budget_usd","is_error":true,"result":"","total_cost_usd":0.51,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'
exit 1
"##,
    )?;
    fixture.ok(&["agent", "limits", "--cost-limit-usd", "0.50"])?;
    for (provider, model, cost) in [
        ("opencode", "vendor/model", 750_000),
        ("claude", "sonnet", 510_000),
    ] {
        fixture.ok(&[
            "model-default",
            "operator",
            "--provider",
            provider,
            "--model",
            model,
        ])?;
        let id = fixture.report(&[])?;
        let shown = fixture.ended(id)?;
        assert_eq!(shown["state"], "over_budget", "{provider}: {shown}");
        assert_eq!(shown["usage"]["cost_microusd"], cost);
        assert_eq!(shown["report"], Value::Null);
        assert!(
            shown["progress"]
                .to_string()
                .contains("Looked at the records")
        );
        assert!(shown["reason"].as_str().unwrap().contains("$0.50"));
    }
    let args = fixture.read("claude-args")?;
    assert!(args.contains("--max-budget-usd\n0.50\n"), "{args}");
    assert!(args.contains("--permission-mode\ndontAsk\n"));
    assert!(args.contains("Bash(df *)\n") && args.contains("Bash(ps *)\n"));
    Ok(())
}

#[test]
fn a_project_with_a_running_investigation_is_not_deleted_and_takes_its_reports_along() -> Result {
    let fixture = Fixture::new()?;
    let id = fixture.report(&[])?;
    fixture.ended(id)?;
    fixture.mode("slow")?;
    let running = fixture.report(&[])?;
    fixture.found(running, "Fixture agent read")?;
    let delete = ["project-delete", "1", "--confirm", "project"];
    let output = fixture.command().args(delete).output()?;
    assert_eq!(output.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&output.stderr).contains("agent report is being written"));
    // An upgrade ends reports still being written.
    let output = fixture.command().arg("shutdown").output()?;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(fixture.show(running)?["state"], "stopped");
    let output = fixture.command().args(delete).output()?;
    assert!(output.status.success(), "{output:?}");
    let db = Connection::open(fixture.root().join("state/ktask-dev/ktask.sqlite3"))?;
    for table in [
        "investigations",
        "investigation_progress",
        "investigation_references",
        "investigation_fix_steps",
        "project_investigation_limits",
    ] {
        let rows: i64 = db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?;
        assert_eq!(rows, 0, "{table}");
    }
    assert!(
        fs::read_dir(fixture.root().join("state/ktask-dev/investigations"))?
            .next()
            .is_none()
    );
    assert!(fixture.project().join("README.md").exists());
    Ok(())
}

const SIZES: [(u16, u16); 2] = [(70, 22), (120, 36)];

/// Clicks a button or presses its key.
fn press(t: &mut Terminal, mouse: bool, label: &str, key: &str) -> Result {
    if mouse { t.click(label) } else { t.send(key) }
}

fn shows(t: &mut Terminal, what: &str, parts: &[&str]) -> Result<String> {
    let screen = t.screen(what, |s| parts.iter().all(|part| s.contains(part)))?;
    println!("{}\n{screen}", what.to_uppercase());
    Ok(screen)
}

/// Scrolls the open report until `part` is on screen, by wheel or by PgDn.
fn scroll_to(t: &mut Terminal, mouse: bool, part: &str) -> Result {
    for _ in 0..20 {
        if t.current_screen().contains(part) {
            let screen = t.still(part, |s| s.contains(part))?;
            println!("SCROLLED TO {part}\n{screen}");
            return Ok(());
        }
        let before = t.current_screen();
        if mouse {
            t.wheel(true, 30, 8)?;
        } else {
            t.send("\x1b[6~")?;
        }
        t.screen("report scrolls", |s| s != before)?;
    }
    Err(format!("never scrolled to {part}:\n{}", t.current_screen()).into())
}

fn close(t: &mut Terminal, mouse: bool, back: &str) -> Result {
    press(t, mouse, "[Close]", "\x1b")?;
    t.screen("report closed", |s| {
        s.contains(back) && !s.contains("[Close]")
    })?;
    Ok(())
}

#[test]
fn reports_start_progress_and_read_back_by_keyboard_and_mouse_at_both_sizes() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            println!("=== {cols}x{rows} mouse={mouse} ===");
            drive(cols, rows, mouse)?;
        }
    }
    Ok(())
}

fn drive(cols: u16, rows: u16, mouse: bool) -> Result {
    let fixture = Fixture::new()?;
    fixture.ok(&[
        "add",
        "--title",
        "Explain me",
        "--goal",
        "Be investigated",
        "--criterion",
        "Reported",
    ])?;
    // A second project, listed first, to start a report from Projects.
    let other = fixture.root().join("another");
    fs::create_dir_all(&other)?;
    support::empty_repository(&other)?;
    let output = fixture
        .command()
        .arg("--project")
        .arg(&other)
        .args([
            "model-default",
            "operator",
            "--provider",
            "codex",
            "--model",
        ])
        .arg("operator")
        .output()?;
    assert!(output.status.success(), "{output:?}");
    fixture.mode("gated")?;
    let release = fixture.root().join("release-report");
    let mut t = Terminal::open_project(DEV, fixture.root(), &fixture.project(), cols, rows)?;
    t.screen("current task", |s| s.contains("Current #1 · Pending"))?;

    // Start from Current Task and watch it live.
    press(&mut t, mouse, "i Investigate", "i")?;
    shows(
        &mut t,
        "report form",
        &[
            "Agent reports · project",
            "No reports yet.",
            "< New report >",
        ],
    )?;
    if mouse {
        t.click("[Apply]")?;
    } else {
        t.send("\tWhy is it waiting?\r")?;
    }
    shows(
        &mut t,
        "live progress",
        &["Agent report #1 · Running · project", "Fixture agent read"],
    )?;
    fs::write(&release, "")?;
    shows(
        &mut t,
        "finished report",
        &[
            "Agent report #1 · Finished",
            "WHAT HAPPENED",
            "Fixture account of what happened.",
        ],
    )?;
    scroll_to(&mut t, mouse, "CAUSE")?;
    scroll_to(&mut t, mouse, "[owner only]")?;
    close(&mut t, mouse, "Current #1")?;
    fs::remove_file(&release)?;

    // Start from Projects, for the project selected there.
    press(&mut t, mouse, "0 Projects", "0")?;
    t.screen("projects", |s| s.contains("Projects · 2 registered"))?;
    press(&mut t, mouse, "another", "k")?;
    shows(&mut t, "other project selected", &["›   another"])?;
    press(&mut t, mouse, "i Investigate", "i")?;
    shows(&mut t, "projects form", &["Agent reports · another"])?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "projects live progress",
        &["Agent report #2 · Running · another", "Fixture agent read"],
    )?;
    // Stop it from the view; what it found stays, marked unfinished.
    press(&mut t, mouse, "s Stop", "s")?;
    shows(
        &mut t,
        "stopped report",
        &[
            "Agent report #2 · Stopped · another",
            "NOT A FINISHED REPORT: Stopped by owner",
        ],
    )?;
    close(&mut t, mouse, "Projects · 2 registered")?;

    // Read the open project's past report from Projects.
    press(&mut t, mouse, "* project", "j")?;
    shows(&mut t, "own project selected", &["› * project"])?;
    press(&mut t, mouse, "i Investigate", "i")?;
    shows(
        &mut t,
        "past reports",
        &["Agent reports · project", "1 report(s):", "#1 · Finished"],
    )?;
    press(&mut t, mouse, "< New report", " ")?;
    t.screen("past report chosen", |s| s.contains("< #1 · Finished"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "past report",
        &["Agent report #1 · Finished · project", "WHAT HAPPENED"],
    )?;
    // A second one, then back to the older by its button or key.
    press(&mut t, mouse, "b Reports", "b")?;
    t.screen("form again", |s| s.contains("< New report >"))?;
    fs::write(&release, "")?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "second report",
        &["Agent report #3 · Finished", "WHAT HAPPENED"],
    )?;
    press(&mut t, mouse, "[Older]", "[")?;
    shows(&mut t, "older report", &["Agent report #1 · Finished"])?;
    press(&mut t, mouse, "[Newer]", "]")?;
    shows(&mut t, "newer report", &["Agent report #3 · Finished"])?;
    close(&mut t, mouse, "Projects · 2 registered")?;

    // The limits are configuration of the open project.
    press(&mut t, mouse, "4 Config", "4")?;
    t.screen("config", |s| s.contains("[i Report limits]"))?;
    press(&mut t, mouse, "i Report limits", "i")?;
    shows(
        &mut t,
        "limits form",
        &["Agent report limits", "Time limit per report", "2.00"],
    )?;
    if mouse {
        t.click("Time limit per report")?;
    }
    t.send("\x017")?;
    t.screen("time limit typed", |s| s.contains("7▏"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    t.screen("limits saved", |s| s.contains("Agent report limits saved"))?;
    assert_eq!(
        fixture.ok(&["agent", "limits"])?["limits"]["time_limit_ms"],
        420_000
    );
    t.send("q")?;
    t.finish()?;

    let listed = fixture.ok(&["agent", "list"])?;
    assert_eq!(listed["count"], 2, "{listed}");
    assert_eq!(fixture.show(1)?["state"], "finished");
    assert_eq!(
        fixture.show(1)?["question"],
        if mouse {
            Value::Null
        } else {
            Value::from("Why is it waiting?")
        }
    );
    Ok(())
}

#[test]
fn demo_reports_are_a_fixture_that_never_opens_storage_or_an_agent() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            println!("=== DEMO {cols}x{rows} mouse={mouse} ===");
            let sandbox = support::sandbox()?;
            let mut t = Terminal::open_at(DEV, sandbox.path(), cols, rows)?;
            t.screen("demo", |s| s.contains("Current #1 · Failed"))?;
            press(&mut t, mouse, "i Investigate", "i")?;
            shows(
                &mut t,
                "demo form",
                &["Agent reports · demo workspace", "2 report(s):"],
            )?;
            press(&mut t, mouse, "[Apply]", "\r")?;
            shows(&mut t, "demo running", &["Agent report #3 · Running"])?;
            shows(&mut t, "demo progress", &["The demo check failed on its"])?;
            shows(
                &mut t,
                "demo finished",
                &["Agent report #3 · Finished", "WHAT HAPPENED"],
            )?;
            press(&mut t, mouse, "[Older]", "[")?;
            shows(
                &mut t,
                "demo stopped",
                &["Agent report #2 · Stopped", "NOT A FINISHED REPORT"],
            )?;
            close(&mut t, mouse, "Current #1")?;
            press(&mut t, mouse, "0 Projects", "0")?;
            t.screen("demo projects", |s| {
                s.contains("The simulator has one mock workspace") && s.contains("[i Investigate]")
            })?;
            press(&mut t, mouse, "i Investigate", "i")?;
            shows(&mut t, "demo projects form", &["3 report(s):"])?;
            press(&mut t, mouse, "[Cancel]", "\x1b")?;
            t.screen("closed", |s| !s.contains("[Cancel]"))?;
            t.send("q")?;
            t.finish()?;
            assert!(
                !sandbox
                    .path()
                    .join("state/ktask-dev/ktask.sqlite3")
                    .exists()
            );
            assert!(
                !sandbox
                    .path()
                    .join("state/ktask-dev/investigations")
                    .exists()
            );
        }
    }
    Ok(())
}
