//! Terminal user interface.
//!
//! Must stay headlessly testable: keep decision logic pure and confine
//! terminal I/O to a thin shell. See docs/TESTING.md.

pub mod app;
pub mod event;
pub mod testing;
pub mod types;

pub use app::{App, OUTPUT_WINDOW, render, update};
pub use event::AppEvent;
pub use types::{Action, Overlay, Screen, TaskView, ViewOp};

use ktask_core::{EventSeq, Journal, Result};
use std::path::Path;

/// A cursor over a project's journal that yields only what is new.
///
/// The journal is the one place every process writes, so tailing it is what
/// makes a run started by a separate `ktask-rs run` visible here. The tail
/// holds its own connection and the sequence number of the last event it
/// handed out; each [`poll`](JournalTail::poll) asks the journal for
/// `events_since(last_seq)`, so a poll costs in proportion to what is new,
/// never to the length of the journal. The first poll starts from zero and
/// therefore delivers the history once.
#[derive(Debug)]
pub struct JournalTail {
    journal: Journal,
    last_seq: EventSeq,
}

impl JournalTail {
    /// Opens the journal at `path` and positions the tail before its first
    /// event.
    ///
    /// # Errors
    ///
    /// See [`Journal::open`].
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            journal: Journal::open(path)?,
            last_seq: EventSeq::new(0),
        })
    }

    /// The sequence number of the last event this tail delivered, or zero
    /// before it has delivered any.
    #[must_use]
    pub fn last_seq(&self) -> EventSeq {
        self.last_seq
    }

    /// Returns the events recorded since the previous poll as
    /// [`AppEvent::Core`] values, oldest first, and moves the tail past them.
    ///
    /// A failed poll leaves the tail where it was, so the next one retries
    /// the same events rather than skipping them.
    ///
    /// # Errors
    ///
    /// See [`Journal::events_since`].
    pub fn poll(&mut self) -> Result<Vec<AppEvent>> {
        let events = self.journal.events_since(self.last_seq)?;
        if let Some(last) = events.last() {
            self.last_seq = last.seq;
        }
        Ok(events.into_iter().map(AppEvent::Core).collect())
    }

    /// Polls the journal and folds whatever is new into `app`: the interface's
    /// reaction to a clock tick, alongside any in-process bus.
    ///
    /// # Errors
    ///
    /// See [`JournalTail::poll`]. `app` is consumed, so a caller that wants to
    /// keep the old state across a failed poll should clone it first.
    pub fn tick(&mut self, app: App) -> Result<App> {
        Ok(self.poll()?.into_iter().fold(app, update))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::{EventKind, TaskId};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    /// A journal file in its own directory under the system temp directory,
    /// removed on drop.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ktask-tui-tail-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).expect("scratch dir");
            Self(dir)
        }

        fn journal(&self) -> PathBuf {
            self.0.join("journal.db")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn queue(journal: &mut Journal, id: u32, title: &str) -> EventSeq {
        journal
            .append(
                Some(TaskId::new(id)),
                &EventKind::TaskQueued {
                    title: title.into(),
                },
            )
            .expect("append")
    }

    fn titles(app: &App) -> Vec<&str> {
        app.tasks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn tails_the_journal_across_processes() {
        let scratch = Scratch::new();
        // The interface and the run each hold their own connection to the
        // same file, as two processes would; nothing but the file is shared.
        let mut tail = JournalTail::open(&scratch.journal()).expect("open tail");
        let mut writer = Journal::open(&scratch.journal()).expect("open writer");
        let mut app = App::new((80, 24));

        queue(&mut writer, 1, "First");
        app = tail.tick(app).expect("tick");
        assert_eq!(titles(&app), ["First"]);

        queue(&mut writer, 2, "Second");
        app = tail.tick(app).expect("tick");
        assert_eq!(titles(&app), ["First", "Second"]);
    }

    #[test]
    fn tail_reflects_events_appended_from_another_thread_without_a_shared_channel() {
        let scratch = Scratch::new();
        let mut tail = JournalTail::open(&scratch.journal()).expect("open tail");
        let mut writer = Journal::open(&scratch.journal()).expect("open writer");
        let handle = std::thread::spawn(move || {
            for id in 1..=5 {
                queue(&mut writer, id, &format!("Task {id}"));
                std::thread::sleep(Duration::from_millis(5));
            }
        });

        let mut app = App::new((80, 24));
        let deadline = Instant::now() + Duration::from_secs(30);
        while app.tasks.len() < 5 {
            assert!(Instant::now() < deadline, "tail never saw the writer");
            app = tail.tick(app).expect("tick");
            std::thread::sleep(Duration::from_millis(2));
        }
        handle.join().expect("writer thread");

        assert_eq!(
            titles(&app),
            ["Task 1", "Task 2", "Task 3", "Task 4", "Task 5"]
        );
        assert_eq!(tail.last_seq(), EventSeq::new(5));
    }

    #[test]
    fn tail_first_poll_delivers_the_existing_history_in_order() {
        let scratch = Scratch::new();
        let mut writer = Journal::open(&scratch.journal()).expect("open writer");
        queue(&mut writer, 1, "First");
        queue(&mut writer, 2, "Second");

        let mut tail = JournalTail::open(&scratch.journal()).expect("open tail");
        assert_eq!(tail.last_seq(), EventSeq::new(0));
        let events = tail.poll().expect("poll");
        let seqs: Vec<u64> = events
            .iter()
            .map(|e| match e {
                AppEvent::Core(event) => event.seq.get(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(tail.last_seq(), EventSeq::new(2));
    }

    #[test]
    fn tail_poll_delivers_each_event_once_never_rereading_the_journal() {
        let scratch = Scratch::new();
        let mut writer = Journal::open(&scratch.journal()).expect("open writer");
        let mut tail = JournalTail::open(&scratch.journal()).expect("open tail");
        queue(&mut writer, 1, "First");
        queue(&mut writer, 2, "Second");
        assert_eq!(tail.poll().expect("poll").len(), 2);

        assert!(tail.poll().expect("poll").is_empty());

        let third = queue(&mut writer, 3, "Third");
        let events = tail.poll().expect("poll");
        assert_eq!(events.len(), 1);
        assert!(matches!(&events[0], AppEvent::Core(e) if e.seq == third));
        assert_eq!(tail.last_seq(), third);
    }

    #[test]
    fn tail_tick_with_nothing_new_returns_the_app_unchanged() {
        let scratch = Scratch::new();
        let mut tail = JournalTail::open(&scratch.journal()).expect("open tail");
        let app = App::new((80, 24));
        assert_eq!(tail.tick(app.clone()).expect("tick"), app);
    }

    #[test]
    fn tail_open_fails_when_the_journal_cannot_be_opened() {
        let scratch = Scratch::new();
        let missing = scratch.0.join("no-such-dir").join("journal.db");
        assert!(JournalTail::open(&missing).is_err());
    }
}
