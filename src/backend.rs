//! What the terminal screens need from a workspace. `demo::DemoWorkspace` simulates one in
//! memory for `--demo`; `persistent::StoredWorkspace` runs one on `application::Service`.
//! The screens keep only view state and never ask which one they have.
use crate::{
    app::{Action, EvidenceSection, Screen},
    application::{EvidenceCursor, EvidenceSearch},
    dialog::Dialog,
    presentation::{self as model, Status, Task, attempt_time, budget, model_label},
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
    /// Whether work this workspace started still needs `tick` to finish while another
    /// project is on screen.
    fn busy(&self) -> bool {
        false
    }
    /// The owner is back at this workspace after another project was on screen.
    fn resumed(&mut self) {}

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

    // Wording that differs between a simulated and a stored workspace. The defaults describe
    // stored tasks, local or linked; the simulator replaces them.

    /// The stored task behind a row, which usage and budgets are read from.
    fn stored_task(&self, _id: usize) -> Option<&crate::application::Task> {
        None
    }
    /// The header's description of the data on screen.
    fn label(&self) -> &'static str;
    /// The Config or About screen's text.
    fn information(&self, screen: &Screen) -> String;
    /// Buttons on the Config screen; their keys work only when listed.
    fn config_controls(&self) -> Vec<(&'static str, Action)>;
    /// Status lines about attempts and budget, under the state line.
    fn status_progress(&self, task: &Task) -> Vec<String> {
        vec![if task.attempts.is_empty() {
            "No attempts yet".to_owned()
        } else {
            format!("Attempt {}", task.attempts.len())
        }]
    }
    /// The running step the Status panel highlights, if this workspace records steps.
    fn status_step(&self, task: &Task) -> Option<String> {
        (task.status() == Status::Running).then(|| {
            format!(
                "Step: {}",
                model::running_step(task).map_or_else(|| model::active_phase(task), str::to_owned)
            )
        })
    }
    /// Model, usage and budget lines of the Status panel; `compact` panels have 7 rows.
    fn status_meter(&self, task: &Task, width: usize, compact: bool) -> Vec<String> {
        let attempt = task.attempts.last();
        // An attempt runs on the revision it started with.
        let settings = &attempt
            .map_or_else(|| task.definition(), |a| &a.revision)
            .settings;
        let model = &settings.models[model::active_role(task)];
        let mut usage = attempt.map_or_else(Vec::new, |attempt| {
            let tokens = if attempt.usage_tokens.chars().count() <= width {
                attempt.usage_tokens.clone()
            } else if attempt.usage_tokens_short.chars().count() <= width {
                attempt.usage_tokens_short.clone()
            } else if attempt.usage_tokens.contains('?') {
                "Tokens ? · Enter: details".to_owned()
            } else {
                "Tokens · Enter: details".to_owned()
            };
            vec![attempt_time(attempt), tokens, attempt.usage_cost.clone()]
        });
        let mut lines = Vec::new();
        if compact && usage.len() == 3 {
            lines.push(usage.remove(1));
        }
        lines.push(format!(
            "Model {}",
            model_label(&model.name, model.effort.as_deref())
        ));
        lines.extend(usage);
        lines.push(budget(task.used(), settings.attempt_limit));
        lines
    }
    /// What a running task is doing, before its Pause/Stop hint.
    fn running_situation(&self, task: &Task) -> String {
        model::active_phase(task)
    }
    /// The situation of a pending current task that can run.
    fn ready_situation(&self, auto_advance: bool) -> &'static str {
        if auto_advance {
            "Ready · r Run queue; checks and repair run automatically"
        } else {
            "Run one selected · m Run mode to continue the queue"
        }
    }
    /// The output pane of a current task with no attempt that can run.
    fn idle_output(&self) -> &'static str {
        "Ready. r starts an attempt in the background supervisor; this pane follows its log.\nDefaults and checks live in Config; c pins models for this task."
    }
    fn activity_title(&self, task: usize) -> String {
        format!(" Activity · #{task} · l: all ")
    }
    /// History's toolbar description.
    fn history_scope(&self) -> &'static str {
        "closed tasks"
    }
    fn history_columns(&self) -> &'static [Column] {
        &[
            Column {
                name: "Finished",
                left: true,
            },
            Column {
                name: "Attempts",
                left: false,
            },
            Column {
                name: "Time",
                left: false,
            },
            Column {
                name: "Usage",
                left: false,
            },
        ]
    }
    fn history_cells(&self, task: &Task) -> Vec<String> {
        vec![
            model::finished_label(task.closed_at_ms, crate::application::now_ms()),
            task.attempts.len().to_string(),
            model::duration(task.elapsed_ms()),
            crate::project_view::usage(self.stored_task(task.id)).history_label(),
        ]
    }
    /// The details pane's cost of a task.
    fn task_cost(&self, task: &Task) -> String {
        crate::project_view::usage(self.stored_task(task.id)).cost_label()
    }
    /// The details pane's per-attempt lines.
    fn attempt_lines(&self, task: &Task) -> String {
        task.attempts
            .iter()
            .enumerate()
            .map(|(i, a)| {
                format!(
                    "\nAttempt {} · {} · {}\n{}",
                    i + 1,
                    a.state.label(),
                    attempt_time(a),
                    a.usage
                )
            })
            .collect()
    }
}
