//! Terminal read models. Persistent projections are read-only; demo mutations stay in demo mode.
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
}
impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ai => "AI",
            Self::Human => "Human",
        }
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
        let models = ROLES
            .iter()
            .zip(&self.settings.models)
            .map(|(role, model)| {
                format!(
                    "{role}: {} / {}",
                    model.name,
                    model.effort.as_deref().unwrap_or("none")
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
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
    pub revision: Revision,
    pub session: Option<usize>,
    pub state: Status,
    pub resume_at: Option<Instant>,
    /// The stored wall-clock resume time, shown as is: converting `resume_at` back through a
    /// second clock reading can land just before a minute boundary.
    pub resume_at_ms: Option<i64>,
    pub logs: Vec<String>,
    pub outcome: String,
    /// Final report: worker summary, supervisor verdict and step findings. Empty while unknown.
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
            logs: vec![format!(
                "Executor: {} / {} · revision {} · session {session} (mock)",
                model.name,
                model.effort.as_deref().unwrap_or("none"),
                revision.number
            )],
            revision,
            session: Some(session),
            state: Status::Running,
            resume_at: None,
            resume_at_ms: None,
            outcome: "Running".into(),
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
}

/// A limit in milliseconds, 0 meaning off, as the owner reads it.
pub fn limit_label(ms: u64) -> String {
    crate::application::limits::label((ms > 0).then_some(ms as i64))
}
