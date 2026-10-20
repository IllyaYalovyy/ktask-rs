use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{scratch::Scratch, storage::Database};
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");
struct Fixture {
    scratch: Scratch,
    root: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let scratch = support::sandbox()?;
        let root = scratch.path().join("project");
        fs::create_dir(&root)?;
        fs::create_dir(scratch.path().join("bin"))?;
        fs::write(root.join("README.md"), "Initial project\n")?;
        let f = Self { scratch, root };
        f.git(&["init", "-q"])?;
        f.git(&["add", "README.md"])?;
        f.git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "baseline",
        ])?;
        f.provider("normal")?;
        f.ok(&["check-set", "build", "--command", "test -f result.txt"])?;
        for role in ["execution", "review", "validation", "remediation"] {
            f.ok(&[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ])?;
        }
        Ok(f)
    }
    fn git(&self, args: &[&str]) -> Result<String> {
        let o = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args(args)
            .output()?;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8(o.stdout)?)
    }
    fn process(&self) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(&self.root)
            .env_clear()
            .env("HOME", self.scratch.path())
            .env("TMPDIR", self.scratch.path())
            .env("XDG_STATE_HOME", self.scratch.path().join("state"))
            .env("XDG_CACHE_HOME", self.scratch.path().join("cache"))
            .env("XDG_CONFIG_HOME", self.scratch.path().join("config"))
            .env("XDG_RUNTIME_DIR", self.scratch.path().join("runtime"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin",
                    self.scratch.path().join("bin").display()
                ),
            );
        c
    }
    fn ok(&self, args: &[&str]) -> Result<Value> {
        let o = self.process().arg("--json").args(args).output()?;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        Ok(serde_json::from_slice::<Value>(&o.stdout)?["data"].clone())
    }
    fn add(&self, title: &str) -> Result {
        self.ok(&[
            "add",
            "--title",
            title,
            "--goal",
            "Create result.txt containing verified",
            "--criterion",
            "result.txt contains verified",
        ])?;
        Ok(())
    }
    fn db(&self) -> Result<Database> {
        Ok(Database::open_in(
            &self.scratch.path().join("state/ktask-dev"),
        )?)
    }
    fn state(&self, state: &str) -> Result {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let d = self.ok(&["show", "1"])?;
            if d["status"] == state {
                return Ok(());
            }
            if Instant::now() > until {
                return Err(format!("Expected {state}: {d}").into());
            }
            thread::sleep(Duration::from_millis(40));
        }
    }
    fn provider(&self, mode: &str) -> Result {
        let script = format!(
            r##"#!/bin/sh
model=unknown
session=""
while [ "$#" -gt 0 ]; do
 case "$1" in -m) shift; model="$1";; resume) shift; session="$1";; esac
 shift
done
if [ '{mode}' = check-launch-failure ]; then
 printf '%s %s\n' "$$" "$PPID" >> "$HOME/launch-fixture-pids"
fi
prompt=$(cat)
printf '%s' "$prompt" > "$HOME/prompt-$model"
case "$prompt" in *"Repair plan from the failure judge"*) printf '%s' "$prompt" > "$HOME/prompt-repair-worker";; esac
[ -n "$session" ] || session="session-$model-$$"
printf '{{"type":"thread.started","thread_id":"%s"}}\n' "$session"
printf '%s\n' "$model" >> "$HOME/provider-invocations"
mkdir -p "$TMPDIR/generated/deep"
printf debris > "$TMPDIR/generated/deep/file"
case "$prompt" in *"Interactive Remediate action"*)
 printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"insert\",\"tasks\":[{{\"title\":\"Manual repair prerequisite\",\"goal\":\"Finish the preserved result\",\"criteria\":[\"result.txt contains verified\"]}}],\"reason\":\"Preserve the partial implementation before the repair\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
esac
case "$prompt" in *"You are the failure judge"*)
 printf '%s' "$prompt" > "$HOME/prompt-judge"
 if [ '{mode}' = judge-error-once ] && [ ! -f "$HOME/judge-error" ]; then
  touch "$HOME/judge-error"
  printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0
 fi
 judge_mode='{mode}'
 case "$prompt" in *"Human answer"*) judge_mode=normal;; esac
 case "$judge_mode" in
  judge-insert) if [ ! -f "$HOME/judge-inserted" ]; then touch "$HOME/judge-inserted"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"insert\",\"tasks\":[{{\"title\":\"Repair prerequisite\",\"goal\":\"Finish the preserved result\",\"criteria\":[\"result.txt contains verified\"]}}],\"reason\":\"Preserve the partial implementation before the repair\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; else printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Verify the prerequisite and finish the original\",\"reason\":\"Prerequisite is complete\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; fi; exit 0;;
  judge-revise|judge-revise-always) if [ '{mode}' = judge-revise-always ] || [ ! -f "$HOME/judge-revised" ]; then touch "$HOME/judge-revised"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"revise\",\"title\":\"Clarified task\",\"goal\":\"Do the clarified thing.\",\"criteria\":[\"Clarified check passes\"],\"reason\":\"The goal contradicted the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; else printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Verify the revised task\",\"reason\":\"Revision is ready\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; fi; exit 0;;
  judge-ask) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"ask\",\"question\":\"Keep or drop the legacy export?\",\"reason\":\"Both readings satisfy the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Rerun the check after writing result.txt\",\"reason\":\"The check failed on a missing file\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
 esac;;
esac
case '{mode}' in
 check-launch-failure)
  if [ "$model" = execution ] && [ ! -f "$HOME/check-launch-armed" ]; then
   touch "$HOME/check-launch-armed"
   chmod 600 "$HOME/check-launch-ktask-dev"
  fi;;
 minor-required|validation-critical)
  if [ '{mode}' = validation-critical ] && [ "$model" = validation ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0; fi
  if [ '{mode}' = minor-required ]; then
   case "$prompt" in *"Final decision after"*) printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0;; esac
   if [ "$model" = review ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Mandatory proof is missing\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Required boundary behavior is not verified\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: boundary case needs correction\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{}}}}'; exit 0; fi
  fi;;
 review-critical|review-minor|pause-tenth|review-nine)
  case "$prompt" in *"Final decision after"*) printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Critical boundary case remains unhandled\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: missing boundary handling produces incorrect results; implement the boundary case\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0;; esac
  if [ "$model" = review ]; then
   count=0; [ ! -f "$HOME/review-count" ] || count=$(cat "$HOME/review-count")
   count=$((count + 1)); printf '%s' "$count" > "$HOME/review-count"
   if [ '{mode}' = pause-tenth ] && [ "$count" = 11 ]; then touch "$HOME/tenth-review-started"; sleep 30; fi
   if [ '{mode}' = review-nine ] && [ "$count" -gt 9 ]; then :
   elif [ '{mode}' = review-minor ]; then printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": true, \"evidence\": \"Optional naming polish\"}}], \"findings\": [{{\"severity\": \"minor\", \"description\": \"result.txt:1: optional naming polish\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0
   else printf '%s\n' '{{"type": "item.completed", "item": {{"type": "agent_message", "text": "{{\"outcome\": \"changes_requested\", \"summary\": \"Review findings to address\", \"question\": null, \"criteria\": [{{\"ordinal\": 1, \"passed\": false, \"evidence\": \"Critical boundary case remains unhandled\"}}], \"findings\": [{{\"severity\": \"critical\", \"description\": \"result.txt:1: missing boundary handling produces incorrect results; implement the boundary case\"}}]}}"}}}}' '{{"type": "turn.completed", "usage": {{"input_tokens": 100, "cached_input_tokens": 40, "output_tokens": 10}}}}'; exit 0
   fi
  fi;;
 large-evidence) case "$model" in
  execution|remediation)
   printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"large check transcript","aggregated_output":"START'
   head -c 1250000 /dev/zero | tr '\000' x
   printf 'END","exit_code":0}}}}\n';;
  review)
   printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"Review passed\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected rendered PTY screens and the supervisor check transcript\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  validation)
   case "$prompt" in *"Inspected rendered PTY screens and the supervisor check transcript"*) :;;
    *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"failed\",\"summary\":\"Missing independent inspection evidence\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"The review summary alone does not establish inspection\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
   esac;;
 esac;;
 review-once) if [ "$model" = review ] && [ ! -f "$HOME/review-failed" ]; then touch "$HOME/review-failed"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"failed\",\"summary\":\"result.txt:1: missing edge-case handling; repair and prove the boundary.\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"Boundary case fails\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0; fi;;
 transient) if [ ! -f "$HOME/transport-retried" ]; then touch "$HOME/transport-retried"; printf '{{"type":"turn.failed","error":{{"message":"connection reset"}}}}\n'; exit 2; fi;;
 slow-repair) if [ "$model" = remediation ]; then touch "$HOME/repair-started"; sleep 30; fi;;
 slow-review) if [ "$model" = review ]; then touch "$HOME/review-started"; sleep 30; fi;;
 slow) sleep 3;;
 descendants) setsid sleep 30 & printf '%s' "$!" > descendant.pid; sleep 30;;
 rate) printf '{{"type":"turn.failed","error":{{"message":"rate_limit reached"}}}}\n'; exit 2;;
 malformed-work) printf 'verified\n' > result.txt; printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0;;
 malformed) printf '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}\n'; exit 0;;
 fail) printf '{{"type":"turn.failed","error":{{"message":"transport lost"}}}}\n'; exit 2;;
 question) case "$prompt" in *"Human answer"*) :;; *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
 ask-once) case "$prompt" in *"You returned blocked"*) :;; *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
 review-asks) case "$model" in review) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
esac
case "$model" in execution|remediation) printf 'verified\n' > result.txt;; esac
printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"complete\",\"summary\":\"Verified result\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected result.txt\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'
"##
        );
        let path = self.scratch.path().join("bin/codex");
        fs::write(&path, script)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
        Ok(())
    }
    fn fail_provider(&self) -> Result {
        self.provider("malformed-work")?;
        let output = self.process().args(["run", "--one"]).output()?;
        assert_eq!(output.status.code(), Some(4));
        self.provider("normal")?;
        Ok(())
    }
    /// Revoke only this sandbox's copied executable after implementation. The running
    /// supervisor survives, but its next guardian spawn gets a real EACCES from the OS.
    fn fail_check_launch(&self) -> Result<PathBuf> {
        let binary = self.scratch.path().join("check-launch-ktask-dev");
        fs::copy(BIN, &binary)?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        self.provider("check-launch-failure")?;
        self.add("Recover check guardian launch")?;
        self.ok(&["edit", "1", "--attempt-limit", "1"])?;
        self.add("Must wait for verified recovery")?;
        Ok(binary)
    }
    fn launch_workers_reaped(&self) -> Result {
        for pid in
            fs::read_to_string(self.scratch.path().join("launch-fixture-pids"))?.split_whitespace()
        {
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "fixture provider/guardian {pid} survived cleanup"
            );
        }
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.root.join(".git/ktask-worker.lock"))?;
        lease.try_lock()?;
        assert_eq!(lease.metadata()?.len(), 0, "uncleared guardian marker");
        println!(
            "CHECK LAUNCH CLEANUP: provider/guardian PIDs absent; worker lease free and marker empty"
        );
        self.no_scratch()
    }
    fn supervisor_stopped(&self) -> Result {
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.root.join(".git/ktask-runner.lock"))?;
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            match lease.try_lock() {
                Ok(()) => {
                    println!("CHECK LAUNCH CLEANUP: supervisor lease released");
                    return Ok(());
                }
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < until => {
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }
    /// A project check that fails exactly once, so the next attempt goes through the judge.
    fn fail_first_check(&self) -> Result {
        let marker = self.scratch.path().join("check-ran");
        let command = format!(
            "test -f '{0}' || {{ : > '{0}'; echo 'first check fails'; exit 7; }}",
            marker.display()
        );
        self.ok(&["check-set", "build", "--command", &command])?;
        Ok(())
    }
    fn no_scratch(&self) -> Result {
        let root = self.scratch.path().join("cache/ktask-dev/workers");
        if root.exists() {
            assert_eq!(
                fs::read_dir(root)?.count(),
                1,
                "Only .sweep.lock may remain"
            );
        }
        Ok(())
    }
}
struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn review_cycles_accept_only_minor_findings_after_ten_repairs_in_real_tui() -> Result {
    for (mouse, cols, rows) in [(false, 70, 22), (true, 120, 30)] {
        for critical in [false, true] {
            let f = Fixture::new()?;
            f.provider(if critical {
                "review-critical"
            } else {
                "review-minor"
            })?;
            f.add("Normal review and fix cycle")?;
            f.add("Next task")?;
            f.ok(&["edit", "1", "--attempt-limit", "1"])?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            println!(
                "REVIEW LOOP READY mouse={mouse} critical={critical}\n{}",
                t.screen("ready", |s| s.contains("r Run queue"))?
            );
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            f.state(if critical { "failed" } else { "succeeded" })?;
            println!(
                "REVIEW LOOP DECISION mouse={mouse} critical={critical}\n{}",
                t.screen("decision", |s| {
                    if critical {
                        s.contains("Critical findings remain")
                    } else {
                        s.contains("Queue complete.")
                    }
                })?
            );
            let task = f.ok(&["show", "1"])?;
            assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
            assert_eq!(task["attempts_used"], 1);
            let db = f.db()?;
            let round: i64 = db.connection().query_row(
                "SELECT repair_round FROM attempt_progress WHERE attempt_id=1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(round, 10);
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempts WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                if critical { 1 } else { 0 }
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM model_calls WHERE state='failed'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                0
            );
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='remediation'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                20
            );
            assert!(
                task["attempts"][0]["steps"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|s| s["role"] == "review" && s["state"] == "changes_requested")
            );
            if critical {
                assert!(
                    task["attempts"][0]["outcome"]
                        .as_str()
                        .unwrap()
                        .contains("10 repair rounds")
                );
                assert!(
                    f.ok(&["show", "2"])?["attempts"]
                        .as_array()
                        .unwrap()
                        .is_empty()
                );
            } else {
                assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
                let prompt = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
                // The second task also receives the retained advisory findings.
                assert!(prompt.contains("minor") && prompt.contains("optional naming polish"));
            }
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn pause_at_tenth_review_resumes_the_phase_without_an_eleventh_repair() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("pause-tenth")?;
        f.add("Preserve tenth round across resume")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        let until = Instant::now() + Duration::from_secs(20);
        while !f.scratch.path().join("tenth-review-started").exists() {
            assert!(Instant::now() < until, "tenth review never started");
            thread::sleep(Duration::from_millis(20));
        }
        println!(
            "TENTH REVIEW ACTIVE mouse={mouse}\n{}",
            t.screen("tenth review", |s| s
                .contains("Workflow · Review · repair round 10/10"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "TENTH REVIEW PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| s.contains("PAUSED") && s.contains("r Resume"))?
        );
        f.provider("review-critical")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        f.state("failed")?;
        println!(
            "TENTH REVIEW RESUMED mouse={mouse}\n{}",
            t.screen("critical decision", |s| s
                .contains("Critical findings remain"))?
        );
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT count(*) FROM attempts", [], |r| r.get::<_, i64>(0))?,
            1
        );
        assert_eq!(
            db.connection().query_row(
                "SELECT count(*) FROM attempt_steps WHERE role='execution'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(
            db.connection().query_row(
                "SELECT count(*) FROM attempt_steps WHERE role='remediation'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            20
        );
        assert_eq!(
            db.connection()
                .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                    .get::<_, i64>(0))?,
            10
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn implementation_model_cannot_review_repair_or_validate_itself() -> Result {
    for role in ["review", "remediation", "validation"] {
        let f = Fixture::new()?;
        f.add("Require stronger judge model")?;
        f.ok(&["edit", "1", "--model", &format!("{role}=codex/execution")])?;
        let output = f.process().args(["run", "--json"]).output()?;
        assert_eq!(output.status.code(), Some(4));
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains(&format!("stronger {role} model distinct"))
        );
        assert!(!f.scratch.path().join("provider-invocations").exists());
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        for mouse in [false, true] {
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
            t.screen("ready", |s| s.contains("r Run queue"))?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            println!(
                "MODEL SEPARATION {role} mouse={mouse}\n{}",
                t.screen("model configuration required", |s| s
                    .contains("Configure a stronger"))?
            );
            assert!(!f.scratch.path().join("provider-invocations").exists());
            t.send("q")?;
            t.finish()?;
        }
    }
    Ok(())
}

#[test]
fn mandatory_criteria_and_validation_findings_still_block_after_ten_rounds() -> Result {
    for (mode, mouse) in [("minor-required", false), ("validation-critical", true)] {
        let f = Fixture::new()?;
        f.provider(mode)?;
        f.add("Mandatory evidence cannot be waived")?;
        assert_eq!(
            f.process().args(["run", "--one"]).output()?.status.code(),
            Some(4)
        );
        f.state("failed")?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        let outcome = task["attempts"][0]["outcome"].as_str().unwrap();
        assert!(outcome.contains("Critical findings remain after 10 repair rounds"));
        assert!(outcome.contains("Required boundary behavior is not verified"));
        assert!(!outcome.contains("\"assessment\""));
        let db = f.db()?;
        assert_eq!(
            db.connection()
                .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                    .get::<_, i64>(0))?,
            10
        );
        if mode == "validation-critical" {
            assert_eq!(
                db.connection().query_row(
                    "SELECT count(*) FROM attempt_steps WHERE role='review' AND state='succeeded'",
                    [],
                    |r| r.get::<_, i64>(0)
                )?,
                11
            );
        }
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        println!(
            "MANDATORY DECISION {mode}\n{}",
            t.screen("failure", |s| s.contains("FAILED"))?
        );
        if mouse {
            t.click("Enter Details")?;
        } else {
            t.send("\r")?;
        }
        println!(
            "MANDATORY DECISION DETAILS {mode}\n{}",
            t.screen("full decision", |s| s
                .contains("Required boundary behavior")
                || s.contains("Critical findings remain"))?
        );
        t.send("\x1b")?;
        t.screen("details closed", |s| s.contains("FAILED"))?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn changing_source_while_review_is_paused_invalidates_saved_proof() -> Result {
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "test \"$(cat result.txt)\" = verified",
    ])?;
    f.add("Recheck source edited while paused")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.screen("ready", |s| s.contains("r Run queue"))?;
    t.send("r")?;
    let until = Instant::now() + Duration::from_secs(10);
    while !f.scratch.path().join("review-started").exists() {
        assert!(Instant::now() < until, "review never started");
        thread::sleep(Duration::from_millis(20));
    }
    println!(
        "PROVEN SOURCE BEFORE PAUSE\n{}",
        t.screen("review", |s| s.contains("Workflow · Review"))?
    );
    assert!(
        f.git(&["diff", "--cached", "--name-only"])?.is_empty(),
        "fingerprinting must preserve the user's index"
    );
    t.click("p Pause")?;
    t.screen("paused", |s| s.contains("PAUSED") && s.contains("r Resume"))?;
    fs::write(f.root.join("result.txt"), "unverified change\n")?;
    fs::write(f.root.join(".gitignore"), "included.secret\n")?;
    fs::write(f.root.join("included.secret"), "deliberately staged\n")?;
    f.git(&["add", "-f", "included.secret"])?;
    f.provider("normal")?;
    t.send("r")?;
    f.state("succeeded")?;
    println!(
        "CHANGED SOURCE RECHECKED\n{}",
        t.screen("verified after repair", |s| s.contains("Queue complete."))?
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert!(
        task["attempts"][0]["logs"]
            .to_string()
            .contains("Worktree changed since verification")
    );
    let db = f.db()?;
    let checks = db
        .connection()
        .prepare("SELECT state FROM attempt_steps WHERE role='check' ORDER BY id")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(checks, ["succeeded", "failed", "succeeded"]);
    assert_eq!(
        db.connection()
            .query_row("SELECT repair_round FROM attempt_progress", [], |r| r
                .get::<_, i64>(0))?,
        1
    );
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert!(f.git(&["status", "--porcelain"])?.is_empty());
    assert_eq!(
        f.git(&["show", "HEAD:included.secret"])?,
        "deliberately staged\n"
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_repair_judge_is_repeated_before_a_retry_worker_runs() -> Result {
    let f = Fixture::new()?;
    f.provider("judge-error-once")?;
    f.fail_first_check()?;
    f.add("Retry the failed judge before repairing")?;
    let output = f.process().args(["run", "--one"]).output()?;
    assert_eq!(output.status.code(), Some(0));
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT phase FROM attempt_progress ORDER BY attempt_id LIMIT 1",
            [],
            |r| r.get::<_, String>(0)
        )?,
        "complete"
    );
    f.state("succeeded")?;
    let roles = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert_eq!(
        roles.lines().collect::<Vec<_>>(),
        [
            "execution",
            "remediation",
            "remediation",
            "remediation",
            "review",
            "validation"
        ]
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress ORDER BY attempt_id DESC LIMIT 1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn automatic_revisions_cannot_reset_the_ten_round_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("judge-revise-always")?;
    f.add("Repeated revisions still have one repair budget")?;
    f.ok(&["check-set", "build", "--command", "exit 7"])?;
    assert_eq!(f.process().args(["run"]).output()?.status.code(), Some(4));
    f.state("failed")?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["selected_revision"], 11);
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"].as_array().unwrap().len(), 11);
    let db = f.db()?;
    assert_eq!(
        db.connection()
            .query_row("SELECT automatic_replans FROM tasks WHERE id=1", [], |r| {
                r.get::<_, i64>(0)
            })?,
        10
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT max(repair_round) FROM attempt_progress", [], |r| {
                r.get::<_, i64>(0)
            })?,
        1
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role='execution'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role='remediation'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        11
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempts WHERE state='failed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn check_evidence_preserves_unicode_split_across_pipe_reads() -> Result {
    for exit_code in [0, 7] {
        let f = Fixture::new()?;
        let command = format!(
            r"printf '\342'; printf '\360' >&2; sleep 0.1; printf '\234\223\n'; printf '\237\231\202\n' >&2; exit {exit_code}"
        );
        f.ok(&["check-set", "build", "--command", &command])?;
        f.add("Unicode evidence")?;
        let result = f.process().args(["run", "--json"]).output()?;
        assert_eq!(
            result.status.code(),
            Some(if exit_code == 0 { 0 } else { 4 })
        );
        let db = f.db()?;
        let artifacts = db
            .connection()
            .prepare("SELECT relative_path,size_bytes FROM artifacts ORDER BY attempt_id")?
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        assert_eq!(artifacts.len(), if exit_code == 0 { 1 } else { 11 });
        for (relative, size) in artifacts {
            let text =
                fs::read_to_string(f.scratch.path().join("state/ktask-dev").join(&relative))?;
            assert!(matches!(text.as_str(), "✓\n🙂\n" | "🙂\n✓\n"), "{text:?}");
            assert_eq!(size as usize, text.len());
            println!("UTF-8 CHECK EVIDENCE (exit={exit_code}, {relative})\n{text}");
        }
        let task = f.ok(&["show", "1"])?;
        for attempt in task["attempts"].as_array().unwrap() {
            let logs = attempt["logs"].to_string();
            assert!(logs.contains('✓') && logs.contains('🙂'));
            assert!(!logs.contains('\u{fffd}'));
        }
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn reviewers_receive_current_proof_and_authorized_repair_context() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "if [ ! -f \"$HOME/checked-once\" ]; then touch \"$HOME/checked-once\"; printf 'previous failed check\\n'; exit 7; fi; printf 'real check output\\n'; printf 'stderr evidence\\n' >&2; test -f result.txt",
    ])?;
    f.add("Evidence handoff")?;
    f.ok(&["run"])?;
    let db = f.db()?;
    let (relative, size): (String, i64) = db.connection().query_row(
        "SELECT relative_path,size_bytes FROM artifacts WHERE attempt_id=1 ORDER BY id DESC LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let artifact = f.scratch.path().join("state/ktask-dev").join(relative);
    let text = fs::read_to_string(&artifact)?;
    assert!(text.contains("real check output\n"));
    assert!(text.contains("stderr evidence\n"));
    assert!(!text.contains("execution:"));
    assert!(!text.contains("previous failed check"));
    assert_eq!(size as usize, text.len());
    let failed_relative: String = db.connection().query_row(
        "SELECT relative_path FROM artifacts WHERE attempt_id=1 ORDER BY id LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    let failed_artifact = f
        .scratch
        .path()
        .join("state/ktask-dev")
        .join(failed_relative);
    assert_eq!(
        fs::read_to_string(&failed_artifact)?,
        "previous failed check\n"
    );
    let remediation = fs::read_to_string(f.scratch.path().join("prompt-remediation"))?;
    let writable_checks = "Run the project's checks and test suite yourself";
    assert!(remediation.contains(writable_checks));
    assert!(remediation.contains(failed_artifact.to_str().unwrap()));
    assert!(remediation.contains("Check exited 7"));
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
        assert!(prompt.contains(artifact.to_str().unwrap()));
        assert!(prompt.contains("\"exit_code\":0"));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("must not ask to"));
        assert!(!prompt.contains(writable_checks));
        assert!(!prompt.contains("Previous attempt:"));
        assert!(prompt.contains("scope only; not proof of current success"));
        assert!(prompt.contains("Repair plan from the failure judge"));
        assert!(prompt.contains(failed_artifact.to_str().unwrap()));
        assert!(prompt.contains("Check exited 7"));
    }
    let execution = fs::read_to_string(f.scratch.path().join("prompt-execution"))?;
    assert!(execution.contains(writable_checks));
    assert!(execution.contains("temporary files only under the supplied TMPDIR"));
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failed_check_transcript_is_retained_for_remediation() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        "printf 'concrete failure evidence\\n'; exit 1",
    ])?;
    f.add("Preserve failure evidence")?;
    let result = f.process().args(["run", "--json"]).output()?;
    assert_eq!(result.status.code(), Some(4));
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-remediation"))?;
    let db = f.db()?;
    let relative: String = db.connection().query_row(
        "SELECT relative_path FROM artifacts ORDER BY id DESC LIMIT 1 OFFSET 1",
        [],
        |r| r.get(0),
    )?;
    let artifact = f.scratch.path().join("state/ktask-dev").join(relative);
    assert!(prompt.contains(artifact.to_str().unwrap()));
    assert_eq!(fs::read_to_string(artifact)?, "concrete failure evidence\n");
    assert!(!f.scratch.path().join("prompt-review").exists());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn actual_runner_streams_checks_validates_checkpoints_and_advances() -> Result {
    let f = Fixture::new()?;
    f.add("First")?;
    f.add("Second")?;
    let result = f.ok(&["run"])?;
    println!("RUNNER {result}");
    assert!(f.ok(&["list"])?["tasks"].as_array().unwrap().is_empty());
    assert_eq!(
        f.ok(&["list", "--scope", "history"])?["tasks"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let db = f.db()?;
    let c = db.connection();
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attempt_steps WHERE state='succeeded'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        8
    );
    assert_eq!(
        c.query_row("SELECT sum(input_tokens) FROM model_calls", [], |r| r
            .get::<_, i64>(0))?,
        360
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM criterion_results WHERE state='passed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        6
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM attempts WHERE head_commit IS NOT NULL",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    assert!(f.git(&["status", "--porcelain"])?.is_empty());
    f.no_scratch()?;
    Ok(())
}
#[test]
fn check_failure_is_real_and_remediation_stays_with_current() -> Result {
    let f = Fixture::new()?;
    f.ok(&["check-set", "build", "--command", "exit 7"])?;
    f.add("Failure")?;
    f.add("Must not start")?;
    let failed = f.process().args(["run", "--queue"]).output()?;
    assert_eq!(failed.status.code(), Some(4));
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role='remediation'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        // Ten repair rounds, each with a read-only judge and one worker.
        20
    );
    let worker_prompt = fs::read_to_string(f.scratch.path().join("prompt-repair-worker"))?;
    assert!(
        worker_prompt.contains("Repair plan from the failure judge"),
        "{worker_prompt}"
    );
    assert!(f.root.join("result.txt").exists());
    f.no_scratch()?;
    Ok(())
}
#[test]
fn failure_judge_can_rewrite_the_task_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.fail_first_check()?;
    f.add("Contradictory task")?;
    f.provider("judge-revise")?;
    let output = f.process().args(["run"]).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(task["title"], "Clarified task");
    assert_eq!(task["selected_revision"], 2);
    let states: Vec<_> = task["attempts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["state"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(states, ["interrupted", "succeeded"]);
    assert_eq!(task["attempts_used"], 1);
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress ORDER BY attempt_id DESC LIMIT 1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    let (actor_kind, actor, reason): (String, String, String) = db.connection().query_row(
        "SELECT created_by_kind, created_by, change_reason FROM task_revisions WHERE task_id=1 AND revision=2",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;
    assert_eq!((actor_kind.as_str(), actor.as_str()), ("ai", "judge"));
    assert_eq!(reason, "The goal contradicted the criteria");
    f.no_scratch()?;
    Ok(())
}
#[test]
fn judge_evidence_stays_within_the_provider_input_limit() -> Result {
    let f = Fixture::new()?;
    f.provider("judge")?;
    let marker = f.scratch.path().join("check-ran");
    let command = format!(
        "test -f '{0}' || {{ : > '{0}'; echo 'first check fails'; exit 7; }}; test -f '{0}-second' || {{ : > '{0}-second'; head -c 1500000 /dev/zero | tr '\\0' x; echo; echo 'second check fails'; exit 7; }}",
        marker.display()
    );
    f.ok(&["check-set", "build", "--command", &command])?;
    f.add("Noisy failure")?;
    f.ok(&["run"])?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
    assert!(prompt.contains("You are the failure judge"));
    assert!(
        prompt.contains("second check fails"),
        "the failure itself survives trimming"
    );
    assert!(prompt.contains("Authorized repair plan:\nRerun the check after writing result.txt"));
    assert!(prompt.contains("Recorded judgment output:\n{\"judgment\":"));
    assert!(
        prompt.chars().count() < 1_048_576,
        "Codex rejects longer turns; prompt was {} chars",
        prompt.chars().count()
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn failure_judge_asks_the_owner_only_as_a_product_question() -> Result {
    let f = Fixture::new()?;
    f.fail_first_check()?;
    f.add("Ambiguous task")?;
    f.fail_provider()?;
    f.provider("judge-ask")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let task = f.ok(&["show", "1"])?;
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1]["question"], "Keep or drop the legacy export?");
    assert_eq!(task["attempts_used"], 0);
    f.ok(&["answer", "1", "--text", "Human answer: keep it"])?;
    f.state("succeeded")?;
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        2
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn pause_resume_keeps_attempt_session_and_cleans_worker_tmp() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Pause me")?;
    let mut run = Running(f.process().args(["run"]).stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    thread::sleep(Duration::from_millis(200));
    f.ok(&["pause", "1"])?;
    run.0.wait()?;
    f.state("paused")?;
    let recover = f.process().arg("recover").output()?;
    assert_eq!(recover.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&recover.stderr).contains("yielded this attempt normally"));
    f.no_scratch()?;
    f.provider("normal")?;
    f.ok(&["resume"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(task["lifecycle"], "succeeded");
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE invocation='resume'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn paused_repair_resumes_its_reserved_round_and_same_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("slow-repair")?;
        f.fail_first_check()?;
        f.add("Pause and resume repair")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while !f.scratch.path().join("repair-started").exists() {
            assert!(Instant::now() < deadline, "repair worker never started");
            thread::sleep(Duration::from_millis(10));
        }
        t.screen("repair running", |s| {
            s.contains("p Pause") && s.contains("Attempt 1") && s.contains("Remediation … running")
        })?;
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "REPAIR PAUSED mouse={mouse}\n{}",
            t.screen("repair paused", |s| s.contains("[r Resume]")
                && s.contains("PAUSED"))?
        );
        let before = f.ok(&["show", "1"])?;
        let attempt = before["attempts"][0]["id"].clone();
        f.provider("normal")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "REPAIR RESUMED mouse={mouse}\n{}",
            t.screen("repair complete", |s| s.contains("Queue complete."))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], attempt);
        assert_eq!(after["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nremediation\nremediation\nremediation\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn questions_require_answer_and_reuse_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let recover = f.process().arg("recover").output()?;
    assert_eq!(recover.status.code(), Some(4));
    assert!(String::from_utf8_lossy(&recover.stderr).contains("yielded this attempt normally"));
    let failed = f.process().arg("resume").output()?;
    assert!(!failed.status.success());
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    f.ok(&["resume"])?;
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn permission_style_questions_are_nudged_away_without_asking_the_owner() -> Result {
    let f = Fixture::new()?;
    f.provider("ask-once")?;
    f.add("Just do it")?;
    f.ok(&["run"])?;
    f.state("succeeded")?;
    // Every role asked once and was nudged once; nothing reached the owner.
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nexecution\nreview\nreview\nvalidation\nvalidation\n"
    );
    let nudge = fs::read_to_string(f.scratch.path().join("prompt-review"))?;
    assert!(nudge.contains("read-only by design"), "{nudge}");
    let db = f.db()?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE invocation='nudge' AND role='execution'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn reviewer_that_keeps_asking_stops_the_task_without_a_question_or_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("review-asks")?;
    f.add("Review me")?;
    let output = f.process().arg("run").output()?;
    assert!(!output.status.success());
    f.state("failed")?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["attempts_used"], 0);
    let outcome = shown["attempts"][0]["outcome"].as_str().unwrap_or_default();
    assert!(
        outcome.contains("review asked instead of deciding"),
        "{shown}"
    );
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nreview\nreview\nreview\nreview\nreview\nreview\n"
    );
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn waiting_attempt_can_be_dropped_then_the_task_abandoned_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    for args in [
        vec!["abandon", "1", "--reason", "x", "--attempt"],
        vec!["abandon", "1", "--reason", "x"],
    ] {
        let refused = f.process().args(&args).output()?;
        assert_eq!(
            refused.status.code(),
            Some(4),
            "pending work is deleted, not abandoned"
        );
    }
    f.ok(&["run"])?;
    f.state("waiting")?;
    let dropped = f.ok(&[
        "abandon",
        "1",
        "--reason",
        "Owner stopped this attempt",
        "--attempt",
    ])?;
    assert_eq!(dropped["scope"], "attempt");
    f.state("interrupted")?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["attempts_used"], 0);
    let attempt = &shown["attempts"].as_array().unwrap()[0];
    assert_eq!(attempt["state"], "interrupted");
    assert!(
        attempt["outcome"]
            .as_str()
            .unwrap()
            .contains("Owner stopped")
    );
    let abandoned = f.ok(&["abandon", "1", "--reason", "Not worth another attempt"])?;
    assert_eq!(abandoned["scope"], "task");
    let history = f.ok(&["list", "--scope", "history"])?;
    assert_eq!(history["count"], 1);
    assert_eq!(history["tasks"][0]["status"], "cancelled");
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["resolution_reason"], "Not worth another attempt");
    assert_eq!(shown["attempts"].as_array().unwrap().len(), 1);
    f.no_scratch()?;
    Ok(())
}
#[test]
fn waiting_task_is_abandoned_whole_in_one_command() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    f.ok(&["abandon", "1", "--reason", "Wrong direction"])?;
    let shown = f.ok(&["show", "1"])?;
    assert_eq!(shown["lifecycle"], "cancelled");
    assert_eq!(shown["attempts_used"], 0);
    assert_eq!(shown["attempts"][0]["state"], "interrupted");
    assert_eq!(f.ok(&["list"])?["count"], 0);
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_shows_the_open_question_and_abandons_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Ask owner")?;
        f.ok(&["run"])?;
        f.state("waiting")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        // The question itself is on the screen, before any dialog opens.
        let waiting = t.screen("question panel", |s| {
            s.contains("Question from the worker") && s.contains("Which content?")
        })?;
        println!("QUESTION SCREEN (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("The worker needs your answer"));
        assert!(waiting.contains("Attempt 1"));
        assert!(waiting.contains("Execution   ! waiting"));
        assert!(waiting.contains("Activity · #1"));
        assert!(!waiting.contains("Execution controls always target current"));
        assert!(waiting.contains("[w Answer]") && waiting.contains("[x Abandon]"));
        if mouse {
            t.click("Which content?")?;
            t.screen("click opens answer", |s| s.contains("Answer AI question"))?;
            t.send("\x1b")?;
            t.screen("closed", |s| !s.contains("Answer AI question"))?;
            t.click("x Abandon")?;
        } else {
            t.send("x")?;
        }
        let form = t.screen("abandon form", |s| s.contains("Abandon #1 · Ask owner"))?;
        println!("ABANDON FORM (mouse={mouse})\n{form}");
        assert!(form.contains("Whole task") && form.contains("History as cancelled"));
        t.send("Not worth answering")?;
        t.screen("reason typed", |s| s.contains("Not worth answering"))?;
        if mouse {
            // Default scope: the whole task leaves the queue in one step.
            t.click("Apply")?;
            let empty = t.screen("queue empty", |s| s.contains("No current task"))?;
            println!("AFTER ABANDON (mouse={mouse})\n{empty}");
            t.send("3")?;
            let history = t.screen("history", |s| s.contains("CANCELLED"))?;
            assert!(history.contains("Ask owner"));
        } else {
            // Tab to Scope, Space switches it to this attempt only.
            t.send("\t ")?;
            t.screen("attempt scope", |s| s.contains("This attempt"))?;
            t.send("\r")?;
            let interrupted = t.screen("interrupted", |s| {
                s.contains("Current #1 · INTERRUPTED") && s.contains("Report · attempt 1")
            })?;
            println!("ATTEMPT DROPPED (mouse={mouse})\n{interrupted}");
            assert!(interrupted.contains("Abandoned: Not worth answering"));
            assert!(interrupted.contains("[t Retry]") && interrupted.contains("[x Abandon]"));
            assert!(interrupted.contains("Failed · t Retry · d Remediate · x Abandon"));
            assert!(!interrupted.contains("Question from the worker"));
            t.send("o")?;
            t.screen("raw log", |s| {
                s.contains("Log · following · attempt 1 · o: report")
            })?;
            // Queue's Delete on a failed task is the same abandon.
            t.send("2")?;
            t.screen("queue", |s| s.contains("[d Delete]"))?;
            t.send("d")?;
            let form = t.screen("task form", |s| s.contains("Abandon #1 · Ask owner"))?;
            assert!(
                !form.contains("Whole task"),
                "a failed task has no attempt to keep"
            );
            t.send("Done by hand\r")?;
            t.screen("queue empty", |s| s.contains("No tasks here."))?;
        }
        t.send("q")?;
        t.finish()?;
        let shown = f.ok(&["show", "1"])?;
        assert_eq!(shown["attempts_used"], 0);
        assert_eq!(shown["attempts"][0]["state"], "interrupted");
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn supervisor_death_stops_worker_and_recovery_is_explicit() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Crash")?;
    let mut run = Running(f.process().args(["run"]).stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    thread::sleep(Duration::from_millis(200));
    run.0.kill()?;
    run.0.wait()?;
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let out = f.process().arg("recover").output()?;
        if out.status.success() {
            break;
        }
        if Instant::now() > until {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned().into());
        }
        thread::sleep(Duration::from_millis(40));
    }
    f.state("interrupted")?;
    f.no_scratch()?;
    assert!(!f.root.join("result.txt").exists());
    Ok(())
}
#[test]
fn real_tui_shows_liveness_and_recovers_with_both_inputs_and_cleanup_guard() -> Result {
    for (mouse, unconfirmed_worker) in [(true, false), (false, true)] {
        let f = Fixture::new()?;
        f.provider("slow")?;
        f.add("Recover orphaned attempt")?;
        let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
        f.state("running")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
        println!(
            "LIVE\n{}",
            t.screen("live supervisor", |s| {
                s.contains("RUNNING · supervisor alive")
            })?
        );
        run.0.kill()?;
        run.0.wait()?;
        let deadline = Instant::now() + Duration::from_secs(5);
        let runner_lock = f.root.join(".git/ktask-runner.lock");
        loop {
            let lock = fs::File::open(&runner_lock)?;
            if lock.try_lock().is_ok()
                && fs::metadata(f.root.join(".git/ktask-worker.lock"))?.len() == 0
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "guardian did not release its lease"
            );
            thread::sleep(Duration::from_millis(40));
        }
        println!(
            "ABANDONED\n{}",
            t.screen("abandoned current", |s| {
                s.contains("ABANDONED · supervisor absent") && s.contains("R Recover")
            })?
        );
        for (key, name) in [
            ("2", "Queue"),
            ("3", "History"),
            ("4", "Config"),
            ("5", "About"),
        ] {
            t.send(key)?;
            let screen = t.screen(name, |s| {
                s.contains("ABANDONED · supervisor absent") && s.contains(name)
            })?;
            println!("{name}\n{screen}");
        }
        t.send("1")?;
        t.screen("current again", |s| s.contains("R Recover"))?;
        if mouse {
            t.click("R Recover")?;
        } else {
            t.send("R")?;
        }
        println!(
            "RECOVER DIALOG\n{}",
            t.screen("recover dialog", |s| s
                .contains("Recover abandoned attempt")
                && s.contains("Worker cleanup"))?
        );

        // The lease check is repeated at Apply, even if the dialog opened while free.
        let live_guardian = fs::File::open(&runner_lock)?;
        live_guardian.lock()?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        println!(
            "LIVE GUARD\n{}",
            t.screen("live guardian refused", |s| s
                .contains("already owned by a runner/worker"))?
        );
        drop(live_guardian);

        let before = f.ok(&["show", "1"])?;
        let head = f.git(&["rev-parse", "HEAD"])?;
        let files = f.git(&["status", "--porcelain"])?;
        if unconfirmed_worker {
            fs::write(
                f.root.join(".git/ktask-worker.lock"),
                "guardian interrupted\n",
            )?;
            t.send("\r")?;
            println!(
                "CLEANUP REQUIRED\n{}",
                t.screen("cleanup required", |s| s.contains("cleanup confirmation"))?
            );
            assert_eq!(f.ok(&["show", "1"])?["status"], "running");
            t.send(" ")?;
            t.send("\r")?;
        } else {
            t.click("Apply")?;
        }
        println!(
            "RECOVERED\n{}",
            t.screen("recovered", |s| s.contains("INTERRUPTED")
                && s.contains("Recover orphaned attempt"))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["status"], "interrupted");
        assert_eq!(before["attempts"][0]["logs"], after["attempts"][0]["logs"]);
        assert_eq!(
            before["attempts"][0]["steps"],
            after["attempts"][0]["steps"]
        );
        assert_eq!(
            before["attempts"][0]["report"],
            after["attempts"][0]["report"]
        );
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
        assert_eq!(f.git(&["status", "--porcelain"])?, files);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn real_tui_recovers_lost_paused_and_waiting_supervisors() -> Result {
    for (state, mouse) in [("paused", false), ("waiting", true)] {
        let f = Fixture::new()?;
        f.provider(if state == "paused" { "slow" } else { "rate" })?;
        f.add("Recover stopped supervisor")?;
        let mut run = Running(
            f.process()
                .args(if state == "paused" {
                    vec!["run"]
                } else {
                    vec!["run", "--queue"]
                })
                .stdout(Stdio::null())
                .spawn()?,
        );
        if state == "paused" {
            f.state("running")?;
            f.ok(&["pause", "1"])?;
            run.0.wait()?;
            f.state("paused")?;
            // Reproduce a crash after the state commit and before the handoff event.
            f.db()?.connection().execute(
                "DELETE FROM events WHERE kind='supervisor.released' AND attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
                [],
            )?;
        } else {
            f.state("waiting")?;
            let mut live = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
            println!(
                "LIVE WAIT\n{}",
                live.screen("waiting supervisor alive", |s| {
                    s.contains("WAITING · supervisor alive")
                })?
            );
            live.send("q")?;
            live.finish()?;
            run.0.kill()?;
            run.0.wait()?;
        }
        let runner_lock = f.root.join(".git/ktask-runner.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if fs::File::open(&runner_lock)?.try_lock().is_ok() {
                break;
            }
            assert!(Instant::now() < deadline, "supervisor kept its lease");
            thread::sleep(Duration::from_millis(40));
        }
        let before = f.ok(&["show", "1"])?;
        let head = f.git(&["rev-parse", "HEAD"])?;
        let files = f.git(&["status", "--porcelain"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
        println!(
            "ABANDONED {state}\n{}",
            t.screen("abandoned attempt", |s| {
                s.contains("ABANDONED · supervisor absent") && s.contains("[R Recover]")
            })?
        );
        for (key, screen_name) in [
            ("2", "Queue"),
            ("3", "History"),
            ("4", "Config"),
            ("5", "About"),
        ] {
            t.send(key)?;
            println!(
                "{state} {screen_name}\n{}",
                t.screen(screen_name, |s| s.contains("ABANDONED · supervisor absent"))?
            );
        }
        t.send("1")?;
        t.screen("current recovery control", |s| s.contains("[R Recover]"))?;
        if mouse {
            t.click("R Recover")?;
            t.screen("recover dialog", |s| {
                s.contains("Recover abandoned attempt")
            })?;
            t.click("Apply")?;
        } else {
            t.send("R")?;
            t.send("\r")?;
        }
        println!(
            "RECOVERED {state}\n{}",
            t.screen("recovered attempt", |s| s.contains("INTERRUPTED"))?
        );
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["status"], "interrupted");
        for field in ["logs", "steps", "report"] {
            assert_eq!(before["attempts"][0][field], after["attempts"][0][field]);
        }
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
        assert_eq!(f.git(&["status", "--porcelain"])?, files);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn real_tui_recovers_lost_question_and_rejects_stale_dialog() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Recover unanswered question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let before = f.ok(&["show", "1"])?;
    f.db()?.connection().execute(
        "DELETE FROM events WHERE kind='supervisor.released' AND attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
        [],
    )?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    println!(
        "ABANDONED QUESTION\n{}",
        t.screen("lost question supervisor", |s| {
            s.contains("ABANDONED · supervisor absent") && s.contains("[R Recover]")
        })?
    );
    t.send("R")?;
    println!(
        "QUESTION RECOVER DIALOG\n{}",
        t.screen("question recovery dialog", |s| s
            .contains("Recover abandoned attempt"))?
    );
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    t.send("\r")?;
    println!(
        "STALE RECOVER REFUSED\n{}",
        t.screen("stale recovery", |s| s
            .contains("Current task changed; reopen Recover"))?
    );
    assert_eq!(f.ok(&["show", "1"])?["status"], "waiting");
    t.send("\x1b")?;
    t.screen("abandoned after refresh", |s| {
        s.contains("[R Recover]")
            && s.contains("Answer saved; resume")
            && !s.contains("Recover abandoned attempt")
    })?;
    t.send("R")?;
    t.screen("fresh recovery dialog", |s| {
        s.contains("Recover abandoned attempt")
    })?;
    t.send("\r")?;
    println!(
        "RECOVERED QUESTION\n{}",
        t.screen("recovered question", |s| s.contains("INTERRUPTED"))?
    );
    let after = f.ok(&["show", "1"])?;
    assert_eq!(after["status"], "interrupted");
    for field in ["logs", "steps", "report"] {
        assert_eq!(before["attempts"][0][field], after["attempts"][0][field]);
    }
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT state FROM questions WHERE attempt_id=(SELECT id FROM attempts WHERE task_id=1)",
            [],
            |r| r.get::<_, String>(0),
        )?,
        "answered"
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_runner_mouse_pause_keyboard_resume_and_live_history() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Observed execution")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    t.screen("ready", |s| s.contains("Observed execution"))?;
    t.click("r Run queue")?;
    println!(
        "RUNNING\n{}",
        t.screen("running", |s| s.contains("RUNNING")
            && s.contains("new session"))?
    );
    t.click("p Pause")?;
    println!(
        "PAUSED\n{}",
        t.screen("paused", |s| s.contains("PAUSED · supervisor absent")
            && s.contains("[r Resume]")
            && !s.contains("[R Recover]"))?
    );
    f.provider("normal")?;
    t.send("r")?;
    println!(
        "COMPLETE\n{}",
        t.screen("complete", |s| s.contains("Queue complete.")
            && s.contains("History 1"))?
    );
    t.click("3 History")?;
    println!(
        "HISTORY\n{}",
        t.screen("history", |s| s.contains("Observed execution")
            && s.contains("DONE"))?
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
#[ignore = "Explicit opt-in: uses the installed authenticated Codex and incurs model usage"]
fn live_codex_executes_reviews_and_validates_an_isolated_project() -> Result {
    let model = std::env::var("KTASK_LIVE_MODEL")?;
    let f = Fixture::new()?;
    for role in ["execution", "review", "validation", "remediation"] {
        f.ok(&[
            "model-default",
            role,
            "--provider",
            "codex",
            "--model",
            &model,
        ])?;
    }
    f.add("Live provider smoke test")?;
    let home = std::env::var_os("HOME").ok_or("HOME missing")?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".codex"));
    let output = f
        .process()
        .env("PATH", std::env::var_os("PATH").ok_or("PATH missing")?)
        .env("CODEX_HOME", codex_home)
        .args(["--json", "run"])
        .output()?;
    println!(
        "LIVE stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let task = f.ok(&["show", "1"])?;
    println!("LIVE TASK {}", serde_json::to_string_pretty(&task)?);
    assert!(output.status.success());
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?.trim(),
        "verified"
    );
    assert!(f.git(&["status", "--porcelain"])?.is_empty());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn protocol_errors_timeouts_and_transport_never_fake_success_or_spend_coding_budget() -> Result {
    for mode in ["malformed", "fail", "slow"] {
        let f = Fixture::new()?;
        f.provider(mode)?;
        f.add("Must fail honestly")?;
        if mode == "slow" {
            f.ok(&["edit", "1", "--timeout-ms", "200"])?;
        }
        let o = f.process().arg("run").output()?;
        assert_eq!(o.status.code(), Some(4));
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "failed");
        assert_eq!(task["attempts_used"], if mode == "slow" { 1 } else { 0 });
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn rate_limit_keeps_same_attempt_and_stores_resume_time_without_spending_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Wait")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"][0]["wait_reason"], "rate_limit");
    assert!(task["attempts"][0]["resume_at_ms"].is_i64());
    assert_eq!(f.process().arg("resume").output()?.status.code(), Some(4));
    let db = f.db()?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM questions", [], |r| r.get::<_, i64>(0))?,
        0
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn escaped_descendants_are_stopped_and_duplicate_runners_are_rejected() -> Result {
    let f = Fixture::new()?;
    f.provider("descendants")?;
    f.add("Descendants")?;
    let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    let until = Instant::now() + Duration::from_secs(5);
    while !f.root.join("descendant.pid").exists() {
        assert!(Instant::now() < until);
        thread::sleep(Duration::from_millis(20));
    }
    let pid = fs::read_to_string(f.root.join("descendant.pid"))?;
    let duplicate = f.process().arg("run").output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    f.ok(&["pause", "1"])?;
    run.0.wait()?;
    f.state("paused")?;
    assert!(
        !PathBuf::from(format!("/proc/{pid}")).exists(),
        "escaped descendant survived cleanup"
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_question_answer_mouse_and_keyboard_automatically_resume() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Ask owner")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("current", |s| s.contains("Ask owner"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let waiting = t.screen("question", |s| s.contains("QUESTION"))?;
        println!("QUESTION CONTROLS (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("w Answer"));
        assert!(waiting.contains("QUESTION · supervisor absent"));
        assert!(!waiting.contains("[R Recover]"));
        assert!(!waiting.contains("[r R"));
        let waiting_task = f.ok(&["show", "1"])?;
        let attempts = waiting_task["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 1);
        let attempt_id = attempts[0]["id"].clone();
        let invocations = f.scratch.path().join("provider-invocations");
        let calls_before = fs::read_to_string(&invocations)?;
        // The first blocked answer is nudged once before the owner is asked.
        assert_eq!(calls_before, "execution\nexecution\n");
        let log = f.scratch.path().join("state/ktask-dev/runner-1.log");
        let log_before = fs::read(&log)?;
        t.send("r")?;
        let still_waiting = t.screen("premature Run rejected", |s| {
            s.contains("Unavailable here") && s.contains("QUESTION")
        })?;
        println!("PREMATURE RUN REJECTED (mouse={mouse})\n{still_waiting}");
        assert!(still_waiting.contains("w Answer"));
        assert!(!still_waiting.contains("[r R"));
        assert_eq!(f.ok(&["show", "1"])?["attempts"], waiting_task["attempts"]);
        assert_eq!(fs::read_to_string(&invocations)?, calls_before);
        assert_eq!(fs::read(&log)?, log_before);
        if mouse {
            t.click("w Answer")?;
        } else {
            t.send("w")?;
        }
        println!(
            "ANSWER FORM (mouse={mouse})\n{}",
            t.screen("answer", |s| s.contains("Which content?"))?
        );
        t.send("Human answer: verified")?;
        t.screen("entered answer", |s| s.contains("Human answer: verified"))?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let complete = t.screen("complete", |s| s.contains("Queue complete."))?;
        println!("RESUMED SAME ATTEMPT (mouse={mouse})\n{complete}");
        let completed = f.ok(&["show", "1"])?;
        assert_eq!(completed["status"], "succeeded");
        assert_eq!(completed["attempts"][0]["id"], attempt_id);
        assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(&invocations)?,
            "execution\nexecution\nexecution\nreview\nvalidation\n"
        );
        for role in ["execution", "review", "validation"] {
            let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
            assert!(prompt.contains("Which content?\nHuman answer: verified"));
        }
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn closing_tui_does_not_stop_supervisor() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Keep running")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("current", |s| s.contains("Keep running"))?;
    t.send("r")?;
    t.screen("running", |s| s.contains("RUNNING"))?;
    t.send("q")?;
    t.finish()?;
    f.state("succeeded")?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn queue_rate_wait_auto_resumes_without_counting_waiting_time() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Automatic backoff")?;
    let mut run = Running(
        f.process()
            .args(["run", "--queue"])
            .stdout(Stdio::null())
            .spawn()?,
    );
    f.state("waiting")?;
    let before = f.ok(&["show", "1"])?["attempts"][0]["active_ms"].clone();
    thread::sleep(Duration::from_millis(200));
    assert_eq!(f.ok(&["show", "1"])?["attempts"][0]["active_ms"], before);
    f.provider("normal")?;
    // Advance the recorded deadline instead of sleeping a minute in the test suite.
    f.db()?.connection().execute(
        "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE state='waiting'",
        [],
    )?;
    f.state("succeeded")?;
    assert!(run.0.wait()?.success());
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        1
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_rate_wait_pause_with_mouse_and_keyboard_stops_automatic_retry() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("rate")?;
        f.add("Rate limited")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("current", |s| s.contains("Rate limited"))?;
        t.send("4")?;
        t.screen("queue continuation is the default", |s| {
            s.contains("Run mode: Continue queue")
        })?;
        t.send("1")?;
        t.send("r")?;
        println!(
            "RATE WAIT mouse={mouse}\n{}",
            t.screen("waiting", |s| s.contains("WAITING")
                && s.contains("Resumes in"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "RATE WAIT PAUSED mouse={mouse}\n{}",
            t.screen("scheduler stopped", |s| {
                s.contains("Automatic continuation stopped")
                    && s.contains("Session: Queue · paused")
            })?
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn unconfirmed_worker_death_blocks_execution_until_explicit_operator_confirmation() -> Result {
    let f = Fixture::new()?;
    f.add("Guard against orphaned work")?;
    let marker = f.root.join(".git/ktask-worker.lock");
    fs::write(&marker, "guardian interrupted\n")?;
    for args in [vec!["run"], vec!["recover"]] {
        let output = f.process().args(args).output()?;
        assert_eq!(output.status.code(), Some(4));
        assert!(String::from_utf8_lossy(&output.stderr).contains("cleanup confirmation"));
    }
    assert!(
        f.ok(&["show", "1"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    f.ok(&["recover", "--confirm-worker-cleanup"])?;
    assert_eq!(fs::metadata(marker)?.len(), 0);
    f.ok(&["run"])?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_persisted_single_and_continue_modes_with_two_tasks() -> Result {
    for mouse in [false, true] {
        for queue in [false, true] {
            let f = Fixture::new()?;
            f.add("First configured task")?;
            f.add("Second configured task")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("startup", |s| s.contains("First configured task"))?;
            if mouse {
                t.click("4 Config")?;
            } else {
                t.send("4")?;
            }
            t.screen("configuration", |s| s.contains("e Preferences"))?;
            if mouse {
                t.click("e Preferences")?;
            } else {
                t.send("e")?;
            }
            println!(
                "RUN PREFERENCES mouse={mouse} queue={queue}\n{}",
                t.screen("preferences", |s| s.contains("Project preferences"))?
            );
            // Continuing the queue is the default; running one task is the opt-in.
            if !queue {
                if mouse {
                    t.click("< Continue queue >")?;
                } else {
                    t.send("\t\t ")?;
                }
            }
            if mouse {
                t.click("Apply")?;
            } else {
                t.send("\x13")?;
            }
            t.screen("saved", |s| s.contains("Project configuration saved"))?;
            t.send("q")?;
            t.finish()?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("restart", |s| s.contains("First configured task"))?;
            if mouse {
                t.click(if queue { "r Run queue" } else { "r Run one" })?;
            } else {
                t.send("r")?;
            }
            let final_screen = t.screen("run mode outcome", |s| {
                if queue {
                    s.contains("Queue 0") && s.contains("History 2")
                } else {
                    s.contains("Second configured task") && s.contains("History 1")
                }
            })?;
            println!("RUN RESULT mouse={mouse} queue={queue}\n{final_screen}");
            let second = f.ok(&["show", "2"])?;
            assert_eq!(second["status"], if queue { "succeeded" } else { "ready" });
            assert_eq!(
                second["attempts"].as_array().unwrap().len(),
                usize::from(queue)
            );
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn real_tui_configuration_errors_and_live_check_guard() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.add("Configuration preflight")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let original = service.task(1)?.unwrap().definition().definition.clone();
    let mut missing = original.clone();
    missing.models[0] = None;
    let task = service.task(1)?.unwrap();
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &missing,
        true,
    )?;
    let checks = service.project_checks()?;
    let mut disabled = checks[0].clone();
    disabled.enabled = false;
    service.save_check(&checks, &disabled)?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
    t.screen("startup", |s| s.contains("Configuration preflight"))?;
    t.send("r")?;
    println!(
        "MISSING CHECKS\n{}",
        t.screen("missing checks", |s| s
            .contains("Configure an enabled check"))?
    );
    service.save_check(&service.project_checks()?, &checks[0])?;
    t.send("r")?;
    println!(
        "MISSING MODELS\n{}",
        t.screen("missing models", |s| s
            .contains("Configure all four task roles"))?
    );
    assert!(service.task(1)?.unwrap().attempts.is_empty());
    let task = service.task(1)?.unwrap();
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &original,
        true,
    )?;
    f.provider("rate")?;
    t.send("r")?;
    t.screen("rate wait", |s| s.contains("WAITING"))?;
    t.send("4c \r")?;
    t.screen("edit check", |s| s.contains("Edit project check"))?;
    t.send("\t\x01false\x13")?;
    println!(
        "LIVE CHECK GUARD\n{}",
        t.screen("live inputs guarded", |s| s
            .contains("Cannot change checks during a live attempt"))?
    );
    assert_eq!(service.project_checks()?, checks);
    t.send("\x1b\x03")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_recovers_connection_and_checks_then_continues_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("transient")?;
        f.fail_first_check()?;
        f.add("Recover connection and failed check")?;
        f.add("Continue without another Run")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let ready = t.screen("queue overview", |s| s.contains("r Run queue"))?;
        println!("AUTONOMOUS QUEUE mouse={mouse}\n{ready}");
        assert!(ready.contains("Goal: Create result.txt"));
        assert!(ready.contains("Proof: result.txt contains verified"));
        assert!(ready.contains("Next #2: Continue without another Run"));
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CONNECTION RECOVERY mouse={mouse}\n{}",
            t.screen("bounded retry", |s| s.contains("automatic retry 1/2"))?
        );
        let complete = t.screen("two verified tasks", |s| {
            s.contains("Queue complete.") && s.contains("History 2")
        })?;
        println!("AUTONOMOUS RESULT mouse={mouse}\n{complete}");
        assert!(complete.contains("no queued work"));
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["attempts_used"], 1);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nexecution\nremediation\nremediation\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        assert!(f.git(&["status", "--porcelain"])?.is_empty());
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_approval_and_human_ack_continue_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        for human in [false, true] {
            let f = Fixture::new()?;
            f.add("Completed before owner gate")?;
            f.ok(&[
                "add",
                "--title",
                "Owner gate",
                "--goal",
                "Confirm direction",
                "--criterion",
                "Direction confirmed",
                "--kind",
                if human { "human" } else { "ai" },
                "--approval",
                if human { "ready" } else { "draft" },
            ])?;
            f.add("Continue after owner gate")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
            t.screen("first current", |s| s.contains("r Run queue"))?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            println!(
                "OWNER GATE mouse={mouse} human={human}\n{}",
                t.screen("owner gate", |s| s.contains("Current #2")
                    && s.contains("Owner gate"))?
            );
            if mouse {
                t.click(if human { "y Acknowledge" } else { "v Approve" })?;
            } else {
                t.send(if human { "y" } else { "v" })?;
            }
            println!(
                "GATE CLEARED mouse={mouse} human={human}\n{}",
                t.screen("automatic continuation", |s| s.contains("Queue complete.")
                    && s.contains("History 3"))?
            );
            assert_eq!(f.ok(&["show", "3"])?["status"], "succeeded");
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn durable_session_survives_closed_tui_and_cli_intervention() -> Result {
    for (queue, human, mouse) in [
        (true, true, true),
        (true, false, false),
        (false, true, false),
        (false, false, true),
    ] {
        let f = Fixture::new()?;
        f.add("First verified task")?;
        f.ok(&[
            "add",
            "--title",
            "Owner gate",
            "--goal",
            "Confirm direction",
            "--criterion",
            "Direction confirmed",
            "--kind",
            if human { "human" } else { "ai" },
            "--approval",
            if human { "ready" } else { "draft" },
        ])?;
        f.add("Work after gate")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        if !queue {
            // The saved preference controls the next explicit Run, not a prior session.
            let service = ktask_rs_mk3::application::Service::open(f.db()?, &f.root, None)?;
            let config = f.scratch.path().join("config/ktask-dev");
            let old = service.preferences(&config)?;
            let mut single = old.clone();
            single.continuation = ktask_rs_mk3::application::preferences::Continuation::Single;
            service.save_preferences(&config, &old, &single)?;
            drop(service);
            t.send("q")?;
            t.finish()?;
            t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        }
        let label = if queue { "r Run queue" } else { "r Run one" };
        t.screen("start", |s| s.contains(label))?;
        if mouse {
            t.click(label)?;
        } else {
            t.send("r")?;
        }
        let gate = t.screen("durable gate", |s| {
            s.contains("Current #2")
                && s.contains("Owner gate")
                && s.contains(if queue {
                    "Session: Queue · waiting"
                } else {
                    "Session: Run one · stopped"
                })
        })?;
        println!("DURABLE GATE queue={queue} human={human} mouse={mouse}\n{gate}");
        t.send("q")?;
        t.finish()?;
        f.ok(&[if human { "ack" } else { "approve" }, "2"])?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let after = reopened.screen("restored session", |s| {
            if queue {
                s.contains("History 3") && s.contains("Session: Queue · complete")
            } else {
                s.contains("Session: Run one · stopped")
                    && s.contains(if human { "Current #3" } else { "Current #2" })
            }
        })?;
        println!("RESTORED SESSION queue={queue} human={human} mouse={mouse}\n{after}");
        assert_eq!(
            f.ok(&["show", "3"])?["attempts"].as_array().unwrap().len(),
            usize::from(queue)
        );
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn two_windows_share_one_session_and_pause_blocks_handoff() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Slow current")?;
    f.add("Must not start after pause")?;
    let mut first = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let mut second = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    first.screen("ready", |s| s.contains("r Run queue"))?;
    first.send("r")?;
    let live = second.screen("shared live session", |s| {
        s.contains("Current #1 · RUNNING") && s.contains("Session: Queue · running")
    })?;
    println!("SECOND WINDOW LIVE\n{live}");
    let duplicate = f.process().args(["run", "--background"]).output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    second.click("p Pause")?;
    let paused = first.screen("cross window pause", |s| {
        s.contains("Current #1 · PAUSED") && s.contains("Session: Queue · paused")
    })?;
    println!("FIRST WINDOW PAUSED\n{paused}");
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    first.send("q")?;
    first.finish()?;
    second.send("q")?;
    second.finish()?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let restored = reopened.screen("pause retained", |s| s.contains("Session: Queue · paused"))?;
    println!("REOPENED PAUSE\n{restored}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn pause_at_ready_task_handoff_never_starts_the_next_attempt() -> Result {
    let f = Fixture::new()?;
    f.add("Complete before handoff")?;
    f.add("Do not start across pause")?;
    // The fourth rev-parse HEAD is the next task's preflight, after the
    // first checkpoint. Delay Git itself to expose the real handoff boundary.
    let marker = f.scratch.path().join("handoff-open");
    let counter = f.scratch.path().join("head-count");
    let git = f.scratch.path().join("bin/git");
    fs::write(
        &git,
        format!(
            "#!/bin/sh\nif [ \"$3\" = rev-parse ] && [ \"$4\" = HEAD ]; then\n count=$(cat '{}' 2>/dev/null || printf 0)\n count=$((count+1))\n printf '%s' \"$count\" > '{}'\n if [ \"$count\" = 4 ]; then touch '{}'; sleep 3; fi\nfi\nexec /usr/bin/git \"$@\"\n",
            counter.display(),
            counter.display(),
            marker.display()
        ),
    )?;
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755))?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.send("r")?;
    let handoff = t.screen("ready handoff", |s| {
        marker.exists()
            && s.contains("Current #2 · PENDING")
            && s.contains("Session: Queue · running")
            && s.contains("p Pause")
    })?;
    println!("READY HANDOFF\n{handoff}");
    t.click("p Pause")?;
    let paused = t.screen("handoff paused", |s| {
        s.contains("Session: Queue · paused")
            && s.contains("Queue paused before the next task")
            && !s.contains("Supervisor preparing the next task")
    })?;
    println!("PAUSED AT HANDOFF\n{paused}");
    thread::sleep(Duration::from_millis(3200));
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_answer_after_tui_exit_resumes_the_same_queue_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question before exit")?;
    f.add("Continue after answer")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.send("r")?;
    let question = t.screen("question waiting", |s| {
        s.contains("QUESTION · supervisor absent") && s.contains("Session: Queue · waiting")
    })?;
    println!("QUESTION BEFORE EXIT\n{question}");
    let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
    t.send("q")?;
    t.finish()?;
    f.provider("normal")?;
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    let done = reopened.screen("automatic answer continuation", |s| {
        s.contains("History 2") && s.contains("Session: Queue · complete")
    })?;
    println!("ANSWER AFTER EXIT\n{done}");
    let first = f.ok(&["show", "1"])?;
    assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(first["attempts"][0]["id"], attempt);
    assert_eq!(
        f.ok(&["show", "2"])?["attempts"].as_array().unwrap().len(),
        1
    );
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn intervention_marker_survives_supervisor_handoff_and_is_claimed_once() -> Result {
    use ktask_rs_mk3::application::Service;

    let f = Fixture::new()?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut first = Service::open(f.db()?, &f.root, None)?;
    first.start_execution_session(true)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join(".git/ktask-runner.lock"))?;
    lease.lock()?;
    let receipt = f.ok(&["ack", "1"])?;
    assert!(receipt.get("continuation_error").is_none());
    assert_eq!(
        first.execution_session()?.unwrap().state,
        "resume_requested"
    );
    assert!(first.settle_execution_session("waiting", "Old supervisor stopped")?);
    assert_eq!(
        first.execution_session()?.unwrap().state,
        "resume_requested"
    );
    lease.unlock()?;

    let mut second = Service::open(f.db()?, &f.root, None)?;
    assert!(first.resume_execution_session()?);
    assert!(!second.resume_execution_session()?);
    f.ok(&["run", "--queue", "--continue-session"])?;
    assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn live_supervisor_consumes_intervention_marker_before_next_attempt() -> Result {
    use ktask_rs_mk3::application::Service;

    let f = Fixture::new()?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    service.start_execution_session(true)?;
    let lease = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(f.root.join(".git/ktask-runner.lock"))?;
    lease.lock()?;
    f.ok(&["ack", "1"])?;
    assert_eq!(
        service.execution_session()?.unwrap().state,
        "resume_requested"
    );
    service.supervisor_continuing_queue()?;
    assert_eq!(service.execution_session()?.unwrap().state, "running");
    assert!(!service.settle_execution_session("waiting", "Waiting for next answer")?);
    assert_eq!(service.execution_session()?.unwrap().state, "waiting");
    lease.unlock()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn final_human_ack_completes_queue_session_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Complete before final gate")?;
        f.ok(&[
            "add",
            "--title",
            "Final owner gate",
            "--goal",
            "Acknowledge completion",
            "--criterion",
            "Owner acknowledged",
            "--kind",
            "human",
        ])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("ready before final owner gate", |s| {
            s.contains("r Run queue")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        t.screen("final owner gate", |s| {
            s.contains("Current #2") && s.contains("Session: Queue · waiting")
        })?;
        if mouse {
            t.click("y Acknowledge")?;
        } else {
            t.send("y")?;
        }
        let complete = t.screen("final human ack", |s| {
            s.contains("Queue complete.") && s.contains("Session: Queue · complete")
        })?;
        println!("FINAL HUMAN ACK mouse={mouse}\n{complete}");
        t.send("q")?;
        t.finish()?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        let restored = reopened.screen("final ack retained", |s| {
            s.contains("Session: Queue · complete")
                && s.contains("Queue complete after intervention")
        })?;
        println!("FINAL HUMAN ACK REOPENED mouse={mouse}\n{restored}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn run_one_answer_updates_stop_reason_and_waits_for_explicit_resume() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};

    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question in Run one")?;
    f.add("Must wait for another Run")?;
    let service = Service::open(f.db()?, &f.root, None)?;
    let config = f.scratch.path().join("config/ktask-dev");
    let old = service.preferences(&config)?;
    let mut single = old.clone();
    single.continuation = Continuation::Single;
    service.save_preferences(&config, &old, &single)?;
    drop(service);
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.send("r")?;
    t.screen("run one question", |s| {
        s.contains("Current #1 · QUESTION") && s.contains("Session: Run one · waiting")
    })?;
    t.send("q")?;
    t.finish()?;
    f.provider("normal")?;
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    let stopped = reopened.screen("answered Run one waits", |s| {
        s.contains("Session: Run one · stopped")
            && s.contains("Run one awaits explicit Run or Resume")
    })?;
    println!("RUN ONE ANSWER STOPPED\n{stopped}");
    assert!(
        f.ok(&["show", "2"])?["attempts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    reopened.send("r")?;
    let next = reopened.screen("Run one resumed only current", |s| {
        s.contains("Current #2 · PENDING") && s.contains("Session: Run one · stopped")
    })?;
    println!("RUN ONE ANSWER RESUMED\n{next}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_intervention_records_lease_inspection_failure_on_reopened_screen() -> Result {
    let f = Fixture::new()?;
    f.add("Complete before gate")?;
    f.ok(&[
        "add",
        "--title",
        "Owner gate",
        "--goal",
        "Acknowledge before work",
        "--criterion",
        "Owner acknowledged",
        "--kind",
        "human",
    ])?;
    f.add("Work after gate")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    t.send("r")?;
    t.screen("gate before CLI error", |s| {
        s.contains("Current #2") && s.contains("Session: Queue · waiting")
    })?;
    t.send("q")?;
    t.finish()?;

    let lease = f.root.join(".git/ktask-runner.lock");
    fs::set_permissions(&lease, fs::Permissions::from_mode(0o000))?;
    let receipt = f.ok(&["ack", "2"])?;
    assert!(
        receipt["continuation_error"]
            .as_str()
            .is_some_and(|s| s.contains("Permission denied"))
    );
    let service = ktask_rs_mk3::application::Service::open(f.db()?, &f.root, None)?;
    let session = service.execution_session()?.unwrap();
    assert_eq!(session.state, "resume_requested");
    assert!(session.reason.contains("Permission denied"));
    fs::set_permissions(&lease, fs::Permissions::from_mode(0o644))?;

    let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 30)?;
    let screen = reopened.screen("retained continuation error", |s| {
        s.contains("Session: Queue · supervisor absent")
            && s.contains("Continuation could not be confirmed")
    })?;
    println!("CONTINUATION ERROR AFTER REOPEN\n{screen}");
    reopened.send("r")?;
    let complete = reopened.screen("explicit recovery after inspection error", |s| {
        s.contains("Queue complete.") && s.contains("Session: Queue · complete")
    })?;
    println!("CONTINUATION ERROR RECOVERED\n{complete}");
    reopened.send("q")?;
    reopened.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn cli_run_modes_share_preferences_and_allow_explicit_overrides() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};
    for explicit_queue in [false, true] {
        let f = Fixture::new()?;
        f.add("First CLI task")?;
        f.add("Second CLI task")?;
        let service = Service::open(f.db()?, &f.root, None)?;
        let config = f.scratch.path().join("config/ktask-dev");
        let original = service.preferences(&config)?;
        let mut saved = original.clone();
        if explicit_queue {
            saved.continuation = Continuation::Single;
        }
        service.save_preferences(&config, &original, &saved)?;
        f.ok(&["run", if explicit_queue { "--queue" } else { "--one" }])?;
        assert_eq!(
            f.ok(&["show", "2"])?["status"],
            if explicit_queue { "succeeded" } else { "ready" }
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_pause_during_connection_recovery_stops_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("fail")?;
        f.add("Pause connection recovery")?;
        f.add("Must remain pending")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        println!(
            "RETRY BEFORE PAUSE mouse={mouse}\n{}",
            t.screen("retrying", |s| s.contains("retry 1/2")
                && s.contains("RUNNING"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "RETRY PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| s.contains("PAUSED")
                && s.contains("supervisor absent"))?
        );
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["attempts_used"], 0);
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn real_tui_current_run_mode_control_changes_saved_single_preference() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("First visible mode task")?;
        f.add("Second visible mode task")?;
        let service = Service::open(f.db()?, &f.root, None)?;
        let config = f.scratch.path().join("config/ktask-dev");
        let original = service.preferences(&config)?;
        let mut single = original.clone();
        single.continuation = Continuation::Single;
        service.save_preferences(&config, &original, &single)?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        println!(
            "SAVED SINGLE MODE mouse={mouse}\n{}",
            t.screen("single visible", |s| s.contains("r Run one")
                && s.contains("Run one selected"))?
        );
        if mouse {
            t.click("m Run mode")?;
        } else {
            t.send("m")?;
        }
        println!(
            "CURRENT MODE FORM mouse={mouse}\n{}",
            t.screen("mode form", |s| s.contains("Project preferences"))?
        );
        if mouse {
            t.click("< Run one >")?;
            t.click("Apply")?;
        } else {
            t.send("\t\t \x13")?;
        }
        t.screen("queue visible", |s| s.contains("r Run queue"))?;
        assert_eq!(
            service.preferences(&config)?.continuation,
            Continuation::Queue
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "MODE CHANGED RESULT mouse={mouse}\n{}",
            t.screen("queue finished", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn automatic_prerequisite_preserves_work_and_original_repair_round() -> Result {
    let f = Fixture::new()?;
    f.fail_first_check()?;
    f.add("Original with useful unfinished work")?;
    f.ok(&["edit", "1", "--timeout-ms", "unlimited"])?;
    f.provider("judge-insert")?;
    f.ok(&["run", "--queue"])?;
    let original = f.ok(&["show", "1"])?;
    let repair = f.ok(&["show", "2"])?;
    assert_eq!(original["lifecycle"], "succeeded");
    assert_eq!(original["attempts"][0]["state"], "interrupted");
    assert_eq!(original["attempts"][1]["state"], "succeeded");
    assert_eq!(repair["lifecycle"], "succeeded");
    let preservation = f.git(&["log", "--format=%H %s"])?;
    assert!(
        preservation.contains("preserve unfinished task #1, attempt #1"),
        "{preservation}"
    );
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert!(f.git(&["status", "--porcelain"])?.trim().is_empty());
    let db = f.db()?;
    assert_eq!(db.connection().query_row("SELECT repair_round FROM attempt_progress p JOIN attempts a ON a.id=p.attempt_id WHERE a.task_id=1 ORDER BY a.id DESC LIMIT 1", [], |r| r.get::<_,i64>(0))?, 1);
    let limit: Option<i64> = db.connection().query_row(
        "SELECT attempt_timeout_ms FROM task_revisions WHERE task_id=2",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(
        limit, None,
        "repair must inherit the unlimited parent deadline"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn exhausted_original_gets_one_retry_after_prerequisite_in_mouse_and_keyboard_tuis() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("review-critical")?;
        f.add("Original at its coding limit")?;
        f.ok(&["edit", "1", "--attempt-limit", "1"])?;
        assert_eq!(
            f.process().args(["run", "--one"]).output()?.status.code(),
            Some(4)
        );
        assert_eq!(f.ok(&["show", "1"])?["attempts_used"], 1);
        f.provider("judge-insert")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        println!(
            "EXHAUSTED CURRENT mouse={mouse}\n{}",
            t.screen("failed current", |s| s.contains("FAILED")
                && s.contains("Budget 1 of 1 used"))?
        );
        if mouse {
            t.click("t Retry")?;
        } else {
            t.send("t")?;
        }
        println!(
            "RECOVERED CURRENT mouse={mouse}\n{}",
            t.screen("prerequisite and original completed", |s| s
                .contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        let original = f.ok(&["show", "1"])?;
        assert_eq!(original["lifecycle"], "succeeded");
        assert_eq!(original["attempts_used"], 2);
        assert_eq!(original["definition"]["attempt_limit"], 2);
        assert_eq!(original["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(original["attempts"][0]["revision"], 2);
        assert_eq!(original["revisions"][1]["definition"]["attempt_limit"], 1);
        assert_eq!(original["revisions"][2]["definition"]["attempt_limit"], 2);
        let db = f.db()?;
        assert_eq!(
            db.connection().query_row(
                "SELECT recovery_root_id FROM tasks WHERE id=2",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        assert_eq!(db.connection().query_row("SELECT count(*) FROM attempt_steps s JOIN attempts a ON a.id=s.attempt_id WHERE a.task_id=1 AND s.role='remediation' AND s.report LIKE 'Repair:%'", [], |r| r.get::<_,i64>(0))?, 11);
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn configured_task_deadline_counts_earlier_attempt_time_and_identifies_the_limit() -> Result {
    let f = Fixture::new()?;
    f.provider("malformed-work")?;
    f.add("Deadline across retries")?;
    assert!(f.ok(&["show", "1"])?["definition"]["timeout_ms"].is_null());
    f.ok(&["edit", "1", "--timeout-ms", "10000"])?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    f.db()?
        .connection()
        .execute("UPDATE attempts SET active_ms=9999 WHERE task_id=1", [])?;
    f.provider("normal")?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 2);
    assert!(
        task["attempts"][1]["outcome"]
            .as_str()
            .unwrap()
            .contains("configured task-wide limit")
    );
    assert_eq!(task["attempts_used"], 1);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_individual_check_timeout_remains_separate_from_task_deadline() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Keep supervisor checks bounded")?;
        f.add("Blocked behind timed out check")?;
        f.ok(&[
            "check-set",
            "build",
            "--command",
            "sleep 30",
            "--timeout-ms",
            "200",
        ])?;
        if mouse {
            f.ok(&["edit", "1", "--timeout-ms", "30000"])?;
        } else {
            assert!(f.ok(&["show", "1"])?["definition"]["timeout_ms"].is_null());
        }
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        t.screen("check timeout fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "INDIVIDUAL CHECK TIMEOUT mouse={mouse}\n{}",
            t.screen("individual check timeout", |s| s.contains("FAILED")
                && s.contains("Individual supervisor check timeout reached"))?
        );
        f.supervisor_stopped()?;
        let failed = f.ok(&["show", "1"])?;
        println!("INDIVIDUAL CHECK TIMEOUT TASK {failed}");
        assert!(failed["attempts"][0]["active_ms"].as_i64().unwrap() >= 200);
        assert!(
            !failed["attempts"][0]["outcome"]
                .as_str()
                .unwrap()
                .contains("task-wide")
        );
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT failure_category FROM attempts WHERE task_id=1",
                [],
                |r| r.get::<_, String>(0),
            )?,
            "timeout"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn repeated_rate_limits_stop_after_three_waits_without_spending_coding_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Rate limit must stop")?;
    for wait in 1..=3 {
        f.ok(&["run", "--one"])?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "waiting");
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT rate_waits FROM attempts WHERE task_id=1",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            wait
        );
        f.db()?.connection().execute(
            "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE task_id=1",
            [],
        )?;
    }
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert!(
        task["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("after 3 waits")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn zero_round_replan_rejudges_before_the_next_worker() -> Result {
    let f = Fixture::new()?;
    f.provider("malformed-work")?;
    f.add("Judge every replan")?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    f.provider("judge-revise")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let reports: Vec<String> = f.db()?.connection().prepare("SELECT report FROM attempt_steps WHERE role='remediation' AND report IS NOT NULL ORDER BY id")?.query_map([], |row| row.get(0))?.collect::<rusqlite::Result<_>>()?;
    assert!(
        reports
            .iter()
            .any(|report| report.contains("Task revised by the failure judge"))
    );
    assert!(reports.iter().any(|report| report.starts_with("Repair:")));
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT automatic_replans FROM tasks WHERE id=1",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        1
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn retry_reservation_preserves_revision_history_and_rejects_stale_edit() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.provider("review-critical")?;
    f.add("Original with exhausted limit")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let original = service.current_task()?.unwrap();
    let target = VersionedTask {
        id: original.id,
        version: original.version,
    };
    let mut repair = original.definition().definition.clone();
    repair.title = "Prerequisite".into();
    ktask_rs_mk3::runner::remediation::apply(
        &mut service,
        target,
        None,
        &[repair],
        "Repair first",
        true,
    )?;
    let revised = service.task(target.id)?.unwrap();
    assert!(revised.version > target.version);
    assert_eq!(revised.revisions[1].definition.attempt_limit, Some(1));
    assert_eq!(revised.revisions[2].definition.attempt_limit, Some(2));
    assert_eq!(revised.attempts[0].revision, 2);
    assert!(
        service
            .revise(target, &original.definition().definition, true)
            .is_err()
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn background_startup_returns_actual_error_in_cli_and_tui() -> Result {
    for (mouse, cols, rows) in [(false, 70, 22), (true, 120, 30)] {
        let f = Fixture::new()?;
        f.add("Cannot start with unrelated work")?;
        fs::write(f.root.join("unrelated.txt"), "keep this user work")?;
        fs::write(
            f.scratch.path().join("state/ktask-dev/runner-1.log"),
            "OLD unrelated startup failure\n",
        )?;
        let result = f
            .process()
            .args(["--json", "run", "--background"])
            .output()?;
        assert_eq!(result.status.code(), Some(4));
        let error = String::from_utf8(result.stderr)?;
        assert!(error.contains("uncommitted work"), "{error}");
        assert!(!error.contains("OLD unrelated"), "{error}");
        assert!(!error.contains("inspect /"), "{error}");
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
        t.screen("pending", |s| {
            s.contains("Cannot start with unrelated work")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "STARTUP ERROR mouse={mouse}\n{}",
            t.screen("real startup reason", |s| s.contains("uncommitted work"))?
        );
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            fs::read_to_string(f.root.join("unrelated.txt"))?,
            "keep this user work"
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn tui_remediation_handoff_preserves_dirty_work_and_failed_attempt() -> Result {
    use ktask_rs_mk3::application::{Service, preferences::Continuation};
    for (mouse, cols, rows) in [(false, 70, 22), (true, 120, 30)] {
        let f = Fixture::new()?;
        f.add("Failed original with partial result")?;
        f.ok(&[
            "edit",
            "1",
            "--attempt-limit",
            "1",
            "--timeout-ms",
            "unlimited",
        ])?;
        f.ok(&["check-set", "build", "--command", "false"])?;
        f.provider("malformed-work")?;
        let failed = f.process().args(["run", "--one"]).output()?;
        assert_eq!(failed.status.code(), Some(4));
        assert!(String::from_utf8(failed.stderr)?.contains("protocol"));
        f.provider("normal")?;
        let before = f.ok(&["show", "1"])?;
        assert!(!f.git(&["status", "--porcelain"])?.trim().is_empty());
        let service = Service::open(f.db()?, &f.root, None)?;
        let config = f.scratch.path().join("config/ktask-dev");
        let old = service.preferences(&config)?;
        let mut single = old.clone();
        single.continuation = Continuation::Single;
        service.save_preferences(&config, &old, &single)?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
        t.screen("failed original", |s| s.contains("Current #1 · FAILED"))?;
        if mouse {
            t.click("d Remediate")?;
        } else {
            t.send("d")?;
        }
        println!(
            "DIRTY HANDOFF PROPOSAL mouse={mouse}\n{}",
            t.screen("proposal", |s| s.contains("Failure judgment"))?
        );
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        println!(
            "DIRTY HANDOFF APPLIED mouse={mouse}\n{}",
            t.screen("repair ready", |s| s.contains("Current #2 · PENDING"))?
        );
        assert_eq!(f.ok(&["show", "1"])?["attempts"], before["attempts"]);
        assert_eq!(
            f.ok(&["show", "2"])?["definition"]["timeout_ms"],
            Value::Null
        );
        assert!(f.git(&["status", "--porcelain"])?.trim().is_empty());
        assert!(
            f.git(&["log", "-1", "--format=%s"])?
                .contains("preserve unfinished task #1, attempt #1")
        );
        assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
        f.ok(&["check-set", "build", "--command", "test -f result.txt"])?;
        f.ok(&["edit", "1", "--attempt-limit", "3"])?;
        if mouse {
            t.click("r Run one")?;
        } else {
            t.send("r")?;
        }
        println!(
            "DIRTY HANDOFF REPAIR COMPLETE mouse={mouse}\n{}",
            t.screen("original resumes eligibility", |s| s
                .contains("Current #1 · FAILED")
                && s.contains("History 1"))?
        );
        t.send("q")?;
        t.finish()?;
        f.ok(&["run", "--queue"])?;
        assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn detached_runner_survives_launching_cli_and_keeps_stderr() -> Result {
    let f = Fixture::new()?;
    f.add("Detached task")?;
    let provider = f.scratch.path().join("bin/codex");
    let script = fs::read_to_string(&provider)?.replace(
        "case \"$model\" in execution|remediation)",
        "sleep 0.3\nprintf 'detached diagnostic\\n' >&2\ncase \"$model\" in execution|remediation)",
    );
    fs::write(provider, script)?;
    f.ok(&["run", "--queue", "--background"])?;
    f.state("succeeded")?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let task = f.ok(&["show", "1"])?;
    assert!(
        task["attempts"][0]["logs"]
            .to_string()
            .contains("detached diagnostic")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn manual_handoff_rejects_stale_target_and_changed_baseline_without_committing() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.add("Failed original")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    f.ok(&["check-set", "build", "--command", "false"])?;
    f.fail_provider()?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let original = service.current_task()?.unwrap();
    let target = VersionedTask {
        id: original.id,
        version: original.version,
    };
    let mut repair = original.definition().definition.clone();
    repair.title = "Repair".into();
    let before = f.git(&["rev-parse", "HEAD"])?;
    let stale = VersionedTask {
        version: target.version - 1,
        ..target
    };
    let error = ktask_rs_mk3::runner::remediation::apply(
        &mut service,
        stale,
        None,
        &[repair.clone()],
        "Repair partial work",
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("Current task changed"));
    assert_eq!(f.git(&["rev-parse", "HEAD"])?, before);
    f.git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "external baseline change",
    ])?;
    let changed = f.git(&["rev-parse", "HEAD"])?;
    let error = ktask_rs_mk3::runner::remediation::apply(
        &mut service,
        target,
        None,
        &[repair],
        "Repair partial work",
        true,
    )
    .unwrap_err();
    assert!(error.to_string().contains("changed HEAD"));
    assert_eq!(f.git(&["rev-parse", "HEAD"])?, changed);
    assert!(service.task(2)?.is_none());
    assert_eq!(service.current_task()?.unwrap().version, original.version);
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_preserves_large_output_and_full_review_evidence_through_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("large-evidence")?;
        f.add("Large output and independent evidence")?;
        f.add("Following task")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        println!(
            "LARGE EVIDENCE START mouse={mouse}\n{}",
            t.screen("ready", |s| s.contains("r Run queue"))?
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "LARGE EVIDENCE COMPLETE mouse={mouse}\n{}",
            t.screen("verified queue", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        for id in ["1", "2"] {
            let task = f.ok(&["show", id])?;
            assert_eq!(task["lifecycle"], "succeeded");
            assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
            let logs = task["attempts"][0]["logs"].to_string();
            assert!(logs.contains(&format!("START{}END", "x".repeat(1250000))));
        }
        let prompt = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
        let report: Value = serde_json::from_str(prompt.rsplit_once("\nreview: ").unwrap().1)?;
        assert_eq!(report["task_id"], 2);
        assert_eq!(report["attempt_id"], 2);
        assert_eq!(report["role"], "review");
        let review_id: i64 = f.db()?.connection().query_row(
            "SELECT id FROM attempt_steps WHERE attempt_id=2 AND role='review'",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(report["step_id"], review_id);
        assert_eq!(report["assessment"]["summary"], "Review passed");
        assert_eq!(
            report["assessment"]["criteria"][0]["evidence"],
            "Inspected rendered PTY screens and the supervisor check transcript"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn review_failure_is_judged_repaired_and_queue_continues_automatically() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("review-once")?;
        f.add("Review repair")?;
        f.add("Following task")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        println!(
            "REVIEW RECOVERY START mouse={mouse}\n{}",
            t.screen("ready", |s| s.contains("r Run queue"))?
        );
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "REVIEW RECOVERY COMPLETE mouse={mouse}\n{}",
            t.screen("queue completed after review repair", |s| s
                .contains("Queue complete.")
                && s.contains("History 2"))?
        );
        t.send("q")?;
        t.finish()?;
        let first = f.ok(&["show", "1"])?;
        assert!(
            first["attempts"][0]["steps"]
                .as_array()
                .unwrap()
                .iter()
                .any(|step| step["role"] == "review" && step["state"] == "changes_requested")
        );
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["lifecycle"], "succeeded");
        assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nremediation\nremediation\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn review_converges_after_nine_repairs_with_a_single_attempt_budget() -> Result {
    let f = Fixture::new()?;
    f.provider("review-nine")?;
    f.add("One attempt includes normal repair rounds")?;
    f.add("Following task")?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    f.ok(&["run", "--queue"])?;
    let first = f.ok(&["show", "1"])?;
    assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(first["attempts_used"], 1);
    assert_eq!(first["lifecycle"], "succeeded");
    assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
    let db = f.db()?;
    assert_eq!(
        db.connection().query_row(
            "SELECT repair_round FROM attempt_progress WHERE attempt_id=1",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        9
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='remediation'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        18
    );
    assert_eq!(db.connection().query_row("SELECT count(*) FROM attempt_steps WHERE attempt_id=1 AND role='review' AND state='changes_requested'", [], |r| r.get::<_,i64>(0))?, 9);
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM attempts WHERE state='failed'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        0
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn executable_busy_recheck_retains_both_outcomes_without_a_second_coding_attempt() -> Result {
    for persistent in [false, true] {
        let f = Fixture::new()?;
        f.add("Busy executable check")?;
        f.ok(&["edit", "1", "--attempt-limit", "1"])?;
        let command = if persistent {
            "printf 'Error: Os { code: 26, kind: ExecutableFileBusy, message: Text file busy }\\n'; exit 1"
        } else {
            "if [ ! -f \"$HOME/check-once\" ]; then touch \"$HOME/check-once\"; printf 'Error: Os { code: 26, kind: ExecutableFileBusy, message: Text file busy }\\n'; exit 1; fi; echo 'recheck passed'"
        };
        f.ok(&["check-set", "build", "--command", command])?;
        let result = f.process().args(["run"]).output()?;
        assert_eq!(result.status.success(), !persistent);
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        let db = f.db()?;
        let mut query = db
            .connection()
            .prepare("SELECT state,exit_code FROM attempt_steps WHERE role='check' ORDER BY id")?;
        let checks = query
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i32>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if persistent {
            assert_eq!(checks, vec![("failed".into(), 1); 22]);
            assert_eq!(task["status"], "failed");
        } else {
            assert_eq!(checks, vec![("failed".into(), 1), ("succeeded".into(), 0)]);
        }
        assert_eq!(
            db.connection()
                .query_row("SELECT count(*) FROM artifacts", [], |r| r.get::<_, i64>(0))?,
            if persistent { 22 } else { 2 }
        );
        if !persistent {
            assert_eq!(
                fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
                "execution\nreview\nvalidation\n"
            );
            for role in ["review", "validation"] {
                let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
                assert!(
                    prompt.contains("Earlier supervisor check failed with executable contention")
                );
                assert!(prompt.contains("does not prove the root cause is fixed"));
                assert_eq!(prompt.matches("step-").count(), 2);
            }
        } else {
            assert_eq!(task["attempts_used"], 1);
        }
        f.no_scratch()?;
    }
    Ok(())
}

/// Read persisted failures and their independently retained supervisor transcripts.
fn assert_check_launch_records(f: &Fixture, failed: usize, passed: bool) -> Result<Vec<i64>> {
    let db = f.db()?;
    let records = db.connection().prepare(
        "SELECT id,attempt_id,state,exit_code,started_at_ms,report FROM attempt_steps WHERE task_id=1 AND role='check' ORDER BY id",
    )?.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
        r.get::<_, Option<i32>>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?,
    )))?.collect::<rusqlite::Result<Vec<_>>>()?;
    println!("CHECK LAUNCH RECORDS {records:#?}");
    assert_eq!(records.len(), failed + usize::from(passed));
    for (index, (id, attempt, state, code, _, report)) in records.iter().enumerate() {
        assert_eq!(*attempt, 1, "automatic retries must retain their attempt");
        if index < failed {
            assert_eq!(state, "failed");
            assert_eq!(*code, None, "failed launch never executes the check");
            assert!(report.contains("Permission denied"), "{report}");
        } else {
            assert_eq!(state, "succeeded");
            assert_eq!(*code, Some(0));
        }
        let path = format!("evidence/attempt-1/step-{id}.log");
        let artifact: (String, String, i64) = db.connection().query_row(
            "SELECT name,media_type,size_bytes FROM artifacts WHERE attempt_id=1 AND relative_path=?1 AND pruned_at_ms IS NULL",
            [&path], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let transcript = fs::read_to_string(f.scratch.path().join("state/ktask-dev").join(&path))?;
        println!("CHECK LAUNCH ARTIFACT {path} {artifact:?}\n{transcript}");
        assert_eq!(artifact.0, "build");
        assert_eq!(artifact.2, transcript.len() as i64);
    }
    let progress: (i64, String) = db.connection().query_row(
        "SELECT repair_round,phase FROM attempt_progress WHERE attempt_id=1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    assert_eq!(
        progress.0, 0,
        "infrastructure failures must not start coding repairs"
    );
    Ok(records.iter().map(|r| r.4).collect())
}

#[test]
fn real_tui_check_launch_failure_recovers_same_attempt_and_continues_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH TRANSIENT mouse={mouse}\n{}",
            t.screen("check launch backoff", |s| s
                .contains("Check infrastructure failed")
                && s.contains("retry 1/2 after 1s"))?
        );
        let retrying = f.ok(&["show", "1"])?;
        assert_eq!(retrying["attempts_used"], 0);
        assert_eq!(retrying["attempts"].as_array().unwrap().len(), 1);
        assert_check_launch_records(&f, 1, false)?;
        f.launch_workers_reaped()?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        println!(
            "CHECK LAUNCH TRANSIENT COMPLETE mouse={mouse}\n{}",
            t.screen("queue recovered", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts_used"], 1);
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        let launches = assert_check_launch_records(&f, 1, true)?;
        assert!(
            launches[1] - launches[0] >= 1000,
            "1s backoff skipped: {launches:?}"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
    }
    Ok(())
}

#[test]
fn real_tui_check_launch_failure_exhausts_three_launches_and_blocks_queue() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        for (retry, seconds) in [(1, 1), (2, 2)] {
            println!(
                "CHECK LAUNCH PERSISTENT mouse={mouse} retry={retry}\n{}",
                t.screen("check launch bounded backoff", |s| s
                    .contains("Check infrastructure failed")
                    && s.contains(&format!("retry {retry}/2 after {seconds}s")))?
            );
        }
        println!(
            "CHECK LAUNCH EXHAUSTED mouse={mouse}\n{}",
            t.screen("launch retries exhausted", |s| s.contains("FAILED")
                && s.contains("Permission denied"))?
        );
        let first = f.ok(&["show", "1"])?;
        println!("CHECK LAUNCH EXHAUSTED TASK {first}");
        assert_eq!(first["status"], "failed");
        assert_eq!(first["attempts_used"], 0);
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT failure_category FROM attempts WHERE task_id=1",
                [],
                |r| r.get::<_, String>(0)
            )?,
            "environment"
        );
        let second = f.ok(&["show", "2"])?;
        assert_eq!(second["status"], "ready");
        assert!(second["attempts"].as_array().unwrap().is_empty());
        let launches = assert_check_launch_records(&f, 3, false)?;
        assert!(
            launches[1] - launches[0] >= 1000,
            "1s backoff skipped: {launches:?}"
        );
        assert!(
            launches[2] - launches[1] >= 2000,
            "2s backoff skipped: {launches:?}"
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n"
        );
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
        t.send("q")?;
        t.finish()?;
        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        println!(
            "CHECK LAUNCH FAILURE RETAINED mouse={mouse}\n{}",
            reopened.screen("retained environment failure", |s| s.contains("FAILED")
                && s.contains("Permission denied"))?
        );
        assert_check_launch_records(&f, 3, false)?;
        reopened.send("q")?;
        reopened.finish()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}

#[test]
fn real_tui_check_launch_failure_pause_cancels_backoff_and_resumes_same_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("launch fixture ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let failures = if mouse { 2 } else { 1 };
        println!(
            "CHECK LAUNCH BEFORE PAUSE mouse={mouse}\n{}",
            t.screen("check launch backoff before pause", |s| s
                .contains("Check infrastructure failed")
                && s.contains(&format!("retry {failures}/2")))?
        );
        let active_before_pause = f.ok(&["show", "1"])?["attempts"][0]["active_ms"]
            .as_i64()
            .unwrap();
        thread::sleep(Duration::from_millis(250));
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "CHECK LAUNCH PAUSED mouse={mouse}\n{}",
            t.screen("paused launch retries", |s| s.contains("PAUSED")
                && s.contains("supervisor absent")
                && s.contains("r Resume"))?
        );
        let first = f.ok(&["show", "1"])?;
        let paused_active = first["attempts"][0]["active_ms"].as_i64().unwrap();
        assert!(
            paused_active >= active_before_pause + 200,
            "cancelled backoff time was lost: {active_before_pause} -> {paused_active}"
        );
        println!("CHECK LAUNCH CANCELLED ACTIVE TIME {active_before_pause} -> {paused_active}");
        assert_eq!(first["attempts_used"], 0);
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        let before = assert_check_launch_records(&f, failures, false)?;
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(assert_check_launch_records(&f, failures, false)?, before);
        assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"][0]["active_ms"],
            paused_active,
            "paused time must stay excluded"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let resumed_at = Instant::now();
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH RESUMED mouse={mouse}\n{}",
            t.screen("resumed launch recovery", |s| s.contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["status"], "succeeded");
        assert_eq!(first["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(first["attempts_used"], 1);
        let resumed_active = first["attempts"][0]["active_ms"].as_i64().unwrap();
        assert!(resumed_active >= paused_active);
        assert!(
            resumed_active - paused_active <= resumed_at.elapsed().as_millis() as i64 + 100,
            "resume counted paused time or the previous segment twice"
        );
        println!(
            "CHECK LAUNCH RESUMED ACTIVE TIME {paused_active} -> {resumed_active}; 2200ms pause excluded"
        );
        assert_check_launch_records(&f, failures, true)?;
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}

fn assert_deadline_retry_finished(f: &Fixture, first_attempt: &Value, limit: i64) -> Result {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        let retried = f.ok(&["show", "1"])?;
        if retried["attempts"].as_array().unwrap().len() == 2 && retried["status"] == "failed" {
            println!("CHECK LAUNCH DEADLINE RETRY TASK {retried}");
            assert_eq!(&retried["attempts"][0], first_attempt);
            assert!(
                retried["attempts"][1]["outcome"]
                    .as_str()
                    .unwrap()
                    .contains("configured task-wide limit")
            );
            let total = retried["attempts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a["active_ms"].as_i64().unwrap())
                .sum::<i64>();
            assert!(total >= limit);
            return Ok(());
        }
        assert!(
            Instant::now() < until,
            "deadline Retry did not finish: {retried}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn real_tui_check_launch_backoff_deadline_survives_restart_and_requires_explicit_increase() -> Result
{
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        let limit = 2500;
        f.ok(&["edit", "1", "--timeout-ms", "2500", "--attempt-limit", "3"])?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        t.screen("deadline launch fixture ready", |s| {
            s.contains("r Run queue")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        println!(
            "CHECK LAUNCH DEADLINE BACKOFF mouse={mouse}\n{}",
            t.screen("deadline during second launch backoff", |s| s
                .contains("Check infrastructure failed")
                && s.contains("retry 2/2 after 2s"))?
        );
        let before = f.ok(&["show", "1"])?;
        assert_eq!(before["attempts_used"], 0);
        assert!(before["attempts"][0]["active_ms"].as_i64().unwrap() < limit);
        let launches = assert_check_launch_records(&f, 2, false)?;
        println!(
            "CHECK LAUNCH DEADLINE EXPIRED mouse={mouse}\n{}",
            t.screen("configured deadline expired", |s| s.contains("FAILED")
                && s.contains("configured task-wide limit"))?
        );
        f.supervisor_stopped()?;
        let failed = f.ok(&["show", "1"])?;
        let first_attempt = failed["attempts"][0].clone();
        let recorded = first_attempt["active_ms"].as_i64().unwrap();
        println!("CHECK LAUNCH DEADLINE TASK {failed}");
        assert!(recorded >= limit, "backoff time not persisted: {recorded}");
        assert_eq!(
            failed["attempts_used"], 1,
            "task deadline is an agent failure"
        );
        assert_eq!(assert_check_launch_records(&f, 2, false)?, launches);
        assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n"
        );
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        f.launch_workers_reaped()?;
        t.send("q")?;
        t.finish()?;

        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let mut t =
            Terminal::open_project(binary.to_str().unwrap(), f.scratch.path(), &f.root, 120, 34)?;
        println!(
            "CHECK LAUNCH DEADLINE RESTART mouse={mouse}\n{}",
            t.screen("restart retains configured deadline failure", |s| s
                .contains("FAILED")
                && s.contains("t Retry")
                && s.contains("configured task-wide limit"))?
        );
        if mouse {
            t.click("t Retry")?;
        } else {
            t.send("t")?;
        }
        assert_deadline_retry_finished(&f, &first_attempt, limit)?;
        f.supervisor_stopped()?;
        println!(
            "CHECK LAUNCH DEADLINE RETRY BLOCKED mouse={mouse}\n{}",
            t.screen("retry still blocked by configured deadline", |s| s
                .contains("FAILED")
                && s.contains("configured task-wide limit"))?
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n",
            "expired retry must not start even a judge worker"
        );
        assert_eq!(assert_check_launch_records(&f, 2, false)?, launches);
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
        f.launch_workers_reaped()?;

        f.ok(&["edit", "1", "--timeout-ms", "30000"])?;
        t.screen("increased task deadline ready for retry", |s| {
            s.contains("t Retry")
        })?;
        if mouse {
            t.click("t Retry")?;
        } else {
            t.send("t")?;
        }
        println!(
            "CHECK LAUNCH DEADLINE INCREASE RECOVERED mouse={mouse}\n{}",
            t.screen("increased deadline permits judged recovery", |s| s
                .contains("Queue complete.")
                && s.contains("History 2"))?
        );
        let recovered = f.ok(&["show", "1"])?;
        assert_eq!(recovered["status"], "succeeded");
        assert_eq!(recovered["attempts"][0], first_attempt);
        assert_eq!(recovered["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nremediation\nremediation\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        let judge_prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
        assert!(judge_prompt.contains("You are the failure judge"));
        assert!(judge_prompt.contains("read-only"));
        println!("CHECK LAUNCH DEADLINE RECOVERY JUDGE\n{judge_prompt}");
        t.send("q")?;
        t.finish()?;
        f.supervisor_stopped()?;
        f.launch_workers_reaped()?;
    }
    Ok(())
}
