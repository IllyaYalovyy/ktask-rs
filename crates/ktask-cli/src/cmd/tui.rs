//! `ktask-rs tui`: launches the interface described in `docs/CONTRACT.md`
//! section 4. Requires a terminal.
//!
//! This is the only module in the crate that names `ktask-tui`. The command
//! is a separate process from any `ktask-rs run`, so it has no in-process
//! [`ktask_core::Bus`] to subscribe to; the journal, which every process
//! writes, is the bus between them. The command loads the history from it
//! before the terminal is touched, then keeps a [`JournalTail`] running on a
//! thread that feeds each newly recorded event into the channel
//! [`ktask_tui::terminal::run`] drains.
//!
//! Nothing here writes to the journal: closing the interface never disturbs
//! a run. The actions the operator asks for in the interface are carried out
//! by running this same binary's command of the same name
//! ([`ktask_tui::actions`]), against this project, so they are the CLI's own
//! operations and go on when the interface is closed.

use ktask_core::{Config, Event, Project, Result, RunOutcome, journal_path};
use ktask_tui::actions::{Command, Dispatcher};
use ktask_tui::terminal::{self, is_not_a_terminal};
use ktask_tui::{App, AppEvent, JournalTail};
use std::io::{self, IsTerminal};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

/// The size the state starts with. The shell draws into the real terminal
/// whatever this says; it only seeds the view state until the first resize.
const INITIAL_SIZE: (u16, u16) = (80, 24);

/// How long the follower thread waits between looks at the journal.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(100);

/// Opens the interface on the real terminal over `project`'s journal.
///
/// Exits 2 (a usage error) when stdout is not a terminal, before the journal
/// is touched; 1 when the journal cannot be read or the terminal fails.
pub(crate) fn run(project: &Project, _config: &Config) -> RunOutcome {
    run_with(project, io::stdout().is_terminal(), |app, rx| {
        let actions = Dispatcher::new(Command::current()?, &project.root);
        terminal::run(app, rx, actions)
    })
}

/// [`run`] with the terminal check and the interface itself passed in, so a
/// test needs neither a terminal nor a real event loop.
fn run_with(
    project: &Project,
    stdout_is_terminal: bool,
    launch: impl FnOnce(App, Receiver<Event>) -> Result<()>,
) -> RunOutcome {
    if !stdout_is_terminal {
        return RunOutcome::Usage {
            detail: "tui: ktask-rs tui needs a terminal, but stdout is not one; \
                     use `ktask-rs status` for scripting"
                .to_string(),
        };
    }

    let mut tail = match JournalTail::open(&journal_path(&project.state_dir)) {
        Ok(tail) => tail,
        Err(err) => return check_failed(format!("tui: could not open the journal: {err}")),
    };
    let app = match tail.tick(App::new(INITIAL_SIZE)) {
        Ok(app) => app,
        Err(err) => return check_failed(format!("tui: could not read the journal: {err}")),
    };

    let (tx, rx) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    let follower = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || follow(tail, &tx, &stop))
    };
    let launched = launch(app, rx);
    stop.store(true, Ordering::Relaxed);
    // The follower only ever sleeps, polls and sends, so it cannot panic
    // short of a broken journal driver; either way the interface is done.
    let _ = follower.join();

    match launched {
        Ok(()) => RunOutcome::Drained,
        Err(err) if is_not_a_terminal(&err) => RunOutcome::Usage {
            detail: format!("tui: {err}"),
        },
        Err(err) => check_failed(format!("tui: {err}")),
    }
}

fn check_failed(detail: String) -> RunOutcome {
    RunOutcome::CheckFailed { detail }
}

/// Forwards every event recorded after `tail`'s position to `tx` until
/// `stop` is set or the receiver is gone.
///
/// A failed poll is skipped, not fatal: [`JournalTail::poll`] leaves the tail
/// where it was, so the next look retries the same events.
fn follow(mut tail: JournalTail, tx: &mpsc::Sender<Event>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(FOLLOW_INTERVAL);
        let Ok(events) = tail.poll() else { continue };
        for event in events {
            if let AppEvent::Core(event) = event
                && tx.send(event).is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{Error, EventKind, Journal, TaskId};
    use std::time::Instant;

    fn project(dir: &tempfile::TempDir) -> Project {
        Project {
            root: dir.path().join("repo"),
            id: "tui-fixture".to_string(),
            state_dir: dir.path().to_path_buf(),
        }
    }

    fn queue(journal: &mut Journal, id: u32, title: &str) {
        journal
            .append(
                Some(TaskId::new(id)),
                &EventKind::TaskQueued {
                    title: title.into(),
                },
            )
            .expect("append");
    }

    fn titles(app: &App) -> Vec<&str> {
        app.tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn a_non_terminal_stdout_exits_2_with_a_message_and_never_launches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut launched = false;

        let outcome = run_with(&project(&dir), false, |_, _| {
            launched = true;
            Ok(())
        });

        assert!(!launched, "the interface must not start without a terminal");
        assert!(
            matches!(&outcome, RunOutcome::Usage { detail }
                if detail.contains("needs a terminal") && detail.contains("stdout")),
            "{outcome:?}"
        );
        assert!(
            !journal_path(&project(&dir).state_dir).exists(),
            "a refused launch must not touch the journal"
        );
    }

    #[test]
    fn the_real_entry_point_refuses_when_stdout_is_captured() {
        // Under the test harness stdout is not a terminal, so `run` takes the
        // refusal path without opening anything.
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run(&project(&dir), &Config::default());
        assert!(matches!(outcome, RunOutcome::Usage { .. }), "{outcome:?}");
    }

    #[test]
    fn the_interface_starts_with_the_journals_history_already_loaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project(&dir);
        let mut journal = Journal::open_for(&project).expect("journal");
        queue(&mut journal, 1, "First");
        queue(&mut journal, 2, "Second");
        let mut seen = Vec::new();

        let outcome = run_with(&project, true, |app, _| {
            seen = titles(&app).into_iter().map(str::to_owned).collect();
            Ok(())
        });

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(seen, ["First", "Second"]);
    }

    #[test]
    fn events_recorded_while_it_is_open_reach_the_interface_once_and_in_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project(&dir);
        let mut journal = Journal::open_for(&project).expect("journal");
        queue(&mut journal, 1, "Before");
        let mut received = Vec::new();

        let outcome = run_with(&project, true, |_, rx| {
            queue(&mut journal, 2, "During A");
            queue(&mut journal, 3, "During B");
            let deadline = Instant::now() + Duration::from_secs(30);
            while received.len() < 2 && Instant::now() < deadline {
                if let Ok(event) = rx.recv_timeout(Duration::from_millis(50)) {
                    received.push(event.seq.get());
                }
            }
            // Nothing already in the history is sent a second time.
            assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
            Ok(())
        });

        assert_eq!(outcome, RunOutcome::Drained);
        assert_eq!(received, [2, 3]);
    }

    #[test]
    fn a_launch_that_finds_no_terminal_is_a_usage_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_with(&project(&dir), true, |_, _| {
            Err(Error::Io(io::Error::new(
                io::ErrorKind::Unsupported,
                "no terminal",
            )))
        });
        assert!(
            matches!(&outcome, RunOutcome::Usage { detail } if detail.contains("no terminal")),
            "{outcome:?}"
        );
    }

    #[test]
    fn any_other_launch_failure_exits_1_and_says_what_failed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outcome = run_with(&project(&dir), true, |_, _| {
            Err(Error::Io(io::Error::other("draw broke")))
        });
        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail } if detail.contains("draw broke")),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_journal_that_cannot_be_opened_exits_1_without_launching() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = Project {
            state_dir: dir.path().join("no-such-dir"),
            ..project(&dir)
        };
        let mut launched = false;

        let outcome = run_with(&project, true, |_, _| {
            launched = true;
            Ok(())
        });

        assert!(!launched);
        assert!(
            matches!(&outcome, RunOutcome::CheckFailed { detail } if detail.contains("journal")),
            "{outcome:?}"
        );
    }

    #[test]
    fn the_follower_stops_when_the_receiver_is_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project(&dir);
        let mut journal = Journal::open_for(&project).expect("journal");
        let tail = JournalTail::open(&journal_path(&project.state_dir)).expect("tail");
        let (tx, rx) = mpsc::channel();
        drop(rx);
        queue(&mut journal, 1, "Orphan");

        // Returns instead of looping forever: the send fails.
        follow(tail, &tx, &AtomicBool::new(false));
    }

    #[test]
    fn the_follower_stops_when_told_to() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = project(&dir);
        drop(Journal::open_for(&project).expect("journal"));
        let tail = JournalTail::open(&journal_path(&project.state_dir)).expect("tail");
        let (tx, _rx) = mpsc::channel();

        follow(tail, &tx, &AtomicBool::new(true));
    }
}
