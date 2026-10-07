//! M4-04 on the real binary: after an attempt ends `failed` or `failed-unknown`, the tool runs
//! a resolver — an agent in the resolve role — before anything else happens. `retry` starts a
//! fresh attempt; `stop` ends the task `failed` with the resolver's own reason; `skip` ends the
//! task `skipped` with the resolver's own reason and the run goes on to the next task
//! (M4-07); a resolver that reports nothing ends the task `failed-unknown`; `max-attempts`
//! caps how many attempts get a resolver at all; and `ktask-rs report` only accepts `retry`,
//! `stop` or `skip` while the resolve step is the one running, never otherwise.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::path::PathBuf;

use repo::{git_repository, scratch};
use support::{Outcome, Result, Sandbox};
use tempfile::TempDir;

/// A sandbox with a git repository called `my-app`, `max-attempts` left at its default of 3
/// unless a test changes it.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            work,
            repository,
            _keep: keep,
        })
    }

    /// Runs `ktask-rs` with `args` inside the repository.
    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    /// Runs `ktask-rs run` inside the repository.
    fn run_the_queue(&self) -> Result<Outcome> {
        self.run(&["run"])
    }

    /// Adds a task with `title` and body `body`, one criterion, kind `agent`.
    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        let added = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            body,
        ])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(())
    }

    /// `ktask-rs status`'s own stdout lines.
    fn status_lines(&self) -> Result<Vec<String>> {
        let status = self.run(&["status"])?;
        assert_eq!(status.code, Some(0), "{}", status.stderr);
        Ok(status.stdout.lines().map(str::to_owned).collect())
    }
}

/// A bash block for attempt 1 of a task: fails the implementation step with `reason`; the
/// review and test steps, if ever reached, approve and accept; the resolve step, when it runs,
/// reports whatever `resolve_branch` says to.
fn failing_once_body(reason: &str, resolve_branch: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  {resolve_branch}\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" failed --reason \"{reason}\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
    )
}

/// A bash block for a task that simply succeeds: the implementation step reports `done` at
/// once, and the review and test steps, if ever reached, approve and accept.
fn succeeding_body() -> String {
    "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n"
        .to_owned()
}

#[test]
fn a_retry_decision_starts_a_second_attempt_and_the_resolution_shows_between_the_two() -> Result<()>
{
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body("it broke", "ktask-rs report --token \"$1\" retry"),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tdone\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tretry\tusage none",
            "\tattempt 2: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 2: review\techo\t0s\tapproved\tusage none",
            "\tattempt 2: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 2: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn a_stop_decision_ends_the_task_failed_with_the_resolvers_own_reason_not_the_attempts()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" stop --reason \"not worth retrying\"",
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert!(
        outcome
            .stdout
            .contains("task 1: failed: not worth retrying"),
        "{}",
        outcome.stdout
    );
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tstop\tnot worth retrying\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn a_skip_decision_ends_the_task_skipped_with_the_resolvers_own_reason_and_the_run_moves_on_to_the_next_task()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" skip --reason \"no longer relevant\"",
        ),
    )?;
    fixture.add_agent_task("b", &succeeding_body())?;

    let outcome = fixture.run_the_queue()?;

    // Unlike `stop`, `skip` does not stop the run: task b is attempted and finishes.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tskipped\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tskip\tno longer relevant\tusage none",
            "#2\tdone\tb\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );
    Ok(())
}

#[test]
fn skip_without_a_reason_is_refused_and_the_task_stays_running_its_attempt() -> Result<()> {
    let fixture = Fixture::new()?;
    let skip_stderr = fixture.work.join("skip-stderr");
    let skip_exit = fixture.work.join("skip-exit");
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" skip 2> \"{}\"; echo $? > \"{}\"\n  \
                 ktask-rs report --token \"$1\" stop --reason \"skip was refused\"",
                skip_stderr.display(),
                skip_exit.display(),
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    let exit: i32 = std::fs::read_to_string(&skip_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&skip_stderr)?;
    assert!(stderr.contains("needs a reason"), "{stderr}");
    // The refused `skip` recorded nothing: the task ended through the `stop` that followed it.
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tstop\tskip was refused\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn list_hides_a_skipped_task_the_same_as_a_cancelled_one_and_all_shows_it() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        &failing_once_body(
            "it broke",
            "ktask-rs report --token \"$1\" skip --reason \"no longer relevant\"",
        ),
    )?;
    fixture.add_agent_task("b", &succeeding_body())?;
    let outcome = fixture.run_the_queue()?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);

    let hidden = fixture.run(&["list"])?;
    assert_eq!(hidden.code, Some(0), "{}", hidden.stderr);
    assert_eq!(hidden.stdout, "1\t#2\tdone\tagent\tb\n");

    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.code, Some(0), "{}", all.stderr);
    assert_eq!(
        all.stdout,
        "1\t#1\tskipped\tagent\ta\n2\t#2\tdone\tagent\tb\n"
    );
    Ok(())
}

/// A JSON task for a `supersede` file: `title`, one criterion, and `succeeding_body` as its
/// body, so the new task's own first attempt succeeds at once when the run reaches it.
fn new_task(title: &str) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "criteria": ["it works"],
        "body": succeeding_body(),
    })
}

#[test]
fn a_supersede_decision_replaces_the_task_with_the_new_ones_and_the_run_continues_with_the_first()
-> Result<()> {
    let fixture = Fixture::new()?;
    let tasks_file = fixture.work.join("tasks.json");
    std::fs::write(
        &tasks_file,
        serde_json::json!([
            new_task("part one"),
            new_task("part two"),
            new_task("part three")
        ])
        .to_string(),
    )?;
    fixture.add_agent_task(
        "too large",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" supersede --tasks \"{}\"",
                tasks_file.display()
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    // Unlike `stop`, `supersede` does not stop the run: it carries on through every new task.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tsuperseded\ttoo large\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tsupersede\tsuperseded by 3 tasks: 2, 3, 4\tusage none",
            "#2\tdone\tpart one\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
            "#3\tdone\tpart two\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
            "#4\tdone\tpart three\tusage none",
            "\tattempt 1: implementation\techo\t0s\tdone\tusage none",
            "\tattempt 1: review\techo\t0s\tapproved\tusage none",
            "\tattempt 1: testing\techo\t0s\taccepted\tusage none",
            "\tattempt 1: commit\t-\t0s\tpassed\tnothing was changed",
        ]
    );

    let hidden = fixture.run(&["list"])?;
    assert_eq!(hidden.code, Some(0), "{}", hidden.stderr);
    assert_eq!(
        hidden.stdout,
        "1\t#2\tdone\tagent\tpart one\n2\t#3\tdone\tagent\tpart two\n\
         3\t#4\tdone\tagent\tpart three\n"
    );

    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.code, Some(0), "{}", all.stderr);
    assert_eq!(
        all.stdout,
        "1\t#1\tsuperseded\tagent\ttoo large\n2\t#2\tdone\tagent\tpart one\n\
         3\t#3\tdone\tagent\tpart two\n4\t#4\tdone\tagent\tpart three\n"
    );
    Ok(())
}

/// A TOML task for a `supersede` file, the counterpart of `new_task`.
fn new_toml_task(title: &str) -> String {
    format!(
        "[[tasks]]\ntitle = \"{title}\"\ncriteria = [\"it works\"]\nbody = '''\n{}'''\n\n",
        succeeding_body()
    )
}

#[test]
fn a_supersede_decision_reads_a_toml_tasks_file_and_the_run_continues_with_its_tasks() -> Result<()>
{
    let fixture = Fixture::new()?;
    let tasks_file = fixture.work.join("tasks.toml");
    std::fs::write(
        &tasks_file,
        new_toml_task("part one") + &new_toml_task("part two"),
    )?;
    fixture.add_agent_task(
        "too large",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" supersede --tasks \"{}\"",
                tasks_file.display()
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.code, Some(0), "{}", all.stderr);
    assert_eq!(
        all.stdout,
        "1\t#1\tsuperseded\tagent\ttoo large\n2\t#2\tdone\tagent\tpart one\n\
         3\t#3\tdone\tagent\tpart two\n"
    );
    Ok(())
}

#[test]
fn a_supersede_tasks_file_that_is_neither_json_nor_toml_is_refused_before_it_is_read() -> Result<()>
{
    let fixture = Fixture::new()?;
    let missing_file = fixture.work.join("tasks.md");
    let supersede_stderr = fixture.work.join("supersede-stderr");
    let supersede_exit = fixture.work.join("supersede-exit");
    fixture.add_agent_task(
        "too large",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" supersede --tasks \"{}\" 2> \"{}\"; \
                 echo $? > \"{}\"\n  \
                 ktask-rs report --token \"$1\" stop --reason \"supersede was refused\"",
                missing_file.display(),
                supersede_stderr.display(),
                supersede_exit.display(),
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    let exit: i32 = std::fs::read_to_string(&supersede_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&supersede_stderr)?;
    assert!(
        stderr.contains("only .json and .toml files are imported"),
        "{stderr}"
    );
    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.stdout, "1\t#1\tfailed\tagent\ttoo large\n");
    Ok(())
}

#[test]
fn an_invalid_tasks_file_is_refused_with_the_same_messages_import_gives_and_changes_nothing()
-> Result<()> {
    let fixture = Fixture::new()?;
    let bad_file = fixture.work.join("bad.json");
    std::fs::write(&bad_file, "[{\"title\": \"  \"}]")?;
    let supersede_stderr = fixture.work.join("supersede-stderr");
    let supersede_exit = fixture.work.join("supersede-exit");
    fixture.add_agent_task(
        "too large",
        &failing_once_body(
            "it broke",
            &format!(
                "ktask-rs report --token \"$1\" supersede --tasks \"{}\" 2> \"{}\"; \
                 echo $? > \"{}\"\n  \
                 ktask-rs report --token \"$1\" stop --reason \"supersede was refused\"",
                bad_file.display(),
                supersede_stderr.display(),
                supersede_exit.display(),
            ),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    let exit: i32 = std::fs::read_to_string(&supersede_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&supersede_stderr)?;
    assert!(stderr.contains("1 task is invalid"), "{stderr}");
    assert!(stderr.contains("the title is empty"), "{stderr}");
    assert!(
        stderr.contains("a task needs at least one acceptance criterion"),
        "{stderr}"
    );
    // The refused `supersede` recorded nothing and added no task: the task ended through the
    // `stop` that followed it.
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ttoo large\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tstop\tsupersede was refused\tusage none",
        ]
    );
    let all = fixture.run(&["list", "--all"])?;
    assert_eq!(all.code, Some(0), "{}", all.stderr);
    assert_eq!(all.stdout, "1\t#1\tfailed\tagent\ttoo large\n");
    Ok(())
}

#[test]
fn supersede_without_tasks_is_refused_and_supersede_outside_the_resolve_step_too() -> Result<()> {
    let fixture = Fixture::new()?;

    let no_tasks = fixture.run(&["report", "--token", "my-app/1/1", "supersede"])?;
    assert_eq!(no_tasks.code, Some(2), "{}", no_tasks.stdout);
    assert!(
        no_tasks.stderr.contains("needs --tasks"),
        "{}",
        no_tasks.stderr
    );

    let tasks_file = fixture.work.join("tasks.json");
    std::fs::write(&tasks_file, serde_json::json!([new_task("x")]).to_string())?;
    let retry_with_tasks = fixture.run(&[
        "report",
        "--token",
        "my-app/1/1",
        "retry",
        "--tasks",
        &tasks_file.to_string_lossy(),
    ])?;
    assert_eq!(
        retry_with_tasks.code,
        Some(2),
        "{}",
        retry_with_tasks.stdout
    );
    assert!(
        retry_with_tasks.stderr.contains("does not accept --tasks"),
        "{}",
        retry_with_tasks.stderr
    );
    Ok(())
}

#[test]
fn a_resolver_that_reports_nothing_ends_the_task_failed_unknown_with_what_was_observed()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &failing_once_body("it broke", "true"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains(
            "task 1: failed-unknown: the provider exited with code 0 and reported nothing"
        ),
        "{}",
        outcome.stdout
    );
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed-unknown\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tfailed-unknown\tthe provider exited with code 0 and reported nothing\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn with_max_attempts_1_the_resolver_never_runs_and_the_task_ends_failed_at_once() -> Result<()> {
    let fixture = Fixture::new()?;
    let set = fixture.run(&["settings", "set", "max-attempts", "1"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    // The resolve branch panics the queue (an unknown command) if it is ever reached, so this
    // test fails loudly rather than silently passing were the resolver run anyway.
    fixture.add_agent_task("a", &failing_once_body("it broke", "exit 99"))?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn with_max_attempts_3_the_third_failure_ends_the_task_without_a_resolution_line() -> Result<()> {
    let fixture = Fixture::new()?;
    // Every attempt fails and the resolver always retries — attempts 1 and 2 each get a
    // resolution, but the third, reaching `max-attempts`, does not.
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" retry\nelse\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nfi\n```\n",
    )?;

    let outcome = fixture.run_the_queue()?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stdout);
    assert_eq!(
        fixture.status_lines()?,
        [
            "#1\tfailed\ta\tusage none",
            "\tattempt 1: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 1: resolve\techo\t0s\tretry\tusage none",
            "\tattempt 2: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
            "\tattempt 2: resolve\techo\t0s\tretry\tusage none",
            "\tattempt 3: implementation\techo\t0s\tfailed\tit broke\trouted: decide — agent failed\tusage none",
        ]
    );
    Ok(())
}

#[test]
fn report_refuses_a_decision_outside_the_resolve_step_and_a_non_decision_inside_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let outside_stderr = fixture.work.join("outside-stderr");
    let outside_exit = fixture.work.join("outside-exit");
    let inside_stderr = fixture.work.join("inside-stderr");
    let inside_exit = fixture.work.join("inside-exit");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelif [ \"$3\" = \"resolve\" ]; then\n  ktask-rs report --token \"$1\" done 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" retry\nelif [ \"$2\" = \"1\" ]; then\n  ktask-rs report --token \"$1\" retry 2> \"{}\"; echo $? > \"{}\"\n  ktask-rs report --token \"$1\" failed --reason \"it broke\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
            inside_stderr.display(),
            inside_exit.display(),
            outside_stderr.display(),
            outside_exit.display(),
        ),
    )?;

    let outcome = fixture.run_the_queue()?;

    // The wrong decisions were refused, but each script went on to report validly afterwards,
    // so the run completes as if they were never tried.
    assert_eq!(outcome.code, Some(0), "{}", outcome.stdout);
    assert_eq!(fixture.status_lines()?[0], "#1\tdone\ta\tusage none");

    let exit: i32 = std::fs::read_to_string(&outside_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&outside_stderr)?;
    assert!(
        stderr.contains("does not belong to the implementation step"),
        "{stderr}"
    );
    assert!(stderr.contains("done"), "{stderr}");
    assert!(stderr.contains("needs-input"), "{stderr}");
    assert!(!stderr.contains("retry or stop"), "{stderr}");

    let exit: i32 = std::fs::read_to_string(&inside_exit)?.trim().parse()?;
    assert_eq!(exit, 2);
    let stderr = std::fs::read_to_string(&inside_stderr)?;
    assert!(
        stderr.contains("does not belong to the resolve step"),
        "{stderr}"
    );
    assert!(stderr.contains("expected retry or stop"), "{stderr}");
    Ok(())
}
