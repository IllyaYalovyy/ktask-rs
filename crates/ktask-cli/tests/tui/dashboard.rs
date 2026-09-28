//! The dashboard: the queue screen shows, under each task that has an attempt, the same
//! line `status` prints, from the same use case, and it updates while a run started
//! elsewhere is in progress.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{self, Signal};
use nix::unistd::Pid;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

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

/// A bash block that reports `outcome` for whatever token it is given as `$1`.
fn reporting_body(outcome: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome}\n```\n")
}

/// A bash block that reports `outcome` with `--reason` for whatever token it is given.
fn reporting_body_with_reason(outcome: &str, reason: &str) -> String {
    format!("```bash\nktask-rs report --token \"$1\" {outcome} --reason \"{reason}\"\n```\n")
}

/// A bash block that waits for the file at `go` to exist, then reports `done`: an attempt
/// that stays running until the test lets it finish.
fn gated_body(go: &Path) -> String {
    format!(
        "```bash\nwhile [ ! -f \"{}\" ]; do sleep 0.02; done\nktask-rs report --token \"$1\" done\n```\n",
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
    assert_eq!(lines[4], ">  1  #1  pending  agent  a");
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
    assert_eq!(lines[4], ">  1  #1  running  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("running"),
        "{}",
        lines[5]
    );
    let first_elapsed = attempt_seconds(&lines[5])?;
    let frames_before = terminal.frame_count();

    // Nothing else touches the journal while the attempt is still gated on `go`: any further
    // frame, and any increase in the elapsed time shown, comes from the loop's own tick.
    std::thread::sleep(Duration::from_millis(2_500));

    let screen = terminal.screen();
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

    let screen = terminal.wait_for("the task done with its attempt line", |screen| {
        screen.contents().contains("done")
            && lines_inside_frame(&screen.contents())
                .get(4)
                .is_some_and(|line| line.starts_with(">  1  #1  done"))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  done  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
    );
    assert!(
        screen.contains("pending 0") && screen.contains("running 0") && screen.contains("done 1"),
        "{screen}"
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

/// Waits, for up to a few seconds, until `condition` holds, polling every 20ms; fails naming
/// `what` when it never does.
fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::sleep(Duration::from_millis(20));
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
    assert_eq!(lines[4], ">  1  #1  interrupted  agent  a");
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
        screen.contents().contains("implementation") && screen.contents().ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  done  agent  a");
    assert!(
        lines[5].contains("implementation · echo") && lines[5].ends_with("done"),
        "{}",
        lines[5]
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
    assert_eq!(lines[4], ">  1  #1  done  agent  x");
    assert_eq!(lines[6], "   2  #2  failed  agent  b");
    assert!(lines[7].ends_with("failed: it broke"), "{}", lines[7]);
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
    assert_eq!(lines[4], ">  1  #1  done  agent  x");
    assert_eq!(lines[6], "   2  #2  failed  agent  c");
    assert!(lines[7].ends_with("too-large: split me"), "{}", lines[7]);
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
    assert_eq!(lines[4], ">  1  #1  done  agent  x");
    assert_eq!(lines[6], "   2  #2  blocked  agent  d");
    assert!(
        lines[7].ends_with("needs-input: which path?"),
        "{}",
        lines[7]
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
    assert_eq!(lines[4], ">  1  #1  done  agent  x");
    assert_eq!(lines[6], "   2  #2  failed-unknown  agent  e");
    assert!(lines[7].contains("failed-unknown:"), "{}", lines[7]);
    assert!(lines[7].contains("reported nothing"), "{}", lines[7]);
    assert_eq!(lines[2], summary(1, 0, 0, 1));
    drop(terminal);

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
    let screen = terminal.wait_for("the queue with a's attempt line", |screen| {
        let contents = screen.contents();
        contents.contains("implementation") && contents.ends_with('┘')
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  done  agent  a");
    assert!(lines[5].contains("implementation"), "{}", lines[5]);
    assert_eq!(lines[6], "   2  #2  pending  agent  b");

    // Down moves past the two-line block of the attempted task onto the very next task.
    terminal.send("j")?;
    let screen = terminal.wait_for("the selection on b", |screen| {
        lines_inside_frame(&screen.contents())
            .get(6)
            .is_some_and(|line| line.starts_with('>'))
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[6], ">  2  #2  pending  agent  b");
    assert!(lines[5].contains("implementation"), "{}", lines[5]);

    terminal.send("k")?;
    terminal.wait_for("the selection back on a", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.starts_with('>'))
    })?;

    terminal.send("G")?;
    terminal.wait_for("the selection on the last task", |screen| {
        lines_inside_frame(&screen.contents())
            .get(6)
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
    assert!(!screen.contains("implementation"), "{screen}");
    assert!(!screen.contains("pending"), "{screen}");
    terminal.send("?")?;
    let screen = terminal.wait_for("the queue back with a's attempt line", |screen| {
        let contents = screen.contents();
        !contents.contains("Keys") && contents.contains("implementation")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  done  agent  a");
    assert!(lines[5].contains("implementation"), "{}", lines[5]);

    // The cancelled task, never attempted, appears in its place with no attempt line of its
    // own, and the attempted task's block is unaffected.
    terminal.send("a")?;
    let screen = terminal.wait_for("the cancelled task shown", |screen| {
        screen.contents().contains("cancelled  agent  c")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[4], ">  1  #1  done  agent  a");
    assert!(lines[5].contains("implementation"), "{}", lines[5]);
    assert_eq!(lines[6], "   2  #2  pending  agent  b");
    assert_eq!(lines[7], "   3  #3  cancelled  agent  c");

    terminal.send("a")?;
    terminal.wait_for("the cancelled task hidden again", |screen| {
        !screen.contents().contains("cancelled  agent  c")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
