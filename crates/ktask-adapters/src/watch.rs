//! Notices when a project's journal file changes, using the platform's file-notification
//! mechanism (`inotify` on Linux, through the `notify` crate) instead of polling it.

use std::path::Path;
use std::sync::mpsc::{self, Receiver};

use ktask_core::{JournalError, JournalWatch};
use notify::{RecommendedWatcher, RecursiveMode, Watcher as _};

/// Watches one journal file for changes made by any process, including this one.
#[derive(Debug)]
pub struct FileJournalWatch {
    /// Kept alive only to keep the watch running; never read from directly.
    _watcher: RecommendedWatcher,
    events: Receiver<notify::Result<notify::Event>>,
}

impl FileJournalWatch {
    /// Starts watching `path` for changes: a single file when `recursive` is false, or every
    /// file under it, including ones created in a subdirectory that does not exist yet, when
    /// it is true — used to watch every registered project's journal at once, so a project
    /// switched to after the watch started is covered too.
    ///
    /// # Errors
    ///
    /// Fails when the platform's file-notification mechanism cannot be started or cannot
    /// watch `path`.
    pub fn open(path: &Path, recursive: bool) -> Result<Self, JournalError> {
        let doing = format!("cannot watch the journal {}", path.display());
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)
            .map_err(|e| JournalError::new(format!("{doing}: {e}")))?;
        watcher
            .watch(path, mode)
            .map_err(|e| JournalError::new(format!("{doing}: {e}")))?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }
}

impl JournalWatch for FileJournalWatch {
    fn wait(&self) -> Result<(), JournalError> {
        loop {
            match self.events.recv() {
                // A recursive watch reports one of these for every pre-existing subdirectory
                // the moment it starts watching it, as the mechanism that keeps watching new
                // ones as they appear — not a change to anything, so it is not one this
                // should wake for.
                Ok(Ok(event)) if event.kind.is_access() => {}
                Ok(Ok(_)) => return Ok(()),
                Ok(Err(e)) => {
                    return Err(JournalError::new(format!("the journal watch failed: {e}")));
                }
                Err(_) => return Err(JournalError::new("the journal watch stopped")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn a_change_written_to_the_watched_file_wakes_a_blocked_wait() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("journal.db");
        std::fs::write(&path, "").unwrap();
        let watch = FileJournalWatch::open(&path, false).unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(watch.wait()));

        std::fs::write(&path, "changed").unwrap();

        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("wait() returned once the file changed")
            .unwrap();
    }

    #[test]
    fn a_path_whose_directory_does_not_exist_is_an_error_naming_it() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("journal.db");
        let error = FileJournalWatch::open(&path, false)
            .unwrap_err()
            .to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }

    #[test]
    fn a_recursive_watch_wakes_on_a_file_in_a_subdirectory_created_after_it_started() {
        let dir = TempDir::new().unwrap();
        let watch = FileJournalWatch::open(dir.path(), true).unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(watch.wait()));

        let project = dir.path().join("my-app");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("journal.db"), "changed").unwrap();

        receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("wait() returned once a file changed under a new subdirectory")
            .unwrap();
    }

    #[test]
    fn a_recursive_watch_over_a_directory_that_already_has_a_project_in_it_does_not_wake_on_its_own()
     {
        // Establishing a recursive watch over a directory that already has a subdirectory in
        // it makes the platform's watcher open that subdirectory to keep watching it — not a
        // change to anything, so a wait blocked on this watch must not wake for it.
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("my-app");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("journal.db"), "unchanged").unwrap();

        let watch = FileJournalWatch::open(dir.path(), true).unwrap();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || sender.send(watch.wait()));

        assert_eq!(
            receiver.recv_timeout(Duration::from_millis(500)),
            Err(mpsc::RecvTimeoutError::Timeout),
            "the watch woke on its own setup, with nothing actually changed"
        );
    }
}
