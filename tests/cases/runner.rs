use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{scratch::Scratch, storage::Database};
use serde_json::Value;
use std::{
    fs,
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
prompt=$(cat)
printf '%s' "$prompt" > "$HOME/prompt-$model"
[ -n "$session" ] || session="session-$model-$$"
printf '{{"type":"thread.started","thread_id":"%s"}}\n' "$session"
printf '%s\n' "$model" >> "$HOME/provider-invocations"
mkdir -p "$TMPDIR/generated/deep"
printf debris > "$TMPDIR/generated/deep/file"
case '{mode}' in
 slow) sleep 3;;
 descendants) setsid sleep 30 & printf '%s' "$!" > descendant.pid; sleep 30;;
 rate) printf '{{"type":"turn.failed","error":{{"message":"rate_limit reached"}}}}\n'; exit 2;;
 malformed) printf '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}\n'; exit 0;;
 fail) printf '{{"type":"turn.failed","error":{{"message":"transport lost"}}}}\n'; exit 2;;
 question) case "$prompt" in *"Human answer"*) :;; *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"blocked\",\"summary\":\"Need direction\",\"question\":\"Which content?\",\"criteria\":[]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;; esac;;
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
        assert_eq!(artifacts.len(), if exit_code == 0 { 1 } else { 3 });
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
fn reviewers_receive_durable_supervisor_evidence_not_stale_attempt_claims() -> Result {
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
        "SELECT relative_path,size_bytes FROM artifacts WHERE attempt_id=2",
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
        "SELECT relative_path FROM artifacts WHERE attempt_id=1",
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
    assert!(remediation.contains(failed_artifact.to_str().unwrap()));
    assert!(remediation.contains("Check exited 7"));
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
        assert!(prompt.contains(artifact.to_str().unwrap()));
        assert!(prompt.contains("\"exit_code\":0"));
        assert!(prompt.contains("read-only"));
        assert!(!prompt.contains("Previous attempt:"));
        assert!(!prompt.contains(failed_artifact.to_str().unwrap()));
        assert!(!prompt.contains("Check exited 7"));
    }
    let execution = fs::read_to_string(f.scratch.path().join("prompt-execution"))?;
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
        "SELECT relative_path FROM artifacts WHERE attempt_id=2",
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
    let result = f.ok(&["run", "--queue"])?;
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
    assert_eq!(task["attempts"].as_array().unwrap().len(), 3);
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
        2
    );
    assert!(f.root.join("result.txt").exists());
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
fn questions_require_answer_and_reuse_attempt() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
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
fn real_tui_runner_mouse_pause_keyboard_resume_and_live_history() -> Result {
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Observed execution")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    t.screen("ready", |s| s.contains("Observed execution"))?;
    t.click("r Run/resume")?;
    println!(
        "RUNNING\n{}",
        t.screen("running", |s| s.contains("RUNNING")
            && s.contains("new session"))?
    );
    t.click("p Pause")?;
    println!("PAUSED\n{}", t.screen("paused", |s| s.contains("PAUSED"))?);
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
        assert_eq!(task["attempts_used"], 0);
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
    f.ok(&["run"])?;
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
fn real_tui_question_answer_mouse_and_keyboard_resume() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Ask owner")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
        t.screen("current", |s| s.contains("Ask owner"))?;
        if mouse {
            t.click("r Run/resume")?;
        } else {
            t.send("r")?;
        }
        let waiting = t.screen("question", |s| s.contains("QUESTION"))?;
        println!("QUESTION CONTROLS (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("w Answer"));
        assert!(!waiting.contains("r Run/resume"));
        let waiting_task = f.ok(&["show", "1"])?;
        let attempts = waiting_task["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 1);
        let attempt_id = attempts[0]["id"].clone();
        let invocations = f.scratch.path().join("provider-invocations");
        let calls_before = fs::read_to_string(&invocations)?;
        assert_eq!(calls_before, "execution\n");
        let log = f.scratch.path().join("state/ktask-dev/runner-1.log");
        let log_before = fs::read(&log)?;
        t.send("r")?;
        let still_waiting = t.screen("premature Run rejected", |s| {
            s.contains("Unavailable here") && s.contains("QUESTION")
        })?;
        println!("PREMATURE RUN REJECTED (mouse={mouse})\n{still_waiting}");
        assert!(still_waiting.contains("w Answer"));
        assert!(!still_waiting.contains("r Run/resume"));
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
        let answered = t.screen("answer controls", |s| {
            s.contains("Saved to SQLite") && s.contains("r Run/resume")
        })?;
        println!("ANSWERED CONTROLS (mouse={mouse})\n{answered}");
        assert!(!answered.contains("w Answer"));
        t.send("w")?;
        t.screen("answered question cannot reopen", |s| {
            s.contains("Unavailable here") && !s.contains("Answer AI question")
        })?;
        if mouse {
            t.click("r Run/resume")?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("complete", |s| s.contains("Queue complete."))?;
        println!("RESUMED SAME ATTEMPT (mouse={mouse})\n{complete}");
        let completed = f.ok(&["show", "1"])?;
        assert_eq!(completed["status"], "succeeded");
        assert_eq!(completed["attempts"][0]["id"], attempt_id);
        assert_eq!(completed["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            fs::read_to_string(&invocations)?,
            "execution\nexecution\nreview\nvalidation\n"
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
fn real_tui_rate_wait_and_mouse_pause_stop_automatic_retry() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Rate limited")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("current", |s| s.contains("Rate limited"))?;
    t.send("4e")?;
    t.screen("preferences", |s| s.contains("Project preferences"))?;
    t.send("\t\t \x13")?;
    t.screen("queue continuation selected", |s| {
        s.contains("Run mode: Continue queue")
    })?;
    t.send("1")?;
    t.send("r")?;
    println!(
        "RATE WAIT\n{}",
        t.screen("waiting", |s| s.contains("WAITING")
            && s.contains("resume in"))?
    );
    t.click("p Pause")?;
    t.screen("scheduler stopped", |s| {
        s.contains("Automatic continuation stopped")
    })?;
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
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
            if queue {
                if mouse {
                    t.click("< Run one >")?;
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
                t.click("r Run")?;
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
