//! What the terminal screens need from a workspace. `demo::DemoWorkspace` simulates one in
//! memory for `--demo`; `persistent::StoredWorkspace` runs one on `application::Service`.
//! The screens keep only view state and never ask which one they have.
use crate::{
    app::{Action, EvidenceSection, Screen},
    application::{EvidenceCursor, EvidenceSearch},
    dialog::Dialog,
    presentation::Task,
};

/// A History column after the title: its header and whether its cells align left.
pub(crate) struct Column {
    pub name: &'static str,
    pub left: bool,
}

/// Where a details dialog reads evidence: an attempt at an output watermark.
#[derive(Clone, Copy)]
pub(crate) struct EvidenceAt {
    pub task: usize,
    /// The attempt's index in its task.
    pub index: usize,
    /// The stored attempt id, or the index for simulated attempts.
    pub attempt: i64,
    pub watermark: i64,
}

pub(crate) trait Backend {
    // Commands. Each backend builds the forms it applies.

    /// Run, Pause, Approve or Readiness, at once; Run on a question opens the answer form.
    fn act(&mut self, screen: &mut Screen, action: Action);
    /// The form or report `action` opens; None when this workspace offers nothing for it.
    fn open(&mut self, screen: &mut Screen, action: Action) -> Result<Option<Dialog>, String>;
    /// Apply a submitted form: Ok(true) closes it, Ok(false) keeps its next step open.
    fn apply(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String>;
    fn stats(&self, screen: &Screen) -> Result<Dialog, String>;
    /// Advance live state; true when the screen must redraw.
    fn tick(&mut self, screen: &mut Screen) -> bool;

    // Task details: the body of an Inspect dialog and the evidence it pages through. The
    // defaults describe a workspace without stored evidence pages.

    fn inspect_body(&self, screen: &Screen, dialog: &Dialog) -> String;
    /// The label of the search field a section offers, if any.
    fn find_label(&self, _section: EvidenceSection) -> Option<&'static str> {
        None
    }
    fn watermark(&self, _attempt: i64) -> Result<i64, String> {
        Ok(0)
    }
    /// The stored id of a transcript artifact, which keeps its reading position.
    fn artifact_id(&self, _attempt: i64, _index: usize) -> Option<i64> {
        None
    }
    fn artifact_count(&self, _attempt: i64) -> Result<usize, String> {
        Ok(0)
    }
    /// Pages of Output (section 0) or of the selected transcript (section 2).
    fn page_count(
        &self,
        _screen: &Screen,
        _at: EvidenceAt,
        _section: EvidenceSection,
        _artifact: usize,
    ) -> Result<usize, String> {
        Ok(1)
    }
    fn chunk_count(&self, _at: EvidenceAt, _page: usize) -> Result<usize, String> {
        Ok(1)
    }
    /// Review (`reviews`) or failure steps of an attempt.
    fn step_count(&self, screen: &Screen, at: EvidenceAt, reviews: bool) -> Result<usize, String>;
    fn search(
        &self,
        _screen: &Screen,
        _at: EvidenceAt,
        _section: EvidenceSection,
        _artifact: usize,
        _query: &str,
        _cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch, String> {
        Ok(EvidenceSearch::Exhausted)
    }

    // Wording that differs between a simulated and a stored workspace.

    /// The header's description of the data on screen.
    fn label(&self) -> &'static str;
    /// The Config or About screen's text.
    fn information(&self, screen: &Screen) -> String;
    /// Buttons on the Config screen; their keys work only when listed.
    fn config_controls(&self) -> Vec<(&'static str, Action)>;
    /// Status lines about attempts and budget, under the state line.
    fn status_progress(&self, task: &Task) -> Vec<String>;
    /// The running step the Status panel highlights, if this workspace records steps.
    fn status_step(&self, task: &Task) -> Option<String>;
    /// Model, usage and budget lines of the Status panel; `compact` panels have 7 rows.
    fn status_meter(&self, task: &Task, width: usize, compact: bool) -> Vec<String>;
    /// What a running task is doing, before its Pause/Stop hint.
    fn running_situation(&self, task: &Task) -> String;
    /// The situation of a pending current task that can run.
    fn ready_situation(&self, auto_advance: bool) -> &'static str;
    /// The output pane of a current task with no attempt that can run.
    fn idle_output(&self) -> &'static str;
    fn activity_title(&self, task: usize) -> String;
    /// History's toolbar description.
    fn history_scope(&self) -> &'static str;
    fn history_columns(&self) -> &'static [Column];
    fn history_cells(&self, task: &Task) -> Vec<String>;
    /// The details pane's cost of a task.
    fn task_cost(&self, task: &Task) -> String;
    /// The details pane's per-attempt lines.
    fn attempt_lines(&self, task: &Task) -> String;
}
