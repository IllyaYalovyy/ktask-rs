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
    /// Starts watching the journal file at `path` for changes.
    ///
    /// # Errors
    ///
    /// Fails when the platform's file-notification mechanism cannot be started or cannot
    /// watch `path`.
    pub fn open(path: &Path) -> Result<Self, JournalError> {
        let doing = format!("cannot watch the journal {}", path.display());
        let (sender, events) = mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)
            .map_err(|e| JournalError::new(format!("{doing}: {e}")))?;
        watcher
            .watch(path, RecursiveMode::NonRecursive)
            .map_err(|e| JournalError::new(format!("{doing}: {e}")))?;
        Ok(Self {
            _watcher: watcher,
            events,
        })
    }
}

impl JournalWatch for FileJournalWatch {
    fn wait(&self) -> Result<(), JournalError> {
        match self.events.recv() {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => Err(JournalError::new(format!("the journal watch failed: {e}"))),
            Err(_) => Err(JournalError::new("the journal watch stopped")),
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
        let watch = FileJournalWatch::open(&path).unwrap();
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
        let error = FileJournalWatch::open(&path).unwrap_err().to_string();
        assert!(error.contains(&path.display().to_string()), "{error}");
    }
}
