use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::storage::Database;
use serde_json::Value;
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");
#[path = "providers.rs"]
mod providers;
struct Fixture {
    scratch: support::Sandbox,
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
    /// Another Git project, set up like the first, in the same ktask instance.
    fn second_project(&self, name: &str) -> Result<PathBuf> {
        let root = self.scratch.path().join(name);
        fs::create_dir(&root)?;
        fs::write(root.join("README.md"), "Second project\n")?;
        let git = |args: &[&str]| self.git_in(&root, args).map(drop);
        git(&["init", "-q"])?;
        git(&["add", "README.md"])?;
        git(&[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-qm",
            "baseline",
        ])?;
        self.ok_in(
            &root,
            &["check-set", "build", "--command", "test -f result.txt"],
        )?;
        for role in ["execution", "review", "validation", "remediation"] {
            let args = [
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                role,
            ];
            self.ok_in(&root, &args)?;
        }
        Ok(root)
    }
    fn git(&self, args: &[&str]) -> Result<String> {
        self.git_in(&self.root, args)
    }
    fn git_in(&self, project: &Path, args: &[&str]) -> Result<String> {
        let o = Command::new("git")
            .arg("-C")
            .arg(project)
            .args(args)
            .output()?;
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        Ok(String::from_utf8(o.stdout)?)
    }
    fn process(&self) -> Command {
        self.process_in(&self.root)
    }
    /// The same ktask instance (state, cache, config) used from another project.
    fn process_in(&self, project: &Path) -> Command {
        let mut c = Command::new(BIN);
        c.current_dir(project)
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
        self.ok_in(&self.root, args)
    }
    fn ok_in(&self, project: &Path, args: &[&str]) -> Result<Value> {
        let o = self.process_in(project).arg("--json").args(args).output()?;
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
        let mut last = Value::Null;
        support::eventually(&format!("task 1 {state}"), || {
            last = self.ok(&["show", "1"])?;
            Ok((last["status"] == state).then_some(()))
        })
        .map_err(|error| format!("{error}; last: {last}").into())
    }
    fn provider(&self, mode: &str) -> Result {
        let script = format!(
            r##"#!/bin/sh
model=unknown
session=""
network=off
while [ "$#" -gt 0 ]; do
 case "$1" in -m) shift; model="$1";; resume) shift; session="$1";; sandbox_workspace_write.network_access=true) network=on;; esac
 shift
done
printf '%s %s\n' "$model" "$network" >> "$HOME/provider-network"
if [ '{mode}' = check-launch-failure ]; then
 printf '%s %s\n' "$$" "$PPID" >> "$HOME/launch-fixture-pids"
fi
prompt=$(cat)
printf '%s' "$prompt" > "$HOME/prompt-$model"
if [ "$model" = review ] && [ ! -f "$HOME/first-review-prompt" ]; then printf '%s' "$prompt" > "$HOME/first-review-prompt"; fi
if [ "$model" = validation ] && [ ! -f "$HOME/first-validation-prompt" ]; then printf '%s' "$prompt" > "$HOME/first-validation-prompt"; fi
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
if [ '{mode}' = noisy-review-once ] && [ "$model" = review ]; then
 i=0
 while [ "$i" -lt 2100 ]; do
  printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"late output","aggregated_output":"late output %s","exit_code":0}}}}\n' "$i"
  i=$((i + 1))
 done
fi
case '{mode}' in
 follow-up)
  if [ "$model" = execution ]; then
   export XDG_STATE_HOME="$TMPDIR/no-supervisor-state"
   callback=$(printf '%s\n' "$prompt" | sed -n 's/.*exact executable: \(.*\) --title TITLE.*/\1/p')
   [ -n "$callback" ] || exit 12
   eval "$callback --title 'Discovered follow-up' --goal 'Investigate discovered work' --criterion 'Review the discovery'" > "$HOME/follow-up-receipt" || exit 13
   eval "$callback --title 'Second discovery' --goal 'Check second issue' --criterion 'Review second issue'" > "$HOME/second-follow-up-receipt" || exit 14
   prefix=$(printf '%s' "$callback" | sed 's/ follow-up$//')
   for forbidden in 'run --one' 'approve 1' 'ack 1' 'resolve 1 --outcome failed-skipped --reason denied' 'delete 1' 'edit 1 --title denied'; do
    eval "$prefix $forbidden" > "$HOME/forbidden-out" 2>&1
    printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   done
   no_token=$(printf '%s' "$prefix" | sed "s/ --worker-token '[^']*'//")
   eval "$no_token run --one" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   bad=$(printf '%s' "$callback" | sed "s/--worker-token '[^']*'/--worker-token 'wrong'/")
   eval "$bad --title 'Denied' --goal 'No access' --criterion 'No change'" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   env -u KTASK_WORKER_CALLBACK -u KTASK_CALLBACK_DIR sh -c "eval \"$no_token run --one\"" > "$HOME/forbidden-out" 2>&1
   printf '%s\n' "$?" >> "$HOME/forbidden-statuses"
   touch "$HOME/follow-up-created"
   until [ -f "$HOME/release-follow-up" ]; do sleep 0.02; done
  fi;;
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
 hostile-judge) if [ "$model" = review ]; then
  log="$HOME/judge-writes"
  printf tampered > result.txt; printf 'project %s\n' "$?" >> "$log"
  git show HEAD --output=README.md >/dev/null 2>&1; printf 'git-output %s\n' "$?" >> "$log"
  for f in "$XDG_STATE_HOME"/ktask-dev/evidence/*/*.log; do printf tampered >> "$f"; printf 'evidence %s\n' "$?" >> "$log"; done
  printf '{{}}' > "$XDG_CONFIG_HOME/ktask-dev/injected.json"; printf 'config %s\n' "$?" >> "$log"
  if [ -w "$(cat "$HOME/supervisor-path")" ]; then echo 'supervisor 0' >> "$log"; else echo 'supervisor 1' >> "$log"; fi
  printf scratch > "$TMPDIR/judge-notes"; printf 'scratch %s\n' "$?" >> "$log"
 fi;;
 review-once|noisy-review-once) if [ "$model" = review ] && [ ! -f "$HOME/review-failed" ]; then touch "$HOME/review-failed"; printf '%s\n' '{{"type":"item.completed","item":{{"type":"agent_message","text":"{{\"outcome\":\"failed\",\"summary\":\"result.txt:1: missing edge-case handling; repair and prove the boundary.\",\"question\":null,\"criteria\":[{{\"ordinal\":1,\"passed\":false,\"evidence\":\"Boundary case fails\"}}]}}"}}}}' '{{"type":"turn.completed","usage":{{"input_tokens":100,"cached_input_tokens":40,"output_tokens":10}}}}'; exit 0; fi;;
 transient) if [ ! -f "$HOME/transport-retried" ]; then touch "$HOME/transport-retried"; printf '{{"type":"turn.failed","error":{{"message":"connection reset"}}}}\n'; exit 2; fi;;
 fail-first-attempt) if [ "$(wc -l < "$HOME/provider-invocations")" -le 3 ]; then printf '{{"type":"turn.completed","usage":{{}}}}\n'; exit 0; fi;;
 slow-repair) if [ "$model" = remediation ]; then touch "$HOME/repair-started"; sleep 30; fi;;
 slow-review) if [ "$model" = review ]; then touch "$HOME/review-started"; sleep 30; fi;;
 slow) sleep 3;;
 usage-live) if [ "$model" = execution ]; then sleep 30; fi;;
 overlap) if [ "$model" = execution ]; then
  pwd -P >> "$HOME/overlap-started"
  i=0; while [ ! -f "$HOME/release" ] && [ "$i" -lt 6000 ]; do sleep 0.01; i=$((i + 1)); done
 fi;;
 chatty) if [ "$model" = execution ]; then
  i=0
  while [ "$i" -lt 1500 ]; do
   printf '{{"type":"item.completed","item":{{"type":"command_execution","command":"build step","aggregated_output":"compiled unit %s","exit_code":0}}}}\n' "$i"
   i=$((i + 1))
  done
 fi;;
 descendants) setsid sleep 30 & printf '%s' "$!" > descendant.staged; mv descendant.staged descendant.pid; sleep 30;;
 rate) printf '{{"type":"turn.failed","error":{{"message":"rate_limit reached"}}}}\n'; exit 2;;
 usage) printf '%s\n' '{{"type":"error","message":"You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 26th, 2099 11:32 PM."}}' '{{"type":"turn.failed","error":{{"message":"You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 26th, 2099 11:32 PM."}}}}'; exit 1;;
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
        support::write_executable(&self.scratch.path().join("bin/codex"), script)?;
        Ok(())
    }
    /// Three failed attempts: the supervisor repairs automatically and stops only after the
    /// provider fails three attempts in a row.
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
        support::copy_executable(BIN, &binary)?;
        self.provider("check-launch-failure")?;
        self.add("Recover check guardian launch")?;
        self.ok(&["edit", "1", "--attempt-limit", "1"])?;
        self.add("Must wait for verified recovery")?;
        Ok(binary)
    }
    /// The guardian outlives its supervisor until it has stopped and reaped every worker
    /// process; then it empties and releases the worker lease.
    fn guardian_finished(&self) -> Result {
        let path = self.root.join(".git/ktask-worker.lock");
        support::eventually("guardian finished its cleanup", || {
            let lease = OpenOptions::new().read(true).write(true).open(&path)?;
            Ok((lease.try_lock().is_ok() && lease.metadata()?.len() == 0).then_some(()))
        })
    }
    fn launch_workers_reaped(&self) -> Result {
        self.guardian_finished()?;
        for pid in
            fs::read_to_string(self.scratch.path().join("launch-fixture-pids"))?.split_whitespace()
        {
            assert!(
                !PathBuf::from(format!("/proc/{pid}")).exists(),
                "fixture provider/guardian {pid} survived cleanup"
            );
        }
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
        support::eventually("supervisor lease released", || match lease.try_lock() {
            Ok(()) => Ok(Some(())),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        })?;
        println!("CHECK LAUNCH CLEANUP: supervisor lease released");
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
    /// The /proc stat fields after the command name (state, ppid, pgrp, session, tty_nr,
    /// ...) of this fixture's detached supervisor.
    fn supervisor_stat(&self) -> Result<Vec<String>> {
        let project = self.root.to_string_lossy().into_owned();
        for entry in fs::read_dir("/proc")?.flatten() {
            let cmdline = fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            if cmdline.contains(&project) && cmdline.contains(" run --notify") {
                let stat = fs::read_to_string(entry.path().join("stat"))?;
                let fields = stat[stat.rfind(')').ok_or("bad stat")? + 2..].split(' ');
                return Ok(fields.map(String::from).collect());
            }
        }
        Err("no detached supervisor for this fixture".into())
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
/// Whether the screen shows the named view's own content, not only its tab label.
fn shows_view(screen: &str, name: &str) -> bool {
    match name {
        "Config" => screen.contains("Storage: SQLite output"),
        "About" => screen.contains("A visual workspace for supervising"),
        _ => screen
            .lines()
            .any(|line| line.starts_with(&format!(" {name} · ")) && line.contains(" shown / ")),
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
fn worker_callback_files_only_linked_drafts_and_terminal_shows_both_links() -> Result {
    let f = Fixture::new()?;
    f.provider("follow-up")?;
    f.add("Origin task")?;
    f.add("Later task")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    // Release and reap the held provider even if a PTY assertion unwinds.
    let result = std::panic::catch_unwind(|| -> Result {
        let marker = f.scratch.path().join("follow-up-created");
        support::eventually("deterministic provider creates its draft", || {
            Ok(marker.exists().then_some(()))
        })?;
        let queue = f.ok(&["list"])?;
        let rows = queue["tasks"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|row| row["id"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [1, 3, 4, 2]
        );
        assert_eq!(rows[0]["status"], "running");
        assert_eq!(rows[1]["status"], "new");
        assert_eq!(rows[1]["origin_task_id"], 1);
        let attempt = rows[1]["origin_attempt_id"].as_i64().unwrap();
        assert!(attempt > 0);
        let draft = f.ok(&["show", "3"])?;
        assert_eq!(draft["revisions"][0]["lifecycle"], "draft");
        assert_eq!(
            draft["revisions"][0]["actor"],
            format!("ai: worker attempt #{attempt}")
        );
        assert_eq!(f.ok(&["show", "1"])?["follow_ups"][0]["id"], 3);
        assert_eq!(f.ok(&["show", "1"])?["follow_ups"][1]["id"], 4);
        let statuses = fs::read_to_string(f.scratch.path().join("forbidden-statuses"))?;
        let statuses = statuses.lines().collect::<Vec<_>>();
        assert_eq!(&statuses[..7], &["2"; 7]);
        assert_eq!(statuses[7], "4");
        assert_ne!(statuses[8], "0");
        let invalid = f
            .process()
            .args([
                "--worker-token",
                "wrong",
                "follow-up",
                "--title",
                "Denied",
                "--goal",
                "No access",
                "--criterion",
                "No change",
            ])
            .output()?;
        assert_eq!(invalid.status.code(), Some(4));
        let unchanged = f.ok(&["list"])?;
        assert_eq!(unchanged["tasks"], queue["tasks"]);
        for (cols, rows, mouse) in [(105, 30, false), (70, 22, true)] {
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            println!(
                "FOLLOW-UP RUNNING CURRENT {cols}x{rows}\n{}",
                t.screen("running source with both follow-ups", |s| {
                    s.contains("Current #1 · RUNNING") && s.contains("Follow-ups: #3, #4")
                })?
            );
            if mouse {
                t.click("Enter Details")?;
            } else {
                t.send("\r")?;
            }
            t.screen("running source details opened", |s| {
                s.contains("Task details / attempts")
            })?;
            t.send("\x1b[F")?;
            println!(
                "FOLLOW-UP RUNNING SOURCE DETAIL mouse={mouse} {cols}x{rows}\n{}",
                t.screen("running source details link both drafts", |s| {
                    s.contains("Follow-up #3: Discovered follow-up")
                        && s.contains("Follow-up #4: Second discovery")
                })?
            );
            t.send("\x1b")?;
            t.screen("running source details closed", |s| {
                s.contains("Follow-ups: #3, #4") && !s.contains("Task details / attempts")
            })?;
            t.send("q")?;
            t.finish()?;
        }
        Ok(())
    });
    fs::write(f.scratch.path().join("release-follow-up"), "go")?;
    let status = runner.0.wait()?;
    match result {
        Ok(result) => result?,
        Err(panic) => std::panic::resume_unwind(panic),
    }
    assert!(status.success(), "runner exited {status}");
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let stale_token: String = f.db()?.connection().query_row(
        "SELECT execution_token FROM attempts WHERE task_id=1",
        [],
        |row| row.get(0),
    )?;
    let stale = f
        .process()
        .args([
            "--worker-token",
            &stale_token,
            "follow-up",
            "--title",
            "Denied",
            "--goal",
            "No access",
            "--criterion",
            "No change",
        ])
        .output()?;
    assert_eq!(stale.status.code(), Some(4));
    assert_eq!(f.ok(&["current"])?["id"], 3);
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 105, 30)?;
    println!(
        "FOLLOW-UP CURRENT\n{}",
        t.screen("draft current", |s| s.contains("Current #3 · NEW"))?
    );
    t.send("2")?;
    println!(
        "FOLLOW-UP QUEUE KEYBOARD\n{}",
        t.screen("linked queue", |s| s
            .contains("Follow-up from task #1, attempt"))?
    );
    t.send("\r")?;
    println!(
        "FOLLOW-UP DETAIL KEYBOARD\n{}",
        t.screen("linked detail", |s| {
            s.contains("Task details / attempts") && s.contains("Follow-up from task #1, attempt")
        })?
    );
    t.send("\x1b")?;
    t.screen("detail closed", |s| {
        s.contains("3 History") && !s.contains("Task details / attempts")
    })?;
    t.click("3 History")?;
    println!(
        "FOLLOW-UP HISTORY MOUSE\n{}",
        t.screen("source link", |s| s
            .contains("Follow-up #3: Discovered follow-up"))?
    );
    t.click("Enter Details")?;
    t.send("\x1b[F")?;
    println!(
        "FOLLOW-UP SOURCE DETAIL MOUSE\n{}",
        t.screen("source detail", |s| {
            s.contains("Task details / attempts")
                && s.contains("Follow-up #3: Discovered follow-up")
        })?
    );
    t.send("\x1b")?;
    t.send("q")?;
    t.kill()?;
    f.no_scratch()?;
    Ok(())
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
            assert_eq!(task["attempts"].as_array().unwrap().len(), 1, "{task}");
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
        support::eventually("tenth review started", || {
            Ok(f.scratch
                .path()
                .join("tenth-review-started")
                .exists()
                .then_some(()))
        })?;
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
            t.screen("paused", |s| {
                s.contains("PAUSED · supervisor absent") && s.contains("r Resume")
            })?
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
                t.click("u Readiness")?;
            } else {
                t.send("u")?;
            }
            println!(
                "MODEL SEPARATION READINESS {role} mouse={mouse}\n{}",
                t.screen("readiness names the rule", |s| s
                    .contains("Must be stronger than the execution model"))?
            );
            t.send("\x1b")?;
            t.screen("ready again", |s| {
                s.contains("r Run queue") && !s.contains("Execution readiness")
            })?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            println!(
                "MODEL SEPARATION {role} mouse={mouse}\n{}",
                // The status line, not Activity, which keeps the earlier window's refusal.
                t.screen("model configuration required", |s| s.lines().any(|line| {
                    line.starts_with("command conflict: Configure a stronger")
                }))?
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
            t.screen("full decision", |s| s.contains("Task details / attempts")
                && (s.contains("Required boundary behavior")
                    || s.contains("Critical findings remain")))?
        );
        t.send("\x1b")?;
        t.screen("details closed", |s| {
            s.contains("FAILED") && !s.contains("Task details / attempts")
        })?;
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
    support::eventually("review started", || {
        Ok(f.scratch
            .path()
            .join("review-started")
            .exists()
            .then_some(()))
    })?;
    println!(
        "PROVEN SOURCE BEFORE PAUSE\n{}",
        t.screen("review", |s| s.contains("Workflow · Review"))?
    );
    assert!(
        f.git(&["diff", "--cached", "--name-only"])?.is_empty(),
        "fingerprinting must preserve the user's index"
    );
    t.click("p Pause")?;
    t.screen("paused", |s| {
        s.contains("PAUSED · supervisor absent") && s.contains("r Resume")
    })?;
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
fn real_binary_records_check_counts_and_rejects_regression_at_validation() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/count-baseline\" ]; then touch \"$HOME/count-baseline\"; printf 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; elif [ ! -f \"$HOME/count-regressed\" ]; then touch \"$HOME/count-regressed\"; printf 'test result: ok. 2 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n'; printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; else printf 'test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; printf 'test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; fi; test -f result.txt",
    ])?;
    f.add("Baseline counts")?;
    f.ok(&["run", "--one"])?;
    let first = f.ok(&["show", "1"])?;
    let baseline = first["attempts"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|step| step["role"] == "check")
        .unwrap();
    assert_eq!(
        baseline["counts"],
        serde_json::json!({"passed": 5, "failed": 0, "ignored": 0})
    );
    fs::remove_file(f.scratch.path().join("first-review-prompt"))?;
    fs::remove_file(f.scratch.path().join("first-validation-prompt"))?;

    f.add("Regressed counts")?;
    f.ok(&["run", "--one"])?;
    let second = f.ok(&["show", "2"])?;
    assert_eq!(second["status"], "succeeded");
    let steps = second["attempts"][0]["steps"].as_array().unwrap();
    let check_counts = steps
        .iter()
        .filter(|step| step["role"] == "check")
        .map(|step| step["counts"].clone())
        .collect::<Vec<_>>();
    assert_eq!(
        check_counts[0],
        serde_json::json!({"passed": 4, "failed": 0, "ignored": 1})
    );
    assert_eq!(
        check_counts[1],
        serde_json::json!({"passed": 5, "failed": 0, "ignored": 0})
    );
    let rejected = steps
        .iter()
        .find(|step| step["role"] == "validation" && step["state"] == "changes_requested")
        .unwrap();
    let report = rejected["report"].as_str().unwrap();
    assert!(
        report.contains("passed tests dropped from 5 at the previous checkpoint to 4"),
        "{report}"
    );
    assert!(
        report.contains("ignored tests rose from 0 at the previous checkpoint to 1"),
        "{report}"
    );
    println!("CHECK COUNT REGRESSION REJECTED\n{report}");
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("first-{role}-prompt")))?;
        assert!(
            prompt.contains("current_counts"),
            "{role} prompt missing current counts"
        );
        assert!(
            prompt.contains("previous_checkpoint_counts"),
            "{role} prompt missing previous counts"
        );
        assert!(
            prompt.contains("\"passed\":4"),
            "{role} prompt missing regressed passed count"
        );
        assert!(
            prompt.contains("\"ignored\":1"),
            "{role} prompt missing ignored count"
        );
    }
    f.no_scratch()?;
    Ok(())
}

#[test]
fn failing_tests_block_acceptance_even_when_the_check_exits_zero() -> Result {
    let f = Fixture::new()?;
    // A wrapper that swallows the test runner's status: the first run fails one test yet
    // exits 0. Nothing earlier was checkpointed, and every fixture judge would accept.
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/masked-once\" ]; then touch \"$HOME/masked-once\"; printf 'test result: FAILED. 3 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\\n'; else printf 'test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; fi; true",
    ])?;
    f.add("Masked test failure")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    let steps = task["attempts"][0]["steps"].as_array().unwrap();
    let roles: Vec<_> = steps.iter().map(|s| s["role"].as_str().unwrap()).collect();
    assert_eq!(
        roles,
        [
            "execution",
            "check",
            "remediation",
            "remediation",
            "check",
            "review",
            "validation"
        ],
        "the failing summary sends the attempt back for repair before any review"
    );
    let checks: Vec<_> = steps.iter().filter(|s| s["role"] == "check").collect();
    assert_eq!(checks[0]["state"], "failed");
    assert_eq!(
        checks[0]["counts"],
        serde_json::json!({"passed": 3, "failed": 1, "ignored": 0})
    );
    let report = checks[0]["report"].as_str().unwrap();
    assert!(
        report.contains("exited 0 but its test summary reports 1 failed"),
        "{report}"
    );
    assert_eq!(checks[1]["state"], "succeeded");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn read_only_judges_cannot_change_source_evidence_configuration_or_supervisor() -> Result {
    let f = Fixture::new()?;
    fs::write(
        f.scratch.path().join("supervisor-path"),
        fs::canonicalize(BIN)?.to_string_lossy().as_bytes(),
    )?;
    f.provider("hostile-judge")?;
    f.add("Judge tries to write")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let writes = fs::read_to_string(f.scratch.path().join("judge-writes"))?;
    let refused: Vec<_> = writes
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(target, status)| (target, status != "0"))
        .collect();
    assert!(
        refused.iter().any(|(target, _)| *target == "evidence"),
        "{writes}"
    );
    for (target, was_refused) in refused {
        assert_eq!(was_refused, target != "scratch", "{target}: {writes}");
    }
    // Nothing the judge tried reached the project, its records or its configuration.
    assert_eq!(fs::read_to_string(f.root.join("result.txt"))?, "verified\n");
    assert_eq!(
        fs::read_to_string(f.root.join("README.md"))?,
        "Initial project\n"
    );
    let evidence = f.scratch.path().join("state/ktask-dev/evidence");
    for attempt in fs::read_dir(evidence)? {
        for log in fs::read_dir(attempt?.path())? {
            assert!(!fs::read_to_string(log?.path())?.contains("tampered"));
        }
    }
    assert!(
        !f.scratch
            .path()
            .join("config/ktask-dev/injected.json")
            .exists()
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn missing_counts_after_a_counted_checkpoint_request_validation_repair() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/count-baseline\" ]; then touch \"$HOME/count-baseline\"; printf 'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; elif [ ! -f \"$HOME/count-missing\" ]; then touch \"$HOME/count-missing\"; printf 'check succeeded without a test summary\\n'; else printf 'test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'; fi; test -f result.txt",
    ])?;
    f.add("Counted checkpoint")?;
    f.ok(&["run", "--one"])?;
    f.add("Missing count summary")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "2"])?;
    let steps = task["attempts"][0]["steps"].as_array().unwrap();
    let first_check = steps.iter().find(|step| step["role"] == "check").unwrap();
    assert!(first_check["counts"].is_null());
    let validation = steps
        .iter()
        .find(|step| step["role"] == "validation" && step["state"] == "changes_requested")
        .unwrap();
    assert!(
        validation["report"]
            .as_str()
            .unwrap()
            .contains("reported no test counts")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn plain_show_retains_ordered_check_counts_after_logs_are_truncated() -> Result {
    let f = Fixture::new()?;
    f.provider("noisy-review-once")?;
    f.ok(&[
        "check-set", "build", "--command",
        "if [ ! -f \"$HOME/count-first\" ]; then touch \"$HOME/count-first\"; passed=3; else passed=5; fi; printf 'test result: ok. %s passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n' \"$passed\"; test -f result.txt",
    ])?;
    f.ok(&[
        "check-set",
        "zero\t\u{1b}",
        "--command",
        "printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\\n'",
    ])?;
    f.ok(&["check-set", "uncounted", "--command", "true"])?;
    f.add("Persist check counts beyond log retention")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    let attempt = &task["attempts"][0];
    let checks = attempt["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["role"] == "check")
        .map(|step| (step["check_name"].clone(), step["counts"].clone()))
        .collect::<Vec<_>>();
    let expected_counts = serde_json::json!([
        ["build", {"passed": 3, "failed": 0, "ignored": 1}],
        ["zero\t\u{1b}", {"passed": 0, "failed": 0, "ignored": 0}],
        ["uncounted", null],
        ["build", {"passed": 5, "failed": 0, "ignored": 1}],
        ["zero\t\u{1b}", {"passed": 0, "failed": 0, "ignored": 0}],
        ["uncounted", null]
    ]);
    assert_eq!(serde_json::to_value(checks)?, expected_counts);
    let logs = attempt["logs"].as_array().unwrap();
    assert_eq!(logs.len(), 2000);
    assert!(logs.iter().all(|line| {
        let line = line.as_str().unwrap();
        !line.contains("Check ") && !line.contains("test result:")
    }));
    let shown = f.process().args(["show", "1"]).output()?;
    assert!(shown.status.success());
    let text = String::from_utf8(shown.stdout)?;
    let summaries = text
        .lines()
        .filter_map(|line| line.strip_prefix("  Check "))
        .collect::<Vec<_>>();
    assert_eq!(
        summaries,
        [
            "build: 3 passed, 0 failed, 1 ignored",
            "zero\\t\\u{1b}: 0 passed, 0 failed, 0 ignored",
            "uncounted: test counts unavailable",
            "build: 5 passed, 0 failed, 1 ignored",
            "zero\\t\\u{1b}: 0 passed, 0 failed, 0 ignored",
            "uncounted: test counts unavailable",
        ]
    );
    assert!(!text.contains('\u{1b}'));
    println!(
        "PERSISTED CHECK COUNTS: JSON retained {} logs with no check summaries; plain show:\n{}",
        logs.len(),
        summaries.join("\n")
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn current_screen_shows_recorded_check_counts_in_real_terminal() -> Result {
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.ok(&["check-set", "build", "--command", "printf 'test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out\\n'; test -f result.txt"])?;
    f.add("Visible check counts")?;
    let mut runner = f.process().args(["run", "--one"]).spawn()?;
    support::eventually("review starts after the count-bearing check", || {
        Ok(f.scratch
            .path()
            .join("review-started")
            .exists()
            .then_some(()))
    })?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
    let screen = t.screen("current check counts", |s| {
        s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("CURRENT COUNTS\n{screen}");
    f.ok(&["pause", "1"])?;
    assert!(runner.wait()?.success());
    t.screen("paused current", |s| s.contains("Current #1 · PAUSED"))?;
    t.send("o")?;
    let report = t.screen("paused current count report", |s| {
        s.contains("Report · attempt 1") && s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("PAUSED COUNT REPORT\n{report}");
    t.click("o Log")?;
    t.screen("mouse switches counts report to log", |s| {
        s.contains("Log ·") && s.contains("attempt 1") && !s.contains("Report · attempt 1")
    })?;
    t.click("o Report")?;
    let report = t.screen("mouse reopens paused current count report", |s| {
        s.contains("Report · attempt 1") && s.contains("Check build: 3 passed, 0 failed, 1 ignored")
    })?;
    println!("PAUSED COUNT REPORT MOUSE\n{report}");
    t.send("q")?;
    t.finish()?;
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
    assert_eq!(attempts.len(), 4);
    assert_eq!(attempts[3]["question"], "Keep or drop the legacy export?");
    assert_eq!(task["attempts_used"], 0);
    f.ok(&["answer", "1", "--text", "Human answer: keep it"])?;
    f.state("succeeded")?;
    assert_eq!(
        f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
        4
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
        support::eventually("repair worker started", || {
            Ok(f.scratch
                .path()
                .join("repair-started")
                .exists()
                .then_some(()))
        })?;
        let running = t.screen("repair running", |s| {
            s.contains("RUNNING · supervisor alive")
                && s.contains("p Pause")
                && s.contains("Attempt 1")
                && s.contains("Step: Remediation")
        })?;
        println!("REPAIR RUNNING mouse={mouse}\n{running}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "REPAIR PAUSED mouse={mouse}\n{}",
            t.screen("repair paused", |s| s.contains("[r Resume]")
                && s.contains("PAUSED · supervisor absent"))?
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
fn cli_show_exposes_attempt_wait_and_provider_elapsed_time() -> Result {
    let f = Fixture::new()?;
    f.add("Timing JSON")?;
    f.ok(&["run"])?;
    let shown = f.ok(&["show", "1"])?;
    let attempt = &shown["attempts"][0];
    assert!(attempt["waiting_ms"].as_i64().is_some(), "{shown}");
    let roles = attempt["role_usage"].as_array().unwrap();
    assert!(!roles.is_empty(), "{shown}");
    for role in roles {
        assert!(role["provider_elapsed_ms"].as_i64().is_some(), "{shown}");
    }
    thread::sleep(Duration::from_millis(25));
    let repeated = f.ok(&["show", "1"])?;
    assert_eq!(repeated["attempts"][0]["waiting_ms"], attempt["waiting_ms"]);
    assert_eq!(repeated["attempts"][0]["role_usage"], attempt["role_usage"]);
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
    // Each attempt is repaired automatically (judge, then a repair worker) until the
    // reviewer has failed three attempts in a row.
    assert_eq!(shown["attempts"].as_array().unwrap().len(), 3);
    for attempt in shown["attempts"].as_array().unwrap() {
        let outcome = attempt["outcome"].as_str().unwrap_or_default();
        assert!(
            outcome.contains("review asked instead of deciding"),
            "{shown}"
        );
    }
    let reviews = "review\n".repeat(6);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        format!(
            "execution\n{reviews}remediation\nremediation\n{reviews}remediation\nremediation\n{reviews}"
        )
    );
    let stopped = String::from_utf8_lossy(&output.stderr);
    assert!(stopped.contains("failed 3 attempts in a row"), "{stopped}");
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
        assert!(waiting.contains("Execution   ✓ succeeded"));
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
            t.screen("attempt scope", |s| s.contains("< This attempt >"))?;
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
    let mut refusal = String::new();
    support::eventually("recover succeeds", || {
        let out = f.process().arg("recover").output()?;
        refusal = String::from_utf8_lossy(&out.stderr).into_owned();
        Ok(out.status.success().then_some(()))
    })
    .map_err(|error| format!("{error}; last refusal: {refusal}"))?;
    f.state("interrupted")?;
    f.no_scratch()?;
    assert!(!f.root.join("result.txt").exists());
    // Recovery ends the dead attempt's unfinished step and provider call with it, so a
    // resolved task never blocks storage cleanup as if its work were still live.
    let db = f.db()?;
    let live = |table: &str| -> Result<i64> {
        Ok(db.connection().query_row(
            &format!("SELECT count(*) FROM {table} WHERE state='running'"),
            [],
            |r| r.get(0),
        )?)
    };
    assert_eq!((live("attempt_steps")?, live("model_calls")?), (0, 0));
    f.ok(&[
        "resolve",
        "1",
        "--outcome",
        "failed-skipped",
        "--reason",
        "crashed",
    ])?;
    f.ok(&["retention", "0"])?;
    let storage = f.ok(&["storage"])?;
    assert_eq!(
        storage["preview"]["deferred_for_live_work"], false,
        "{storage}"
    );
    assert_eq!(
        storage["preview"]["candidates"][0]["attempt_id"], 1,
        "{storage}"
    );
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
        let runner_lock = f.root.join(".git/ktask-runner.lock");
        support::eventually("guardian releases its lease", || {
            let lock = fs::File::open(&runner_lock)?;
            Ok((lock.try_lock().is_ok()
                && fs::metadata(f.root.join(".git/ktask-worker.lock"))?.len() == 0)
                .then_some(()))
        })?;
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
                s.contains("ABANDONED · supervisor absent") && shows_view(s, name)
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
                t.screen("cleanup required", |s| {
                    s.contains("Worker died without cleanup confirmation")
                })?
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
        // Finished steps keep their records; the step the dead supervisor left running ends.
        let steps = |task: &Value| task["attempts"][0]["steps"].as_array().cloned().unwrap();
        for (before, after) in steps(&before).iter().zip(&steps(&after)) {
            if before["state"] == "running" {
                assert_eq!(after["state"], "interrupted", "{after}");
                assert_eq!(before["role"], after["role"]);
            } else {
                assert_eq!(before, after);
            }
        }
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
        support::eventually("supervisor releases its lease", || {
            Ok(fs::File::open(&runner_lock)?
                .try_lock()
                .is_ok()
                .then_some(()))
        })?;
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
                t.screen(screen_name, |s| s.contains("ABANDONED · supervisor absent")
                    && shows_view(s, screen_name))?
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
    let judge = std::env::var("KTASK_LIVE_JUDGE_MODEL")?;
    assert_ne!(model, judge, "Live judges must differ from implementation");
    let f = Fixture::new()?;
    f.add("Other project must remain untouched")?;
    let project = f.second_project("live-project")?;
    for role in ["execution", "review", "validation", "remediation"] {
        f.ok_in(
            &project,
            &[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                if role == "execution" { &model } else { &judge },
                "--effort",
                "high",
            ],
        )?;
    }
    let mut ids = Vec::new();
    for (title, goal, criterion) in [
        (
            "Live first task",
            "Create result.txt containing exactly verified followed by a newline. Keep the existing README.",
            "result.txt contains verified",
        ),
        (
            "Live automatic continuation",
            "Create receipt.txt containing exactly ready followed by a newline. Preserve result.txt and the existing README.",
            "receipt.txt contains ready and result.txt still contains verified",
        ),
    ] {
        let receipt = f.ok_in(
            &project,
            &[
                "add",
                "--title",
                title,
                "--goal",
                goal,
                "--criterion",
                criterion,
            ],
        )?;
        ids.push(
            receipt["tasks"][0]["id"]
                .as_i64()
                .ok_or("Missing created task ID")?,
        );
    }
    let home = std::env::var_os("HOME").ok_or("HOME missing")?;
    let codex_home = std::env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home).join(".codex"));
    let output = f
        .process_in(&project)
        .env("PATH", std::env::var_os("PATH").ok_or("PATH missing")?)
        .env("CODEX_HOME", codex_home)
        .args([
            "--json",
            "--actor-kind",
            "ai",
            "--actor",
            "release-validation",
            "run",
            "--queue",
        ])
        .output()?;
    println!(
        "LIVE stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for id in ids {
        let task = f.ok_in(&project, &["show", &id.to_string()])?;
        println!("LIVE TASK {}", serde_json::to_string_pretty(&task)?);
        assert_eq!(task["lifecycle"], "succeeded");
    }
    assert!(output.status.success());
    assert_eq!(
        fs::read_to_string(project.join("result.txt"))?,
        "verified\n"
    );
    assert_eq!(fs::read_to_string(project.join("receipt.txt"))?, "ready\n");
    assert!(f.git_in(&project, &["status", "--porcelain"])?.is_empty());
    assert_eq!(f.ok(&["show", "1"])?["status"], "ready");
    assert!(!f.root.join("result.txt").exists());
    assert!(
        !f.scratch.path().join("provider-invocations").exists(),
        "Live run accidentally invoked a deterministic fixture"
    );
    let db = f.db()?;
    for role in ["execution", "review", "validation"] {
        let expected = if role == "execution" { &model } else { &judge };
        let count: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role=?1 AND model=?2 AND state='succeeded'",
            [role, expected.as_str()],
            |r| r.get(0),
        )?;
        assert!(
            count >= 2,
            "Both live tasks need a successful {role} turn with {expected}"
        );
    }
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &project, 120, 30)?;
    println!(
        "LIVE QUEUE COMPLETE\n{}",
        t.screen("live completion", |s| s.contains("Queue complete.")
            && s.contains("History 2"))?
    );
    t.click("3 History")?;
    println!(
        "LIVE HISTORY MOUSE\n{}",
        t.screen("live history", |s| s
            .contains("Live automatic continuation")
            && s.contains("DONE"))?
    );
    t.send("1")?;
    println!(
        "LIVE CURRENT KEYBOARD\n{}",
        t.screen("live current", |s| s.contains("Queue complete."))?
    );
    t.send("q")?;
    t.finish()?;
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
        // A used-up time limit stops at once; provider failures are repaired automatically
        // until three attempts in a row have failed.
        assert_eq!(
            task["attempts"].as_array().unwrap().len(),
            if mode == "slow" { 1 } else { 3 }
        );
        let stopped = String::from_utf8_lossy(&o.stderr);
        assert!(
            stopped.contains(if mode == "slow" {
                "task time limit"
            } else {
                "failed 3 attempts in a row"
            }),
            "{stopped}"
        );
        f.no_scratch()?;
    }
    Ok(())
}
#[test]
fn failed_attempts_are_repaired_automatically_and_real_stops_show_a_red_banner() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("fail-first-attempt")?;
        f.add("Repaired without the owner")?;
        f.add("Runs next")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let done = t.screen("queue complete", |s| s.contains("Queue complete."))?;
        println!("AUTOMATIC REPAIR COMPLETE mouse={mouse}\n{done}");
        let first = f.ok(&["show", "1"])?;
        assert_eq!(first["lifecycle"], "succeeded");
        // Only the successful attempt counts; the provider failure spent no budget.
        assert_eq!(first["attempts_used"], 1);
        let attempts = first["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 2);
        assert_eq!(attempts[0]["state"], "failed");
        assert_eq!(attempts[0]["failure_category"], "protocol");
        assert_eq!(f.ok(&["show", "2"])?["lifecycle"], "succeeded");
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nexecution\nexecution\nremediation\nremediation\nreview\nvalidation\nexecution\nreview\nvalidation\n"
        );
        t.send("q")?;
        t.finish()?;

        // A used-up time limit cannot be repaired: execution stops with the reason, why it
        // was not resolved, and what to do.
        let f = Fixture::new()?;
        f.provider("slow")?;
        f.add("Too little time")?;
        f.ok(&["edit", "1", "--timeout-ms", "200"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let stopped = t.screen("time limit banner", |s| {
            s.contains("EXECUTION STOPPED")
                && s.contains("Reason: Active time limit reached")
                && s.contains("Not resolved automatically: The task time limit (200 ms")
                && s.contains("Next: c Models")
        })?;
        println!("STOP BANNER mouse={mouse}\n{stopped}");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
            1
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn stage_limit_restarts_with_every_agent_result_and_stops_a_stalled_agent() -> Result {
    // Three turns sleep 3 s each, 9 s in all, so the attempt outlasts the 8 s stage limit
    // that each turn stays well under.
    let f = Fixture::new()?;
    f.provider("slow")?;
    f.add("Three slow stages")?;
    assert!(f.ok(&["show", "1"])?["definition"]["stage_timeout_ms"].is_null());
    f.ok(&["edit", "1", "--stage-limit", "8s"])?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert!(task["attempts"][0]["active_ms"].as_i64().unwrap() > 8000);

    // A reviewer that never answers is cut off at the stage limit and repaired twice.
    let f = Fixture::new()?;
    f.provider("slow-review")?;
    f.add("Stalled reviewer")?;
    f.ok(&["edit", "1", "--stage-limit", "1s"])?;
    let output = f.process().args(["run", "--one"]).output()?;
    assert_eq!(output.status.code(), Some(4));
    let stopped = String::from_utf8_lossy(&output.stderr);
    assert!(
        stopped.contains("stage time limit 3 attempts in a row"),
        "{stopped}"
    );
    let task = f.ok(&["show", "1"])?;
    let attempts = task["attempts"].as_array().unwrap();
    assert_eq!(attempts.len(), 3);
    for attempt in attempts {
        assert!(
            attempt["outcome"]
                .as_str()
                .unwrap()
                .starts_with("Stage time limit reached: the agent ran 1 s"),
            "{task}"
        );
    }
    assert_eq!(task["attempts_used"], 0);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn real_tui_switches_the_stage_limit_on_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Limit me")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 40)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("c Models")?;
        } else {
            t.send("c")?;
        }
        t.screen("models", |s| s.contains("Task models and limits"))?;
        t.send(&"\t".repeat(10))?;
        // Focus on a choice field has no text cursor; the model field had one.
        let off = t.screen("stage limit field", |s| {
            s.contains("Stage time limit") && s.contains("< Off >") && !s.contains('▏')
        })?;
        println!("STAGE LIMIT OFF mouse={mouse}\n{off}");
        if mouse {
            t.click_after("Stage time limit", "< Off >")?;
        } else {
            t.send(" ")?;
        }
        t.screen("stage limit on", |s| s.contains("< 30 min >"))?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        t.screen("saved", |s| !s.contains("Task models and limits"))?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["definition"]["stage_timeout_ms"], 1_800_000);
        assert!(task["definition"]["timeout_ms"].is_null());
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn queue_shows_the_model_each_task_will_use_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Mixed models")?;
        f.add("Sol everywhere")?;
        let mut args = vec!["edit".to_owned(), "2".to_owned()];
        for role in ["execution", "review", "validation", "remediation"] {
            args.extend([
                "--model".into(),
                format!("{role}=codex/gpt-6-sol"),
                "--effort".into(),
                format!("{role}=high"),
            ]);
        }
        f.ok(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 36)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        let queue = t.screen("queue models", |s| {
            s.contains("Queue · 1 / 2 · Model")
                && s.contains("codex/execution (mixed)")
                && s.contains("codex/gpt-6-sol high")
        })?;
        println!("QUEUE MODELS mouse={mouse}\n{queue}");
        let row = |id: &str| {
            queue
                .lines()
                .find(|l| l.contains(&format!("#{id}   PENDING")))
        };
        let mixed = row("1").unwrap();
        assert!(mixed.contains("codex/execution (mixed)"), "{mixed}");
        let sol = row("2").unwrap();
        assert!(
            sol.contains("codex/gpt-6-sol high") && !sol.contains("mixed"),
            "{sol}"
        );
        // The details pane lists every role of the selected task.
        assert!(queue.contains("Review codex/review / default"), "{queue}");
        t.send("q")?;
        t.finish()?;
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
    // Resume asks the provider again at once; a limit that still holds waits again.
    f.ok(&["resume"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        "execution\nexecution\n"
    );
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
    let pid: u32 = support::eventually("descendant started", || {
        Ok(fs::read_to_string(f.root.join("descendant.pid"))
            .ok()
            .map(|pid| pid.parse().expect("descendant pid")))
    })?;
    // The start time tells the descendant apart from a later process given the same pid.
    let started = support::start_time(pid)?.ok_or("descendant exited before cleanup")?;
    let duplicate = f.process().arg("run").output()?;
    assert_eq!(duplicate.status.code(), Some(4));
    f.ok(&["pause", "1"])?;
    run.0.wait()?;
    f.state("paused")?;
    f.guardian_finished()?;
    assert_ne!(
        support::start_time(pid)?,
        Some(started),
        "escaped descendant {pid} survived cleanup:\n{}",
        fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default()
    );
    f.no_scratch()?;
    Ok(())
}
#[test]
fn real_tui_question_answer_mouse_and_keyboard_automatically_resume() -> Result {
    fn stable_evidence(mut attempts: Value) -> Value {
        for attempt in attempts.as_array_mut().unwrap() {
            attempt.as_object_mut().unwrap().remove("waiting_ms");
            for role in attempt["role_usage"].as_array_mut().unwrap() {
                role.as_object_mut().unwrap().remove("provider_elapsed_ms");
            }
        }
        attempts
    }
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
        // The supervisor records the question, then releases the session and exits.
        let waiting = t.screen("question", |s| s.contains("QUESTION · supervisor absent"))?;
        println!("QUESTION CONTROLS (mouse={mouse})\n{waiting}");
        assert!(waiting.contains("w Answer"));
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
        let after_rejected = f.ok(&["show", "1"])?;
        assert!(
            after_rejected["attempts"][0]["waiting_ms"]
                .as_i64()
                .unwrap()
                >= waiting_task["attempts"][0]["waiting_ms"].as_i64().unwrap()
        );
        for (before_role, after_role) in waiting_task["attempts"][0]["role_usage"]
            .as_array()
            .unwrap()
            .iter()
            .zip(
                after_rejected["attempts"][0]["role_usage"]
                    .as_array()
                    .unwrap(),
            )
        {
            assert!(
                after_role["provider_elapsed_ms"].as_i64().unwrap()
                    >= before_role["provider_elapsed_ms"].as_i64().unwrap()
            );
        }
        assert_eq!(
            stable_evidence(after_rejected["attempts"].clone()),
            stable_evidence(waiting_task["attempts"].clone())
        );
        assert_eq!(fs::read_to_string(&invocations)?, calls_before);
        assert_eq!(fs::read(&log)?, log_before);
        if mouse {
            t.click("w Answer")?;
        } else {
            t.send("w")?;
        }
        println!(
            "ANSWER FORM (mouse={mouse})\n{}",
            t.screen("answer", |s| {
                s.contains("Answer AI question") && s.contains("Which content?")
            })?
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
fn agent_network_reaches_only_project_turns_that_change_the_project() -> Result {
    let f = Fixture::new()?;
    let network = f.scratch.path().join("provider-network");
    f.add("Offline by default")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(&network)?,
        "execution off\nreview off\nvalidation off\n"
    );
    fs::remove_file(&network)?;
    assert_eq!(
        f.ok(&["init", "--agent-network", "on"])?["agent_network"],
        true
    );
    f.add("Implementation may install dependencies")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(
        fs::read_to_string(&network)?,
        "execution on\nreview off\nvalidation off\n"
    );
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
    // The supervisor left the TUI's session, so the terminal cannot signal it.
    let terminal = f.supervisor_stat()?[4].clone();
    assert_eq!(
        terminal, "0",
        "detached supervisor has a controlling terminal"
    );
    t.send("q")?;
    t.finish()?;
    f.state("succeeded")?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn live_usage_keeps_step_and_pause_visible_at_minimum_size() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("usage-live")?;
        f.add("Live recorded usage")?;
        let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 70, 22)?;
        t.screen("live execution", |s| {
            s.contains("RUNNING · supervisor alive") && s.contains("Step: Execution")
        })?;
        let db = f.db()?;
        let changed = db.connection().execute(
            "UPDATE model_calls SET input_tokens=123456789012,cache_read_tokens=98765432109,cache_write_tokens=87654321098,output_tokens=76543210987 WHERE role='execution' AND state='running'",
            [],
        )?;
        assert_eq!(changed, 1);
        drop(db);
        let screen = t.screen("live usage and execution control", |s| {
            s.contains("RUNNING · supervisor alive")
                && s.contains("Step: Execution")
                && s.contains("I 123G R 98G W 87G O 76G")
                && s.contains("p Pause")
        })?;
        println!("LIVE USAGE 70x22 mouse={mouse}\n{screen}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        let paused = t.screen("paused live usage", |s| {
            s.contains("PAUSED · supervisor absent")
        })?;
        println!("LIVE USAGE PAUSED 70x22 mouse={mouse}\n{paused}");
        run.0.wait()?;
        f.state("paused")?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
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
fn real_tui_queue_runs_current_with_keyboard_and_mouse_in_both_modes() -> Result {
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
                t.click("2 Queue")?;
                t.screen("queue run control", |s| {
                    shows_view(s, "Queue")
                        && s.contains(if queue { "r Run queue" } else { "r Run one" })
                })?;
                t.click("#2   PENDING  Second configured task")?;
            } else {
                t.send("2j")?;
            }
            let selected = t.screen("later task selected while current remains first", |s| {
                s.contains("› #2   PENDING")
                    && s.contains("Current #1 · PENDING")
                    && s.contains("#2 · PENDING · AI")
            })?;
            println!("QUEUE BEFORE RUN mouse={mouse} queue={queue}\n{selected}");
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
            assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
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
    // The provider-limit wait would otherwise retry forever.
    f.ok(&["pause", "1"])?;
    f.supervisor_stopped()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn readiness_launch_failure_reopens_and_retries_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Readiness recovery")?;
        fs::remove_file(f.scratch.path().join("bin/codex"))?;
        let original = fs::read(f.root.join("README.md"))?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        let initial = t.screen("readiness before launch", |s| s.contains("Run blocked (1)"))?;
        println!("READINESS CURRENT mouse={mouse}\n{initial}");
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let details = t.screen("provider blocker", |s| {
            s.contains("codex executable not found")
        })?;
        assert!(details.contains("OK · Project checks"));
        assert!(details.contains("OK · Workspace baseline"));
        println!("READINESS DETAILS mouse={mouse}\n{details}");
        if mouse {
            t.click("m Models")?;
        } else {
            t.send("m")?;
        }
        t.screen("linked model action", |s| {
            s.contains("Task models and limits")
        })?;
        t.send("\x1b")?;
        t.screen("model form closed", |s| {
            !s.contains("Task models and limits")
        })?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let failed = t.screen("launch failure", |s| {
            s.contains("codex executable not found on PATH")
        })?;
        println!("READINESS LAUNCH FAILED mouse={mouse}\n{failed}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        t.send("q")?;
        t.finish()?;

        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        reopened.screen("reopened current", |s| s.contains("Readiness recovery"))?;
        if mouse {
            reopened.click("u Readiness")?;
        } else {
            reopened.send("u")?;
        }
        let retained = reopened.screen("retained startup error", |s| {
            s.contains("Last launch failure:")
        })?;
        assert!(retained.contains("codex executable not found on PATH"));
        println!("READINESS REOPENED mouse={mouse}\n{retained}");
        f.provider("normal")?;
        if mouse {
            reopened.click("u Refresh")?;
        } else {
            reopened.send("u")?;
        }
        let ready =
            reopened.screen("readiness refreshed", |s| s.contains("Can run · 0 blocker"))?;
        println!("READINESS REPAIRED mouse={mouse}\n{ready}");
        if mouse {
            reopened.click("r Run")?;
        } else {
            reopened.send("r")?;
        }
        let completed = reopened.screen("run after repair", |s| s.contains("Queue complete."))?;
        println!("READINESS RETRIED mouse={mouse}\n{completed}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_lists_independent_blockers_and_recovers_stale_worker() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    let f = Fixture::new()?;
    f.add("Blocked setup")?;
    let mut service = Service::open(f.db()?, &f.root, None)?;
    let task = service.task(1)?.unwrap();
    let mut definition = task.definition().definition.clone();
    definition.models[2] = None;
    service.revise(
        VersionedTask {
            id: 1,
            version: task.version,
        },
        &definition,
        true,
    )?;
    let checks = service.project_checks()?;
    let mut disabled = checks[0].clone();
    disabled.enabled = false;
    service.save_check(&checks, &disabled)?;
    fs::remove_file(f.scratch.path().join("bin/codex"))?;
    fs::write(f.root.join("outside.txt"), "preserve me")?;
    let worker_marker = f.root.join(".git/ktask-worker.lock");
    fs::write(&worker_marker, "stale guardian marker")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 38)?;
    t.send("u")?;
    let details = t.screen("all readiness blockers", |s| {
        s.contains("Worker cleanup is unconfirmed")
    })?;
    for cause in [
        "No enabled check",
        "validation model",
        "codex executable not found",
        "Uncommitted work outside this task",
    ] {
        assert!(details.contains(cause), "{cause} absent:\n{details}");
    }
    println!("READINESS MULTIPLE BLOCKERS\n{details}");
    t.click("c Checks")?;
    let check_form = t.screen("checks link", |s| s.contains("Check to open"))?;
    println!("READINESS CHECK ACTION\n{check_form}");
    t.send("\x1b")?;
    t.screen("checks closed", |s| !s.contains("Project checks"))?;
    t.send("u")?;
    t.screen("readiness again", |s| s.contains("Execution readiness"))?;
    t.click("R Recover")?;
    t.screen("recover action", |s| {
        s.contains("Recover workspace ownership")
    })?;
    t.send(" \r")?;
    let recovered = t.screen("worker ownership recovered", |s| {
        s.contains("Saved to SQLite")
    })?;
    println!("READINESS WORKER RECOVERED\n{recovered}");
    assert_eq!(fs::metadata(&worker_marker)?.len(), 0);
    assert_eq!(
        fs::read_to_string(f.root.join("outside.txt"))?,
        "preserve me"
    );
    assert!(service.task(1)?.unwrap().attempts.is_empty());
    t.send("q")?;
    t.finish()?;
    Ok(())
}

#[test]
fn readiness_recovers_worker_marker_on_failed_current_without_losing_attempt() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Failed with stale worker")?;
        f.fail_provider()?;
        let before = f.ok(&["show", "1"])?;
        assert_eq!(before["status"], "failed");
        let marker = f.root.join(".git/ktask-worker.lock");
        fs::write(&marker, "stale guardian marker")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("failed task ready for readiness", |s| {
            s.contains("Failed with stale worker")
        })?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("failed worker blocker", |s| {
            s.contains("Worker cleanup is unconfirmed")
        })?;
        assert!(blocked.contains("R Recover"));
        println!("READINESS FAILED WORKER mouse={mouse}\n{blocked}");
        if mouse {
            t.click("R Recover")?;
        } else {
            t.send("R")?;
        }
        t.screen("recover failed worker", |s| {
            s.contains("Recover workspace ownership")
        })?;
        t.send(" \r")?;
        let recovered = t.screen("failed worker recovered", |s| s.contains("Saved to SQLite"))?;
        println!("READINESS FAILED RECOVERED mouse={mouse}\n{recovered}");
        assert_eq!(fs::metadata(marker)?.len(), 0);
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["status"], "failed");
        assert_eq!(after["attempts"], before["attempts"]);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_retains_next_task_preflight_failure_after_reopen() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("First task succeeds")?;
        f.add("Second task needs review model")?;
        let mut service = Service::open(f.db()?, &f.root, None)?;
        let second = service.task(2)?.unwrap();
        let mut definition = second.definition().definition.clone();
        definition.models[1] = None;
        service.revise(
            VersionedTask {
                id: 2,
                version: second.version,
            },
            &definition,
            true,
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("first task ready", |s| s.contains("First task succeeds"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let stopped = t.screen("next task preflight stopped", |s| {
            s.contains("Second task needs review model") && s.contains("Run blocked")
        })?;
        println!("READINESS NEXT TASK STOPPED mouse={mouse}\n{stopped}");
        assert!(
            f.ok(&["show", "2"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;

        let mut reopened = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        reopened.screen("second task reopened", |s| {
            s.contains("Second task needs review model")
        })?;
        if mouse {
            reopened.click("u Readiness")?;
        } else {
            reopened.send("u")?;
        }
        let retained = reopened.screen("next task reason retained", |s| {
            s.contains("review model") && s.contains("Queue: stopped")
        })?;
        println!("READINESS NEXT TASK REOPENED mouse={mouse}\n{retained}");
        if mouse {
            reopened.click("m Models")?;
        } else {
            reopened.send("m")?;
        }
        reopened.screen("repair second model", |s| {
            s.contains("Task models and limits")
        })?;
        if mouse {
            reopened.click("Review provider/model")?;
            reopened.send("codex/review")?;
            reopened.click("Apply")?;
        } else {
            reopened.send("\t\tcodex/review\x13")?;
        }
        let saved = reopened.screen("second model saved", |s| s.contains("Saved to SQLite"))?;
        println!("READINESS NEXT TASK REPAIRED mouse={mouse}\n{saved}");
        if mouse {
            reopened.click("r Run queue")?;
        } else {
            reopened.send("r")?;
        }
        let complete = reopened.screen("second task run", |s| s.contains("Queue complete."))?;
        println!("READINESS NEXT TASK COMPLETE mouse={mouse}\n{complete}");
        reopened.send("q")?;
        reopened.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_approval_action_is_available_with_mouse_and_keyboard() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.ok(&[
            "add",
            "--title",
            "Approval readiness",
            "--goal",
            "Create result.txt",
            "--criterion",
            "result.txt contains verified",
            "--approval",
            "draft",
        ])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("draft current", |s| s.contains("Approval readiness"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("approval blocker", |s| {
            s.contains("Current revision needs approval")
        })?;
        assert!(blocked.contains("v Approve"));
        println!("READINESS APPROVAL BLOCKED mouse={mouse}\n{blocked}");
        if mouse {
            t.click("v Approve")?;
        } else {
            t.send("v")?;
        }
        let ready = t.screen("approved current", |s| s.contains("Can run · u Readiness"))?;
        println!("READINESS APPROVED mouse={mouse}\n{ready}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn readiness_distinguishes_open_and_answered_questions() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("question")?;
        f.add("Answer then resume")?;
        f.ok(&["run", "--one"])?;
        f.state("waiting")?;
        let attempt = f.ok(&["show", "1"])?["attempts"][0]["id"].clone();
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("question current", |s| s.contains("Answer then resume"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let waiting = t.screen("open question blocks", |s| {
            s.contains("Answer the open question")
        })?;
        println!("READINESS QUESTION OPEN mouse={mouse}\n{waiting}");
        if mouse {
            t.click("w Answer")?;
        } else {
            t.send("w")?;
        }
        t.screen("answer action", |s| s.contains("Answer AI question"))?;
        t.send("Human answer: verified")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        t.screen("answer saved", |s| s.contains("Answer saved"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("answered question is ready", |s| {
            s.contains("Can run · 0 blocker")
        })?;
        println!("READINESS QUESTION ANSWERED mouse={mouse}\n{ready}");
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        t.screen("same attempt resumes", |s| s.contains("Queue complete."))?;
        assert_eq!(f.ok(&["show", "1"])?["attempts"][0]["id"], attempt);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_refreshes_answered_question_while_browsing_queue() -> Result {
    let f = Fixture::new()?;
    f.provider("question")?;
    f.add("Question while browsing")?;
    f.ok(&["run", "--one"])?;
    f.state("waiting")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 30)?;
    t.screen("question at Current", |s| {
        s.contains("Question while browsing")
    })?;
    t.send("2")?;
    let queue = t.screen("browsing queue", |s| s.contains("Queue · 1 shown"))?;
    println!("READINESS QUEUE BEFORE ANSWER\n{queue}");
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    t.send("1")?;
    let current = t.screen("answer becomes runnable", |s| {
        s.contains("Can run · u Readiness")
    })?;
    println!("READINESS CURRENT AFTER QUEUE ANSWER\n{current}");
    t.send("u")?;
    let details = t.screen("readiness after queue answer", |s| {
        s.contains("Can run · 0 blocker")
    })?;
    println!("READINESS DETAILS AFTER QUEUE ANSWER\n{details}");
    t.send("\x1b")?;
    t.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
    t.send("q")?;
    t.finish()?;
    Ok(())
}

#[test]
fn readiness_shows_live_supervisor_and_worker_owners_without_taking_their_locks() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("Live ownership")?;
        let original = fs::read(f.root.join("README.md"))?;
        let supervisor_path = f.root.join(".git/ktask-runner.lock");
        let worker_path = f.root.join(".git/ktask-worker.lock");
        let supervisor = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&supervisor_path)?;
        let worker = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&worker_path)?;
        supervisor.try_lock()?;
        worker.try_lock()?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("live owner current", |s| s.contains("Live ownership"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("both live owners", |s| {
            s.contains("A live supervisor owns") && s.contains("A live worker owns")
        })?;
        println!("READINESS LIVE OWNERS mouse={mouse}\n{blocked}");
        assert!(
            f.ok(&["show", "1"])?["attempts"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        assert_eq!(fs::metadata(&supervisor_path)?.len(), 0);
        assert_eq!(fs::metadata(&worker_path)?.len(), 0);
        if mouse {
            t.click("c Checks")?;
        } else {
            t.send("c")?;
        }
        let checks = t.screen("linked checks action", |s| s.contains("Check to open"))?;
        println!("READINESS CHECKS LINK mouse={mouse}\n{checks}");
        t.send("\x1b")?;
        t.screen("checks action closed", |s| !s.contains("Project checks"))?;
        drop(worker);
        drop(supervisor);
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("owners released", |s| s.contains("Can run · 0 blocker"))?;
        println!("READINESS OWNERS RELEASED mouse={mouse}\n{ready}");
        assert_eq!(fs::read(f.root.join("README.md"))?, original);
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        let completed = t.screen("run after ownership release", |s| {
            s.contains("Queue complete.")
        })?;
        println!("READINESS OWNERS LAUNCHED mouse={mouse}\n{completed}");
        assert_eq!(
            f.ok(&["show", "1"])?["attempts"].as_array().unwrap().len(),
            1
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
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
    support::write_executable(
        &git,
        format!(
            "#!/bin/sh\nif [ \"$3\" = rev-parse ] && [ \"$4\" = HEAD ]; then\n count=$(cat '{}' 2>/dev/null || printf 0)\n count=$((count+1))\n printf '%s' \"$count\" > '{}'\n if [ \"$count\" = 4 ]; then touch '{}'; sleep 3; fi\nfi\nexec /usr/bin/git \"$@\"\n",
            counter.display(),
            counter.display(),
            marker.display()
        ),
    )?;
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
    // The preserved implementation was committed without review. Both the prerequisite's
    // review and the resumed original's review must still cover it, from the first baseline.
    let baseline = f.git(&["rev-list", "--max-parents=0", "HEAD"])?;
    for prompt in ["first-review-prompt", "prompt-review"] {
        let prompt = fs::read_to_string(f.scratch.path().join(prompt))?;
        let scope = &prompt[prompt.find("Review scope").expect("review scope")..];
        assert!(
            scope.contains(&format!("git diff {}", baseline.trim())),
            "{scope}"
        );
        assert!(
            scope.contains("preserve unfinished task #1, attempt #1"),
            "{scope}"
        );
        assert!(scope.contains("A\tresult.txt"), "{scope}");
    }
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
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 100, 40)?;
        println!(
            "EXHAUSTED CURRENT mouse={mouse}\n{}",
            t.screen("failed current", |s| s.contains("EXECUTION STOPPED")
                && s.contains("Ten repair rounds left critical findings")
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
    // Three automatic repairs of the provider failure, then the streak stop.
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    f.ok(&["edit", "1", "--timeout-ms", "10000"])?;
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
    // Three automatic repairs of the provider failure, then the deadline stop.
    assert_eq!(task["attempts"].as_array().unwrap().len(), 4);
    assert!(
        task["attempts"][3]["outcome"]
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
            t.screen("individual check timeout", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Individual supervisor check timeout reached")
                && s.contains("failed 3 attempts in a row"))?
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
        // Two automatic repairs (judge, then repair worker) before the stop.
        assert_eq!(failed["attempts"].as_array().unwrap().len(), 3);
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\nremediation\nremediation\nremediation\nremediation\n"
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
fn repeated_rate_limits_keep_waiting_with_capped_backoff_and_never_fail() -> Result {
    let f = Fixture::new()?;
    f.provider("rate")?;
    f.add("Rate limit must not fail the task")?;
    for (wait, minutes) in [1, 2, 4, 8, 8, 8].into_iter().enumerate() {
        f.ok(&["run", "--one"])?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "waiting", "wait {wait}");
        assert_eq!(task["attempts_used"], 0);
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        let backoff: i64 = f.db()?.connection().query_row(
            "SELECT resume_at_ms-waiting_since_ms FROM attempts WHERE task_id=1",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(backoff, minutes * 60_000, "wait {wait}");
        f.db()?.connection().execute(
            "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE task_id=1",
            [],
        )?;
    }
    f.no_scratch()?;
    Ok(())
}

#[test]
fn provider_errors_right_after_a_usage_limit_wait_keep_waiting_instead_of_failing() -> Result {
    let f = Fixture::new()?;
    f.provider("usage")?;
    f.add("Outage")?;
    f.ok(&["run", "--one"])?;
    let expire = "UPDATE attempts SET resume_at_ms=waiting_since_ms WHERE task_id=1";
    f.db()?.connection().execute(expire, [])?;
    // Codex failed compaction with "Bad Request" while still out of quota.
    f.provider("fail")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"][0]["wait_reason"], "rate_limit");
    f.db()?.connection().execute(expire, [])?;
    f.provider("normal")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    let waits: i64 = f.db()?.connection().query_row(
        "SELECT rate_waits FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(waits, 0, "a completed turn ends the provider-limit streak");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn usage_limit_waits_for_the_reset_but_owner_resume_asks_the_provider_now() -> Result {
    use chrono::TimeZone;
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("usage")?;
        f.add("Quota wait")?;
        f.ok(&["run", "--one"])?;
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["status"], "waiting");
        assert_eq!(task["attempts_used"], 0);
        let reset = chrono::Local
            .with_ymd_and_hms(2099, 10, 26, 23, 32, 0)
            .unwrap()
            .timestamp_millis();
        assert_eq!(task["attempts"][0]["resume_at_ms"], reset + 60_000);
        assert!(
            task["attempts"][0]["report"]
                .as_str()
                .unwrap()
                .contains("Provider usage limit; resumes Oct 26 2099 23:33")
        );
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        let current = t.screen("usage limit current", |s| {
            s.contains("Provider limit until Oct 26 2099 23:33 · r Resume")
                && s.contains("Limit resets in 2")
        })?;
        println!("USAGE LIMIT CURRENT mouse={mouse}\n{current}");
        let resets = current.split("Limit resets in ").nth(1).unwrap_or_default();
        assert!(
            resets.split(' ').next().is_some_and(|n| n.ends_with('d')),
            "status shows days, not raw seconds"
        );
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("usage limit readiness", |s| {
            s.contains("Can run · 0 blocker") && s.contains("[r Run]")
        })?;
        println!("USAGE LIMIT READINESS mouse={mouse}\n{ready}");
        // The limit lifted early (credits bought): the owner's Resume asks the provider now.
        f.provider("normal")?;
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("resumed before reset", |s| s.contains("Queue complete."))?;
        println!("USAGE LIMIT RESUMED NOW mouse={mouse}\n{complete}");
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["lifecycle"], "succeeded");
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            f.db()?.connection().query_row(
                "SELECT count(*) FROM events WHERE kind='attempt.wait_ended'",
                [],
                |r| r.get::<_, i64>(0)
            )?,
            1
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
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
            t.screen("real startup reason", |s| {
                s.contains("command conflict: Workspace has uncommitted work")
            })?
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
                .contains("preserve unfinished task #1, attempt #3")
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

/// Read persisted failures and their independently retained supervisor transcripts.
/// `failed` bounds the failed launches: the retry schedule runs on wall-clock time, so how
/// many fail before a test intervenes can vary.
fn assert_check_launch_records(
    f: &Fixture,
    failed: std::ops::RangeInclusive<usize>,
    passed: bool,
) -> Result<Vec<i64>> {
    let db = f.db()?;
    let records = db.connection().prepare(
        "SELECT id,attempt_id,state,exit_code,started_at_ms,report FROM attempt_steps WHERE task_id=1 AND role='check' ORDER BY id",
    )?.query_map([], |r| Ok((
        r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?,
        r.get::<_, Option<i32>>(3)?, r.get::<_, i64>(4)?, r.get::<_, String>(5)?,
    )))?.collect::<rusqlite::Result<Vec<_>>>()?;
    println!("CHECK LAUNCH RECORDS {records:#?}");
    let failed_count = records.len() - usize::from(passed);
    assert!(
        failed.contains(&failed_count),
        "{failed_count} failed launches, expected {failed:?}"
    );
    let failed = failed_count;
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
        // Restore launches before anything else: the two retries leave 3 s for it.
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let retrying = f.ok(&["show", "1"])?;
        assert_eq!(retrying["attempts_used"], 0);
        assert_eq!(retrying["attempts"].as_array().unwrap().len(), 1);
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
        let launches = assert_check_launch_records(&f, 1..=2, true)?;
        for (pause, pair) in [1000, 2000].iter().zip(launches.windows(2)) {
            assert!(pair[1] - pair[0] >= *pause, "backoff skipped: {launches:?}");
        }
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
            t.screen("launch retries exhausted", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Permission denied")
                && s.contains("failed 3 attempts in a row"))?
        );
        let first = f.ok(&["show", "1"])?;
        println!("CHECK LAUNCH EXHAUSTED TASK {first}");
        assert_eq!(first["status"], "failed");
        assert_eq!(first["attempts_used"], 0);
        // The repair attempts cannot even launch their judge, so three in a row stop.
        assert_eq!(first["attempts"].as_array().unwrap().len(), 3);
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
        let launches = assert_check_launch_records(&f, 3..=3, false)?;
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
            reopened.screen("retained environment failure", |s| s
                .contains("EXECUTION STOPPED")
                && s.contains("Permission denied"))?
        );
        assert_check_launch_records(&f, 3..=3, false)?;
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
        // Another backoff can end before Pause lands; none may start after it.
        let before = assert_check_launch_records(&f, failures..=2, false)?;
        let failures = before.len();
        f.launch_workers_reaped()?;
        f.supervisor_stopped()?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        thread::sleep(Duration::from_millis(2200));
        assert_eq!(
            assert_check_launch_records(&f, failures..=failures, false)?,
            before
        );
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
        assert_check_launch_records(&f, failures..=failures, true)?;
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
    let retried = support::eventually("deadline Retry finishes", || {
        let task = f.ok(&["show", "1"])?;
        Ok(
            (task["attempts"].as_array().unwrap().len() == 2 && task["status"] == "failed")
                .then_some(task),
        )
    })?;
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
    Ok(())
}

#[test]
fn real_tui_check_launch_backoff_deadline_survives_restart_and_requires_explicit_increase() -> Result
{
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let binary = f.fail_check_launch()?;
        // The third launch comes at least 3 s of backoff after the first, so a 3 s limit
        // always expires during a backoff once the first launch has failed.
        let limit = 3000;
        f.ok(&["edit", "1", "--timeout-ms", "3000", "--attempt-limit", "3"])?;
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
            t.screen("launch backoff", |s| s
                .contains("Check infrastructure failed"))?
        );
        let expired = t
            .screen("configured deadline expired", |s| {
                s.contains("FAILED") && s.contains("configured task-wide limit")
            })
            .map_err(|error| {
                let show = |id| f.ok(&["show", id]).map(|task| task.to_string());
                format!("{error}\ntask 1: {:?}\ntask 2: {:?}", show("1"), show("2"))
            })?;
        println!("CHECK LAUNCH DEADLINE EXPIRED mouse={mouse}\n{expired}");
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
        let launches = assert_check_launch_records(&f, 1..=2, false)?;
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
                && s.contains("Attempt 2")
                && s.contains("configured task-wide limit"))?
        );
        assert_eq!(
            fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
            "execution\n",
            "expired retry must not start even a judge worker"
        );
        assert_eq!(
            assert_check_launch_records(&f, launches.len()..=launches.len(), false)?,
            launches
        );
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

#[test]
fn readiness_refreshes_external_models_and_reports_reload_errors() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask};
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.add("External setup repair")?;
        let mut service = Service::open(f.db()?, &f.root, None)?;
        let task = service.task(1)?.unwrap();
        let ready = task.definition().definition.clone();
        let mut missing = ready.clone();
        missing.models[1] = None;
        service.revise(
            VersionedTask {
                id: 1,
                version: task.version,
            },
            &missing,
            true,
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("external models current", |s| {
            s.contains("External setup repair")
        })?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let blocked = t.screen("external missing role", |s| {
            s.contains("review model: No model selected")
        })?;
        println!("READINESS EXTERNAL MODEL BLOCKED mouse={mouse}\n{blocked}");
        let task = service.task(1)?.unwrap();
        service.revise(
            VersionedTask {
                id: 1,
                version: task.version,
            },
            &ready,
            true,
        )?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        let refreshed = t.screen("external model fixed in dialog", |s| {
            s.contains("Can run · 0 blocker")
        })?;
        println!("READINESS EXTERNAL MODEL REFRESHED mouse={mouse}\n{refreshed}");
        let db = f.db()?;
        db.connection()
            .execute_batch("ALTER TABLE execution_sessions RENAME TO unavailable_sessions")?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        let error = t.screen("reload failure replaces readiness", |s| {
            s.contains("BLOCK · Refresh failed:")
        })?;
        assert!(!error.contains("Can run · 0 blocker"));
        assert!(!error.contains("[r Run]"));
        println!("READINESS REFRESH ERROR mouse={mouse}\n{error}");
        db.connection()
            .execute_batch("ALTER TABLE unavailable_sessions RENAME TO execution_sessions")?;
        if mouse {
            t.click("u Refresh")?;
        } else {
            t.send("u")?;
        }
        t.screen("reload recovered", |s| s.contains("Can run · 0 blocker"))?;
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("external model run", |s| s.contains("Queue complete."))?;
        println!("READINESS EXTERNAL MODEL COMPLETE mouse={mouse}\n{complete}");
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

#[test]
fn readiness_lets_the_owner_resume_a_rate_limit_wait_at_once() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("rate")?;
        f.add("Readiness rate wait")?;
        f.ok(&["run", "--one"])?;
        let before = f.ok(&["show", "1"])?;
        let attempt = before["attempts"][0]["id"].clone();
        let db = f.db()?;
        db.connection().execute(
            "UPDATE attempts SET resume_at_ms=CAST(strftime('%s','now') AS INTEGER)*1000+6000 WHERE state='waiting'",
            [],
        )?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("waiting current", |s| s.contains("Readiness rate wait"))?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        // A provider-limit wait never blocks the owner: Run asks the provider again now.
        let ready = t.screen("rate limit readiness", |s| {
            s.contains("Can run · 0 blocker")
                && s.contains("[r Run]")
                && s.contains("Limit resets in")
        })?;
        println!("READINESS RATE WAIT mouse={mouse}\n{ready}");
        f.provider("normal")?;
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        let complete = t.screen("rate limit resumed", |s| s.contains("Queue complete."))?;
        println!("READINESS RATE RESUMED mouse={mouse}\n{complete}");
        let after = f.ok(&["show", "1"])?;
        assert_eq!(after["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(after["attempts"][0]["id"], attempt);
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

/// Six projects share one instance and run their queues at the same time, each execution
/// streaming 1,500 output events, so every supervisor writes the shared database at once.
#[test]
fn many_projects_streaming_output_at_once_all_finish() -> Result {
    let f = Fixture::new()?;
    f.provider("chatty")?;
    let mut projects = vec![f.root.clone()];
    for index in 1..6 {
        projects.push(f.second_project(&format!("project-{index}"))?);
    }
    for (index, project) in projects.iter().enumerate() {
        for task in ["first", "second"] {
            let title = format!("Project {index} {task}");
            let add = [
                "add",
                "--title",
                &title,
                "--goal",
                "Create result.txt containing verified",
                "--criterion",
                "result.txt contains verified",
            ];
            f.ok_in(project, &add)?;
        }
    }
    let mut runs = Vec::new();
    for project in &projects {
        let run = f
            .process_in(project)
            .args(["--json", "run"])
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        runs.push(Running(run));
    }
    for (project, run) in projects.iter().zip(&mut runs) {
        let status = run.0.wait()?;
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut run.0.stderr.take().unwrap(), &mut stderr)?;
        assert!(status.success(), "{}: {stderr}", project.display());
    }
    for (index, project) in projects.iter().enumerate() {
        let tasks = f.ok_in(project, &["list", "--scope", "history"])?["tasks"].clone();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 2, "{tasks:?}");
        for task in tasks {
            assert_eq!(task["status"], "succeeded", "{task}");
            assert!(
                task["title"]
                    .as_str()
                    .unwrap()
                    .starts_with(&format!("Project {index} "))
            );
            let id = task["id"].as_i64().unwrap().to_string();
            let shown = f.ok_in(project, &["show", &id])?;
            assert_eq!(shown["attempts"].as_array().unwrap().len(), 1, "{shown}");
        }
        assert_eq!(f.git_in(project, &["status", "--porcelain"])?, "");
    }
    let db = f.db()?;
    let rows: i64 = db.connection().query_row(
        "SELECT count(*) FROM attempt_output WHERE content LIKE '%compiled unit 1499%'",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(rows, 12, "every execution's last streamed event was stored");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn two_projects_run_at_once_and_stay_separate() -> Result {
    let f = Fixture::new()?;
    let second = f.second_project("second")?;
    f.provider("overlap")?;
    f.add("Alpha task")?;
    let beta = [
        "add",
        "--title",
        "Beta task",
        "--goal",
        "Create result.txt containing verified",
        "--criterion",
        "result.txt contains verified",
    ];
    f.ok_in(&second, &beta)?;
    let mut a = Terminal::open_project(BIN, f.scratch.path(), &f.root, 110, 32)?;
    let mut b = Terminal::open_project(BIN, f.scratch.path(), &second, 110, 32)?;
    a.screen("first project", |s| s.contains("Alpha task"))?;
    b.screen("second project", |s| s.contains("Beta task"))?;
    a.send("r")?;
    b.click("r Run queue")?;
    // Each execution waits for the release, so both are inside a provider turn at once.
    let started = support::eventually("both executions running", || {
        let started =
            fs::read_to_string(f.scratch.path().join("overlap-started")).unwrap_or_default();
        Ok((started.lines().count() == 2).then_some(started))
    })?;
    let mut ran_in: Vec<_> = started.lines().map(PathBuf::from).collect();
    ran_in.sort();
    let mut projects = vec![f.root.canonicalize()?, second.canonicalize()?];
    projects.sort();
    assert_eq!(ran_in, projects);
    for (t, name) in [(&a, "first"), (&b, "second")] {
        println!(
            "{name} RUNNING\n{}",
            t.screen(name, |s| s.contains("RUNNING · supervisor alive"))?
        );
    }
    // Pausing one project leaves the other one running.
    b.click("p Pause")?;
    println!(
        "SECOND PAUSED\n{}",
        b.screen("second paused", |s| {
            s.contains("PAUSED · supervisor absent")
                && s.contains("r Resume")
                && s.contains("Paused; worker stopped, Resume continues the same attempt")
        })?
    );
    assert_eq!(f.ok(&["current"])?["status"], "running");
    fs::write(f.scratch.path().join("release"), "")?;
    println!(
        "FIRST COMPLETE\n{}",
        a.screen("first complete", |s| s.contains("Queue complete."))?
    );
    assert_eq!(f.ok_in(&second, &["current"])?["status"], "paused");
    b.send("r")?;
    println!(
        "SECOND COMPLETE\n{}",
        b.screen("second complete", |s| s.contains("Queue complete."))?
    );
    // Neither project's tasks, events or commits appear in the other.
    for (project, own, other) in [
        (&f.root, "Alpha task", "Beta task"),
        (&second, "Beta task", "Alpha task"),
    ] {
        let tasks = f.ok_in(project, &["list", "--scope", "all"])?["tasks"].clone();
        let tasks = tasks.as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0]["title"], own);
        assert_eq!(tasks[0]["status"], "succeeded");
        let id = tasks[0]["id"].as_i64().unwrap().to_string();
        let events = f.ok_in(project, &["events", "--after", "0"])?.to_string();
        assert!(events.contains(own) && !events.contains(other), "{events}");
        let task = f.ok_in(project, &["show", &id])?;
        assert_eq!(task["attempts"].as_array().unwrap().len(), 1, "{task}");
        let log = f.git_in(project, &["log", "--format=%s"])?;
        assert!(log.starts_with(&format!("ktask: task #{id}")), "{log}");
        assert_eq!(
            f.git_in(project, &["show", "HEAD:result.txt"])?,
            "verified\n"
        );
        assert_eq!(f.git_in(project, &["status", "--porcelain"])?, "");
    }
    for mut t in [a, b] {
        t.send("q")?;
        t.finish()?;
    }
    f.no_scratch()?;
    Ok(())
}
