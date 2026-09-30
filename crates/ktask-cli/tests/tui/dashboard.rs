//! The dashboard: the queue screen shows, under each task that has an attempt, one line per
//! step it has run so far, in the same order `status` prints them, from the same use case,
//! and it updates while a run started elsewhere is in progress.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};
use super::tracked_branch::cloned_repository;

const ROWS: u16 = 24;
const COLS: u16 = 110;

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, ahead of whatever
/// is already there, so a task's own bash block — standing in for what a real agent would
/// run — can call back into `ktask-rs report` and find this same binary.
fn with_nested_ktask_rs_on_path(command: &mut Command) {
    let mut paths = Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// A bash block that reports `outcome` for whatever token it is given as `$1`, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body(outcome: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" {outcome}\nfi\n```\n"
    )
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given, for the
/// implementation step; the review and test steps, when reached, approve and accept.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  ktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\nfi\n```\n"
    )
}

/// A bash block that blocks on a fifo at `go` until this test writes to it, then reports
/// `done` (or, for the review step, `approved`; for the test step, `accepted`): an attempt
/// that stays running until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  [ -p \"{0}\" ] || mkfifo \"{0}\"\n  read _ < \"{0}\"\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        go.display()
    )
}

/// A sandbox with a git repository called `my-app`.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left a `run` it started — through the CLI or through the TUI, which
/// starts one detached on purpose so quitting the screen alone does not stop it — nothing of
/// it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
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

    /// Runs `ktask-rs` with `args` inside the repository and expects it to succeed.
    fn cli(&self, args: &[&str]) -> Result<String> {
        let outcome = self.sandbox.run(&self.repository, args)?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }

    fn add_agent_task(&self, title: &str, body: &str) -> Result<()> {
        self.cli(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            body,
        ])?;
        Ok(())
    }

    /// Runs `ktask-rs run` in the foreground and waits for it, its `PATH` carrying the
    /// directory of the `ktask-rs` under test so a task's own bash block can call back into
    /// `ktask-rs report`.
    fn run_the_queue(&self) -> Result<String> {
        let outcome =
            self.sandbox
                .run_with(&self.repository, &["run"], with_nested_ktask_rs_on_path)?;
        Ok(outcome.stdout)
    }

    /// Runs `ktask-rs run` again and again until the queue has nothing left pending or is
    /// empty, so that every task that stops one run is given its attempt.
    fn run_the_queue_to_completion(&self) -> Result<()> {
        for _ in 0..10 {
            let stdout = self.run_the_queue()?;
            if stdout.contains("nothing is pending") || stdout.contains("empty") {
                return Ok(());
            }
        }
        Err("the queue never finished".into())
    }

    /// Spawns `ktask-rs run` in the background, its `PATH` carrying the directory of the
    /// `ktask-rs` under test, and returns at once so the caller can watch it through the TUI.
    fn spawn_run(&self) -> Result<Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        self.sandbox.isolate(&mut command, &self.repository);
        with_nested_ktask_rs_on_path(&mut command);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        Ok(command.spawn()?)
    }

    /// Opens the queue screen and waits until it is drawn whole.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }

    /// Sets the project's health-check command to `command`, so an attempt records a health
    /// check step of its own ahead of the implementation step.
    fn set_health_check(&self, command: &str) -> Result<()> {
        self.cli(&["settings", "set", "health-check", command])?;
        Ok(())
    }

    /// Sets `user.name` and `user.email` on the repository, so the commit step's attempts to
    /// commit are not refused for want of a configured identity.
    fn configure_git_identity(&self) -> Result<()> {
        for args in [
            ["config", "user.email", "test@example.com"],
            ["config", "user.name", "Test"],
        ] {
            let mut command = Command::new("git");
            command.args(args);
            let status = self
                .sandbox
                .isolate(&mut command, &self.repository)
                .status()?;
            assert!(status.success(), "git {args:?}");
        }
        Ok(())
    }

    /// The repository's `HEAD`, short form.
    fn head(&self) -> Result<String> {
        let mut command = Command::new("git");
        command.args(["rev-parse", "--short", "HEAD"]);
        let output = self
            .sandbox
            .isolate(&mut command, &self.repository)
            .output()?;
        assert!(output.status.success(), "{output:?}");
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
}

/// The number of seconds an attempt line built by [`ktask_tui::render`] reports it has run:
/// the third `·`-separated field of `line`, e.g. `12` from `"      implementation · echo ·
/// 12s · running"`.
fn attempt_seconds(line: &str) -> Result<u64> {
    let field = line
        .split(" · ")
        .nth(2)
        .ok_or_else(|| format!("{line:?} has no time-spent field"))?;
    Ok(field.trim_end_matches('s').parse()?)
}

#[test]
fn a_run_started_elsewhere_shows_pending_then_running_with_elapsed_time_increasing_then_done()
-> Result<()> {
    let fixture = Fixture::new()?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the pending task", |screen| {
        screen.contents().contains("pending")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  pending  agent  a");
    assert!(!lines[5].contains("implementation"), "{lines:?}");

    let mut run = fixture.spawn_run()?;
    let screen = terminal.wait_for("the task running with its attempt line", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
            && lines
                .get(5)
                .is_some_and(|line| line.contains("implementation"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("running"),
        "{}",
        lines[5]
    );
    let first_elapsed = attempt_seconds(&lines[5])?;
    let frames_before = terminal.frame_count();

    // Nothing else touches the journal while the attempt is still gated on `go`: any further
    // frame, and any increase in the elapsed time shown, comes from the loop's own tick.
    let screen = terminal.wait_for("the elapsed time to move on its own", |screen| {
        lines_inside_frame(&screen.contents())
            .get(5)
            .and_then(|line| attempt_seconds(line).ok())
            .is_some_and(|seconds| seconds > first_elapsed)
    })?;
    let lines = lines_inside_frame(&screen);
    let second_elapsed = attempt_seconds(&lines[5])?;
    assert!(
        second_elapsed > first_elapsed,
        "elapsed time did not move on its own: {first_elapsed}s then {second_elapsed}s"
    );
    assert!(
        terminal.frame_count() > frames_before,
        "no frame was drawn while the attempt ran with nothing else changing"
    );

    std::fs::write(&go, "")?;
    let status = run.wait()?;
    assert!(status.success(), "{status:?}");

    let screen = terminal.wait_for(
        "the task done with every step of its attempt shown",
        |screen| {
            screen.contents().contains("commit")
                && lines_inside_frame(&screen.contents())
                    .get(4)
                    .is_some_and(|line| line.starts_with(">1  #1  done"))
        },
    )?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    // Every step the attempt ran shows, in order — not only the last, `commit`.
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("review · echo") && lines[6].ends_with("approved"),
        "{}",
        lines[6]
    );
    assert!(
        lines[7].contains("testing · echo") && lines[7].ends_with("accepted"),
        "{}",
        lines[7]
    );
    assert!(
        lines[8].contains("commit · -") && lines[8].ends_with("nothing was changed"),
        "{}",
        lines[8]
    );
    assert!(
        screen.contains("pending 0") && screen.contains("running 0") && screen.contains("done 1"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_task_that_changes_a_file_gets_a_real_commit_and_the_dashboard_shows_its_short_hash()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.configure_git_identity()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  echo hello > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;

    let mut terminal = fixture.open()?;
    let mut run = fixture.spawn_run()?;
    let screen = terminal.wait_for("a's commit line with the task shown done", |screen| {
        let contents = screen.contents();
        contents.contains("committed as")
            && lines_inside_frame(&contents)
                .get(4)
                .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    assert!(run.wait()?.success());

    let hash = fixture.head()?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    // Every step shows, in order — the commit line is the fourth, not the only one.
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("review · echo") && lines[6].ends_with("approved"),
        "{}",
        lines[6]
    );
    assert!(
        lines[7].contains("testing · echo") && lines[7].ends_with("accepted"),
        "{}",
        lines[7]
    );
    assert!(
        lines[8].contains("commit · -") && lines[8].ends_with(&format!("committed as {hash}")),
        "{} (expected hash {hash})",
        lines[8]
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

/// Whether process `pid` is still running. A killed process's entry under `/proc` can briefly
/// outlive the signal that ended it, as a zombie waiting for its new parent to reap it once it
/// is orphaned, so that alone does not count as still running.
fn is_running(pid: u32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .split(')')
        .next_back()
        .and_then(|rest| rest.split_whitespace().next());
    state != Some("Z")
}

/// Waits, for up to a few seconds, until `condition` holds, checking every 20ms; fails naming
/// `what` when it never does.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
    Ok(())
}

#[test]
fn a_run_killed_outright_shows_the_task_interrupted_at_once_with_no_next_run() -> Result<()> {
    let fixture = Fixture::new()?;
    let pid_file = fixture.work.join("provider.pid");
    fixture.add_agent_task(
        "a",
        &format!(
            "```bash\necho $$ > \"{}\"\nsleep 30\n```\n",
            pid_file.display()
        ),
    )?;

    let mut terminal = fixture.open()?;
    let mut run = fixture.spawn_run()?;
    let run_pid = run.id();

    terminal.wait_for("the task running with its attempt line", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(4).is_some_and(|line| line.contains("running"))
    })?;

    wait_until("the provider to record its process id", || {
        pid_file.exists()
    })?;
    let provider_pid: u32 = std::fs::read_to_string(&pid_file)?.trim().parse()?;
    assert!(
        is_running(provider_pid),
        "provider {provider_pid} is not running"
    );

    // `SIGKILL` cannot be caught: the run gets no chance to record anything, unlike
    // `SIGTERM`/`SIGINT`. The dashboard must still stop showing the task `running` once the
    // run that was running it is gone, with no next `run` involved at all.
    signal::kill(Pid::from_raw(i32::try_from(run_pid)?), Signal::SIGKILL)?;
    let status = run.wait()?;
    assert!(!status.success(), "{status:?}");
    wait_until(
        "the provider to end once the run that started it is killed outright",
        || !is_running(provider_pid),
    )?;

    let screen = terminal.wait_for("the task shown interrupted", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines
            .get(4)
            .is_some_and(|line| line.contains("interrupted"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  interrupted  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("interrupted"),
        "{}",
        lines[5]
    );
    assert!(
        screen.contains("running 0") && screen.contains("unknown 1"),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

/// A summary line with `done`, `failed`, `blocked` and `unknown` filled in, the rest at their
/// baseline for a queue of one or two tasks, none cancelled.
fn summary(done: u32, failed: u32, blocked: u32, unknown: u32) -> String {
    format!(
        "pending 0  running 0  done {done}  failed {failed}  blocked {blocked}  unknown {unknown}  cancelled 0"
    )
}

#[test]
fn each_ending_shows_its_own_outcome_and_reason_and_the_summary_counts_it() -> Result<()> {
    // `run` refuses to attempt a task once an earlier one is left `failed`, `blocked` or
    // `failed-unknown`, so a queue that exercises every ending needs one project per ending:
    // a `done` filler task, then the task whose ending is under test.
    let done = Fixture::new()?;
    done.add_agent_task("a", &reporting_body("done"))?;
    done.run_the_queue_to_completion()?;
    let terminal = done.open()?;
    let screen = terminal.wait_for("a's attempt line", |screen| {
        screen.contents().contains("commit") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    // All four steps of the successful attempt show, in order.
    assert!(
        lines[5].contains("implementation") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("review") && lines[6].ends_with("approved"),
        "{}",
        lines[6]
    );
    assert!(
        lines[7].contains("testing") && lines[7].ends_with("accepted"),
        "{}",
        lines[7]
    );
    assert!(
        lines[8].contains("commit") && lines[8].ends_with("nothing was changed"),
        "{}",
        lines[8]
    );
    assert_eq!(lines[2], summary(1, 0, 0, 0));
    drop(terminal);

    let failed = Fixture::new()?;
    failed.add_agent_task("x", &reporting_body("done"))?;
    failed.add_agent_task("b", &reporting_body_with_reason("failed", "it broke"))?;
    failed.run_the_queue()?;
    let terminal = failed.open()?;
    let screen = terminal.wait_for("b's attempt line", |screen| {
        screen.contents().contains("failed: it broke") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    // `failed` (6 characters) is wider than `done` (4), so `done`'s status column pads out
    // to match it.
    assert_eq!(lines[4], ">1  #1  done    agent  x");
    // `x`'s four finished steps stay above `b`'s own header and single failed step.
    assert!(lines[5].contains("implementation") && lines[5].ends_with("done"));
    assert!(lines[6].contains("review") && lines[6].ends_with("approved"));
    assert!(lines[7].contains("testing") && lines[7].ends_with("accepted"));
    assert!(lines[8].contains("commit"));
    assert_eq!(lines[9], " 2  #2  failed  agent  b");
    assert!(lines[10].ends_with("failed: it broke"), "{}", lines[10]);
    assert_eq!(lines[2], summary(1, 1, 0, 0));
    drop(terminal);

    let too_large = Fixture::new()?;
    too_large.add_agent_task("x", &reporting_body("done"))?;
    too_large.add_agent_task("c", &reporting_body_with_reason("too-large", "split me"))?;
    too_large.run_the_queue()?;
    let terminal = too_large.open()?;
    let screen = terminal.wait_for("c's attempt line", |screen| {
        screen.contents().contains("too-large: split me") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done    agent  x");
    assert_eq!(lines[9], " 2  #2  failed  agent  c");
    assert!(lines[10].ends_with("too-large: split me"), "{}", lines[10]);
    assert_eq!(lines[2], summary(1, 1, 0, 0));
    drop(terminal);

    let needs_input = Fixture::new()?;
    needs_input.add_agent_task("x", &reporting_body("done"))?;
    needs_input.add_agent_task(
        "d",
        &reporting_body_with_reason("needs-input", "which path?"),
    )?;
    needs_input.run_the_queue()?;
    let terminal = needs_input.open()?;
    let screen = terminal.wait_for("d's attempt line", |screen| {
        screen.contents().contains("needs-input: which path?") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    // `blocked` (7 characters) is wider than `done` (4), so `done`'s status column pads out
    // to match it.
    assert_eq!(lines[4], ">1  #1  done     agent  x");
    assert_eq!(lines[9], " 2  #2  blocked  agent  d");
    assert!(
        lines[10].ends_with("needs-input: which path?"),
        "{}",
        lines[10]
    );
    assert_eq!(lines[2], summary(1, 0, 1, 0));
    drop(terminal);

    let unknown = Fixture::new()?;
    unknown.add_agent_task("x", &reporting_body("done"))?;
    unknown.add_agent_task("e", "```bash\necho did nothing\n```\n")?;
    unknown.run_the_queue()?;
    let terminal = unknown.open()?;
    let screen = terminal.wait_for("e's attempt line", |screen| {
        screen.contents().contains("failed-unknown:") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    // `failed-unknown` (14 characters) is wider than `done` (4), so `done`'s status column
    // pads out to match it.
    assert_eq!(lines[4], ">1  #1  done            agent  x");
    assert_eq!(lines[9], " 2  #2  failed-unknown  agent  e");
    assert!(lines[10].contains("failed-unknown:"), "{}", lines[10]);
    assert!(lines[10].contains("reported nothing"), "{}", lines[10]);
    assert_eq!(lines[2], summary(1, 0, 0, 1));
    drop(terminal);

    Ok(())
}

#[test]
fn a_changes_requested_review_shows_its_own_outcome_and_findings() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" changes-requested --reason \"needs docs\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("a's review line", |screen| {
        screen.contents().contains("changes-requested") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  failed  agent  a");
    // The implementation step that passed stays visible above the review that failed it.
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("review · echo") && lines[6].ends_with("changes-requested: needs docs"),
        "{}",
        lines[6]
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_rejecting_tester_shows_its_own_outcome_and_what_failed() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task(
        "a",
        "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" rejected --reason \"login is broken\"\nelse\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
    )?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("a's testing line", |screen| {
        screen.contents().contains("rejected") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  failed  agent  a");
    // The implementation and review steps that passed stay visible above the tester's own.
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("review · echo") && lines[6].ends_with("approved"),
        "{}",
        lines[6]
    );
    assert!(
        lines[7].contains("testing · echo") && lines[7].ends_with("rejected: login is broken"),
        "{}",
        lines[7]
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn navigation_the_cancelled_toggle_and_the_key_map_still_work_with_attempt_lines_shown()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.run_the_queue_to_completion()?;
    fixture.add_agent_task("b", "")?;
    fixture.add_agent_task("c", "")?;
    fixture.cli(&["remove", "3"])?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the queue with a's attempt lines", |screen| {
        let contents = screen.contents();
        contents.contains("commit") && contents.ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    // `pending` (7 characters) is wider than `done` (4), so `done`'s status column pads out
    // to match it.
    assert_eq!(lines[4], ">1  #1  done     agent  a");
    // `a`'s block carries one line per step it ran — implementation, review, testing, commit
    // — so `b`'s own header is pushed down to row 9, not row 6.
    assert!(lines[5].contains("implementation"), "{}", lines[5]);
    assert!(lines[8].contains("commit"), "{}", lines[8]);
    assert_eq!(lines[9], " 2  #2  pending  agent  b");

    // Down moves past the five-line block of the attempted task onto the very next task.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection on b", |screen| {
        lines_inside_frame(&screen.contents())
            .get(9)
            .is_some_and(|line| line.starts_with('>'))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[9], ">2  #2  pending  agent  b");
    assert!(lines[8].contains("commit"), "{}", lines[8]);

    terminal.send("k")?;
    terminal.wait_for("the selection back on a", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with('>'))
    })?;

    terminal.send("G")?;
    terminal.wait_for("the selection on the last task", |screen| {
        lines_inside_frame(&screen.contents())
            .get(9)
            .is_some_and(|line| line.starts_with('>'))
    })?;
    terminal.send("g")?;
    terminal.wait_for("the selection back on the first task", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with('>'))
    })?;

    // The key map replaces the queue, attempt lines included, and comes back unchanged.
    terminal.send("?")?;
    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(!screen.contains("commit"), "{screen}");
    assert!(!screen.contains("pending"), "{screen}");
    terminal.send("?")?;
    let screen = terminal.wait_for("the queue back with a's attempt lines", |screen| {
        let contents = screen.contents();
        !contents.contains("Keys") && contents.contains("commit")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done     agent  a");
    assert!(lines[8].contains("commit"), "{}", lines[8]);

    // The cancelled task, never attempted, appears in its place with no step lines of its
    // own, and the attempted task's block is unaffected.
    terminal.send("a")?;
    let screen = terminal.wait_for("the cancelled task shown", |screen| {
        screen.contents().contains("cancelled  agent  c")
    })?;
    let lines = lines_inside_frame(&screen);
    // `cancelled` (9 characters) is now the widest status shown, so `done`'s and `pending`'s
    // columns pad out to match it.
    assert_eq!(lines[4], ">1  #1  done       agent  a");
    assert!(lines[8].contains("commit"), "{}", lines[8]);
    assert_eq!(lines[9], " 2  #2  pending    agent  b");
    assert_eq!(lines[10], " 3  #3  cancelled  agent  c");

    terminal.send("a")?;
    terminal.wait_for("the cancelled task hidden again", |screen| {
        !screen.contents().contains("cancelled  agent  c")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_task_that_commits_and_pushes_shows_the_dashboard_its_push_line() -> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, scratch) = scratch()?;
    let repository = cloned_repository(&sandbox, &scratch, "my-app")?;
    let bare = scratch.join("my-app.git");
    let set = sandbox.run(
        &repository,
        &["settings", "set", "tracked-branch", "origin/main"],
    )?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    let added = sandbox.run(
        &repository,
        &[
            "add",
            "--title",
            "a",
            "--criterion",
            "it works",
            "--body",
            "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  echo fresh > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n",
        ],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    let mut run = {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.arg("run");
        sandbox.isolate(&mut command, &repository);
        with_nested_ktask_rs_on_path(&mut command);
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        command.spawn()?
    };
    let screen = terminal.wait_for("a's push line with the task shown done", |screen| {
        let contents = screen.contents();
        contents.contains("pushed")
            && lines_inside_frame(&contents)
                .get(4)
                .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    assert!(run.wait()?.success());

    let mut rev_parse = Command::new("git");
    rev_parse.args(["rev-parse", "--short", "HEAD"]);
    let head = sandbox.isolate(&mut rev_parse, &repository).output()?;
    assert!(head.status.success());
    let hash = String::from_utf8(head.stdout)?.trim().to_owned();

    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    // Every step of the attempt shows, in order: sync, implementation, review, testing,
    // commit, then push last.
    assert!(lines[5].contains("sync · -"), "{}", lines[5]);
    assert!(
        lines[6].contains("implementation · echo") && lines[6].ends_with("done"),
        "{}",
        lines[6]
    );
    assert!(
        lines[7].contains("review · echo") && lines[7].ends_with("approved"),
        "{}",
        lines[7]
    );
    assert!(
        lines[8].contains("testing · echo") && lines[8].ends_with("accepted"),
        "{}",
        lines[8]
    );
    assert!(lines[9].contains("commit · -"), "{}", lines[9]);
    assert!(
        lines[10].contains("push · -")
            && lines[10].ends_with(&format!("pushed {hash} to origin/main")),
        "{} (expected hash {hash})",
        lines[10]
    );

    // The remote itself, not merely the push's own exit code, holds the commit: this is what
    // the push step confirmed before it showed `done`.
    let mut ls_remote = Command::new("git");
    ls_remote.args([
        "ls-remote",
        bare.to_str().expect("bare path is text"),
        "refs/heads/main",
    ]);
    let remote = sandbox.isolate(&mut ls_remote, &repository).output()?;
    assert!(remote.status.success());
    let remote_tip = String::from_utf8(remote.stdout)?
        .split_whitespace()
        .next()
        .expect("ls-remote printed a hash")
        .to_owned();
    let mut full_rev_parse = Command::new("git");
    full_rev_parse.args(["rev-parse", "HEAD"]);
    let full_head = sandbox.isolate(&mut full_rev_parse, &repository).output()?;
    assert!(full_head.status.success());
    assert_eq!(remote_tip, String::from_utf8(full_head.stdout)?.trim());

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_finished_step_stays_visible_above_the_one_still_running() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_health_check("true")?;
    let go = fixture.work.join("go");
    fixture.add_agent_task("a", &gated_body(&go))?;

    let mut terminal = fixture.open()?;
    let mut run = fixture.spawn_run()?;
    let screen = terminal.wait_for(
        "the health check passed with the implementation still running above it",
        |screen| {
            let lines = lines_inside_frame(&screen.contents());
            lines
                .get(5)
                .is_some_and(|line| line.contains("health check") && line.ends_with("passed"))
                && lines.get(6).is_some_and(|line| {
                    line.contains("implementation") && line.ends_with("running")
                })
        },
    )?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  running  agent  a");
    assert!(
        lines[5].contains("health check · -") && lines[5].ends_with("passed"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("implementation · echo") && lines[6].ends_with("running"),
        "{}",
        lines[6]
    );

    std::fs::write(&go, "")?;
    let status = run.wait()?;
    assert!(status.success(), "{status:?}");

    // The health check's own line is still there, above every step that followed it, once
    // the attempt has finished.
    let screen = terminal.wait_for("the task done with every step still shown", |screen| {
        let contents = screen.contents();
        contents.contains("commit")
            && lines_inside_frame(&contents)
                .get(4)
                .is_some_and(|line| line.starts_with(">1  #1  done"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines[5].contains("health check · -") && lines[5].ends_with("passed"),
        "{}",
        lines[5]
    );
    assert!(
        lines[6].contains("implementation · echo") && lines[6].ends_with("done"),
        "{}",
        lines[6]
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_failing_health_check_gate_shows_on_the_queue_screen_pending_and_clears_once_a_later_run_gets_past_it()
-> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_health_check("exit 1")?;
    fixture.add_agent_task("a", &reporting_body("done"))?;
    fixture.run_the_queue()?;

    let mut terminal = fixture.open()?;
    let screen = terminal.wait_for("the gate stop shown against the pending task", |screen| {
        screen.contents().contains("health check")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  pending  agent  a");
    assert!(
        lines[5].contains("health check · -") && lines[5].contains("failed"),
        "{}",
        lines[5]
    );
    assert!(lines[5].contains("exited with code 1"), "{}", lines[5]);
    assert!(lines[5].contains("fix the health check"), "{}", lines[5]);

    // The health check now passes: a run made from outside the TUI gets past it, and the
    // dashboard — watching the same journal — drops the earlier stop on its own.
    fixture.set_health_check("true")?;
    fixture.run_the_queue()?;
    let screen = terminal.wait_for(
        "the task done with no trace of the earlier stop",
        |screen| {
            let contents = screen.contents();
            lines_inside_frame(&contents)
                .get(4)
                .is_some_and(|line| line.starts_with(">1  #1  done"))
        },
    )?;
    assert!(!screen.contains("exited with code 1"), "{screen}");
    let lines = lines_inside_frame(&screen);
    assert!(
        lines[5].contains("health check · -") && lines[5].ends_with("passed"),
        "{}",
        lines[5]
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn steps_taller_than_the_screen_scroll_behind_an_ellipsis_until_there_is_room_for_all() -> Result<()>
{
    let sandbox = Sandbox::new()?;
    let (_keep, scratch) = scratch()?;
    let repository = cloned_repository(&sandbox, &scratch, "my-app")?;
    let set = sandbox.run(
        &repository,
        &["settings", "set", "tracked-branch", "origin/main"],
    )?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    let set = sandbox.run(&repository, &["settings", "set", "health-check", "true"])?;
    assert_eq!(set.code, Some(0), "{}", set.stderr);
    // Writes a file so the commit step has something to commit, and so the push step —
    // which only runs once the commit step made a commit — is part of the attempt too.
    let body = "```bash\nif [ \"$3\" = \"review\" ]; then\n  ktask-rs report --token \"$1\" approved\nelif [ \"$3\" = \"testing\" ]; then\n  ktask-rs report --token \"$1\" accepted\nelse\n  echo fresh > new.txt\n  ktask-rs report --token \"$1\" done\nfi\n```\n";
    let added = sandbox.run(
        &repository,
        &[
            "add",
            "--title",
            "a",
            "--criterion",
            "it works",
            "--body",
            body,
        ],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);
    let outcome = sandbox.run_with(&repository, &["run"], with_nested_ktask_rs_on_path)?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);

    // The attempt now carries seven steps — sync, health check, implementation, review,
    // testing, commit, push — one more than fits in the four rows this small a terminal
    // leaves for the list.
    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], 9, 110)?;
    let screen = terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    assert_eq!(lines[5], "      …");
    assert!(lines[6].contains("commit · -"), "{}", lines[6]);
    assert!(lines[7].contains("push · -"), "{}", lines[7]);
    assert!(!screen.contains("sync"), "{screen}");
    assert!(!screen.contains("health check"), "{screen}");
    assert!(!screen.contains("implementation"), "{screen}");
    assert!(!screen.contains("review"), "{screen}");
    assert!(!screen.contains("testing"), "{screen}");

    // Resizing to a terminal tall enough for all seven steps drops the ellipsis and shows
    // every one of them, in order.
    terminal.resize(15, 110)?;
    let screen = terminal.wait_for("every step shown with no ellipsis", |screen| {
        let contents = screen.contents();
        contents.contains("push") && !contents.contains('…')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">1  #1  done  agent  a");
    assert!(lines[5].contains("sync · -"), "{}", lines[5]);
    assert!(lines[6].contains("health check · -"), "{}", lines[6]);
    assert!(lines[7].contains("implementation · echo"), "{}", lines[7]);
    assert!(lines[8].contains("review · echo"), "{}", lines[8]);
    assert!(lines[9].contains("testing · echo"), "{}", lines[9]);
    assert!(lines[10].contains("commit · -"), "{}", lines[10]);
    assert!(lines[11].contains("push · -"), "{}", lines[11]);

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_long_title_on_a_narrow_terminal_is_cut_with_a_trailing_ellipsis() -> Result<()> {
    let fixture = Fixture::new()?;
    let long_title = "x".repeat(100);
    fixture.add_agent_task(&long_title, "")?;

    // `>1  #1  pending  agent  ` (24 characters) leaves 14 of the 38-column list — the
    // 40-column terminal minus the frame's one-column border each side — for the title: 13
    // characters of it, then the ellipsis that replaces the rest.
    let mut terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], 24, 40)?;
    let screen = terminal.wait_for("the queue screen", |screen| {
        screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    let expected_title = format!("{}…", "x".repeat(13));
    assert_eq!(
        lines[4],
        format!(">1  #1  pending  agent  {expected_title}")
    );
    assert!(!screen.contains(&long_title), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_long_failure_reason_on_a_narrow_terminal_is_cut_with_a_trailing_ellipsis() -> Result<()> {
    let fixture = Fixture::new()?;
    let long_reason = "y".repeat(100);
    fixture.add_agent_task("a", &reporting_body_with_reason("failed", &long_reason))?;
    fixture.run_the_queue()?;

    let mut terminal = Terminal::launch(&fixture.sandbox, &fixture.repository, &["tui"], 24, 80)?;
    let screen = terminal.wait_for("a's cut reason", |screen| {
        let contents = screen.contents();
        contents.contains("failed: y") && contents.ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    // The 80-column terminal, minus the frame's one-column border each side, leaves 78 for
    // the implementation line: comfortable room for its own fixed text, so only the reason
    // itself needs cutting.
    assert!(
        lines[5].contains("implementation · echo") && lines[5].contains("failed: y"),
        "{}",
        lines[5]
    );
    assert!(lines[5].ends_with('…'), "{}", lines[5]);
    assert_eq!(lines[5].chars().count(), 78, "{}", lines[5]);
    assert!(!screen.contains(&long_reason), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
