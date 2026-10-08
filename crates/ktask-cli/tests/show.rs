//! B-55 on the real binary: `ktask-rs show <id>` and `show <id> --json` print one task in
//! full, in text and JSON, including every field named by the task — a never-attempted task,
//! a task with its own provider and model, a task whose attempt waited for a usage limit then
//! was routed through the resolver into a second attempt, a cancelled task and one sealed done
//! by the operator's own hand — and refuse an unknown id, naming it.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};

const TIMEOUT: Duration = Duration::from_secs(10);

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, so a task's own
/// bash block can call back into `ktask-rs report`.
fn with_nested_ktask_rs_on_path(command: &mut Command) {
    let mut paths = std::path::Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .parent()
        .map(std::path::Path::to_path_buf)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempt to
    /// commit a task's own change is not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        let mut email = Command::new("git");
        email.args(["config", "user.email", "test@example.com"]);
        self.sandbox
            .isolate(&mut email, &self.repository)
            .status()?;
        let mut name = Command::new("git");
        name.args(["config", "user.name", "Test User"]);
        self.sandbox.isolate(&mut name, &self.repository).status()?;
        Ok(())
    }

    fn spawn_the_queue(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        with_nested_ktask_rs_on_path(&mut command);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command.spawn()?)
    }

    fn wait_for_show(&self, what: &str, condition: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            let shown = self.run(&["show", "1"])?;
            if condition(&shown.stdout) {
                return Ok(shown.stdout);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "timed out waiting for {what}; show shows:\n{}",
                    shown.stdout
                )
                .into());
            }
            std::thread::park_timeout(Duration::from_millis(50));
        }
    }
}

#[test]
fn a_never_attempted_task_shows_its_own_fields_inherited_and_no_attempts() -> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.run(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        "the body",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let shown = fixture.run(&["show", "1"])?;

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    let lines: Vec<&str> = shown.stdout.lines().collect();
    assert_eq!(lines[0], "#1 pending");
    assert_eq!(lines[1], "Title: a");
    assert!(lines.contains(&"Kind: agent"));
    assert!(lines.contains(&"Provider: echo (inherited)"));
    assert!(lines.contains(&"Model: none (inherited)"));
    assert!(lines.contains(&"Links:"));
    assert!(lines.contains(&"  none"));
    assert!(lines.contains(&"Body:"));
    assert!(lines.contains(&"  the body"));
    assert!(lines.contains(&"Criteria:"));
    assert!(lines.contains(&"  - it works"));
    assert!(
        !shown.stdout.contains("Attempt"),
        "a never-attempted task has no attempt section: {}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn a_tasks_own_provider_and_model_are_said_own_not_inherited() -> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.run(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--provider",
        "echo",
        "--model",
        "test-model",
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let shown = fixture.run(&["show", "1"])?;

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(
        shown.stdout.contains("Provider: echo (own)"),
        "{}",
        shown.stdout
    );
    assert!(
        shown.stdout.contains("Model: test-model (own)"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn an_unknown_task_is_refused_naming_it_and_all_is_not_needed_for_a_cancelled_one() -> Result<()> {
    let fixture = Fixture::new()?;

    let unknown = fixture.run(&["show", "1"])?;
    assert_eq!(unknown.stdout, "");
    assert!(unknown.stderr.contains('1'), "{}", unknown.stderr);
    assert_eq!(unknown.code, Some(2));

    let added = fixture.run(&["add", "--title", "a", "--criterion", "it works"])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let removed = fixture.run(&["remove", "1"])?;
    assert_eq!(removed.code, Some(0), "{}", removed.stderr);

    let shown = fixture.run(&["show", "1"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert_eq!(shown.stdout.lines().next(), Some("#1 cancelled"));
    Ok(())
}

#[test]
fn a_task_marked_done_by_hand_shows_the_reason_and_when() -> Result<()> {
    let fixture = Fixture::new()?;
    let added = fixture.run(&["add", "--title", "a", "--criterion", "it works"])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let done = fixture.run(&["done", "1", "--reason", "fixed by hand"])?;
    assert_eq!(done.code, Some(0), "{}", done.stderr);

    let shown = fixture.run(&["show", "1"])?;

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(
        shown
            .stdout
            .contains("marked done by the user: fixed by hand"),
        "{}",
        shown.stdout
    );
    Ok(())
}

#[test]
fn a_two_hundred_character_reason_is_carried_whole_not_elided() -> Result<()> {
    let fixture = Fixture::new()?;
    let long = "x".repeat(200);
    let body = format!(
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" stop --reason \"resolver not expected\"\nelse\n  ktask-rs report --token \"$1\" failed --reason \"{long}\"\nfi\n```\n"
    );
    let added = fixture.run(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--body",
        &body,
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let run = fixture.run(&["run"])?;
    assert_eq!(run.code, Some(1), "{}", run.stderr);

    let shown = fixture.run(&["show", "1"])?;

    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    assert!(shown.stdout.contains(&long), "{}", shown.stdout);
    Ok(())
}

/// A bash block whose implementation step: on its first invocation of attempt 1, reports the
/// provider's usage limit was hit (and records a session); on its second (after the run has
/// waited for the limit and retried the very same attempt), fails with `reason`; on attempt 2,
/// succeeds. The resolve step always retries once, and review and testing always pass.
fn limit_then_resolved_retry_body(reason: &str, tries: &std::path::Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry\nelif [ \"$2\" = \"1\" ]; then\n  n=$(cat \"{tries}\" 2>/dev/null || echo 0)\n  echo $((n + 1)) > \"{tries}\"\n  if [ \"$n\" = \"0\" ]; then\n    echo \"KTASK_SESSION: sess-123\"\n    echo \"KTASK_LIMIT: $(( $(date -u +%s) + 2 ))\"\n    exit 1\n  else\n    ktask-rs report --token \"$1\" failed --reason \"{reason}\"\n  fi\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        tries = tries.display(),
    )
}

#[test]
fn a_task_with_two_attempts_a_resolution_a_limit_wait_and_a_per_task_provider_shows_every_field()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    let tries = fixture.repository.join("tries");
    let added = fixture.run(&[
        "add",
        "--title",
        "a",
        "--criterion",
        "it works",
        "--provider",
        "echo",
        "--model",
        "test-model",
        "--body",
        &limit_then_resolved_retry_body("it broke after the limit", &tries),
    ])?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let mut child = fixture.spawn_the_queue()?;
    let waiting = fixture.wait_for_show("the attempt to show waiting", |stdout| {
        stdout.contains("usage limit")
    })?;
    assert!(waiting.contains("Attempt 1"), "{waiting}");

    let status = child.wait()?;
    assert!(status.success(), "{status:?}");

    let shown = fixture.run(&["show", "1"])?;
    assert_eq!(shown.code, Some(0), "{}", shown.stderr);
    let text = shown.stdout;

    // The task's own fields, both said own.
    assert!(text.contains("Provider: echo (own)"), "{text}");
    assert!(text.contains("Model: test-model (own)"), "{text}");

    // Both attempts, newest first.
    let attempt_2_at = text.find("Attempt 2 (latest)").expect("attempt 2 heading");
    let attempt_1_at = text.find("Attempt 1").expect("attempt 1 heading");
    assert!(attempt_2_at < attempt_1_at, "{text}");

    // The resolution: the resolve step, with its own retry outcome, between the two attempts.
    assert!(text.contains("resolve"), "{text}");
    assert!(text.contains("outcome: retry"), "{text}");

    // The limit wait, the session, the routed verdict and the reason, all on attempt 1's
    // implementation step.
    let implementation_1 = text
        .lines()
        .find(|line| line.contains("implementation") && line.contains("it broke after the limit"))
        .expect("attempt 1's implementation line");
    assert!(
        implementation_1.contains("hit the usage limit: waited"),
        "{implementation_1}"
    );
    assert!(
        implementation_1.contains("resumed at"),
        "{implementation_1}"
    );
    assert!(
        implementation_1.contains("session:sess-123"),
        "{implementation_1}"
    );
    assert!(
        implementation_1.contains("routed: decide"),
        "{implementation_1}"
    );
    assert!(
        implementation_1.contains("reason: it broke after the limit"),
        "{implementation_1}"
    );
    assert!(
        implementation_1.contains("usage none"),
        "{implementation_1}"
    );

    // The same facts, structured, in `show --json`.
    let json = fixture.run(&["show", "1", "--json"])?;
    assert_eq!(json.code, Some(0), "{}", json.stderr);
    let value: serde_json::Value = serde_json::from_str(&json.stdout)?;
    assert_eq!(value["provider"], "echo");
    assert_eq!(value["model"], "test-model");
    assert_eq!(value["attempt"]["number"], 2);
    assert_eq!(value["history"][0]["number"], 1);
    let steps = value["history"][0]["steps"].as_array().expect("steps");
    let implementation = steps
        .iter()
        .find(|step| step["step"] == "implementation")
        .expect("the implementation step");
    assert!(
        implementation["limit_wait"]["waited_seconds"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(implementation["session"], "sess-123");
    assert_eq!(implementation["routed"], "decide — agent failed");
    Ok(())
}

#[test]
fn show_and_its_options_are_in_the_help() -> Result<()> {
    let fixture = Fixture::new()?;
    let top = fixture.run(&["--help"])?;
    assert!(top.stdout.contains("show"), "{}", top.stdout);
    let help = fixture.run(&["show", "--help"])?;
    assert_eq!(help.code, Some(0), "{}", help.stderr);
    assert!(help.stdout.contains("--json"), "{}", help.stdout);
    assert!(help.stdout.contains("--project"), "{}", help.stdout);
    Ok(())
}
