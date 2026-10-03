//! A throwaway binary, launched only by the terminal-recovery test in `tests/tui/exit.rs`. It
//! drives [`ktask_tui::run`] exactly as `ktask-rs tui` does, then deliberately crashes once the
//! first frame has drawn, to prove that a panic restores the terminal before its message is
//! printed — through the same code path the shipped binary runs, without adding a crash switch
//! to it.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use ktask_core::{
    Import, JournalError, JournalWatch, Placement, Project, ProviderView, QueueView, RunReport,
    SettingView, StatusSummary, TaskDraft, TaskId,
};
use ktask_tui::Application;

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
        history: HashMap::new(),
        done_by_user: HashMap::new(),
    }
}

/// Every use case answers with a refusal, except the first load, which crashes deliberately —
/// this binary only cares about the first frame drawing before it does.
#[derive(Default)]
struct PanicTestApplication {
    loaded_once: Mutex<bool>,
}

const NOT_SUPPORTED: &str = "not supported in this test binary";

impl Application for PanicTestApplication {
    type LoadError = String;
    type RemoveError = String;
    type RetryError = String;
    type AnswerError = String;
    type DoneError = String;
    type AcknowledgeError = String;
    type AddProblem = String;
    type SettingsError = String;
    type SaveSettingError = String;
    type ProvidersError = String;
    type ProjectsError = String;
    type SwitchError = String;
    type ForgetError = String;
    type RegisterError = String;
    type ImportError = String;
    type RunRefusal = String;

    fn load_queue(&self, _show_cancelled: bool) -> Result<QueueView, String> {
        let mut loaded_once = self
            .loaded_once
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let already_loaded = std::mem::replace(&mut *loaded_once, true);
        assert!(
            !already_loaded,
            "deliberate crash for the terminal-recovery test"
        );
        Ok(empty_queue())
    }

    fn remove_task(&self, _id: TaskId) -> Result<(), String> {
        Ok(())
    }

    fn retry_task(&self, _id: TaskId) -> Result<(), String> {
        Ok(())
    }

    fn answer_task(&self, _id: TaskId, _text: &str) -> Result<(), String> {
        Ok(())
    }

    fn done_task(&self, _id: TaskId, _reason: &str) -> Result<(), String> {
        Ok(())
    }

    fn acknowledge_task(&self, _id: TaskId, _message: Option<&str>) -> Result<(), String> {
        Ok(())
    }

    fn add_task(&self, _draft: &TaskDraft, _placement: Placement) -> Result<TaskId, Vec<String>> {
        Err(vec![NOT_SUPPORTED.to_owned()])
    }

    fn load_settings(&self) -> Result<Vec<SettingView>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn save_setting(&self, _name: &str, _value: &str) -> Result<SettingView, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn load_providers(&self) -> Result<Vec<ProviderView>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn import(&self, _path: &str) -> Result<Import, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn load_projects(&self) -> Result<Vec<Project>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn switch_project(&self, _name: &str) -> Result<QueueView, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn forget_project(&self, _name: &str) -> Result<Vec<Project>, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn register(&self, _name: &str) -> Result<QueueView, String> {
        Err(NOT_SUPPORTED.to_owned())
    }

    fn start_run(&self) -> Result<RunReport, String> {
        Err(NOT_SUPPORTED.to_owned())
    }
}

fn main() {
    let _ = ktask_tui::run(
        ktask_tui::Start::Ready,
        PanicTestApplication::default(),
        NeverChanges,
    );
}
