//! Agent fixes through the real binaries with a fixture agent: a wrong check command
//! corrected and verified, work in the tree preserved, refused steps, a stop mid-fix, a
//! failed verification, a report carried out, and the same flow in the terminal.
use crate::support::{self, Result, Terminal};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");

/// The fixture agent for every role. A fix turn behaves as `$HOME/agent-mode` says.
const AGENT: &str = r##"#!/bin/sh
model=unknown
printf '%s\n' "$*" >> "$HOME/agent-args"
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
case "$prompt" in
 "You are ktask's investigator"*)
  message '{"what_happened":"The check command is wrong.","cause":"It names a missing file.","references":[{"kind":"command","target":"ls","detail":"no such file"}],"fix_steps":[{"class":"configuration","action":"ktask check-set build to test README.md","workaround":false},{"class":"owner","action":"Decide whether to keep the check","workaround":false}],"ktask_defect":null}'
  done_turn; exit 0;;
 "You are planning an agent fix"*)
  message '{"steps":[{"class":"configuration","action":"Correct the check command"},{"class":"owner","action":"Delete the old project"}]}'
  done_turn; exit 0;;
 "You are ktask's fix agent"*) ;;
 *)
  case "$prompt" in *"Wait for release"*)
   printf 'partial\n' > partial.txt
   while [ ! -f "$HOME/release-runner" ]; do sleep 0.05; done;;
  *) printf 'done\n' > delivered.txt;;
  esac
  message '{"outcome":"complete","summary":"Delivered","question":null,"criteria":[{"ordinal":1,"passed":true,"evidence":"delivered.txt"}],"findings":[]}'
  done_turn; exit 0;;
esac
printf '%s' "$prompt" > "$HOME/fix-prompt"
printf '%s' "$TMPDIR" > "$HOME/fix-tmpdir"
run=$(printf '%s\n' "$prompt" | sed -n 's/^  \(.*fix-command --\) <ktask arguments>$/\1/p')
printf '%s' "$run" > "$HOME/fix-run"
ktask() { eval "$run $1" >> "$HOME/commands" 2>&1; printf '%s %s\n' "$?" "$1" >> "$HOME/statuses"; }
mode=$(cat "$HOME/agent-mode" 2>/dev/null || echo nothing)
message "Fix agent read the records"
say '{"type":"item.completed","item":{"type":"command_execution","command":"ls","aggregated_output":"README.md","exit_code":0}}'
reply='{"summary":"Fixture fix summary.","steps":[],"remaining":[]}'
case "$mode" in
 check)
  ktask "check-set build --command 'test -f README.md'"
  ktask "step 'Looked at the project folder' 'README.md exists'";;
 gated)
  while [ ! -f "$HOME/release-fix" ]; do sleep 0.05; done
  ktask "check-set build --command 'test -f README.md'";;
 slow)
  ktask "step 'Installed the fixture tool' 'in the home directory'"
  printf 'half\n' > "$HOME/half-done"
  setsid sleep 300 > /dev/null 2>&1 &
  touch "$HOME/slow-ready"
  while :; do sleep 0.1; done;;
 scope)
  ktask "project-delete 1 --confirm project"
  ktask "done 1"
  ktask "shutdown"
  ktask "model-default execution --provider codex --model x --new-projects"
  ktask "approve 2"
  ktask "--project /tmp add --title t --goal g --criterion c"
  ktask "add --title 'Repair the product code' --goal 'Correct the file' --criterion 'It is correct'"
  git reset --hard HEAD >> "$HOME/commands" 2>&1; printf '%s reset\n' "$?" >> "$HOME/statuses"
  git push --force origin main >> "$HOME/commands" 2>&1; printf '%s push\n' "$?" >> "$HOME/statuses"
  attempted() { if (: >> "$1") 2>/dev/null; then say_to=wrote; else say_to=blocked; fi; printf '%s %s\n' "$2" "$say_to" >> "$HOME/writes"; }
  attempted "$HOME/other/written-by-fix" other
  attempted "$HOME/state/ktask-dev/ktask.sqlite3" database
  attempted "$HOME/host-file" host
  attempted "$TMPDIR/scratch-file" scratch
  "${run%% --worker-token*}" >/dev/null 2>&1 || true
  eval "${run%% --worker-token*} --project $PWD list" > "$HOME/direct" 2>&1; printf '%s direct\n' "$?" >> "$HOME/statuses"
  reply='{"summary":"Tried more than allowed.","steps":[],"remaining":["Decide about the old project"]}';;
 commit)
  git stash pop >> "$HOME/commands" 2>&1
  git add -A; git commit -qm "ktask: agent fix #$KTASK_FIX_ID: keep the notes" >> "$HOME/commands" 2>&1;;
 authored) printf 'rewritten\n' > README.md;;
 discard) rm -f partial.txt;;
 stoptask) ktask "stop 1";;
 dependency)
  printf '#!/bin/sh\nexit 0\n' > "$HOME/bin/fixture-tool"; chmod +x "$HOME/bin/fixture-tool"
  ktask "step 'Installed fixture-tool' 'in $HOME/bin'";;
 probe)
  ktask "probe 'test -f \"$HOME/installed-tool\"'"
  touch "$HOME/installed-tool"
  ktask "step 'Installed the missing tool' '$HOME/installed-tool'";;
 nothing) reply='{"summary":"I could not repair it.","steps":[],"remaining":["Repair the check"]}';;
esac
message "$reply"
done_turn
"##;

struct Fixture {
    sandbox: support::Sandbox,
}

impl Fixture {
    /// A registered project with one tracked file, every model and a check that fails.
    fn new(check: &str) -> Result<Self> {
        let fixture = Self {
            sandbox: support::sandbox()?,
        };
        let project = fixture.project();
        fs::create_dir_all(&project)?;
        support::empty_repository(&project)?;
        fs::write(project.join(".git/info/exclude"), "")?;
        fs::write(project.join("README.md"), "Fixture project\n")?;
        fixture.git(&["add", "README.md"])?;
        fixture.git(&["commit", "-qm", "readme"])?;
        fs::create_dir_all(fixture.root().join("bin"))?;
        support::write_executable(&fixture.root().join("bin/codex"), AGENT)?;
        fixture.ok(&["check-set", "build", "--command", check])?;
        // The planning model completes the setup that init reports on.
        for role in [
            "planning",
            "execution",
            "review",
            "validation",
            "remediation",
            "operator",
        ] {
            fixture.ok(&[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ])?;
        }
        fixture.ok(&["init", "--agent-network", "on"])?;
        Ok(fixture)
    }

    fn root(&self) -> &Path {
        self.sandbox.path()
    }

    fn project(&self) -> PathBuf {
        self.root().join("project")
    }

    fn git(&self, args: &[&str]) -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.project())
            .args(args)
            .output()?;
        assert!(output.status.success(), "git {args:?}: {output:?}");
        Ok(String::from_utf8(output.stdout)?.trim_end().to_owned())
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

    fn run_in(&self, project: &Path, args: &[&str]) -> Result<Output> {
        Ok(self
            .command()
            .arg("--project")
            .arg(project)
            .arg("--json")
            .args(args)
            .output()?)
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        self.run_in(&self.project(), args)
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
        assert_eq!(output.status.code(), Some(status), "{args:?}: {output:?}");
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

    fn add(&self, title: &str) -> Result {
        self.ok(&[
            "add",
            "--title",
            title,
            "--goal",
            title,
            "--criterion",
            "Delivered",
        ])?;
        Ok(())
    }

    fn start(&self, args: &[&str]) -> Result<i64> {
        let mut command = vec!["agent", "fix"];
        command.extend(args);
        Ok(self.ok(&command)?["id"].as_i64().ok_or("fix ID")?)
    }

    fn shown(&self, id: i64) -> Result<Value> {
        self.ok(&["agent", "show", &id.to_string()])
    }

    fn ended(&self, id: i64) -> Result<Value> {
        support::eventually(&format!("agent fix {id} ends"), || {
            let shown = self.shown(id)?;
            Ok((shown["investigation"]["state"] != "running").then_some(shown))
        })
    }

    fn projects(&self) -> Result<Value> {
        let output = self.command().args(["--json", "projects"]).output()?;
        assert!(output.status.success(), "{output:?}");
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].take())
    }

    fn state(&self, task: i64) -> Result<String> {
        Ok(self.ok(&["show", &task.to_string()])?["state"]
            .as_str()
            .ok_or("task state")?
            .to_owned())
    }

    /// The fix's whole record as `agent show` prints it, also kept in the test transcript.
    fn record(&self, id: i64) -> Result<String> {
        let text = self.text(&["agent", "show", &id.to_string()])?;
        println!("{text}");
        Ok(text)
    }
}

fn entries<'a>(shown: &'a Value, list: &str) -> &'a [Value] {
    shown["fix"][list].as_array().map_or(&[], Vec::as_slice)
}

fn step<'a>(shown: &'a Value, action: &str) -> &'a Value {
    entries(shown, "steps")
        .iter()
        .find(|step| step["action"].as_str().is_some_and(|a| a.contains(action)))
        .unwrap_or_else(|| panic!("no step {action:?} in {shown:#}"))
}

fn probe<'a>(shown: &'a Value, kind: &str) -> &'a Value {
    entries(shown, "probes")
        .iter()
        .find(|probe| probe["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} probe in {shown:#}"))
}

fn changed(shown: &Value, kind: &str) -> Vec<String> {
    entries(shown, "changes")
        .iter()
        .filter(|change| change["kind"] == kind)
        .map(|change| format!("{} {}", change["target"], change["detail"]))
        .collect()
}

#[test]
fn a_wrong_check_command_is_corrected_verified_and_the_queue_continues() -> Result {
    let fixture = Fixture::new("test -f no-such-file")?;
    fixture.add("Deliver the file")?;
    fixture.mode("check")?;
    let id = fixture.start(&["The build check names a missing file", "--resume"])?;
    let shown = fixture.ended(id)?;
    fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    assert_eq!(shown["investigation"]["kind"], "fix");
    assert_eq!(shown["investigation"]["model"]["model"], "operator");
    assert_eq!(shown["fix"]["continued"], true, "{shown:#}");
    let check = probe(&shown, "check");
    assert_eq!(
        (&check["passed_before"], &check["passed_after"]),
        (&Value::from(false), &Value::from(true))
    );
    let set = step(&shown, "ktask check-set build");
    assert_eq!(
        (&set["status"], &set["source"]),
        (&"applied".into(), &"ktask".into())
    );
    assert_eq!(
        step(&shown, "Looked at the project folder")["source"],
        "agent"
    );
    let settings = changed(&shown, "setting");
    assert!(
        settings[0].contains("check build")
            && settings[0].contains("test -f no-such-file")
            && settings[0].contains("test -f README.md"),
        "{settings:?}"
    );
    // Attributed to the fix, never to the owner.
    let events = fixture.ok(&["events", "--limit", "1000"])?.to_string();
    assert!(
        events.contains(&format!("agent fix #{id}: ktask check-set build")),
        "{events}"
    );
    // It ran with the operator model, full access inside ktask's own sandbox and its
    // own scratch directory, which is gone afterwards.
    assert!(
        fixture
            .read("agent-args")?
            .contains("sandbox_mode=\"danger-full-access\"")
    );
    assert!(!Path::new(&fixture.read("fix-tmpdir")?).exists());
    // The verified fix continued the queue: the task is delivered.
    support::eventually("the queued task finishes", || {
        Ok((fixture.state(1)? == "done").then_some(()))
    })?;
    support::eventually("the runner exits", || {
        Ok(fixture.projects()?["projects"][0]["runner_alive"]
            .eq(&Value::from(false))
            .then_some(()))
    })?;
    let listed = fixture.text(&["agent", "list"])?;
    assert!(listed.contains(&format!("#{id} · fix · Done")), "{listed}");
    Ok(())
}

#[test]
fn work_found_in_the_tree_is_preserved_in_a_stash_naming_the_fix() -> Result {
    let fixture = Fixture::new("true")?;
    fs::write(fixture.project().join("notes.txt"), "half an idea\n")?;
    fs::write(fixture.project().join("README.md"), "Edited by hand\n")?;
    let id = fixture.start(&["A new task cannot start: the tree is dirty"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    assert_eq!(probe(&shown, "work_tree")["passed_after"], true);
    assert_eq!(fixture.git(&["status", "--porcelain"])?, "");
    let message = format!("ktask: agent fix #{id}: work found in the tree");
    assert!(
        fixture.git(&["stash", "list"])?.contains(&message),
        "stash list"
    );
    assert_eq!(
        fixture.git(&["show", "stash@{0}^3:notes.txt"])?,
        "half an idea"
    );
    assert_eq!(
        fixture.git(&["show", "stash@{0}:README.md"])?,
        "Edited by hand"
    );
    let stashes = changed(&shown, "stash");
    assert!(stashes[0].contains(&message) && stashes[0].contains("notes.txt"));
    assert!(record.contains("The runner stays paused"), "{record}");

    // Committed instead, as instructed, with a message naming the fix.
    fixture.git(&["stash", "pop"])?;
    fixture.mode("commit")?;
    let id = fixture.start(&["Commit the notes that are in the tree"])?;
    let shown = fixture.ended(id)?;
    fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    assert_eq!(fixture.git(&["status", "--porcelain"])?, "");
    assert!(
        changed(&shown, "commit")[0].contains(&format!("agent fix #{id}: keep the notes")),
        "{shown:#}"
    );
    // The entry the agent popped stays recoverable.
    assert!(!changed(&shown, "recovery_ref").is_empty(), "{shown:#}");

    // Files the agent writes itself are named, and the fix is not done.
    fixture.mode("authored")?;
    let id = fixture.start(&["Tidy up"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "unverified", "{shown:#}");
    assert!(record.contains("this fix leaves uncommitted files: M README.md"));
    assert!(record.contains("code changes belong in a task"), "{record}");
    assert_eq!(
        fs::read_to_string(fixture.project().join("README.md"))?,
        "rewritten\n"
    );
    Ok(())
}

#[test]
fn steps_outside_the_scope_are_refused_and_left_for_the_owner() -> Result {
    let fixture = Fixture::new("true")?;
    fixture.add("Keep me")?;
    fixture.ok(&[
        "add",
        "--title",
        "Draft",
        "--goal",
        "g",
        "--criterion",
        "c",
        "--approval",
        "draft",
    ])?;
    let other = fixture.root().join("other");
    fs::create_dir_all(&other)?;
    support::empty_repository(&other)?;
    let output = fixture.run_in(
        &other,
        &["add", "--title", "Other", "--goal", "g", "--criterion", "c"],
    )?;
    assert!(output.status.success(), "{output:?}");
    let head = fixture.git(&["rev-parse", "HEAD"])?;
    fixture.mode("scope")?;
    let id = fixture.start(&["Clean up everything"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    println!("{}", fixture.read("commands")?);
    for action in [
        "ktask project-delete",
        "ktask done 1",
        "ktask shutdown",
        "--new-projects",
        "ktask approve 2",
        "ktask --project /tmp",
        "git reset --hard HEAD",
        "git push --force origin main",
    ] {
        assert_eq!(step(&shown, action)["status"], "refused", "{action}");
    }
    let statuses = fixture.read("statuses")?;
    for refused in ["4 project-delete", "4 done 1", "1 reset", "1 push"] {
        assert!(statuses.contains(refused), "{refused}: {statuses}");
    }
    assert!(
        record.contains("left for the owner: ktask done 1"),
        "{record}"
    );
    assert!(record.contains("left for the owner: Decide about the old project"));
    // Any other way into ktask is refused too, and the sandbox keeps its files and the
    // other project read-only while the account stays writable.
    assert!(fixture.read("direct")?.contains("fix-command"));
    assert_eq!(
        fixture.read("writes")?,
        "other blocked\ndatabase blocked\nhost wrote\nscratch wrote\n"
    );
    assert!(!other.join("written-by-fix").exists());
    assert_eq!(fixture.git(&["rev-parse", "HEAD"])?, head);
    assert_eq!(fixture.state(1)?, "pending");
    assert_eq!(fixture.state(2)?, "needs_approval");
    assert_eq!(fixture.projects()?["count"], 2);
    // The allowed task went into the queue as the fix's own, an AI's.
    let added = fixture.ok(&["show", "4"])?;
    let revision = &added["revisions"][0];
    assert_eq!(
        revision["actor"],
        format!("ai: agent fix #{id}"),
        "{added:#}"
    );
    assert_eq!(step(&shown, "ktask add --title")["status"], "applied");
    Ok(())
}

#[test]
fn a_fix_pauses_a_live_runner_runs_alone_and_stop_keeps_what_it_did() -> Result {
    let fixture = Fixture::new("true")?;
    fixture.add("Wait for release")?;
    fixture.ok(&["run", "--background"])?;
    support::eventually("the worker writes its partial work", || {
        Ok(fixture.project().join("partial.txt").exists().then_some(()))
    })?;
    fixture.mode("slow")?;
    let id = fixture.start(&["The task hangs"])?;
    support::eventually("the fix agent works", || {
        Ok(fixture.root().join("slow-ready").exists().then_some(()))
    })?;
    let shown = fixture.shown(id)?;
    assert_eq!(shown["fix"]["runner_was_running"], true, "{shown:#}");
    assert_eq!(shown["fix"]["paused_task"], 1);
    assert_eq!(fixture.state(1)?, "paused");
    // The paused attempt keeps its work; no runner, second fix or report may start.
    assert!(fixture.project().join("partial.txt").exists());
    let refused = fixture.refused(&["run"], 4)?;
    assert!(
        refused.contains(&format!("Agent fix #{id} is working")),
        "{refused}"
    );
    for args in [&["agent", "fix", "again"][..], &["agent", "report"]] {
        let refused = fixture.refused(args, 4)?;
        assert!(
            refused.contains(&format!("Agent fix #{id} is already running")),
            "{refused}"
        );
    }
    let stopped = fixture.ok(&["agent", "stop", &id.to_string()])?;
    assert_eq!(stopped["investigation"]["state"], "stopped");
    let shown = fixture.shown(id)?;
    let record = fixture.record(id)?;
    assert_eq!(
        step(&shown, "Pause the project runner")["status"],
        "applied"
    );
    assert_eq!(
        step(&shown, "Installed the fixture tool")["status"],
        "applied"
    );
    assert!(record.contains("NOT DONE: Stopped by owner"), "{record}");
    assert!(record.contains("partial.txt"), "{record}");
    assert_eq!(fixture.state(1)?, "paused");
    assert_eq!(fixture.projects()?["projects"][0]["runner_alive"], false);
    // Its processes are reaped; the sandbox check at the end of the test would name any.
    assert!(!Path::new(&fixture.read("fix-tmpdir")?).exists());
    fs::write(fixture.root().join("release-runner"), "")?;
    fixes_of_a_paused_task(&fixture)
}

/// Later fixes of the task the first one paused.
fn fixes_of_a_paused_task(fixture: &Fixture) -> Result {
    // A time limit ends a fix the same way.
    fixture.ok(&["agent", "limits", "--fix-time-limit-ms", "3000"])?;
    let id = fixture.start(&["Hang again", "--resume"])?;
    let shown = fixture.ended(id)?;
    assert_eq!(shown["investigation"]["state"], "timed_out", "{shown:#}");
    assert_eq!(shown["fix"]["continued"], false);
    assert_eq!(fixture.state(1)?, "paused");
    fixture.ok(&["agent", "limits", "--fix-time-limit-ms", "600000"])?;

    // Stopping the task is allowed: its work goes into the usual stash, nothing more.
    fs::copy(
        fixture.project().join("partial.txt"),
        fixture.root().join("partial.saved"),
    )?;
    fixture.mode("stoptask")?;
    let id = fixture.start(&["Give the task a fresh start"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(step(&shown, "ktask stop 1")["status"], "applied");
    assert_eq!(fixture.state(1)?, "pending");
    assert!(
        changed(&shown, "stash")[0].contains("ktask: task #1 attempt #1"),
        "{shown:#}"
    );
    assert!(!record.contains("wrote project files itself"), "{record}");
    assert!(!record.contains("was discarded"), "{record}");

    // Removing a paused task's work is a failed fix, and the work is kept.
    fs::remove_file(fixture.root().join("release-runner"))?;
    fixture.ok(&["run", "--background"])?;
    support::eventually("the worker writes again", || {
        Ok(fixture.project().join("partial.txt").exists().then_some(()))
    })?;
    fixture.mode("discard")?;
    let id = fixture.start(&["Remove the leftovers"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "failed", "{shown:#}");
    assert!(
        record.contains("uncommitted work was discarded"),
        "{record}"
    );
    assert_eq!(
        fixture.git(&[
            "show",
            &format!("refs/ktask/agent-fix-{id}/work:partial.txt")
        ])?,
        "partial"
    );
    fs::write(fixture.root().join("release-runner"), "")?;
    Ok(())
}

#[test]
fn a_fix_over_its_cost_limit_ends_with_the_runner_paused() -> Result {
    let fixture = Fixture::new("true")?;
    fixture.add("Deliver the file")?;
    support::write_executable(
        &fixture.root().join("bin/opencode"),
        r##"#!/bin/sh
cat > /dev/null
printf '%s\n' '{"type":"step_finish","sessionID":"s","part":{"reason":"tool-calls","cost":0.75,"tokens":{"input":10,"output":5,"reasoning":0,"cache":{"read":0,"write":0}}}}'
while :; do sleep 0.1; done
"##,
    )?;
    support::write_executable(
        &fixture.root().join("bin/claude"),
        r##"#!/bin/sh
printf '%s\n' "$@" > "$HOME/claude-args"
cat > /dev/null
printf '%s\n' '{"type":"result","subtype":"error_max_budget_usd","is_error":true,"result":"","total_cost_usd":0.51,"usage":{"input_tokens":10,"output_tokens":5,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}'
exit 1
"##,
    )?;
    fixture.ok(&["agent", "limits", "--fix-cost-limit-usd", "0.50"])?;
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
        let id = fixture.start(&["Spend too much", "--resume"])?;
        let shown = fixture.ended(id)?;
        let record = fixture.record(id)?;
        let run = &shown["investigation"];
        assert_eq!(run["state"], "over_budget", "{provider}: {shown:#}");
        assert_eq!(run["usage"]["cost_microusd"], cost);
        assert_eq!(shown["fix"]["continued"], false);
        assert_eq!(
            step(&shown, "Pause the project runner")["status"],
            "applied"
        );
        assert!(record.contains("The runner stays paused"), "{record}");
    }
    // Claude Code also enforces the limit itself.
    assert!(
        fixture
            .read("claude-args")?
            .contains("--max-budget-usd\n0.50\n")
    );
    assert_eq!(fixture.state(1)?, "pending");
    Ok(())
}

#[test]
fn a_fix_that_repairs_nothing_is_failed_and_leaves_the_runner_paused() -> Result {
    let fixture = Fixture::new("test -f no-such-file")?;
    fixture.add("Deliver the file")?;
    let id = fixture.start(&["Repair the check", "--resume"])?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "failed", "{shown:#}");
    assert_eq!(shown["fix"]["continued"], false);
    assert_eq!(probe(&shown, "check")["passed_after"], false);
    assert!(record.contains("nothing got better"), "{record}");
    assert!(record.contains("still failing: check build"), "{record}");
    assert!(record.contains("left for the owner: Repair the check"));
    assert_eq!(fixture.state(1)?, "pending");
    assert!(!fixture.project().join("delivered.txt").exists());

    // A failure named by the agent is repeated as well: here a missing tool.
    fixture.ok(&["check-set", "build", "--command", "true"])?;
    fixture.mode("probe")?;
    let id = fixture.start(&["Install the missing tool"])?;
    let shown = fixture.ended(id)?;
    fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    let command = probe(&shown, "command");
    assert_eq!(
        (&command["passed_before"], &command["passed_after"]),
        (&Value::from(false), &Value::from(true))
    );
    assert_eq!(
        step(&shown, "Installed the missing tool")["status"],
        "applied"
    );

    // So is a declared requirement: its dependency probe is repeated.
    let task = fixture.ok(&["current"])?["id"].to_string();
    fixture.ok(&[
        "require",
        &task,
        "--kind",
        "executable",
        "--item",
        "fixture-tool",
        "--scope",
        "project",
        "--stage",
        "execution",
        "--reproduction",
        "fixture-tool: not found",
    ])?;
    fixture.mode("dependency")?;
    let id = fixture.start(&["Install fixture-tool"])?;
    let shown = fixture.ended(id)?;
    fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    let dependency = probe(&shown, "dependency");
    assert_eq!(
        (&dependency["passed_before"], &dependency["passed_after"]),
        (&Value::from(false), &Value::from(true))
    );

    // With nothing that failed, a fix is never called done.
    fixture.mode("nothing")?;
    let id = fixture.start(&["Make it better"])?;
    assert_eq!(fixture.ended(id)?["investigation"]["state"], "unverified");
    Ok(())
}

#[test]
fn a_report_is_carried_out_and_a_dry_run_changes_nothing() -> Result {
    let fixture = Fixture::new("test -f no-such-file")?;
    let report = fixture.ok(&["agent", "report"])?["id"]
        .as_i64()
        .ok_or("report ID")?;
    fixture.ended_report(report)?;
    let number = report.to_string();

    fs::write(fixture.project().join("notes.txt"), "keep\n")?;
    let status = fixture.git(&["status", "--porcelain"])?;
    let dry = fixture.text(&["agent", "fix", "--report", &number, "--dry-run"])?;
    println!("{dry}");
    for part in [
        "Nothing was changed",
        "Run check build before and after",
        "Stash the work found in the tree: ?? notes.txt",
        "[configuration change] ktask check-set build to test README.md",
        "[owner only] Decide whether to keep the check (left for the owner)",
    ] {
        assert!(dry.contains(part), "{part}: {dry}");
    }
    let dry = fixture.ok(&["agent", "fix", "Repair the build", "--dry-run"])?;
    assert_eq!(
        dry["plan"]["steps"][0]["action"],
        "Correct the check command"
    );
    assert_eq!(dry["plan"]["steps"][1]["by_fix"], false);
    assert_eq!(fixture.git(&["status", "--porcelain"])?, status);
    assert_eq!(fixture.git(&["stash", "list"])?, "");
    assert_eq!(fixture.ok(&["agent", "list"])?["count"], 1);
    fs::remove_file(fixture.project().join("notes.txt"))?;

    // Only a finished report of this project can be carried out.
    let other = fixture.root().join("other");
    fs::create_dir_all(&other)?;
    support::empty_repository(&other)?;
    let output = fixture.run_in(
        &other,
        &[
            "model-default",
            "operator",
            "--provider",
            "codex",
            "--model",
            "operator",
        ],
    )?;
    assert!(output.status.success(), "{output:?}");
    let output = fixture.run_in(&other, &["agent", "fix", "--report", &number])?;
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("belongs to another project"));
    // Only the owner starts a fix.
    let refused = fixture.refused(
        &[
            "--actor-kind",
            "ai",
            "--actor",
            "helper",
            "agent",
            "fix",
            "x",
        ],
        4,
    )?;
    assert!(refused.contains("started by the owner"), "{refused}");
    fixture.refused(&["agent", "fix"], 2)?;
    fixture.refused(&["agent", "fix", "--report", &number, "and this"], 2)?;
    fixture.refused(&["agent", "fix", "--report", "999"], 4)?;

    fixture.mode("gated")?;
    let id = fixture.start(&["--report", &number])?;
    let refused = fixture.refused(&["agent", "fix", "--report", &id.to_string()], 4)?;
    assert!(refused.contains("already running"), "{refused}");
    fs::write(fixture.root().join("release-fix"), "")?;
    let shown = fixture.ended(id)?;
    let record = fixture.record(id)?;
    assert_eq!(shown["investigation"]["state"], "finished", "{shown:#}");
    assert_eq!(shown["fix"]["report_id"], report);
    assert_eq!(
        step(&shown, "Decide whether to keep the check")["status"],
        "left_for_owner"
    );
    assert!(
        record.contains(&format!("From: Carry out report #{report}")),
        "{record}"
    );
    assert!(
        fixture
            .read("fix-prompt")?
            .contains("1. [configuration change] ktask check-set build to test README.md")
    );
    // A fix is not a report to carry out.
    let refused = fixture.refused(&["agent", "fix", "--report", &id.to_string()], 4)?;
    assert!(refused.contains("not a report"), "{refused}");
    Ok(())
}

impl Fixture {
    fn ended_report(&self, id: i64) -> Result {
        let shown = self.ended_state(id)?;
        assert_eq!(shown, "finished");
        Ok(())
    }

    fn ended_state(&self, id: i64) -> Result<String> {
        support::eventually(&format!("report {id} ends"), || {
            let shown = self.shown(id)?;
            Ok((shown["investigation"]["state"] != "running")
                .then(|| shown["investigation"]["state"].as_str().map(str::to_owned))
                .flatten())
        })
    }
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

/// Keeps a real frame as visual evidence when the run collects it.
fn keep(screen: &str, name: &str, cols: u16, rows: u16, mouse: bool) -> Result {
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        let name = format!("agent-fix-{cols}x{rows}-mouse-{mouse}-{name}");
        support::screen_png(
            screen,
            &PathBuf::from(directory).join(format!("{name}.png")),
        )?;
    }
    Ok(())
}

/// Scrolls the open record until `part` is on screen, by wheel or by PgDn.
fn scroll_to(t: &mut Terminal, mouse: bool, part: &str) -> Result {
    for _ in 0..30 {
        if t.current_screen().contains(part) {
            let screen = t.still(part, |s| s.contains(part))?;
            println!("SCROLLED TO {part}\n{screen}");
            return Ok(());
        }
        let before = t.current_screen();
        if mouse {
            t.wheel(true, 30, 6)?;
        } else {
            t.send("\x1b[6~")?;
        }
        t.screen("record scrolls", |s| s != before)?;
    }
    Err(format!("never scrolled to {part}:\n{}", t.current_screen()).into())
}

fn close(t: &mut Terminal, mouse: bool, back: &str) -> Result {
    press(t, mouse, "[Close]", "\x1b")?;
    t.screen("record closed", |s| {
        s.contains(back) && !s.contains("[Close]")
    })?;
    Ok(())
}

#[test]
fn fixes_are_confirmed_followed_stopped_and_read_by_keyboard_and_mouse_at_both_sizes() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            println!("=== FIX {cols}x{rows} mouse={mouse} ===");
            drive(cols, rows, mouse)?;
        }
    }
    Ok(())
}

fn drive(cols: u16, rows: u16, mouse: bool) -> Result {
    let fixture = Fixture::new("test -f no-such-file")?;
    fixture.add("Repair me")?;
    let report = fixture.ok(&["agent", "report"])?["id"]
        .as_i64()
        .ok_or("report ID")?;
    fixture.ended_report(report)?;
    fixture.mode("gated")?;
    let release = fixture.root().join("release-fix");
    let mut t = Terminal::open_project(DEV, fixture.root(), &fixture.project(), cols, rows)?;
    t.screen("current task", |s| s.contains("Current #1 · Pending"))?;

    // From an instruction, started on Current Task.
    press(&mut t, mouse, "f Fix", "f")?;
    keep(
        &t.screen("form drawn", |s| s.contains("When verified"))?,
        "form",
        cols,
        rows,
        mouse,
    )?;
    t.still("form", |s| s.contains("When verified"))?;
    shows(
        &mut t,
        "fix form",
        &[
            "Agent fixes · project",
            "No fixes yet.",
            "< New fix >",
            "< The instruction below >",
            "< Leave the runner paused >",
        ],
    )?;
    if mouse {
        t.click("Instruction (when not a report)")?;
    } else {
        t.send("\t\t")?;
    }
    t.send("Correct the build check")?;
    t.screen("typed", |s| s.contains("Correct the build check▏"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "confirmation",
        &[
            "Start agent fix · project?",
            "Project: project",
            "Instruction: Correct the build check",
            "[Start fix]",
            "[Cancel]",
        ],
    )?;
    scroll_to(&mut t, mouse, "write product code")?;
    keep(&t.current_screen(), "confirm", cols, rows, mouse)?;
    // Nothing started before the confirmation.
    assert_eq!(fixture.ok(&["agent", "list"])?["count"], 1);
    press(&mut t, mouse, "[Start fix]", "\r")?;
    let live = shows(
        &mut t,
        "live fix",
        &["Agent fix #2 · Running · project", "Fix agent read"],
    )?;
    keep(&live, "live", cols, rows, mouse)?;
    fs::write(&release, "")?;
    let done = shows(
        &mut t,
        "finished fix",
        &["Agent fix #2 · Done · project", "VERIFICATION", "Verified:"],
    )?;
    keep(&done, "done", cols, rows, mouse)?;
    scroll_to(&mut t, mouse, "STEPS")?;
    scroll_to(&mut t, mouse, "CHANGED")?;
    scroll_to(&mut t, mouse, "WHAT REMAINS")?;
    keep(&t.current_screen(), "remains", cols, rows, mouse)?;
    close(&mut t, mouse, "Current #1")?;
    fs::remove_file(&release)?;

    // From a report: read it, then carry it out and stop the fix.
    press(&mut t, mouse, "i Investigate", "i")?;
    shows(&mut t, "report form", &["Agent reports · project"])?;
    press(&mut t, mouse, "< New report", " ")?;
    t.screen("past report chosen", |s| s.contains("< #1 · Finished"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "report with fix button",
        &["Agent report #1 · Finished", "[f Fix]"],
    )?;
    press(&mut t, mouse, "[f Fix]", "f")?;
    shows(
        &mut t,
        "report preselected",
        &["Agent fixes · project", "< Report #1 ·", "1 fix(es):"],
    )?;
    if mouse {
        t.click("< Leave the runner paused")?;
    } else {
        t.send("\t\t\t ")?;
    }
    t.screen("resume chosen", |s| s.contains("< Continue the queue >"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(
        &mut t,
        "report confirmation",
        &[
            "Start agent fix · project?",
            "Carries out the proposed steps of report #1",
            "When verified: continue the queue",
        ],
    )?;
    press(&mut t, mouse, "[Start fix]", "\r")?;
    shows(
        &mut t,
        "second fix",
        &["Agent fix #3 · Running · project", "Fix agent read"],
    )?;
    press(&mut t, mouse, "s Stop", "s")?;
    shows(
        &mut t,
        "stopped fix",
        &[
            "Agent fix #3 · Stopped · project",
            "NOT DONE: Stopped by owner",
        ],
    )?;
    press(&mut t, mouse, "[Older]", "[")?;
    shows(&mut t, "older fix", &["Agent fix #2 · Done"])?;
    press(&mut t, mouse, "[Newer]", "]")?;
    shows(&mut t, "newer fix", &["Agent fix #3 · Stopped"])?;
    close(&mut t, mouse, "Current #1")?;

    // Read a past fix from Projects.
    press(&mut t, mouse, "0 Projects", "0")?;
    t.screen("projects", |s| {
        s.contains("Projects · 1 registered") && s.contains("[f Fix]")
    })?;
    press(&mut t, mouse, "f Fix", "f")?;
    shows(
        &mut t,
        "projects form",
        &["Agent fixes · project", "2 fix(es):"],
    )?;
    press(&mut t, mouse, "< New fix", " ")?;
    t.screen("past fix chosen", |s| s.contains("< #3 · fix · Stopped"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    shows(&mut t, "past fix", &["Agent fix #3 · Stopped · project"])?;
    press(&mut t, mouse, "b Fixes", "b")?;
    t.screen("form again", |s| s.contains("< New fix >"))?;
    press(&mut t, mouse, "[Cancel]", "\x1b")?;
    t.screen("closed", |s| !s.contains("[Cancel]"))?;

    // The fix limits are configuration of the open project.
    press(&mut t, mouse, "4 Config", "4")?;
    t.screen("config", |s| s.contains("[i Report limits]"))?;
    press(&mut t, mouse, "i Report limits", "i")?;
    shows(
        &mut t,
        "limits form",
        &["Agent report limits", "Time limit per fix", "5.00"],
    )?;
    if mouse {
        t.click("Time limit per fix")?;
    } else {
        t.send("\t\t")?;
    }
    t.send("\x019")?;
    t.screen("fix time typed", |s| s.contains("9▏"))?;
    press(&mut t, mouse, "[Apply]", "\r")?;
    t.screen("limits saved", |s| s.contains("Agent report limits saved"))?;
    assert_eq!(
        fixture.ok(&["agent", "limits"])?["fix_limits"]["time_limit_ms"],
        540_000
    );
    t.send("q")?;
    t.finish()?;

    let first = fixture.shown(2)?;
    assert_eq!(first["investigation"]["state"], "finished", "{first:#}");
    assert_eq!(first["fix"]["resume"], false);
    let second = fixture.shown(3)?;
    assert_eq!(second["investigation"]["state"], "stopped", "{second:#}");
    assert_eq!(second["fix"]["report_id"], report);
    assert_eq!(second["fix"]["resume"], true);
    assert_eq!(second["fix"]["continued"], false);
    assert_eq!(fixture.state(1)?, "pending");
    Ok(())
}

#[test]
fn demo_fixes_are_a_fixture_that_never_opens_storage_or_an_agent() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            println!("=== DEMO FIX {cols}x{rows} mouse={mouse} ===");
            let sandbox = support::sandbox()?;
            let mut t = Terminal::open_at(DEV, sandbox.path(), cols, rows)?;
            t.screen("demo", |s| s.contains("Current #1 · Failed"))?;
            press(&mut t, mouse, "f Fix", "f")?;
            shows(
                &mut t,
                "demo form",
                &["Agent fixes · demo workspace", "1 fix(es):"],
            )?;
            // Neither a report nor an instruction: nothing to confirm.
            press(&mut t, mouse, "[Apply]", "\r")?;
            shows(
                &mut t,
                "demo refusal",
                &["Type an instruction, or choose a report"],
            )?;
            if mouse {
                t.click("< The instruction below")?;
            } else {
                t.send("\t ")?;
            }
            t.screen("demo report chosen", |s| s.contains("< Report #1 ·"))?;
            press(&mut t, mouse, "[Apply]", "\r")?;
            shows(
                &mut t,
                "demo confirmation",
                &["Start agent fix · demo workspace?", "report #1"],
            )?;
            press(&mut t, mouse, "[Start fix]", "\r")?;
            shows(&mut t, "demo running", &["Agent fix #4 · Running"])?;
            shows(&mut t, "demo progress", &["The demo check command names"])?;
            shows(
                &mut t,
                "demo finished",
                &["Agent fix #4 · Done", "Verified: check demo-check"],
            )?;
            press(&mut t, mouse, "[Older]", "[")?;
            shows(&mut t, "demo older", &["Agent fix #3 · Done"])?;
            close(&mut t, mouse, "Current #1")?;
            press(&mut t, mouse, "0 Projects", "0")?;
            t.screen("demo projects", |s| {
                s.contains("The simulator has one mock workspace") && s.contains("[f Fix]")
            })?;
            press(&mut t, mouse, "f Fix", "f")?;
            shows(&mut t, "demo projects form", &["2 fix(es):"])?;
            press(&mut t, mouse, "[Cancel]", "\x1b")?;
            t.screen("closed", |s| !s.contains("[Cancel]"))?;
            t.send("q")?;
            t.finish()?;
            for kept in [
                "state/ktask-dev/ktask.sqlite3",
                "state/ktask-dev/investigations",
            ] {
                assert!(!sandbox.path().join(kept).exists(), "{kept}");
            }
        }
    }
    Ok(())
}
