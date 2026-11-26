//! Agent reports on screen, from Current Task and from Projects: the form that starts one
//! or picks a past one, the view that follows it live, and the limits form. A stored
//! project goes through [`Service`]; `--demo` uses a fixture that never opens the database
//! or starts an agent.
use crate::{
    app::{Action, App, View},
    application::{
        Actor, ModelConfig, Service,
        investigation::{
            AgentKind, FixClass, FixStep, Investigation, InvestigationLimits, InvestigationState,
            InvestigationUsage, Progress, ProgressKind, Reference, ReferenceKind, Report,
        },
        now_ms,
    },
    dialog::{Dialog, DialogKind, Field},
    instance::Paths,
    presentation::report_text,
    runner::investigate,
    storage::Database,
};
use ratatui::crossterm::event::KeyCode;
use std::time::{Duration, Instant};

const CHOICE: &str = "Report";
const QUESTION: &str = "Question (blank: explain the current state)";
const TASK: &str = "Task ID (blank: current task)";
const TIME_LIMIT: &str = "Time limit per report (minutes)";
const COST_LIMIT: &str = "Cost limit per report (USD)";
const FIX_TIME_LIMIT: &str = "Time limit per fix (minutes)";
const FIX_COST_LIMIT: &str = "Cost limit per fix (USD)";
const NEW: &str = "New report";
/// Past reports the form offers and lists.
const OFFERED: usize = 50;
const LISTED: usize = 4;
const REFRESH: Duration = Duration::from_millis(300);

struct Stored {
    service: Service,
    paths: Paths,
}

/// The simulator's reports: each refresh of a running one moves it a step further.
struct Demo {
    reports: Vec<Investigation>,
    steps: u32,
    limits: InvestigationLimits,
    fix_limits: InvestigationLimits,
}

const DEMO_FINDINGS: [(u32, &str); 3] = [
    (1, "Reading the demo task's attempts and events"),
    (4, "The demo check failed on its last run"),
    (7, "The demo workspace has disk space to spare"),
];
const DEMO_FINISHED: u32 = 12;

fn demo_report() -> Report {
    Report {
        what_happened: "The demo check failed and the demo task stopped.".into(),
        cause: "A simulated assertion no longer matches the simulated output.".into(),
        ktask_defect: None,
        references: vec![Reference {
            kind: ReferenceKind::Log,
            target: "runner.log:12".into(),
            detail: "The simulated check output".into(),
            verified: None,
        }],
        fix_steps: vec![
            FixStep {
                class: FixClass::Repository,
                action: "Update the simulated assertion".into(),
                workaround: false,
            },
            FixStep {
                class: FixClass::KtaskAction,
                action: "Retry the demo task".into(),
                workaround: false,
            },
        ],
    }
}

fn demo_model() -> ModelConfig {
    ModelConfig {
        provider: "codex".into(),
        model: "demo-operator".into(),
        effort: None,
    }
}

fn demo_investigation(id: i64, question: Option<&str>, task_id: Option<i64>) -> Investigation {
    let limits = InvestigationLimits::default();
    Investigation {
        id,
        kind: AgentKind::Report,
        project_id: 0,
        task_id,
        question: question.map(str::to_owned),
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
    }
}

fn demo_progress(investigation: &mut Investigation, message: &str) {
    investigation.progress.push(Progress {
        sequence: investigation.progress.len() as i64 + 1,
        recorded_at_ms: now_ms(),
        kind: ProgressKind::Finding,
        message: message.into(),
    });
}

impl Demo {
    fn new() -> Self {
        let mut finished = demo_investigation(1, Some("Why did the demo check fail?"), None);
        demo_progress(&mut finished, DEMO_FINDINGS[0].1);
        finished.state = InvestigationState::Finished;
        finished.finished_at_ms = Some(finished.started_at_ms);
        finished.report = Some(demo_report());
        let mut stopped = demo_investigation(2, None, None);
        demo_progress(&mut stopped, DEMO_FINDINGS[0].1);
        stopped.state = InvestigationState::Stopped;
        stopped.finished_at_ms = Some(stopped.started_at_ms);
        stopped.reason = Some("Stopped by owner".into());
        Self {
            reports: vec![stopped, finished],
            steps: 0,
            limits: InvestigationLimits::default(),
            fix_limits: InvestigationLimits {
                version: 0,
                time_limit_ms: 30 * 60 * 1000,
                cost_limit_microusd: 5_000_000,
            },
        }
    }

    fn start(&mut self, task: Option<i64>, question: Option<&str>) -> Result<i64, String> {
        if let Some(running) = self
            .reports
            .iter()
            .find(|r| r.state == InvestigationState::Running)
        {
            return Err(format!(
                "Investigation #{} is already running for this project",
                running.id
            ));
        }
        let id = self.reports.first().map_or(1, |r| r.id + 1);
        self.reports
            .insert(0, demo_investigation(id, question, task));
        self.steps = 0;
        Ok(id)
    }

    fn advance(&mut self, id: i64) {
        let Some(report) = self
            .reports
            .iter_mut()
            .find(|r| r.id == id && r.state == InvestigationState::Running)
        else {
            return;
        };
        self.steps += 1;
        if let Some((_, message)) = DEMO_FINDINGS.iter().find(|(step, _)| *step == self.steps) {
            demo_progress(report, message);
        }
        if self.steps >= DEMO_FINISHED {
            report.state = InvestigationState::Finished;
            report.finished_at_ms = Some(now_ms());
            report.report = Some(demo_report());
        }
    }

    fn stop(&mut self, id: i64) -> Result<(), String> {
        let report = self
            .reports
            .iter_mut()
            .find(|r| r.id == id && r.state == InvestigationState::Running)
            .ok_or_else(|| format!("Investigation #{id} is not running"))?;
        report.state = InvestigationState::Stopped;
        report.finished_at_ms = Some(now_ms());
        report.reason = Some("Stopped by owner".into());
        Ok(())
    }
}

/// The reports of the project a dialog is about.
pub(crate) struct Reports {
    stored: Option<Box<Stored>>,
    demo: Demo,
    name: String,
    refreshed: Instant,
}

impl Reports {
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

    fn list(&mut self) -> Result<Vec<Investigation>, String> {
        let Some(stored) = &mut self.stored else {
            return Ok(self.demo.reports.clone());
        };
        investigate::settle(&mut stored.service, &stored.paths).map_err(|e| e.to_string())?;
        let mut listed = stored.service.investigations().map_err(|e| e.to_string())?;
        // Fixes have their own form and view.
        listed.retain(|item| item.kind == AgentKind::Report);
        Ok(listed)
    }

    fn read(&mut self, id: i64) -> Result<Investigation, String> {
        let found = match &mut self.stored {
            Some(stored) => {
                investigate::settle(&mut stored.service, &stored.paths)
                    .map_err(|e| e.to_string())?;
                stored
                    .service
                    .investigation(id)
                    .map_err(|e| e.to_string())?
            }
            None => self.demo.reports.iter().find(|r| r.id == id).cloned(),
        };
        found.ok_or_else(|| format!("Investigation #{id} does not exist in this project"))
    }

    fn start(&mut self, task: Option<i64>, question: Option<String>) -> Result<i64, String> {
        let Some(stored) = &mut self.stored else {
            return self.demo.start(task, question.as_deref());
        };
        if let Some(task) = task
            && stored
                .service
                .task(task)
                .map_err(|e| e.to_string())?
                .is_none()
        {
            return Err(format!("Task #{task} does not exist in this project"));
        }
        investigate::start(
            &stored.paths,
            stored.service.project_id,
            &investigate::Request {
                task,
                question,
                requester: Actor::default(),
            },
        )
        .map_err(|e| e.to_string())
    }

    /// Asks it to end; the view follows until it has.
    fn stop(&mut self, id: i64) -> Result<(), String> {
        let Some(stored) = &mut self.stored else {
            return self.demo.stop(id);
        };
        investigate::request_stop(&mut stored.service, &stored.paths, id).map_err(|e| e.to_string())
    }

    fn setup(&self) -> Result<(Option<ModelConfig>, InvestigationLimits), String> {
        match &self.stored {
            Some(stored) => Ok((
                stored.service.operator_model().map_err(|e| e.to_string())?,
                stored
                    .service
                    .investigation_limits()
                    .map_err(|e| e.to_string())?,
            )),
            None => Ok((Some(demo_model()), self.demo.limits.clone())),
        }
    }

    fn fix_limits(&self) -> Result<InvestigationLimits, String> {
        match &self.stored {
            Some(stored) => stored.service.fix_limits().map_err(|e| e.to_string()),
            None => Ok(self.demo.fix_limits.clone()),
        }
    }

    /// Saves both limits: `report` and `fix` as (time in ms, cost in micro-USD).
    fn save_limits(
        &mut self,
        version: i64,
        report: (i64, i64),
        fix: (i64, i64),
    ) -> Result<(), String> {
        match &mut self.stored {
            Some(stored) => stored
                .service
                .save_investigation_limits(version, report.0, report.1)
                .and_then(|saved| stored.service.save_fix_limits(saved.version, fix.0, fix.1))
                .map(|_| ())
                .map_err(|e| e.to_string()),
            None => {
                let limits = |(time_limit_ms, cost_limit_microusd)| InvestigationLimits {
                    version: version + 1,
                    time_limit_ms,
                    cost_limit_microusd,
                };
                self.demo.limits = limits(report);
                self.demo.fix_limits = limits(fix);
                Ok(())
            }
        }
    }

    fn form(&mut self) -> Result<Dialog, String> {
        let listed = self.list()?;
        let (model, limits) = self.setup()?;
        let now = now_ms();
        let choices: Vec<String> = std::iter::once(NEW.to_owned())
            .chain(
                listed
                    .iter()
                    .take(OFFERED)
                    .map(|item| report_text::summary(item, now)),
            )
            .collect();
        // A running one is what the owner most likely came to watch.
        let chosen = listed
            .iter()
            .position(|item| item.state == InvestigationState::Running)
            .map_or(0, |index| index + 1);
        let mut body = format!(
            "A read-only agent investigates this project on this host and reports what happened and how to fix it. It changes nothing.\nModel: {} · {} min · {}",
            model.map_or_else(
                || "unconfigured (set the operator or remediation model)".into(),
                |model| model.identifier()
            ),
            minutes(limits.time_limit_ms),
            report_text::usd(limits.cost_limit_microusd),
        );
        if listed.is_empty() {
            body.push_str("\nNo reports yet.");
        } else {
            body.push_str(&format!("\n{} report(s):", listed.len()));
            for item in listed.iter().take(LISTED) {
                let line: String = report_text::summary(item, now).chars().take(72).collect();
                body.push_str(&format!("\n{line}"));
            }
        }
        let choices: Vec<&str> = choices.iter().map(String::as_str).collect();
        Ok(Dialog::new(
            DialogKind::Reports,
            &format!("Agent reports · {}", self.name),
            &body,
            vec![
                Field::choice(CHOICE, &choices, choices[chosen]),
                Field::text(QUESTION, ""),
                Field::text(TASK, ""),
            ],
        ))
    }

    /// The live view of one report; a running one opens at its newest entry.
    fn view(&mut self, id: i64) -> Result<Dialog, String> {
        let found = self.read(id)?;
        let running = found.state == InvestigationState::Running;
        let finished = found.state == InvestigationState::Finished;
        let mut dialog = Dialog::new(
            DialogKind::Report {
                id,
                running,
                finished,
            },
            &self.title(&found),
            &report_text::document(&found, now_ms()),
            vec![],
        );
        if running {
            dialog.scroll = usize::MAX;
        }
        self.refreshed = Instant::now();
        Ok(dialog)
    }

    fn title(&self, found: &Investigation) -> String {
        format!(
            "Agent report #{} · {} · {}",
            found.id,
            found.state.label(),
            self.name
        )
    }

    /// Reloads the view's report; true when the screen changed.
    fn refresh(&mut self, dialog: &mut Dialog) -> bool {
        let DialogKind::Report { id, running, .. } = dialog.kind else {
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
            Ok(found) => {
                let body = report_text::document(&found, now_ms());
                let title = self.title(&found);
                let ended = found.state != InvestigationState::Running;
                if ended {
                    // The finished report is read from its start.
                    dialog.scroll = 0;
                } else if dialog.scroll >= dialog.max_scroll {
                    dialog.scroll = usize::MAX;
                }
                let changed = body != dialog.body || title != dialog.title;
                dialog.kind = DialogKind::Report {
                    id,
                    running: !ended,
                    finished: found.state == InvestigationState::Finished,
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

fn minutes(ms: i64) -> String {
    let minutes = ms as f64 / 60_000.0;
    if ms % 60_000 == 0 {
        format!("{minutes:.0}")
    } else {
        format!("{minutes:.2}")
    }
}

fn parse_amount(text: &str, what: &str) -> Result<f64, String> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| format!("{what} must be a positive number"))
}

impl App {
    /// The project a report action is about: the selected row on Projects, else the open one.
    pub(crate) fn agent_target(&self) -> Option<(i64, String)> {
        let id = if self.view == View::Projects {
            self.hub.selected
        } else {
            self.hub.active
        }?;
        let name = self
            .hub
            .rows
            .iter()
            .find(|row| row.key == id)
            .map_or_else(|| self.project_name.clone(), |row| row.project.name.clone());
        Some((id, name))
    }

    fn bind_reports(&mut self) -> Result<(), String> {
        let (target, paths) = if self.hub.stored() {
            let paths = self.instance.paths().map_err(|e| e.to_string())?;
            let target = self.agent_target().ok_or("No project selected")?;
            (Some(target), Some(paths))
        } else {
            (None, None)
        };
        self.reports.bind(target, paths)
    }

    /// Opens the report form for the project on screen.
    pub(crate) fn open_reports(&mut self) {
        match self.bind_reports().and_then(|()| self.reports.form()) {
            Ok(dialog) => self.dialog = Some(dialog),
            Err(error) => self.message(error),
        }
    }

    /// Apply the report form: start a new report or read the chosen one.
    pub(crate) fn apply_reports(&mut self, dialog: &Dialog) -> Result<Dialog, String> {
        let choice = dialog.get(CHOICE);
        let id = if choice == NEW {
            let task = dialog.get(TASK).trim_start_matches('#');
            let task = if task.is_empty() {
                None
            } else {
                Some(
                    task.parse::<i64>()
                        .ok()
                        .filter(|id| *id > 0)
                        .ok_or("Enter a task ID, or leave it blank")?,
                )
            };
            let question = dialog.get(QUESTION);
            let question = (!question.is_empty()).then(|| question.to_owned());
            let id = self.reports.start(task, question)?;
            self.message(format!(
                "Agent report #{id} started · closing keeps it running"
            ));
            id
        } else {
            choice
                .trim_start_matches('#')
                .split_whitespace()
                .next()
                .and_then(|id| id.parse().ok())
                .ok_or("Choose a report")?
        };
        self.reports.view(id)
    }

    /// Keys of the report view; false leaves the key to the dialog.
    pub(crate) fn report_key(&mut self, code: KeyCode) -> bool {
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(DialogKind::Report { .. })
        ) {
            return false;
        }
        let action = match code {
            KeyCode::Char('s') => Action::ReportStop,
            KeyCode::Char('[') => Action::ReportStep(1),
            KeyCode::Char(']') => Action::ReportStep(-1),
            KeyCode::Char('b') => Action::Investigate,
            KeyCode::Char('f')
                if matches!(
                    self.dialog.as_ref().map(|d| &d.kind),
                    Some(DialogKind::Report { finished: true, .. })
                ) =>
            {
                Action::Fix
            }
            KeyCode::Home => Action::Scroll(i32::MIN),
            KeyCode::End => Action::Scroll(i32::MAX),
            _ => return false,
        };
        self.act(action);
        true
    }

    pub(crate) fn stop_report(&mut self) {
        let Some(DialogKind::Report { id, .. }) = self.dialog.as_ref().map(|d| d.kind.clone())
        else {
            return;
        };
        let result = self.reports.stop(id);
        match result {
            Ok(()) => {
                self.message(format!("Agent report #{id}: stop requested"));
                // The demo fixture ends at once; show it without waiting for a refresh.
                if let Ok(dialog) = self.reports.view(id) {
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

    /// The next older (positive) or newer report of the same project.
    pub(crate) fn step_report(&mut self, delta: i32) {
        let Some(DialogKind::Report { id, .. }) = self.dialog.as_ref().map(|d| d.kind.clone())
        else {
            return;
        };
        let result = self.reports.list().and_then(|listed| {
            let position = listed
                .iter()
                .position(|item| item.id == id)
                .ok_or("That report is gone")?;
            let next = position
                .saturating_add_signed(delta as isize)
                .min(listed.len().saturating_sub(1));
            self.reports.view(listed[next].id)
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

    pub(crate) fn report_limits_form(&mut self) {
        let result = self
            .bind_reports()
            .and_then(|()| Ok((self.reports.setup()?.1, self.reports.fix_limits()?)));
        let dollars = |microusd: i64| format!("{:.2}", microusd as f64 / 1_000_000.0);
        match result {
            Ok((limits, fix)) => {
                self.dialog = Some(Dialog::new(
                    DialogKind::ReportLimits(limits.version),
                    "Agent report limits",
                    "What one report or fix of this project may use; running ones keep theirs.\nCodex reports no cost, so only the time limit bounds it.\nThe model is the operator role in m Defaults; unset, the remediation model.",
                    vec![
                        Field::text(TIME_LIMIT, &minutes(limits.time_limit_ms)),
                        Field::text(COST_LIMIT, &dollars(limits.cost_limit_microusd)),
                        Field::text(FIX_TIME_LIMIT, &minutes(fix.time_limit_ms)),
                        Field::text(FIX_COST_LIMIT, &dollars(fix.cost_limit_microusd)),
                    ],
                ))
            }
            Err(error) => self.message(error),
        }
    }

    pub(crate) fn apply_report_limits(
        &mut self,
        dialog: &Dialog,
        version: i64,
    ) -> Result<(), String> {
        let pair = |time: &str, cost: &str, what: &str| -> Result<(i64, i64), String> {
            let time = parse_amount(dialog.get(time), &format!("The {what} time limit"))?;
            let cost = parse_amount(dialog.get(cost), &format!("The {what} cost limit"))?;
            Ok((
                (time * 60_000.0).round() as i64,
                (cost * 1_000_000.0).round() as i64,
            ))
        };
        let report = pair(TIME_LIMIT, COST_LIMIT, "report")?;
        let fix = pair(FIX_TIME_LIMIT, FIX_COST_LIMIT, "fix")?;
        self.reports.save_limits(version, report, fix)?;
        self.message("Agent report limits saved");
        Ok(())
    }

    /// Follows a running report that is on screen.
    pub(crate) fn tick_reports(&mut self) -> bool {
        let Some(dialog) = &mut self.screen.dialog else {
            return false;
        };
        self.reports.refresh(dialog)
    }
}
