//! A throwaway binary, launched only by the terminal-recovery test in `tests/tui/exit.rs`. It
//! drives [`ktask_tui::run`] exactly as `ktask-rs tui` does, then deliberately crashes once the
//! first frame has drawn, to prove that a panic restores the terminal before its message is
//! printed — through the same code path the shipped binary runs, without adding a crash switch
//! to it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::SystemTime;

use ktask_core::{
    JournalError, JournalWatch, Placement, Project, QueueView, SettingView, StatusSummary,
    TaskDraft, TaskId,
};

/// Never reports a change: this binary only cares about the first frame.
struct NeverChanges;

impl JournalWatch for NeverChanges {
    fn wait(&self) -> Result<(), JournalError> {
        loop {
            std::thread::park();
        }
    }
}

fn empty_queue() -> QueueView {
    QueueView {
        project: Project {
            name: "panic-test".to_owned(),
            path: PathBuf::from("/"),
            registered_at: SystemTime::UNIX_EPOCH,
        },
        summary: StatusSummary::default(),
        tasks: Vec::new(),
        attempts: HashMap::new(),
    }
}

fn main() {
    let mut loaded_once = false;
    let _ = ktask_tui::run(
        ktask_tui::Start::Ready,
        ktask_tui::Actions {
            load: move |_show_cancelled| {
                let already_loaded = std::mem::replace(&mut loaded_once, true);
                assert!(
                    !already_loaded,
                    "deliberate crash for the terminal-recovery test"
                );
                Ok(empty_queue())
            },
            remove: |_id: TaskId| Ok(()),
            add: |_draft: &TaskDraft, _placement: Placement| {
                Err(vec!["not supported in this test binary".to_owned()])
            },
            load_settings: || -> Result<Vec<SettingView>, String> {
                Err("not supported in this test binary".to_owned())
            },
            save_setting: |_name: &str, _value: &str| {
                Err("not supported in this test binary".to_owned())
            },
            import: |_path: &str| Err("not supported in this test binary".to_owned()),
            load_projects: || -> Result<Vec<Project>, String> {
                Err("not supported in this test binary".to_owned())
            },
            switch_project: |_name: &str| Err("not supported in this test binary".to_owned()),
            forget_project: |_name: &str| -> Result<Vec<Project>, String> {
                Err("not supported in this test binary".to_owned())
            },
            register: |_name: &str| Err("not supported in this test binary".to_owned()),
        },
        || Err("not supported in this test binary".to_owned()),
        NeverChanges,
    );
}
