//! Agent fixes on screen, from Current Task, Projects and a finished report: the form that
//! prepares one or picks a past one, the confirmation naming the project and the allowed
//! scope, and the view that follows it. A stored project goes through [`Service`]; `--demo`
//! uses a fixture that never opens the database or starts an agent.
use crate::{
    app::App,
    application::{
        Actor, ModelConfig, Service,
        fix::{Change, ChangeKind, Fix, FixEntry, Probe, ProbeKind, StepSource, StepStatus},
        investigation::{
            AgentKind, Investigation, InvestigationLimits, InvestigationState, InvestigationUsage,
            Progress, ProgressKind,
        },
        now_ms,
    },
    dialog::{Dialog, DialogKind, Field},
    instance::Paths,
    presentation::{fix_text, report_text},
    runner::{fix, investigate},
    storage::Database,
};
use ratatui::crossterm::event::KeyCode;
use std::time::{Duration, Instant};

const CHOICE: &str = "Fix";
const FROM: &str = "Carry out";
const INSTRUCTION: &str = "Instruction (when not a report)";
const WHEN: &str = "When verified";
const NEW: &str = "New fix";
const FROM_INSTRUCTION: &str = "The instruction below";
const STAY: &str = "Leave the runner paused";
const CONTINUE: &str = "Continue the queue";
/// Past fixes and finished reports the form offers; past fixes it lists.
const OFFERED: usize = 50;
const LISTED: usize = 2;
const REFRESH: Duration = Duration::from_millis(300);

/// What a confirmation dialog starts.
#[derive(Clone, Debug)]
pub struct Confirmed {
    pub report: Option<i64>,
    pub instruction: Option<String>,
    pub resume: bool,
}

struct Stored {
    service: Service,
    paths: Paths,
}

/// The simulator's fixes: each refresh of a running one moves it a step further.
struct Demo {
    fixes: Vec<(Investigation, Fix)>,
    steps: u32,
}

const DEMO_FINISHED: u32 = 12;
const DEMO_REPORT: i64 = 1;

fn demo_model() -> ModelConfig {
    ModelConfig {
        provider: "codex".into(),
        model: "demo-operator".into(),
        effort: None,
    }
}

fn demo_fix(id: i64, request: &Confirmed) -> (Investigation, Fix) {
    let limits = InvestigationLimits {
        version: 0,
        time_limit_ms: 30 * 60 * 1000,
        cost_limit_microusd: 5_000_000,
    };
    let investigation = Investigation {
        id,
        kind: AgentKind::Fix,
        project_id: 0,
        task_id: None,
        question: request.instruction.clone().or_else(|| {
            request
                .report
                .map(|report| format!("Carry out report #{report}"))
        }),
        requester_kind: "human".into(),
        requester: "owner".into(),
        model: demo_model(),
        state: InvestigationState::Running,
        time_limit_ms: limits.time_limit_ms,
        cost_limit_microusd: limits.cost_limit_microusd,
        stop_requested_by: None,
        started_at_ms: now_ms(),
        finished_at_ms: None,
        reason: None,
        usage: InvestigationUsage::default(),
        report: None,
        progress: Vec::new(),
    };
    let record = Fix {
        id,
        report_id: request.report,
        resume: request.resume,
        ..Fix::default()
    };
    (investigation, record)
}

/// The scripted course of a demo fix.
fn demo_advance((investigation, record): &mut (Investigation, Fix), step: u32) {
    let mut say = |message: &str| {
        investigation.progress.push(Progress {
            sequence: investigation.progress.len() as i64 + 1,
            recorded_at_ms: now_ms(),
            kind: ProgressKind::Finding,
            message: message.into(),
        });
    };
    let entry = |sequence, source, action: &str, result: &str| FixEntry {
        sequence,
        recorded_at_ms: now_ms(),
        source,
        action: action.into(),
        result: result.into(),
        status: StepStatus::Applied,
    };
    match step {
        1 => {
            say("The demo runner is paused");
            record.steps.push(entry(
                1,
                StepSource::Supervisor,
                "Pause the project runner",
                "simulated",
            ));
            record.probes.push(Probe {
                ordinal: 1,
                kind: ProbeKind::Check,
                target: "demo-check".into(),
                passed_before: Some(false),
                passed_after: None,
                detail: None,
            });
        }
        4 => say("The demo check command names a missing file"),
        7 => {
            record.steps.push(entry(
                2,
                StepSource::Ktask,
                "ktask check-set demo-check --command demo",
                "Check saved (simulated)",
            ));
            record.changes.push(Change {
                kind: ChangeKind::Setting,
                target: "check demo-check".into(),
                detail: "old -> demo".into(),
            });
        }
        DEMO_FINISHED => {
            if let Some(probe) = record.probes.first_mut() {
                probe.passed_after = Some(true);
            }
            record.verification =
                Some("Verified: check demo-check failed before and passes now".into());
            record.summary = Some("The simulated check command was corrected.".into());
            record.continued = record.resume;
            investigation.state = InvestigationState::Finished;
            investigation.finished_at_ms = Some(now_ms());
        }
        _ => {}
    }
}

impl Demo {
    fn new() -> Self {
        let mut done = demo_fix(
            3,
            &Confirmed {
                report: Some(DEMO_REPORT),
                instruction: None,
                resume: false,
            },
        );
        for step in 1..=DEMO_FINISHED {
            demo_advance(&mut done, step);
        }
        Self {
            fixes: vec![done],
            steps: 0,
        }
    }

    fn start(&mut self, request: &Confirmed) -> Result<i64, String> {
        if let Some((running, _)) = self
            .fixes
            .iter()
            .find(|(i, _)| i.state == InvestigationState::Running)
        {
            return Err(format!(
                "Agent fix #{} is already running for this project",
                running.id
            ));
        }
        let id = self.fixes.first().map_or(1, |(i, _)| i.id + 1);
        self.fixes.insert(0, demo_fix(id, request));
        self.steps = 0;
        Ok(id)
    }

    fn running(&mut self, id: i64) -> Option<&mut (Investigation, Fix)> {
        self.fixes
            .iter_mut()
            .find(|(i, _)| i.id == id && i.state == InvestigationState::Running)
    }

    fn advance(&mut self, id: i64) {
        let step = self.steps + 1;
        if let Some(fix) = self.running(id) {
            demo_advance(fix, step);
            self.steps = step;
        }
    }

    fn stop(&mut self, id: i64) -> Result<(), String> {
        let (investigation, record) = self
            .running(id)
            .ok_or_else(|| format!("Agent fix #{id} is not running"))?;
        investigation.state = InvestigationState::Stopped;
        investigation.finished_at_ms = Some(now_ms());
        investigation.reason = Some("Stopped by owner".into());
        record.verification = Some("Not verified: the fix ended before its verification".into());
        Ok(())
    }
}

/// The fixes of the project a dialog is about.
pub(crate) struct Fixes {
    stored: Option<Box<Stored>>,
    demo: Demo,
    name: String,
    refreshed: Instant,
}

impl Fixes {
    pub fn new() -> Self {
        Self {
            stored: None,
            demo: Demo::new(),
            name: "demo workspace".into(),
            refreshed: Instant::now(),
        }
    }

    /// Binds to a stored project; None is the simulator's workspace.
    fn bind(&mut self, project: Option<(i64, String)>, paths: Option<Paths>) -> Result<(), String> {
        self.stored = None;
        let (Some((id, name)), Some(paths)) = (project, paths) else {
            self.name = "demo workspace".into();
            return Ok(());
        };
        let db = Database::open(&paths).map_err(|e| e.to_string())?;
        let service = Service::registered(db, id, name.clone()).map_err(|e| e.to_string())?;
        self.stored = Some(Box::new(Stored { service, paths }));
        self.name = name;
        Ok(())
    }

    /// This project's investigations of `kind`, newest first.
    fn runs(&mut self, kind: AgentKind) -> Result<Vec<Investigation>, String> {
        let Some(stored) = &mut self.stored else {
            return Ok(match kind {
                AgentKind::Fix => self.demo.fixes.iter().map(|(i, _)| i.clone()).collect(),
                AgentKind::Report => Vec::new(),
            });
        };
        investigate::settle(&mut stored.service, &stored.paths).map_err(|e| e.to_string())?;
        let mut listed = stored.service.investigations().map_err(|e| e.to_string())?;
        listed.retain(|item| item.kind == kind);
        Ok(listed)
    }

    fn read(&mut self, id: i64) -> Result<(Investigation, Fix), String> {
        let missing = || format!("Agent fix #{id} does not exist in this project");
        let Some(stored) = &mut self.stored else {
            return self
                .demo
                .fixes
                .iter()
                .find(|(i, _)| i.id == id)
                .cloned()
                .ok_or_else(missing);
        };
        investigate::settle(&mut stored.service, &stored.paths).map_err(|e| e.to_string())?;
        let record = stored
            .service
            .fix(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(missing)?;
        let investigation = stored
            .service
            .investigation(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(missing)?;
        Ok((investigation, record))
    }

    /// The finished reports a fix can carry out, as choice labels.
    fn reports(&mut self) -> Result<Vec<String>, String> {
        let label = |id: i64, question: &str| {
            let shown: String = question.chars().take(40).collect();
            format!("Report #{id} · {shown}")
        };
        if self.stored.is_none() {
            return Ok(vec![label(DEMO_REPORT, "Why did the demo check fail?")]);
        }
        Ok(self
            .runs(AgentKind::Report)?
            .iter()
            .filter(|item| item.state == InvestigationState::Finished)
            .take(OFFERED)
            .map(|item| label(item.id, &report_text::question(item)))
            .collect())
    }

    fn setup(&self) -> Result<(Option<ModelConfig>, InvestigationLimits, bool), String> {
        match &self.stored {
            Some(stored) => Ok((
                stored.service.operator_model().map_err(|e| e.to_string())?,
                stored.service.fix_limits().map_err(|e| e.to_string())?,
                stored.service.agent_network().map_err(|e| e.to_string())?,
            )),
            None => Ok((
                Some(demo_model()),
                InvestigationLimits {
                    version: 0,
                    time_limit_ms: 30 * 60 * 1000,
                    cost_limit_microusd: 5_000_000,
                },
                true,
            )),
        }
    }

    /// The form, preselecting `report` when the owner came from one.
    fn form(&mut self, report: Option<i64>) -> Result<Dialog, String> {
        let listed = self.runs(AgentKind::Fix)?;
        let reports = self.reports()?;
        let now = now_ms();
        let choices: Vec<String> = std::iter::once(NEW.to_owned())
            .chain(
                listed
                    .iter()
                    .take(OFFERED)
                    .map(|item| fix_text::summary(item, now)),
            )
            .collect();
        // A running one is what the owner most likely came to watch.
        let chosen = listed
            .iter()
            .position(|item| item.state == InvestigationState::Running)
            .filter(|_| report.is_none())
            .map_or(0, |index| index + 1);
        let from: Vec<String> = std::iter::once(FROM_INSTRUCTION.to_owned())
            .chain(reports)
            .collect();
        let preselected = report.map_or(0, |id| {
            let prefix = format!("Report #{id} ·");
            from.iter()
                .position(|choice| choice.starts_with(&prefix))
                .unwrap_or(0)
        });
        let mut body = "An agent repairs this project on this host, records every change and verifies the result. Apply shows what it may do before it starts.".to_owned();
        if listed.is_empty() {
            body.push_str("\nNo fixes yet.");
        } else {
            body.push_str(&format!("\n{} fix(es):", listed.len()));
            for item in listed.iter().take(LISTED) {
                let line: String = fix_text::summary(item, now).chars().take(66).collect();
                body.push_str(&format!("\n{line}"));
            }
        }
        let choices: Vec<&str> = choices.iter().map(String::as_str).collect();
        let from: Vec<&str> = from.iter().map(String::as_str).collect();
        Ok(Dialog::new(
            DialogKind::Fixes,
            &format!("Agent fixes · {}", self.name),
            &body,
            vec![
                Field::choice(CHOICE, &choices, choices[chosen]),
                Field::choice(FROM, &from, from[preselected]),
                Field::text(INSTRUCTION, ""),
                Field::choice(WHEN, &[STAY, CONTINUE], STAY),
            ],
        ))
    }

    /// Names the project and the allowed scope before anything starts.
    fn confirmation(&self, request: Confirmed) -> Result<Dialog, String> {
        let (model, limits, network) = self.setup()?;
        let asked = request.report.map_or_else(
            || {
                format!(
                    "Instruction: {}",
                    request.instruction.as_deref().unwrap_or_default()
                )
            },
            |id| format!("Carries out the proposed steps of report #{id}"),
        );
        let body = format!(
            "Project: {}\n{asked}\nWhen verified: {}\nModel: {} · {} · {}{}\nThe runner is paused first.\nMay: use ktask commands for this project; repair its folder and Git state without discarding work; change this account's setup, never with elevated privileges.\nNever: delete or move projects, touch other projects, shut ktask down, approve drafts unless told, or write product code (it adds tasks).",
            self.name,
            if request.resume {
                "continue the queue"
            } else {
                "leave the runner paused"
            },
            model.map_or_else(
                || "unconfigured (set the operator or remediation model)".into(),
                |model| model.identifier()
            ),
            crate::presentation::duration(limits.time_limit_ms.unsigned_abs()),
            report_text::usd(limits.cost_limit_microusd),
            if network {
                ""
            } else {
                " · agent network off: no host changes"
            }
        );
        Ok(Dialog::new(
            DialogKind::FixConfirm(request),
            &format!("Start agent fix · {}?", self.name),
            &body,
            vec![],
        ))
    }

    fn start(&mut self, request: &Confirmed) -> Result<i64, String> {
        let Some(stored) = &mut self.stored else {
            return self.demo.start(request);
        };
        fix::start(
            &stored.paths,
            stored.service.project_id,
            &fix::Request {
                report: request.report,
                instruction: request.instruction.clone(),
                resume: request.resume,
                requester: Actor::default(),
            },
        )
        .map_err(|e| match e {
            crate::storage::Error::Conflict(message) => message,
            other => other.to_string(),
        })
    }

    fn stop(&mut self, id: i64) -> Result<(), String> {
        let Some(stored) = &mut self.stored else {
            return self.demo.stop(id);
        };
        investigate::request_stop(&mut stored.service, &stored.paths, id).map_err(|e| e.to_string())
    }

    fn title(&self, found: &Investigation) -> String {
        format!(
            "Agent fix #{} · {} · {}",
            found.id,
            fix_text::state(found.state),
            self.name
        )
    }

    /// The record of one fix; a running one opens at its newest entry.
    fn view(&mut self, id: i64) -> Result<Dialog, String> {
        let (found, record) = self.read(id)?;
        let running = found.state == InvestigationState::Running;
        let mut dialog = Dialog::new(
            DialogKind::FixView { id, running },
            &self.title(&found),
            &fix_text::document(&found, &record, now_ms()),
            vec![],
        );
        if running {
            dialog.scroll = usize::MAX;
        }
        self.refreshed = Instant::now();
        Ok(dialog)
    }

    /// Reloads the view's record; true when the screen changed.
    fn refresh(&mut self, dialog: &mut Dialog) -> bool {
        let DialogKind::FixView { id, running } = dialog.kind else {
            return false;
        };
        if !running || self.refreshed.elapsed() < REFRESH {
            return false;
        }
        self.refreshed = Instant::now();
        if self.stored.is_none() {
            self.demo.advance(id);
        }
        match self.read(id) {
            Ok((found, record)) => {
                let body = fix_text::document(&found, &record, now_ms());
                let title = self.title(&found);
                let ended = found.state != InvestigationState::Running;
                if ended {
                    // The finished record is read from its start.
                    dialog.scroll = 0;
                } else if dialog.scroll >= dialog.max_scroll {
                    dialog.scroll = usize::MAX;
                }
                let changed = body != dialog.body || title != dialog.title;
                dialog.kind = DialogKind::FixView {
                    id,
                    running: !ended,
                };
                dialog.body = body;
                dialog.title = title;
                changed
            }
            Err(error) => {
                let changed = dialog.error != error;
                dialog.error = error;
                changed
            }
        }
    }
}

fn number(choice: &str) -> Option<i64> {
    choice
        .split_once('#')?
        .1
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

impl App {
    fn bind_fixes(&mut self) -> Result<(), String> {
        let (target, paths) = if self.hub.stored() {
            let paths = self.instance.paths().map_err(|e| e.to_string())?;
            let target = self.agent_target().ok_or("No project selected")?;
            (Some(target), Some(paths))
        } else {
            (None, None)
        };
        self.fixes.bind(target, paths)
    }

    /// Opens the fix form for the project on screen. From a finished report it carries
    /// that report preselected.
    pub(crate) fn open_fixes(&mut self) {
        let report = match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::Report { id, .. }) => Some(*id),
            _ => None,
        };
        match self.bind_fixes().and_then(|()| self.fixes.form(report)) {
            Ok(dialog) => self.dialog = Some(dialog),
            Err(error) => match &mut self.dialog {
                Some(dialog) => dialog.error = error,
                None => self.message(error),
            },
        }
    }

    /// Apply the fix form: confirm a new fix or read the chosen one.
    pub(crate) fn apply_fixes(&mut self, dialog: &Dialog) -> Result<Dialog, String> {
        let choice = dialog.get(CHOICE);
        if choice != NEW {
            return self.fixes.view(number(choice).ok_or("Choose a fix")?);
        }
        let from = dialog.get(FROM);
        let report = if from == FROM_INSTRUCTION {
            None
        } else {
            Some(number(from).ok_or("Choose a report")?)
        };
        let typed = dialog.get(INSTRUCTION);
        let instruction = match (report, typed.is_empty()) {
            (Some(_), true) => None,
            (Some(_), false) => {
                return Err("Choose either a report or an instruction, not both".into());
            }
            (None, false) => Some(typed.to_owned()),
            (None, true) => return Err("Type an instruction, or choose a report".into()),
        };
        self.fixes.confirmation(Confirmed {
            report,
            instruction,
            resume: dialog.get(WHEN) == CONTINUE,
        })
    }

    pub(crate) fn confirm_fix(&mut self, request: &Confirmed) -> Result<Dialog, String> {
        let id = self.fixes.start(request)?;
        self.message(format!(
            "Agent fix #{id} started · closing keeps it running"
        ));
        self.fixes.view(id)
    }

    /// Keys of the fix view; false leaves the key to the dialog.
    pub(crate) fn fix_key(&mut self, code: KeyCode) -> bool {
        use crate::app::Action;
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(DialogKind::FixView { .. })
        ) {
            return false;
        }
        let action = match code {
            KeyCode::Char('s') => Action::FixStop,
            KeyCode::Char('[') => Action::FixStep(1),
            KeyCode::Char(']') => Action::FixStep(-1),
            KeyCode::Char('b') => Action::Fix,
            KeyCode::Home => Action::Scroll(i32::MIN),
            KeyCode::End => Action::Scroll(i32::MAX),
            _ => return false,
        };
        self.act(action);
        true
    }

    pub(crate) fn stop_fix(&mut self) {
        let Some(DialogKind::FixView { id, .. }) = self.dialog.as_ref().map(|d| d.kind.clone())
        else {
            return;
        };
        match self.fixes.stop(id) {
            Ok(()) => {
                self.message(format!("Agent fix #{id}: stop requested"));
                // The demo fixture ends at once; show it without waiting for a refresh.
                if let Ok(dialog) = self.fixes.view(id) {
                    self.dialog = Some(dialog);
                }
            }
            Err(error) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.error = error;
                }
            }
        }
    }

    /// The next older (positive) or newer fix of the same project.
    pub(crate) fn step_fix(&mut self, delta: i32) {
        let Some(DialogKind::FixView { id, .. }) = self.dialog.as_ref().map(|d| d.kind.clone())
        else {
            return;
        };
        let result = self.fixes.runs(AgentKind::Fix).and_then(|listed| {
            let position = listed
                .iter()
                .position(|item| item.id == id)
                .ok_or("That fix is gone")?;
            let next = position
                .saturating_add_signed(delta as isize)
                .min(listed.len().saturating_sub(1));
            self.fixes.view(listed[next].id)
        });
        match result {
            Ok(dialog) => self.dialog = Some(dialog),
            Err(error) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.error = error;
                }
            }
        }
    }

    /// Follows a running fix that is on screen.
    pub(crate) fn tick_fixes(&mut self) -> bool {
        let Some(dialog) = &mut self.screen.dialog else {
            return false;
        };
        self.fixes.refresh(dialog)
    }
}
