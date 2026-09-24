//! What the operator's actions do once they are asked for.
//!
//! [`update`](crate::update) does no I/O, so a key that means "retry" or
//! "resume" only appends an [`Action`] to [`App::outbox`]. This module is the
//! rest of the way. [`Dispatcher::dispatch`] drains the outbox and carries each
//! action out by running the `ktask-rs` command of the same name
//! ([`Action::command`]): the CLI's code, not a second copy of it, so an
//! action cannot behave differently from its command, and a `retry` or a
//! `resume` started here goes on when the interface is closed. The command
//! runs in its own process group with no terminal, so nothing it prints reaches
//! the screen and nothing sent to the interface's group reaches it. Its last
//! line of output is what the operator is told when it finishes
//! ([`Dispatcher::poll`]), through [`App::notice`].
//!
//! The two view operations, attach and open diff, have no command
//! ([`ViewOp`]): [`apply_view`] changes what the interface shows and nothing
//! else.
//!
//! Starting a process is the only I/O and it is behind [`Launch`], so the
//! dispatcher is tested with a launcher that starts nothing, and the real
//! launcher [`Command`] against real processes.

use crate::app::App;
use crate::sanitize::sanitize;
use crate::text::truncate_to_width;
use crate::types::{Action, Screen, ViewOp};
use ktask_core::GateKind;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// The most of a command's output that is read back to find its last line.
const TAIL_BYTES: u64 = 4_096;

/// The widest a message about a finished command is kept, in columns; the
/// screens cut it to what fits.
const NOTICE_WIDTH: usize = 200;

/// The arguments that make `ktask-rs` perform `action` on `project`: the
/// command [`Action::command`] names, the task and the answer or gate it was
/// given as the command's flags, exactly as the contract spells them.
#[must_use]
pub fn arguments(action: &Action, project: &Path) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["--project".into(), project.into(), action.command().into()];
    let mut flag = |name: &str, value: String| {
        args.push(name.into());
        args.push(value.into());
    };
    match action {
        Action::Pause | Action::Interrupt | Action::Resume => {}
        Action::Retry { task } | Action::Cancel { task } => flag("--task", task.to_string()),
        Action::Resolve { task, note } => {
            flag("--task", task.to_string());
            flag("--note", note.clone());
        }
        Action::Acknowledge { task } => {
            if let Some(task) = task {
                flag("--task", task.to_string());
            }
        }
        Action::RerunGate { task, gate } => {
            flag("--task", task.to_string());
            if let Some(gate) = gate {
                flag("--gate", gate_name(*gate));
            }
        }
    }
    args
}

/// The name `--gate` takes for `kind`.
fn gate_name(kind: GateKind) -> String {
    format!("{kind:?}").to_lowercase()
}

/// What an action is called in a message: its command, and the task it acts on.
fn describe(action: &Action) -> String {
    let task = match action {
        Action::Pause | Action::Interrupt | Action::Resume | Action::Acknowledge { task: None } => {
            None
        }
        Action::Retry { task }
        | Action::Resolve { task, .. }
        | Action::Cancel { task }
        | Action::RerunGate { task, .. }
        | Action::Acknowledge { task: Some(task) } => Some(*task),
    };
    match task {
        Some(task) => format!("{} task {task}", action.command()),
        None => action.command().to_owned(),
    }
}

/// How a launched command ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finished {
    /// The exit code, or `None` when it was ended by a signal.
    pub code: Option<i32>,
    /// The last line the command wrote to stdout: what it did.
    pub out: String,
    /// The last line it wrote to stderr: why it did not, or how it went.
    pub err: String,
}

/// Something running.
pub trait Job {
    /// How it ended, once it has; `None` while it is still running. Never
    /// waits.
    fn finished(&mut self) -> Option<Finished>;
}

/// The way commands are started.
pub trait Launch {
    /// What a started command is.
    type Job: Job;

    /// Starts the command with `args` and returns without waiting for it.
    ///
    /// # Errors
    ///
    /// When the command could not be started.
    fn launch(&mut self, args: &[OsString]) -> io::Result<Self::Job>;
}

/// A launcher of `ktask-rs` itself: runs `program` with the arguments
/// [`arguments`] builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    program: PathBuf,
}

impl Command {
    /// A launcher that runs `program`, which should be the `ktask-rs` binary.
    #[must_use]
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// A launcher that runs the binary this process is running.
    ///
    /// # Errors
    ///
    /// When the operating system cannot say which binary that is.
    pub fn current() -> io::Result<Self> {
        std::env::current_exe().map(Self::new)
    }
}

/// A command being run as a child process, its output going to files that no
/// name leads to any more, so they are gone when both ends have closed and
/// the command is not held up by a full pipe if the interface is gone.
#[derive(Debug)]
pub struct Process {
    child: Child,
    out: File,
    err: File,
}

impl Launch for Command {
    type Job = Process;

    fn launch(&mut self, args: &[OsString]) -> io::Result<Process> {
        let out = scratch()?;
        let err = scratch()?;
        let mut command = std::process::Command::new(&self.program);
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out.try_clone()?))
            .stderr(Stdio::from(err.try_clone()?));
        // A signal sent to the interface's process group is not for this.
        #[cfg(unix)]
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let child = command.spawn()?;
        Ok(Process { child, out, err })
    }
}

impl Job for Process {
    fn finished(&mut self) -> Option<Finished> {
        match self.child.try_wait() {
            Ok(None) => None,
            Ok(Some(status)) => Some(Finished {
                code: status.code(),
                out: last_line(&mut self.out),
                err: last_line(&mut self.err),
            }),
            Err(err) => Some(Finished {
                code: None,
                out: String::new(),
                err: err.to_string(),
            }),
        }
    }
}

/// An empty, readable and writable file that is not reachable by name.
fn scratch() -> io::Result<File> {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let path = std::env::temp_dir().join(format!(
        "ktask-tui-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let file = File::options()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;
    // Where a file cannot be removed while it is open it stays, which is
    // untidy and nothing worse.
    let _ = fs::remove_file(&path);
    Ok(file)
}

/// The last non-blank line of the end of `file`, made safe to draw.
fn last_line(file: &mut File) -> String {
    let len = file.metadata().map_or(0, |meta| meta.len());
    let mut bytes = Vec::new();
    let read = file
        .seek(SeekFrom::Start(len.saturating_sub(TAIL_BYTES)))
        .and_then(|_| file.by_ref().take(TAIL_BYTES).read_to_end(&mut bytes));
    if read.is_err() {
        return String::new();
    }
    sanitize(&String::from_utf8_lossy(&bytes))
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| truncate_to_width(line, NOTICE_WIDTH))
        .unwrap_or_default()
}

/// What the operator is told when `action` has finished with `finished`.
fn outcome(action: &Action, finished: &Finished) -> String {
    let command = action.command();
    match finished.code {
        Some(0) if finished.out.is_empty() => format!("{command}: done"),
        Some(0) => finished.out.clone(),
        Some(code) => match (finished.err.is_empty(), finished.out.is_empty()) {
            (false, _) => finished.err.clone(),
            (true, false) => finished.out.clone(),
            (true, true) => format!("{command}: failed (exit {code})"),
        },
        None if finished.err.is_empty() => format!("{command}: ended by a signal"),
        None => finished.err.clone(),
    }
}

/// An action that has been started and not yet finished.
#[derive(Debug)]
struct Running<J> {
    action: Action,
    job: J,
}

/// Carries out the operator's actions: starts what [`App::outbox`] holds and
/// reports each command's end. It belongs to the shell; the state it reads and
/// writes is only [`App::outbox`] and [`App::notice`].
#[derive(Debug)]
pub struct Dispatcher<L: Launch> {
    launcher: L,
    project: PathBuf,
    running: Vec<Running<L::Job>>,
}

impl<L: Launch> Dispatcher<L> {
    /// A dispatcher that starts commands with `launcher`, all of them acting
    /// on the project at `project`.
    #[must_use]
    pub fn new(launcher: L, project: impl Into<PathBuf>) -> Self {
        Self {
            launcher,
            project: project.into(),
            running: Vec::new(),
        }
    }

    /// How many started commands have not finished.
    #[must_use]
    pub fn running(&self) -> usize {
        self.running.len()
    }

    /// Starts every action in the outbox, oldest first, and leaves the outbox
    /// empty. An action already running is not started twice; one that could
    /// not be started says so. The notice is the last thing said.
    pub fn dispatch(&mut self, app: &mut App) {
        for action in app.take_outbox() {
            let what = describe(&action);
            if self.running.iter().any(|running| running.action == action) {
                app.notice = Some(format!("{what}: already in progress"));
                continue;
            }
            match self.launcher.launch(&arguments(&action, &self.project)) {
                Ok(job) => {
                    app.notice = Some(format!("{what}: started"));
                    self.running.push(Running { action, job });
                }
                Err(err) => {
                    let why = sanitize(&err.to_string());
                    app.notice = Some(format!("{what}: could not start: {}", why.trim()));
                }
            }
        }
    }

    /// Collects the commands that have finished since the last look and says
    /// how the last of them ended.
    pub fn poll(&mut self, app: &mut App) {
        let mut unfinished = Vec::new();
        for mut running in std::mem::take(&mut self.running) {
            match running.job.finished() {
                Some(finished) => app.notice = Some(outcome(&running.action, &finished)),
                None => unfinished.push(running),
            }
        }
        self.running = unfinished;
    }
}

/// Whether `op` has something to show, or why it has not: no task is running
/// to attach to, or the task to open is not in the queue.
///
/// # Errors
///
/// The reason, in the words a notice uses.
pub fn check_view(app: &App, op: &ViewOp) -> Result<(), String> {
    match op {
        ViewOp::Attach => {
            if crate::screen::queue::running(app).is_some() {
                Ok(())
            } else {
                Err("attach: no task is running; there is nothing to attach to".to_owned())
            }
        }
        ViewOp::OpenDiff { task } => {
            if app.tasks.iter().any(|view| view.id == *task) {
                Ok(())
            } else {
                Err(format!("open-diff: no task {task} in the queue"))
            }
        }
    }
}

/// Carries out a view operation: shows the live run, following it, or shows a
/// task's diff.
///
/// # Errors
///
/// The reason, when [`check_view`] finds nothing to show. The interface is
/// left as it was.
pub fn apply_view(app: &mut App, op: &ViewOp) -> Result<(), String> {
    check_view(app, op)?;
    match op {
        ViewOp::Attach => {
            app.screen = Screen::LiveRun;
            app.follow = true;
            app.scroll.remove(&Screen::LiveRun);
        }
        ViewOp::OpenDiff { task } => {
            // The git screen shows the task selected on the queue.
            let row = app.tasks.iter().position(|view| view.id == *task);
            app.selected.insert(Screen::Queue, row.unwrap_or_default());
            app.screen = Screen::Git;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TaskView;
    use ktask_core::TaskId;
    use std::collections::VecDeque;
    use std::time::{Duration, Instant};

    fn task(id: u32) -> TaskId {
        TaskId::new(id)
    }

    fn strings(args: &[OsString]) -> Vec<&str> {
        args.iter()
            .map(|arg| arg.to_str().expect("utf-8"))
            .collect()
    }

    fn every_action() -> Vec<Action> {
        vec![
            Action::Pause,
            Action::Interrupt,
            Action::Resume,
            Action::Retry { task: task(3) },
            Action::Resolve {
                task: task(4),
                note: "use sqlite, not postgres".into(),
            },
            Action::Acknowledge { task: None },
            Action::Acknowledge {
                task: Some(task(5)),
            },
            Action::Cancel { task: task(6) },
            Action::RerunGate {
                task: task(7),
                gate: None,
            },
            Action::RerunGate {
                task: task(8),
                gate: Some(GateKind::Lint),
            },
        ]
    }

    // ---- the command every action is ----

    #[test]
    fn actions_become_the_cli_commands_with_the_flags_the_contract_spells() {
        let project = Path::new("/work/repo");
        let want: [&[&str]; 10] = [
            &["pause"],
            &["interrupt"],
            &["resume"],
            &["retry", "--task", "3"],
            &[
                "resolve",
                "--task",
                "4",
                "--note",
                "use sqlite, not postgres",
            ],
            &["ack"],
            &["ack", "--task", "5"],
            &["cancel", "--task", "6"],
            &["rerun-gate", "--task", "7"],
            &["rerun-gate", "--task", "8", "--gate", "lint"],
        ];
        for (action, want) in every_action().iter().zip(want) {
            let args = arguments(action, project);
            let mut expected = vec!["--project", "/work/repo"];
            expected.extend(want);
            assert_eq!(strings(&args), expected, "{action:?}");
        }
    }

    #[test]
    fn actions_every_command_they_name_is_an_action_of_the_contract() {
        let commands: Vec<&str> = every_action().iter().map(Action::command).collect();
        for command in [
            "pause",
            "interrupt",
            "resume",
            "retry",
            "resolve",
            "ack",
            "cancel",
            "rerun-gate",
        ] {
            assert!(commands.contains(&command), "no action runs {command}");
        }
    }

    #[test]
    fn actions_gate_names_are_the_ones_the_command_line_parses() {
        let names: Vec<String> = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
        ]
        .into_iter()
        .map(gate_name)
        .collect();
        assert_eq!(
            names,
            [
                "baseline", "targeted", "verify", "lint", "format", "build", "privacy"
            ]
        );
    }

    #[test]
    fn actions_a_note_is_one_argument_whatever_it_holds() {
        let action = Action::Resolve {
            task: task(1),
            note: "--task 9; rm -rf /\n\"quoted\"".into(),
        };
        let args = arguments(&action, Path::new("/p"));
        assert_eq!(args.len(), 7);
        assert_eq!(args[6], OsString::from("--task 9; rm -rf /\n\"quoted\""));
    }

    // ---- what is told about an action ----

    #[test]
    fn actions_are_described_by_command_and_task() {
        let described: Vec<String> = every_action().iter().map(describe).collect();
        assert_eq!(
            described,
            [
                "pause",
                "interrupt",
                "resume",
                "retry task 3",
                "resolve task 4",
                "ack",
                "ack task 5",
                "cancel task 6",
                "rerun-gate task 7",
                "rerun-gate task 8",
            ]
        );
    }

    fn finished(code: Option<i32>, out: &str, err: &str) -> Finished {
        Finished {
            code,
            out: out.into(),
            err: err.into(),
        }
    }

    #[test]
    fn actions_a_success_says_what_the_command_printed_or_that_it_is_done() {
        let cancel = Action::Cancel { task: task(2) };
        assert_eq!(
            outcome(&cancel, &finished(Some(0), "task 2 cancelled", "noise")),
            "task 2 cancelled"
        );
        assert_eq!(
            outcome(&cancel, &finished(Some(0), "", "noise")),
            "cancel: done"
        );
    }

    #[test]
    fn actions_a_failure_says_why_from_stderr_then_stdout_then_the_exit_code() {
        let retry = Action::Retry { task: task(2) };
        assert_eq!(
            outcome(&retry, &finished(Some(2), "out", "retry: task 2 is queued")),
            "retry: task 2 is queued"
        );
        assert_eq!(outcome(&retry, &finished(Some(1), "out", "")), "out");
        assert_eq!(
            outcome(&retry, &finished(Some(1), "", "")),
            "retry: failed (exit 1)"
        );
    }

    #[test]
    fn actions_a_command_ended_by_a_signal_says_so_or_says_why() {
        let pause = Action::Pause;
        assert_eq!(
            outcome(&pause, &finished(None, "", "")),
            "pause: ended by a signal"
        );
        assert_eq!(
            outcome(&pause, &finished(None, "", "wait failed")),
            "wait failed"
        );
    }

    // ---- dispatching ----

    /// A launcher that starts nothing, remembers what it was asked and hands
    /// out jobs the test finishes by hand.
    #[derive(Debug, Default)]
    struct Fake {
        launched: Vec<Vec<String>>,
        jobs: VecDeque<Option<Finished>>,
        refuse: bool,
    }

    #[derive(Debug)]
    struct FakeJob(Option<Finished>);

    impl Job for FakeJob {
        fn finished(&mut self) -> Option<Finished> {
            self.0.take()
        }
    }

    impl Launch for Fake {
        type Job = FakeJob;

        fn launch(&mut self, args: &[OsString]) -> io::Result<FakeJob> {
            if self.refuse {
                return Err(io::Error::other("no such binary"));
            }
            self.launched
                .push(strings(args).into_iter().map(str::to_owned).collect());
            Ok(FakeJob(self.jobs.pop_front().flatten()))
        }
    }

    fn app_asking(actions: Vec<Action>) -> App {
        App {
            outbox: actions,
            ..App::new((80, 24))
        }
    }

    #[test]
    fn actions_dispatch_starts_every_action_in_the_outbox_once_and_empties_it() {
        let mut dispatcher = Dispatcher::new(Fake::default(), "/work/repo");
        let mut app = app_asking(every_action());
        dispatcher.dispatch(&mut app);
        assert!(app.outbox.is_empty());
        assert_eq!(dispatcher.running(), every_action().len());
        let commands: Vec<&str> = dispatcher
            .launcher
            .launched
            .iter()
            .map(|args| args[2].as_str())
            .collect();
        assert_eq!(
            commands,
            [
                "pause",
                "interrupt",
                "resume",
                "retry",
                "resolve",
                "ack",
                "ack",
                "cancel",
                "rerun-gate",
                "rerun-gate"
            ]
        );
        assert!(
            dispatcher
                .launcher
                .launched
                .iter()
                .all(|args| args[..2] == ["--project", "/work/repo"])
        );
        // Dispatching again starts nothing.
        dispatcher.dispatch(&mut app);
        assert_eq!(dispatcher.launcher.launched.len(), every_action().len());
    }

    #[test]
    fn actions_dispatch_says_what_it_started() {
        let mut dispatcher = Dispatcher::new(Fake::default(), "/p");
        let mut app = app_asking(vec![Action::Retry { task: task(3) }]);
        dispatcher.dispatch(&mut app);
        assert_eq!(app.notice.as_deref(), Some("retry task 3: started"));
    }

    #[test]
    fn actions_dispatch_with_an_empty_outbox_changes_nothing() {
        let mut dispatcher = Dispatcher::new(Fake::default(), "/p");
        let mut app = App::new((80, 24));
        app.notice = Some("kept".into());
        let before = app.clone();
        dispatcher.dispatch(&mut app);
        assert_eq!(app, before);
        assert!(dispatcher.launcher.launched.is_empty());
    }

    #[test]
    fn actions_dispatch_does_not_start_an_action_that_is_already_running() {
        let mut dispatcher = Dispatcher::new(Fake::default(), "/p");
        let retry = Action::Retry { task: task(3) };
        dispatcher.dispatch(&mut app_asking(vec![retry.clone()]));
        let mut app = app_asking(vec![retry, Action::Retry { task: task(4) }]);
        dispatcher.dispatch(&mut app);
        assert_eq!(dispatcher.running(), 2);
        assert_eq!(dispatcher.launcher.launched.len(), 2);
        assert_eq!(app.notice.as_deref(), Some("retry task 4: started"));

        let mut app = app_asking(vec![Action::Retry { task: task(3) }]);
        dispatcher.dispatch(&mut app);
        assert_eq!(dispatcher.launcher.launched.len(), 2);
        assert_eq!(
            app.notice.as_deref(),
            Some("retry task 3: already in progress")
        );
    }

    #[test]
    fn actions_dispatch_reports_a_command_that_could_not_be_started() {
        let launcher = Fake {
            refuse: true,
            ..Fake::default()
        };
        let mut dispatcher = Dispatcher::new(launcher, "/p");
        let mut app = app_asking(vec![Action::Pause]);
        dispatcher.dispatch(&mut app);
        assert!(app.outbox.is_empty());
        assert_eq!(dispatcher.running(), 0);
        assert_eq!(
            app.notice.as_deref(),
            Some("pause: could not start: no such binary")
        );
    }

    #[test]
    fn actions_poll_reports_a_finished_command_once_and_forgets_it() {
        let launcher = Fake {
            jobs: VecDeque::from([Some(finished(Some(0), "task 6 cancelled", ""))]),
            ..Fake::default()
        };
        let mut dispatcher = Dispatcher::new(launcher, "/p");
        let mut app = app_asking(vec![Action::Cancel { task: task(6) }]);
        dispatcher.dispatch(&mut app);
        dispatcher.poll(&mut app);
        assert_eq!(app.notice.as_deref(), Some("task 6 cancelled"));
        assert_eq!(dispatcher.running(), 0);
        app.notice = None;
        dispatcher.poll(&mut app);
        assert_eq!(app.notice, None);
    }

    #[test]
    fn actions_poll_leaves_a_running_command_running_and_says_nothing() {
        let launcher = Fake {
            jobs: VecDeque::from([None]),
            ..Fake::default()
        };
        let mut dispatcher = Dispatcher::new(launcher, "/p");
        let mut app = app_asking(vec![Action::Resume]);
        dispatcher.dispatch(&mut app);
        app.notice = None;
        dispatcher.poll(&mut app);
        assert_eq!(dispatcher.running(), 1);
        assert_eq!(app.notice, None);
    }

    #[test]
    fn actions_poll_reaps_every_finished_command_and_keeps_the_others() {
        let launcher = Fake {
            jobs: VecDeque::from([
                Some(finished(Some(0), "first", "")),
                None,
                Some(finished(Some(0), "third", "")),
            ]),
            ..Fake::default()
        };
        let mut dispatcher = Dispatcher::new(launcher, "/p");
        let mut app = app_asking(vec![Action::Pause, Action::Interrupt, Action::Resume]);
        dispatcher.dispatch(&mut app);
        dispatcher.poll(&mut app);
        assert_eq!(dispatcher.running(), 1);
        assert_eq!(dispatcher.running[0].action, Action::Interrupt);
        assert_eq!(app.notice.as_deref(), Some("third"));
    }

    #[test]
    fn actions_a_finished_command_may_be_started_again() {
        let launcher = Fake {
            jobs: VecDeque::from([Some(finished(Some(1), "", "retry: task 3 failed again"))]),
            ..Fake::default()
        };
        let mut dispatcher = Dispatcher::new(launcher, "/p");
        let retry = Action::Retry { task: task(3) };
        let mut app = app_asking(vec![retry.clone()]);
        dispatcher.dispatch(&mut app);
        dispatcher.poll(&mut app);
        assert_eq!(app.notice.as_deref(), Some("retry: task 3 failed again"));
        app.outbox.push(retry);
        dispatcher.dispatch(&mut app);
        assert_eq!(dispatcher.launcher.launched.len(), 2);
        assert_eq!(dispatcher.running(), 1);
    }

    // ---- real processes ----

    /// Runs `script` under `sh` and waits for it.
    fn run_sh(script: &str) -> Finished {
        let mut launcher = Command::new("sh");
        let mut job = launcher
            .launch(&["-c".into(), script.into()])
            .expect("sh starts");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(done) = job.finished() {
                return done;
            }
            assert!(Instant::now() < deadline, "the command never finished");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn actions_a_real_command_reports_its_exit_code_and_last_lines() {
        let done = run_sh("echo first; echo 'task 3 acknowledged'; echo oops >&2; exit 0");
        assert_eq!(done, finished(Some(0), "task 3 acknowledged", "oops"));
        let done = run_sh("echo out; echo 'why not' >&2; exit 2");
        assert_eq!(done, finished(Some(2), "out", "why not"));
    }

    #[test]
    fn actions_a_real_command_is_not_reported_until_it_has_ended() {
        let mut launcher = Command::new("sh");
        let mut job = launcher
            .launch(&["-c".into(), "sleep 30".into()])
            .expect("sh starts");
        assert_eq!(job.finished(), None);
        job.child.kill().expect("kill");
        let deadline = Instant::now() + Duration::from_secs(30);
        let done = loop {
            if let Some(done) = job.finished() {
                break done;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(done.code, None);
    }

    #[test]
    fn actions_a_real_commands_output_is_made_safe_and_bounded() {
        let done = run_sh("printf 'ok \\033[31mred\\033[0m\\n' >&2; exit 1");
        assert_eq!(done.err, "ok red");
        // A flood cannot fill anything: only the end of the output is read,
        // and only its last line kept.
        let done = run_sh("head -c 1000000 /dev/zero | tr '\\0' 'z'; echo; echo end");
        assert_eq!(done.out, "end");
        let done = run_sh("head -c 1000000 /dev/zero | tr '\\0' 'z'");
        assert_eq!(done.out.chars().count(), NOTICE_WIDTH);
    }

    #[test]
    fn actions_a_real_command_that_cannot_start_is_an_error() {
        let mut launcher = Command::new("/no/such/ktask-rs");
        assert!(launcher.launch(&[]).is_err());
    }

    #[test]
    fn actions_a_real_command_has_no_terminal_input() {
        // `read` would hang on an open terminal; on no input it ends at once.
        let done = run_sh("read line; echo \"got:$line\"");
        assert_eq!(done.code, Some(0));
        assert_eq!(done.out, "got:");
    }

    #[test]
    fn actions_the_current_binary_is_a_launcher() {
        let launcher = Command::current().expect("current exe");
        assert_eq!(
            launcher.program,
            std::env::current_exe().expect("current exe")
        );
    }

    #[test]
    fn actions_a_dispatcher_runs_a_real_command_to_the_end() {
        let mut dispatcher = Dispatcher::new(Command::new("true"), "/p");
        let mut app = app_asking(vec![Action::Pause]);
        dispatcher.dispatch(&mut app);
        let deadline = Instant::now() + Duration::from_secs(30);
        while dispatcher.running() > 0 {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
            dispatcher.poll(&mut app);
        }
        assert_eq!(app.notice.as_deref(), Some("pause: done"));
    }

    // ---- the view operations ----

    fn view(id: u32, state: &str) -> TaskView {
        TaskView {
            id: task(id),
            title: format!("Task {id}"),
            state: state.to_owned(),
            protocol: String::new(),
            phase: None,
            attempts: 0,
            elapsed: None,
        }
    }

    fn app_with(states: &[&str]) -> App {
        App {
            tasks: states
                .iter()
                .zip(1..)
                .map(|(state, id)| view(id, state))
                .collect(),
            ..App::new((80, 24))
        }
    }

    #[test]
    fn actions_attach_shows_the_live_run_following_it() {
        let mut app = app_with(&["Done", "Running"]);
        app.follow = false;
        app.scroll.insert(Screen::LiveRun, 12);
        assert_eq!(apply_view(&mut app, &ViewOp::Attach), Ok(()));
        assert_eq!(app.screen, Screen::LiveRun);
        assert!(app.follow);
        assert_eq!(app.scroll.get(&Screen::LiveRun), None);
        assert!(app.outbox.is_empty());
    }

    #[test]
    fn actions_attach_needs_a_task_in_flight_and_otherwise_changes_nothing() {
        for states in [&[][..], &["Queued", "Done", "Failed", "Cancelled"][..]] {
            let mut app = app_with(states);
            app.follow = false;
            let before = app.clone();
            assert_eq!(
                apply_view(&mut app, &ViewOp::Attach),
                Err("attach: no task is running; there is nothing to attach to".to_owned())
            );
            assert_eq!(app, before);
        }
    }

    #[test]
    fn actions_attach_accepts_every_state_in_which_an_attempt_is_in_flight() {
        for state in [
            "Preflight",
            "Running",
            "Remediating",
            "Verifying",
            "Publishing",
        ] {
            let mut app = app_with(&[state]);
            assert_eq!(apply_view(&mut app, &ViewOp::Attach), Ok(()), "{state}");
        }
    }

    #[test]
    fn actions_open_diff_selects_the_task_on_the_queue_and_shows_the_git_screen() {
        let mut app = app_with(&["Done", "Failed", "Queued"]);
        assert_eq!(
            apply_view(&mut app, &ViewOp::OpenDiff { task: task(2) }),
            Ok(())
        );
        assert_eq!(app.screen, Screen::Git);
        assert_eq!(app.selected.get(&Screen::Queue), Some(&1));
        assert!(app.outbox.is_empty());
    }

    #[test]
    fn actions_open_diff_of_a_task_not_in_the_queue_changes_nothing() {
        let mut app = app_with(&["Done"]);
        let before = app.clone();
        assert_eq!(
            apply_view(&mut app, &ViewOp::OpenDiff { task: task(9) }),
            Err("open-diff: no task 9 in the queue".to_owned())
        );
        assert_eq!(app, before);
    }

    // ---- from a key to the command ----

    fn core(id: u32, kind: ktask_core::EventKind) -> crate::event::AppEvent {
        crate::event::AppEvent::Core(ktask_core::Event {
            seq: ktask_core::EventSeq::new(1),
            ts: time::OffsetDateTime::UNIX_EPOCH,
            task_id: Some(task(id)),
            kind,
        })
    }

    fn enter(harness: &mut crate::testing::Harness) {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        harness.send(crate::event::AppEvent::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    /// Asks for every action of the contract's section 4 by the keys that
    /// mean them, in the states that allow each, and returns what was asked.
    fn asked_for_by_keys() -> Vec<Action> {
        use crate::testing::Harness;
        use ktask_core::{AttemptId, DecisionRequest, EventKind, PauseReason};

        let mut asked = Vec::new();

        // A running task: pause and interrupt.
        let mut harness = Harness::from_app(app_with(&["Running"]));
        harness.key('p');
        harness.key('i');
        asked.extend(harness.app().outbox.clone());

        // A failed task, and one at a human gate, with nothing running.
        let mut harness = Harness::from_app(app_with(&["Failed", "Queued"]));
        harness.send(core(
            2,
            EventKind::Paused {
                reason: PauseReason::HumanGate,
            },
        ));
        for key in ['r', 'c', 'x', 'R', 'j', 'A'] {
            harness.key(key);
        }
        asked.extend(harness.app().outbox.clone());

        // A question, answered in the inbox.
        let mut harness = Harness::from_app(app_with(&["Queued"]));
        for kind in [
            EventKind::PreflightStarted,
            EventKind::PreflightPassed {
                base_sha: "abc".into(),
            },
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "direct".into(),
                pid: 1,
                base_sha: "abc".into(),
            },
            EventKind::DecisionRaised {
                request: DecisionRequest {
                    question: "Which?".into(),
                    options: vec!["a".into(), "b".into()],
                    tradeoffs: "none".into(),
                    impact: "none".into(),
                    recommended: None,
                },
            },
        ] {
            harness.send(core(1, kind));
        }
        for key in ['6', 'a', 'a', 'b'] {
            harness.key(key);
        }
        enter(&mut harness);
        asked.extend(harness.app().outbox.clone());
        asked
    }

    #[test]
    fn actions_every_action_of_the_contract_is_asked_for_by_a_key() {
        let asked = asked_for_by_keys();
        let commands: std::collections::BTreeSet<&str> =
            asked.iter().map(Action::command).collect();
        let contract: std::collections::BTreeSet<&str> = [
            "pause",
            "interrupt",
            "resume",
            "retry",
            "resolve",
            "ack",
            "cancel",
            "rerun-gate",
        ]
        .into_iter()
        .collect();
        assert_eq!(commands, contract, "{asked:?}");
        assert!(asked.contains(&Action::Resolve {
            task: task(1),
            note: "ab".into()
        }));
        assert!(asked.contains(&Action::Acknowledge {
            task: Some(task(2))
        }));
    }

    #[test]
    fn actions_every_action_asked_for_by_a_key_is_started_as_its_command() {
        let asked = asked_for_by_keys();
        let mut dispatcher = Dispatcher::new(Fake::default(), "/work/repo");
        let mut app = app_asking(asked.clone());
        dispatcher.dispatch(&mut app);
        assert!(app.outbox.is_empty());
        let launched: Vec<Vec<String>> = dispatcher.launcher.launched.clone();
        assert_eq!(launched.len(), asked.len());
        for (action, args) in asked.iter().zip(&launched) {
            assert_eq!(args[..2], ["--project", "/work/repo"]);
            assert_eq!(args[2], action.command());
        }
        assert!(launched.contains(&vec![
            "--project".to_owned(),
            "/work/repo".to_owned(),
            "resolve".to_owned(),
            "--task".to_owned(),
            "1".to_owned(),
            "--note".to_owned(),
            "ab".to_owned(),
        ]));
    }

    #[test]
    fn actions_the_two_view_operations_are_reached_by_keys_and_ask_for_nothing() {
        use crate::testing::Harness;
        let mut harness = Harness::from_app(app_with(&["Done", "Running"]));
        harness.key('a');
        assert_eq!(harness.app().screen, Screen::LiveRun);
        harness.key('1');
        harness.key('d');
        assert_eq!(harness.app().screen, Screen::Git);
        assert!(harness.app().outbox.is_empty());
    }
}
