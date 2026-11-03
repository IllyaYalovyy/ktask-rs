//! Terminal read models. Persistent projections are read-only; demo mutations stay in demo mode.
pub mod task_text;

use std::time::Instant;

/// The task state machine shared with the CLI and the runner (application::lifecycle).
pub use crate::application::lifecycle::{Action as TaskAction, State as Status};

/// The short upper-case badge a task row shows for its state.
pub fn badge(status: Status) -> &'static str {
    match status {
        Status::NeedsApproval => "APPROVE?",
        Status::Pending => "PENDING",
        Status::Running => "RUNNING",
        Status::Paused => "PAUSED",
        Status::NeedsInput => "INPUT?",
        Status::Failed => "FAILED",
        Status::Done => "DONE",
        Status::Cancelled => "CANCELLED",
        Status::Superseded => "REPLACED",
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Ai,
    Human,
    Docs,
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ai => "AI",
            Self::Human => "Human",
            Self::Docs => "Docs",
        }
    }
    pub const LABELS: [&'static str; 3] = ["AI", "Human", "Docs"];
    /// The kind a form choice or imported row names; unknown labels are rejected by callers.
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "AI" => Some(Self::Ai),
            "Human" => Some(Self::Human),
            "Docs" => Some(Self::Docs),
            _ => None,
        }
    }
    pub fn runs_ai(self) -> bool {
        self != Self::Human
    }
}

pub const ROLES: [&str; 4] = ["Execution", "Review", "Validation", "Remediation"];
#[derive(Clone)]
pub struct Model {
    pub name: String,
    pub effort: Option<String>,
}
#[derive(Clone)]
pub struct Settings {
    pub models: [Model; 4],
    pub attempt_limit: usize,
    /// Task limit: active time across all attempts. 0 = off.
    pub timeout_ms: u64,
    /// Stage limit: one agent turn without a result. 0 = off.
    pub stage_timeout_ms: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            models: std::array::from_fn(|i| Model {
                name: if i == 0 { "economy" } else { "capable" }.into(),
                effort: Some(if i == 0 { "medium" } else { "high" }.into()),
            }),
            attempt_limit: 3,
            timeout_ms: 0,
            stage_timeout_ms: 0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RevisionState {
    Ready,
    Draft,
    Rejected,
    Retired,
}
#[derive(Clone)]
pub struct Revision {
    pub number: usize,
    pub state: RevisionState,
    pub title: String,
    pub goal: String,
    pub check: String,
    pub kind: Kind,
    pub settings: Settings,
    pub actor: String,
    pub repaired: bool,
}
impl Revision {
    pub fn summary(&self) -> String {
        let models = role_models(&self.settings);
        format!(
            "Revision {} · {}\n{models}\nAttempt limit: {} · task limit: {} · stage limit: {}",
            self.number,
            if self.state == RevisionState::Draft {
                "Needs approval"
            } else {
                &self.actor
            },
            if self.settings.attempt_limit == 0 {
                "unlimited".into()
            } else {
                self.settings.attempt_limit.to_string()
            },
            limit_label(self.settings.timeout_ms),
            limit_label(self.settings.stage_timeout_ms)
        )
    }
}

#[derive(Clone)]
pub struct Attempt {
    /// The stored attempt's id; None for simulated attempts.
    pub id: Option<i64>,
    pub revision: Revision,
    pub session: Option<usize>,
    pub state: Status,
    pub resume_at: Option<Instant>,
    /// The stored wall-clock resume time, shown as is: converting `resume_at` back through a
    /// second clock reading can land just before a minute boundary.
    pub resume_at_ms: Option<i64>,
    pub logs: Vec<String>,
    pub outcome: String,
    /// Supervisor-owned status, kept outside rendered worker claims.
    pub supervisor_status: String,
    /// Rendered claim, check and verdict sections. Empty while unknown.
    pub report: String,
    /// Question the worker is waiting on, while open.
    pub question: Option<String>,
    /// Supervisor steps in order: (role label, state).
    pub steps: Vec<(String, String)>,
    pub step: usize,
    pub cost_cents: usize,
    pub elapsed_ms: u64,
    pub waiting_ms: u64,
    pub usage: String,
    pub usage_tokens: String,
    /// The same counts for narrow screens, cache reads and writes together.
    pub usage_tokens_short: String,
    pub usage_cost: String,
}
impl Attempt {
    pub fn start(revision: Revision, session: usize) -> Self {
        let model = &revision.settings.models[0];
        Self {
            id: None,
            logs: vec![format!(
                "Executor: {} · revision {} · session {session} (mock)",
                model_label(&model.name, model.effort.as_deref()),
                revision.number
            )],
            revision,
            session: Some(session),
            state: Status::Running,
            resume_at: None,
            resume_at_ms: None,
            outcome: "Running".into(),
            supervisor_status: String::new(),
            report: String::new(),
            question: None,
            steps: Vec::new(),
            step: 0,
            cost_cents: 0,
            elapsed_ms: 0,
            waiting_ms: 0,
            usage: String::new(),
            usage_tokens: String::new(),
            usage_tokens_short: String::new(),
            usage_cost: String::new(),
        }
    }
}

#[derive(Clone)]
pub struct Task {
    // SQLite mode: the stored state, never written back as a task payload. Demo mode: a
    // stopped failure is Pending while its failed attempt stays as recorded.
    pub stored_status: Option<Status>,
    pub stored_budget: Option<usize>,
    pub id: usize,
    pub origin: Option<(usize, usize)>,
    pub follow_ups: Vec<(usize, String)>,
    pub revisions: Vec<Revision>,
    pub attempts: Vec<Attempt>,
    pub resolution: Option<Status>,
    pub resolution_reason: String,
    pub answer: String,
    pub needs_question: bool,
    pub needs_rate_limit: bool,
    pub repairs: Option<usize>,
    /// Epoch milliseconds when the task left the queue; None in demo mode and while open.
    pub closed_at_ms: Option<i64>,
    /// Why a failed task stopped where the supervisor cannot resolve it; None otherwise.
    pub unresolved: Option<crate::application::stop::Stop>,
}
impl Task {
    pub fn pending(id: usize, title: &str) -> Self {
        Self {
            stored_status: None,
            stored_budget: None,
            id,
            origin: None,
            follow_ups: Vec::new(),
            revisions: vec![Revision {
                number: 1,
                state: RevisionState::Ready,
                title: title.into(),
                goal: "Make the requested change.".into(),
                check: "Automated checks pass.".into(),
                kind: Kind::Ai,
                settings: Settings::default(),
                actor: "Human: owner".into(),
                repaired: false,
            }],
            attempts: Vec::new(),
            resolution: None,
            resolution_reason: String::new(),
            answer: String::new(),
            needs_question: false,
            needs_rate_limit: false,
            repairs: None,
            closed_at_ms: None,
            unresolved: None,
        }
    }
    pub fn ready(&self) -> Option<&Revision> {
        self.revisions
            .iter()
            .rev()
            .find(|r| r.state == RevisionState::Ready)
    }
    pub fn draft(&self) -> Option<&Revision> {
        let ready = self.ready().map_or(0, |r| r.number);
        self.revisions
            .iter()
            .rev()
            .find(|r| r.state == RevisionState::Draft && r.number > ready)
    }
    pub fn definition(&self) -> &Revision {
        self.ready()
            .or_else(|| self.draft())
            .unwrap_or_else(|| self.revisions.last().expect("task definition"))
    }
    pub fn status(&self) -> Status {
        if let Some(status) = self.stored_status {
            return status;
        }
        self.resolution.unwrap_or_else(|| {
            if self.ready().is_none() || self.draft().is_some() {
                return Status::NeedsApproval;
            }
            self.attempts.last().map_or(Status::Pending, |a| a.state)
        })
    }
    /// Needs input with its question still open; once answered, the forward move resumes.
    pub fn awaits_answer(&self) -> bool {
        self.status() == Status::NeedsInput
            && self.attempts.last().is_some_and(|a| a.question.is_some())
    }
    /// Running, but waiting for the provider to accept work again.
    pub fn waiting_for_limit(&self) -> bool {
        self.status() == Status::Running
            && self
                .attempts
                .last()
                .is_some_and(|a| a.resume_at.is_some() || a.resume_at_ms.is_some())
    }
    pub fn used(&self) -> usize {
        if let Some(used) = self.stored_budget {
            return used;
        }
        self.attempts
            .iter()
            .filter(|a| matches!(a.state, Status::Failed | Status::Done))
            .count()
    }
    pub fn revise(&mut self, mut revision: Revision) {
        revision.number = self.revisions.last().map_or(1, |r| r.number + 1);
        self.revisions.push(revision);
    }
    pub fn cost(&self) -> usize {
        self.attempts.iter().map(|a| a.cost_cents).sum()
    }
    /// Active and waiting time across all attempts.
    pub fn elapsed_ms(&self) -> u64 {
        self.attempts.iter().fold(0_u64, |total, attempt| {
            total.saturating_add(attempt.elapsed_ms.saturating_add(attempt.waiting_ms))
        })
    }
}

/// "Budget 1 of 3 used", or "Budget 1 used · no limit" when the attempt limit is off (0).
pub fn budget(used: usize, limit: usize) -> String {
    if limit == 0 {
        format!("Budget {used} used · no limit")
    } else {
        format!("Budget {used} of {limit} used")
    }
}

/// A limit in milliseconds, 0 meaning off, as the owner reads it.
pub fn limit_label(ms: u64) -> String {
    crate::application::limits::label((ms > 0).then_some(ms as i64))
}

/// A model and its effort as every screen and command names them: "codex/gpt / high".
pub fn model_label(model: &str, effort: Option<&str>) -> String {
    format!("{model} / {}", effort.unwrap_or("none"))
}

/// The one role/model line: "Review: codex/gpt / high", or "Review: unconfigured".
pub fn role_model(role: &str, model: Option<(&str, Option<&str>)>) -> String {
    match model {
        Some((model, effort)) => format!("{role}: {}", model_label(model, effort)),
        None => format!("{role}: unconfigured"),
    }
}

/// [`role_model`] for a stored role setting.
pub fn role_setting(role: &str, model: Option<&crate::application::ModelConfig>) -> String {
    let identifier = model.map(crate::application::ModelConfig::identifier);
    role_model(
        role,
        identifier
            .as_deref()
            .zip(model)
            .map(|(identifier, model)| (identifier, model.effort.as_deref())),
    )
}

/// One role/model line per role.
pub fn role_models(settings: &Settings) -> String {
    ROLES
        .iter()
        .zip(&settings.models)
        .map(|(role, m)| role_model(role, Some((&m.name, m.effort.as_deref()))))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The execution model and effort a Queue row shows: "codex/gpt high"; details list every role.
pub fn model_summary(settings: &Settings) -> String {
    let m = &settings.models[0];
    match &m.effort {
        Some(effort) => format!("{} {effort}", m.name),
        None => m.name.clone(),
    }
}

/// Cents as dollars: "$0.08". Simulated costs read like reported ones.
pub fn money(cents: usize) -> String {
    crate::application::usage_report::dollars(
        i64::try_from(cents).unwrap_or(i64::MAX / 10_000) * 10_000,
    )
}

/// Elapsed milliseconds as History and Status show them: "39m 12s".
pub fn duration(ms: u64) -> String {
    crate::application::limits::elapsed(ms.min(i64::MAX as u64) as i64)
}

/// Local completion time for History rows, e.g. "Oct 26 23:32"; "-" when unrecorded.
pub fn finished_label(closed_at_ms: Option<i64>, now_ms: i64) -> String {
    closed_at_ms.map_or_else(|| "-".into(), |at| crate::clock::format_local(at, now_ms))
}

/// "Active 39m 12s", adding the waiting time only when there was some.
pub fn attempt_time(attempt: &Attempt) -> String {
    let active = duration(attempt.elapsed_ms);
    if attempt.waiting_ms == 0 {
        format!("Active {active}")
    } else {
        format!("Active {active} · waited {}", duration(attempt.waiting_ms))
    }
}

/// What the supervisor is doing on a running task, in the owner's words.
pub fn active_phase(task: &Task) -> String {
    if let Some(line) = task
        .attempts
        .last()
        .and_then(|a| a.supervisor_status.lines().next())
        && (line.starts_with("Workflow ·") || line.starts_with("Git delivery ·"))
    {
        return line.to_owned();
    }
    if let Some(message) = task.attempts.last().and_then(|attempt| attempt.logs.last())
        && message.starts_with("Provider connection failed")
    {
        return "Provider connection retry · work and session retained".into();
    }
    match running_step(task) {
        Some("Execution") => "AI implementing the task".into(),
        Some("Remediation") => "AI planning or applying fixes".into(),
        Some("Review") => "Independent review of the changes".into(),
        Some("Validation") => "Independent validation of the evidence".into(),
        Some(label) => format!("Checking: {label}"),
        None => "Supervisor preparing the next step".into(),
    }
}

/// The label of the latest attempt's running step.
pub fn running_step(task: &Task) -> Option<&str> {
    task.attempts
        .last()
        .and_then(|a| a.steps.iter().rev().find(|(_, state)| state == "running"))
        .map(|(label, _)| label.as_str())
}

/// The role whose model is at work: the workflow phase of a running attempt, else its
/// latest step. The judge plans with the remediation model; the worker fixes with execution's.
pub fn active_role(task: &Task) -> usize {
    let attempt = task.attempts.last();
    let role = attempt
        .and_then(|a| a.steps.last())
        .map(|(label, _)| label.as_str());
    let phase = attempt
        .filter(|_| task.status() == Status::Running)
        .and_then(|a| a.supervisor_status.strip_prefix("Workflow · "))
        .and_then(|report| report.split(" · ").next());
    match phase.or(role) {
        Some("Review") => 1,
        Some("Validate" | "Critical findings need a decision" | "Validation") => 2,
        Some("Plan fixes" | "Remediation") => 3,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(steps: &[(&str, &str)], status: &str) -> Attempt {
        let mut attempt = Attempt::start(Task::pending(1, "t").revisions[0].clone(), 1);
        attempt.steps = steps
            .iter()
            .map(|(label, state)| ((*label).into(), (*state).into()))
            .collect();
        attempt.supervisor_status = status.into();
        attempt
    }

    #[test]
    fn role_and_model_lines_share_one_format() {
        assert_eq!(model_label("codex/gpt", Some("high")), "codex/gpt / high");
        assert_eq!(model_label("codex/gpt", None), "codex/gpt / none");
        assert_eq!(
            role_model("Review", Some(("codex/gpt", Some("high")))),
            "Review: codex/gpt / high"
        );
        assert_eq!(role_model("Review", None), "Review: unconfigured");
        let stored = crate::application::ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: None,
        };
        assert_eq!(
            role_setting("Execution", Some(&stored)),
            "Execution: claude/sonnet / none"
        );
        assert_eq!(role_setting("Execution", None), "Execution: unconfigured");
        assert_eq!(
            role_models(&Settings::default()),
            "Execution: economy / medium\nReview: capable / high\nValidation: capable / high\nRemediation: capable / high"
        );
        let mut settings = Settings::default();
        assert_eq!(model_summary(&settings), "economy medium");
        settings.models[0].effort = None;
        assert_eq!(model_summary(&settings), "economy");
    }

    #[test]
    fn money_and_durations_read_as_people_say_them() {
        assert_eq!(money(0), "$0.00");
        assert_eq!(money(8), "$0.08");
        assert_eq!(money(1234), "$12.34");
        assert_eq!(duration(0), "0s");
        assert_eq!(duration(2_352_000), "39m 12s");
        assert_eq!(
            duration(u64::MAX),
            crate::application::limits::elapsed(i64::MAX)
        );
        let mut run = attempt(&[], "");
        run.elapsed_ms = 2_352_000;
        assert_eq!(attempt_time(&run), "Active 39m 12s");
        run.waiting_ms = 61_000;
        assert_eq!(attempt_time(&run), "Active 39m 12s · waited 1m 01s");
    }

    #[test]
    fn budget_names_the_limit_only_when_there_is_one() {
        assert_eq!(budget(1, 3), "Budget 1 of 3 used");
        assert_eq!(budget(2, 0), "Budget 2 used · no limit");
    }

    #[test]
    fn finish_label_is_local_and_dashes_when_unrecorded() {
        let now = crate::clock::local_ms(2026, 10, 27, 9, 0).unwrap();
        let at = crate::clock::local_ms(2026, 10, 26, 23, 32).unwrap();
        assert_eq!(finished_label(Some(at), now), "Oct 26 23:32");
        assert_eq!(finished_label(None, now), "-");
    }

    #[test]
    fn active_phase_and_role_follow_the_running_step() {
        let mut task = Task::pending(1, "t");
        assert_eq!(active_role(&task), 0);
        task.attempts.push(attempt(
            &[("Execution", "succeeded"), ("Review", "running")],
            "",
        ));
        assert_eq!(active_phase(&task), "Independent review of the changes");
        assert_eq!(active_role(&task), 1);
        task.attempts.push(attempt(
            &[("Execution", "succeeded")],
            "Workflow · Plan fixes · round 2",
        ));
        assert_eq!(active_phase(&task), "Workflow · Plan fixes · round 2");
        assert_eq!(active_role(&task), 3);
        task.attempts.push(attempt(&[("Checks", "running")], ""));
        assert_eq!(active_phase(&task), "Checking: Checks");
    }
}
