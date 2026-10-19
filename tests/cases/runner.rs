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
case "$prompt" in *"You are the failure judge"*)
 printf '%s' "$prompt" > "$HOME/prompt-judge"
 case '{mode}' in
  judge-revise) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"revise\",\"title\":\"Clarified task\",\"goal\":\"Do the clarified thing.\",\"criteria\":[\"Clarified check passes\"],\"reason\":\"The goal contradicted the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  judge-ask) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"ask\",\"question\":\"Keep or drop the legacy export?\",\"reason\":\"Both readings satisfy the criteria\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
  *) printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"judgment\":{{\"action\":\"repair\",\"plan\":\"Rerun the check after writing result.txt\",\"reason\":\"The check failed on a missing file\"}}}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0;;
 esac;;
esac
case '{mode}' in
 transient) if [ ! -f "$HOME/transport-retried" ]; then touch "$HOME/transport-retried"; printf '{{"type":"turn.failed","error":{{"message":"connection reset"}}}}\n'; exit 2; fi;;
 slow) sleep 3;;
 descendants) setsid sleep 30 & printf '%s' "$!" > descendant.pid; sleep 30;;
 rate) printf '{{"type":"turn.failed","error":{{"message":"rate_limit reached"}}}}\n'; exit 2;;
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
        assert!(!prompt.contains(failed_artifact.to_str().unwrap()));
        assert!(!prompt.contains("Check exited 7"));
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
        // Each repair attempt opens with the read-only failure judge, then the worker.
        4
    );
    let worker_prompt = fs::read_to_string(f.scratch.path().join("prompt-remediation"))?;
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
    f.provider("judge-revise")?;
    f.fail_first_check()?;
    f.add("Contradictory task")?;
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
    assert_eq!(states, ["failed", "interrupted", "succeeded"]);
    assert_eq!(task["attempts_used"], 2);
    let db = f.db()?;
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
        "test -f '{0}' || {{ : > '{0}'; head -c 1500000 /dev/zero | tr '\\0' x; echo; echo 'first check fails'; exit 7; }}",
        marker.display()
    );
    f.ok(&["check-set", "build", "--command", &command])?;
    f.add("Noisy failure")?;
    f.ok(&["run"])?;
    assert_eq!(f.ok(&["show", "1"])?["lifecycle"], "succeeded");
    let prompt = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
    assert!(prompt.contains("You are the failure judge"));
    assert!(
        prompt.contains("first check fails"),
        "the failure itself survives trimming"
    );
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
    f.provider("judge-ask")?;
    f.fail_first_check()?;
    f.add("Ambiguous task")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let task = f.ok(&["show", "1"])?;
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 2);
    assert_eq!(attempts[1]["question"], "Keep or drop the legacy export?");
    assert_eq!(task["attempts_used"], 1);
    f.ok(&["answer", "1", "--text", "Human answer: keep it"])?;
    f.ok(&["resume"])?;
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
        "execution\nreview\nreview\n"
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
fn real_tui_rate_wait_and_mouse_pause_stop_automatic_retry() -> Result {
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
        "RATE WAIT\n{}",
        t.screen("waiting", |s| s.contains("WAITING")
            && s.contains("Resumes in"))?
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
        assert_eq!(first["attempts"].as_array().unwrap().len(), 2);
        assert_eq!(first["attempts_used"], 2);
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
            println!(
                "OWNER GATE mouse={mouse} human={human}\n{}",
                t.screen("owner gate", |s| s.contains("Owner gate"))?
            );
            if mouse {
                t.click(if human { "y Acknowledge" } else { "v Approve" })?;
            } else {
                t.send(if human { "y" } else { "v" })?;
            }
            println!(
                "GATE CLEARED mouse={mouse} human={human}\n{}",
                t.screen("automatic continuation", |s| s.contains("Queue complete.")
                    && s.contains("History 2"))?
            );
            assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
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
