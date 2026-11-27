//! A linked host's project on the same screens as a local one. It is read and changed only
//! through the link: reads are the host's own application queries, forms become the task
//! commands its CLI offers, and its state machine decides. Nothing of the host is opened
//! here. Requests run beside the screen; the last answer stays readable while offline.
use crate::{
    app::{Action, EvidenceSection, Screen, View},
    application::{
        self as domain, Definition, EvidenceCursor, EvidenceSearch, ModelConfig, now_ms,
    },
    backend::{Backend, EvidenceAt},
    clock,
    dialog::{Dialog, DialogKind},
    forms,
    instance::{Instance, Paths},
    lens_ui,
    link::screen::{self as wire, Failed, Known, ProjectView, Query},
    presentation::{Status, Task},
    project_view::{
        self, APPROVAL, FormChoices, ProjectData, before_id, chosen_category, control_notice,
        versioned,
    },
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        mpsc::{self, Receiver, TryRecvError},
    },
    thread,
    time::{Duration, Instant},
};

/// How soon after an answer the host is read again; a slow read waits at least as long as
/// it took, so that a large project does not keep its host busy.
const POLL: Duration = Duration::from_millis(700);
/// How old the held readiness may become.
const READINESS: Duration = Duration::from_secs(5);
/// How long a screen waits for the first read, and for a page of evidence.
const OPENING: Duration = Duration::from_secs(20);
const PAGE: Duration = Duration::from_secs(5);

/// What the screens know about the link behind a remote project.
pub(crate) struct LinkState {
    pub host: String,
    /// Set while the host cannot be reached: when it was last heard from, if known.
    pub offline: Option<Option<i64>>,
    /// The host lets this computer read only.
    pub read_only: bool,
}

/// What a parent cannot do to a child's project.
fn local_only(action: Action) -> bool {
    matches!(
        action,
        Action::Configure
            | Action::ProjectModels
            | Action::UserDefaults
            | Action::Profiles
            | Action::Categories
            | Action::Lenses
            | Action::ProjectChecks
            | Action::Delivery
            | Action::Storage
            | Action::BuildCache
            | Action::Tools
            | Action::ExportSettings
            | Action::ImportSettings
            | Action::ReportLimits
            | Action::Models
            | Action::Breakdown
            | Action::Supersede
            | Action::CopyToQueue
            | Action::Import
            | Action::Export
            | Action::Investigate
            | Action::Fix
    )
}

fn changes(action: Action) -> bool {
    matches!(
        action,
        Action::Run
            | Action::Pause
            | Action::Stop
            | Action::Restart
            | Action::MarkDone
            | Action::CancelTask
            | Action::Approve
            | Action::Reject
            | Action::Answer
            | Action::Add
            | Action::Insert
            | Action::Edit
            | Action::Move
    )
}

impl LinkState {
    /// "devbox is offline · last contact Nov 26 12:31".
    pub fn offline_text(&self) -> Option<String> {
        let contact = self.offline?.map_or_else(
            || "never".to_owned(),
            |at| clock::format_local(at, now_ms()),
        );
        Some(format!("{} is offline · last contact {contact}", self.host))
    }

    /// The header's description of the data on screen.
    pub fn label(&self) -> String {
        match self.offline {
            Some(Some(at)) => format!(
                "OFFLINE · last contact {}",
                clock::format_local(at, now_ms())
            ),
            Some(None) => "OFFLINE".into(),
            None if self.read_only => "linked · read-only".into(),
            None => "linked · live".into(),
        }
    }

    /// Why the link does not offer `action`, whatever the task allows.
    pub fn refusal(&self, action: Action) -> Option<String> {
        if local_only(action) {
            return Some(format!(
                "Local-only: not available over the link · do this on {}",
                self.host
            ));
        }
        let asks = changes(action)
            || matches!(
                action,
                Action::Inspect | Action::InspectSection(_) | Action::Stats | Action::Readiness
            );
        if asks && let Some(offline) = self.offline_text() {
            return Some(format!("{offline} · actions wait until it reconnects"));
        }
        (self.read_only && changes(action)).then(|| {
            format!(
                "The link to {} is read-only · ktask link control on there allows changes",
                self.host
            )
        })
    }

    /// Whether a button stays on screen, disabled, to say why it is refused.
    pub fn shows_disabled(&self, action: Action) -> bool {
        !local_only(action) && self.refusal(action).is_some()
    }
}

/// Which command an answer belongs to.
#[derive(Clone, Copy, Debug)]
enum Sent {
    Run,
    Pause,
    Saved,
    Added,
    Control(Action, usize),
}

struct Waiting {
    ticket: u64,
    sent: Sent,
    command: Vec<String>,
    answer: Receiver<Result<Value, Failed>>,
}

/// One read of the project, as the thread that made it reports it.
struct Polled {
    answer: Result<ProjectView, Failed>,
    /// When the host was last heard from, looked up when the read failed.
    contact: Option<i64>,
    /// How long the host took, however long the answer then waits to be collected.
    took: Duration,
}

/// The parts of the project as last received.
#[derive(Default)]
struct Held {
    session: Option<domain::session::ExecutionSession>,
    supervisor_alive: bool,
    queue: Vec<domain::Task>,
    history: Vec<domain::Task>,
    events: Vec<(Option<i64>, String)>,
}

pub(crate) struct RemoteWorkspace {
    paths: Arc<Paths>,
    host: String,
    project: i64,
    stored: BTreeMap<usize, domain::Task>,
    held: Held,
    known: Known,
    choices: FormChoices,
    launch_issue: Option<String>,
    inspected: Instant,
    poll: Option<Receiver<Polled>>,
    next_poll: Instant,
    waiting: Vec<Waiting>,
    /// Commands that succeeded, shown once a read made after them is on screen, so that a
    /// form opened next sees what they changed.
    done: Vec<(u64, Sent, Value)>,
    tickets: u64,
    /// The host's last refusal to be read, said once.
    complaint: Option<String>,
    /// A task just added, to select once a read lists it.
    reveal: Option<usize>,
}

/// Runs `work` on its own thread and delivers what it returns.
fn beside<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Receiver<T> {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(work());
    });
    receive
}

fn describe(host: &str, failed: Failed) -> String {
    match failed {
        Failed::Unreachable(why) => format!("{host} is offline ({why}) · nothing was sent"),
        Failed::Lost => format!(
            "Lost the connection to {host} · whether it ran is unknown; check the task before repeating it"
        ),
        Failed::Refused(why) => why,
    }
}

impl RemoteWorkspace {
    /// Reads the project once, so that the screens open filled.
    pub fn open(
        screen: &mut Screen,
        instance: Instance,
        host: &str,
        project: i64,
    ) -> Result<Self, String> {
        let paths = Arc::new(instance.paths().map_err(|e| e.to_string())?);
        let mut workspace = Self::unread(paths, host, project);
        screen.remote = Some(LinkState {
            host: host.into(),
            offline: None,
            read_only: false,
        });
        let first = workspace
            .start_poll(true)
            .recv_timeout(OPENING)
            .map_err(|_| format!("{host} did not answer in time · try again"))?;
        match first.answer {
            Ok(view) => workspace.take(screen, view)?,
            Err(failed) => return Err(describe(host, failed)),
        };
        workspace.next_poll = Instant::now() + POLL.max(first.took);
        Ok(workspace)
    }

    /// A workspace that holds nothing of the project yet.
    fn unread(paths: Arc<Paths>, host: &str, project: i64) -> Self {
        Self {
            paths,
            host: host.into(),
            project,
            stored: BTreeMap::new(),
            held: Held::default(),
            known: Known::default(),
            choices: FormChoices::default(),
            launch_issue: None,
            inspected: Instant::now(),
            poll: None,
            next_poll: Instant::now(),
            waiting: Vec::new(),
            done: Vec::new(),
            tickets: 0,
            complaint: None,
            reveal: None,
        }
    }

    fn start_poll(&self, inspect: bool) -> Receiver<Polled> {
        let (paths, host, project) = (self.paths.clone(), self.host.clone(), self.project);
        let known = Known {
            inspect,
            ..self.known.clone()
        };
        beside(move || {
            let asked = Instant::now();
            let answer = wire::ask(&paths, &host, project, &Query::Project(known));
            let took = asked.elapsed();
            let contact = matches!(answer, Err(Failed::Unreachable(_) | Failed::Lost))
                .then(|| wire::last_contact(&paths, &host))
                .flatten();
            Polled {
                answer,
                contact,
                took,
            }
        })
    }

    /// Applies a read to the screens; true when what they show changed.
    fn take(&mut self, screen: &mut Screen, view: ProjectView) -> Result<bool, String> {
        let first = self.known.queue.is_empty();
        let mut changed = first
            || view.session != self.held.session
            || view.supervisor_alive != self.held.supervisor_alive;
        self.held.session = view.session;
        self.held.supervisor_alive = view.supervisor_alive;
        if let Some(queue) = view.queue.data {
            self.held.queue = queue;
            changed = true;
        }
        if let Some(history) = view.history.data {
            self.held.history = history;
            changed = true;
        }
        if let Some(events) = view.events.data {
            self.held.events = events;
            changed = true;
        }
        self.known.queue = view.queue.digest;
        self.known.history = view.history.digest;
        self.known.events = view.events.digest;
        self.choices = view.choices;
        self.launch_issue = view.launch_issue;
        let mut redraw = changed || screen.auto_advance != view.continue_queue;
        screen.auto_advance = view.continue_queue;
        if let Some(link) = &mut screen.remote {
            redraw |= link.read_only == view.control;
            link.read_only = !view.control;
        }
        if changed {
            self.stored = project_view::show(
                screen,
                ProjectData {
                    session: self.held.session.clone(),
                    queue: self.held.queue.clone(),
                    history: self.held.history.clone(),
                    events: self.held.events.clone(),
                    supervisor_alive: self.held.supervisor_alive,
                },
            )?;
        }
        self.known.running = project_view::running(screen).map(|id| id as i64);
        if let Some(readiness) = view.readiness {
            redraw |= screen.readiness.as_ref() != Some(&readiness);
            screen.readiness = Some(readiness);
            self.inspected = Instant::now();
        }
        if let Some(id) = self.reveal
            && screen.queue.iter().any(|task| task.id == id)
        {
            screen.queue_catalog.selected = Some(id);
            self.reveal = None;
            redraw = true;
        }
        Ok(redraw)
    }

    /// Closes the forms and shows the notices of commands whose effect is now on screen.
    fn announce(&mut self, screen: &mut Screen) -> bool {
        let done = std::mem::take(&mut self.done);
        let changed = !done.is_empty();
        for (ticket, sent, data) in done {
            if screen
                .dialog
                .as_ref()
                .is_some_and(|dialog| dialog.ticket == Some(ticket))
            {
                screen.dialog = None;
            }
            self.succeeded(screen, sent, &data);
        }
        changed
    }

    fn receive(
        &mut self,
        screen: &mut Screen,
        answer: Result<ProjectView, Failed>,
        contact: Option<i64>,
    ) -> bool {
        let was_offline = screen.remote.as_ref().is_some_and(|l| l.offline.is_some());
        match answer {
            Ok(view) => {
                if let Some(link) = &mut screen.remote {
                    link.offline = None;
                }
                let changed = match self.take(screen, view) {
                    Ok(changed) => changed,
                    Err(error) => {
                        screen.message(error);
                        return true;
                    }
                };
                if was_offline {
                    screen.message(format!("{} is back · live again", self.host));
                }
                self.complaint = None;
                // Countdowns of a waiting task advance with each read.
                changed || was_offline || screen.current().is_some_and(Task::waiting_for_limit)
            }
            Err(Failed::Refused(error)) => {
                let news = self.complaint.as_ref() != Some(&error);
                if news {
                    screen.message(format!("{}: {error}", self.host));
                    self.complaint = Some(error);
                }
                news
            }
            Err(Failed::Unreachable(_) | Failed::Lost) => {
                let Some(link) = &mut screen.remote else {
                    return false;
                };
                let contact = contact.or_else(|| link.offline.flatten());
                let changed = link.offline != Some(contact);
                link.offline = Some(contact);
                if !was_offline && let Some(text) = link.offline_text() {
                    screen.message(format!("{text} · showing its last state"));
                }
                // Read it afresh once it is back.
                self.known.running = None;
                changed
            }
        }
    }

    /// Applies the answers of commands sent earlier.
    fn finish(&mut self, screen: &mut Screen) -> bool {
        let mut finished = Vec::new();
        self.waiting.retain(|waiting| {
            let answer = match waiting.answer.try_recv() {
                Ok(answer) => answer,
                Err(TryRecvError::Empty) => return true,
                Err(TryRecvError::Disconnected) => Err(Failed::Lost),
            };
            finished.push((waiting.ticket, waiting.sent, answer));
            false
        });
        let changed = !finished.is_empty();
        for (ticket, sent, answer) in finished {
            match answer {
                Ok(data) => self.done.push((ticket, sent, data)),
                Err(failed) => {
                    let message = describe(&self.host, failed);
                    match screen
                        .dialog
                        .as_mut()
                        .filter(|dialog| dialog.ticket == Some(ticket))
                    {
                        Some(dialog) => {
                            dialog.ticket = None;
                            dialog.error = message;
                        }
                        None => screen.message(message),
                    }
                }
            }
        }
        if changed {
            // A read already under way may predate the change: make another one now.
            self.poll = None;
            self.next_poll = Instant::now();
            self.known.running = None;
            self.inspected = Instant::now()
                .checked_sub(READINESS)
                .unwrap_or_else(Instant::now);
        }
        changed
    }

    fn succeeded(&mut self, screen: &mut Screen, sent: Sent, data: &Value) {
        let host = &self.host;
        let mut notice = match sent {
            Sent::Run => {
                format!("Supervisor started on {host} · quitting this screen does not stop it")
            }
            Sent::Pause if data["runner_was_running"] == false => {
                format!("Nothing is running on {host}")
            }
            Sent::Pause => format!("Paused on {host} · r resumes"),
            Sent::Saved => format!("Saved on {host}"),
            Sent::Added => {
                let id = data["tasks"][0]["id"].as_u64().map(|id| id as usize);
                if let Some(id) = id {
                    screen.view = View::Queue;
                    screen.queue_catalog.query.clear();
                    screen.queue_catalog.filter = 0;
                    self.reveal = Some(id);
                }
                id.map_or_else(
                    || format!("Saved on {host}"),
                    |id| format!("Saved on {host} · added #{id}"),
                )
            }
            Sent::Control(Action::Restart, id) => {
                format!("#{id} restarted on {host} · running from its current HEAD")
            }
            Sent::Control(action, id) => format!(
                "{} · on {host}",
                control_notice(id as i64, action, data["stash"].as_str().map(str::to_owned))
            ),
        };
        if let Some(error) = data["continuation_error"].as_str() {
            notice.push_str(&format!(" · queue waiting: {error}"));
        }
        screen.message(notice);
    }

    /// Sends a command once: the screen may not show its effect before the answer, so the
    /// same command waiting for one is not repeated.
    fn send(&mut self, screen: &mut Screen, sent: Sent, args: Vec<String>) -> u64 {
        if let Some(waiting) = self.waiting.iter().find(|w| w.command == args) {
            screen.message(format!("Still waiting for {} to answer", self.host));
            return waiting.ticket;
        }
        self.tickets += 1;
        let (paths, host, project) = (self.paths.clone(), self.host.clone(), self.project);
        let command = args.clone();
        self.waiting.push(Waiting {
            ticket: self.tickets,
            sent,
            command,
            answer: beside(move || wire::command(&paths, &host, project, &args)),
        });
        screen.message(format!("Sending to {}…", self.host));
        self.tickets
    }

    /// One read the screen waits for, for at most a few seconds.
    fn ask<T: DeserializeOwned + Send + 'static>(&self, query: Query) -> Result<T, String> {
        let (paths, host, project) = (self.paths.clone(), self.host.clone(), self.project);
        beside(move || wire::ask(&paths, &host, project, &query))
            .recv_timeout(PAGE)
            .map_err(|_| format!("{} did not answer in time", self.host))?
            .map_err(|failed| match failed {
                Failed::Unreachable(why) => format!("{} is offline ({why})", self.host),
                Failed::Lost => format!("Lost the connection to {}", self.host),
                Failed::Refused(why) => why,
            })
    }

    fn answer_form(&self, screen: &Screen) -> Result<Dialog, String> {
        let id = screen.current().ok_or("No current task")?.id;
        let question = self
            .stored
            .get(&id)
            .and_then(|task| task.attempts.last())
            .and_then(|attempt| attempt.question.as_deref())
            .ok_or("No open question; refresh the task")?;
        Ok(project_view::with_version(
            forms::answer(id, question),
            &self.stored,
            id,
        ))
    }

    /// The command a submitted form asks the host to run; None when it changes nothing.
    fn command(&self, dialog: &Dialog) -> Result<Option<(Sent, Vec<String>)>, String> {
        let target = || {
            dialog
                .expected
                .ok_or_else(|| "Missing task version; reopen the form".to_owned())
        };
        let on = |verb: &str| -> Result<Vec<String>, String> {
            let target = target()?;
            Ok(vec![
                verb.into(),
                target.id.to_string(),
                format!("--if-version={}", target.version),
            ])
        };
        let required = |label: &str, what: &str| {
            let value = dialog.get(label);
            (!value.is_empty())
                .then(|| value.to_owned())
                .ok_or_else(|| format!("{what} is required"))
        };
        let controlled = |action, id: &usize, verb| -> Result<_, String> {
            let mut args = on(verb)?;
            args.push(format!("--reason={}", dialog.get(forms::REASON)));
            if action == Action::Restart {
                args.push("--background".into());
            }
            let sent = match action {
                Action::CancelTask => Sent::Saved,
                _ => Sent::Control(action, *id),
            };
            Ok((sent, args))
        };
        Ok(Some(match &dialog.kind {
            DialogKind::Stop(id) => controlled(Action::Stop, id, "stop")?,
            DialogKind::Restart(id) => controlled(Action::Restart, id, "restart")?,
            DialogKind::MarkDone(id) => controlled(Action::MarkDone, id, "done")?,
            DialogKind::CancelTask(id) => controlled(Action::CancelTask, id, "cancel")?,
            DialogKind::Answer(_) => {
                let mut args = on("answer")?;
                args.push(format!("--text={}", required(forms::ANSWER, "An answer")?));
                (Sent::Saved, args)
            }
            DialogKind::Reject(_) => {
                let mut args = on("reject")?;
                args.push(format!(
                    "--reason={}",
                    required(forms::REJECT_REASON, "A reason")?
                ));
                (Sent::Saved, args)
            }
            DialogKind::Move(_) => {
                let mut args = on("move")?;
                if let Some(before) = before_id(dialog.get(forms::BEFORE))? {
                    args.push(format!("--before={before}"));
                }
                (Sent::Saved, args)
            }
            DialogKind::Task { id: None, insert } => (Sent::Added, add_args(dialog, *insert)?),
            DialogKind::Task { id: Some(id), .. } => {
                let task = self.stored.get(id).ok_or("Task no longer available")?;
                let mut args = on("edit")?;
                args.extend(edit_args(dialog, task, &self.choices)?);
                (Sent::Saved, args)
            }
            _ => return Ok(None),
        }))
    }
}

/// The definition a task form describes, checked by the rules the host applies too.
fn checked(dialog: &Dialog, category: Option<String>) -> Result<Definition, String> {
    let definition = Definition {
        title: dialog.get(forms::TITLE).into(),
        goal: dialog.get(forms::GOAL).into(),
        kind: project_view::kind(dialog.get(forms::KIND)),
        category,
        lenses: lens_ui::selection(dialog)?,
        criteria: dialog
            .get(forms::CHECK)
            .lines()
            .map(str::to_owned)
            .collect(),
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    definition.validate().map_err(|e| e.to_string())?;
    Ok(definition)
}

fn lens_args(args: &mut Vec<String>, lenses: Option<&[String]>) {
    match lenses {
        Some([]) => args.push("--no-lenses".into()),
        Some(names) => args.extend(names.iter().map(|name| format!("--lens={name}"))),
        None => {}
    }
}

fn add_args(dialog: &Dialog, insert: bool) -> Result<Vec<String>, String> {
    let kind = project_view::kind(dialog.get(forms::KIND));
    let category = chosen_category(dialog.get(forms::CATEGORY), &(domain::Kind::Ai, None), kind)?;
    let definition = checked(dialog, category)?;
    let mut args = vec![
        "add".to_owned(),
        format!("--title={}", definition.title),
        format!("--goal={}", definition.goal),
        format!("--kind={}", definition.kind.as_str()),
        format!("--approval={}", approval(dialog)),
    ];
    args.extend(
        definition
            .criteria
            .iter()
            .map(|criterion| format!("--criterion={criterion}")),
    );
    if let Some(category) = &definition.category {
        args.push(format!("--category={category}"));
    }
    lens_args(&mut args, definition.lenses.as_deref());
    let (model, effort) = (dialog.get(forms::MODEL), dialog.get(forms::EFFORT));
    if !model.is_empty() {
        ModelConfig::from_fields(model, effort)?;
        args.push(format!("--model=execution={model}"));
        // With a new model, blank effort means none, as it does locally.
        let effort = if effort.is_empty() { "none" } else { effort };
        args.push(format!("--effort=execution={effort}"));
    } else if !effort.is_empty() {
        args.push(format!("--effort=execution={effort}"));
    }
    if insert && let Some(before) = before_id(dialog.get(forms::BEFORE))? {
        args.push(format!("--before={before}"));
    }
    Ok(args)
}

fn approval(dialog: &Dialog) -> &'static str {
    if dialog.get(APPROVAL) == "Ready" {
        "ready"
    } else {
        "draft"
    }
}

/// The changes of an Edit form as `ktask edit` options.
fn edit_args(
    dialog: &Dialog,
    task: &domain::Task,
    choices: &FormChoices,
) -> Result<Vec<String>, String> {
    let revision = task.definition();
    let stored = &revision.definition;
    let previous = (stored.kind, stored.category.clone());
    let kind = project_view::kind(dialog.get(forms::KIND));
    let category = chosen_category(dialog.get(forms::CATEGORY), &previous, kind)?;
    let definition = checked(dialog, category)?;
    let mut args = vec![
        format!("--title={}", definition.title),
        format!("--goal={}", definition.goal),
        format!("--approval={}", approval(dialog)),
    ];
    if stored.criteria.join("\n") != dialog.get(forms::CHECK) {
        args.extend(
            definition
                .criteria
                .iter()
                .map(|criterion| format!("--criterion={criterion}")),
        );
    }
    // Naming the kind is how `edit` returns the category and lenses to their defaults.
    let default_category =
        definition.category.is_none() && stored.category.is_some() && kind != domain::Kind::Human;
    let default_lenses = definition.lenses.is_none()
        && !(revision.lenses.is_empty() && choices.lens_defaults.is_empty());
    let reset = kind != stored.kind || default_category || default_lenses;
    if reset {
        args.push(format!("--kind={}", kind.as_str()));
    }
    if let Some(category) = &definition.category
        && (reset || stored.category.as_ref() != Some(category))
    {
        args.push(format!("--category={category}"));
    }
    lens_args(&mut args, definition.lenses.as_deref());
    Ok(args)
}

impl Backend for RemoteWorkspace {
    fn act(&mut self, screen: &mut Screen, action: Action) {
        match action {
            Action::Readiness => {
                screen.dialog = Some(project_view::readiness_dialog(
                    screen,
                    self.launch_issue.as_deref(),
                ));
            }
            Action::Run if screen.current().is_some_and(Task::awaits_answer) => {
                match self.answer_form(screen) {
                    Ok(dialog) => screen.dialog = Some(dialog),
                    Err(error) => screen.message(error),
                }
            }
            Action::Run
                if screen
                    .current()
                    .is_some_and(|task| task.status() == Status::NeedsDependency) =>
            {
                screen.message(format!(
                    "Local-only: set up the dependency and recheck it on {}",
                    self.host
                ));
            }
            Action::Run => {
                self.send(screen, Sent::Run, vec!["run".into(), "--background".into()]);
            }
            Action::Pause => {
                self.send(screen, Sent::Pause, vec!["pause".into()]);
            }
            Action::Approve => {
                let Some(task) = screen
                    .selected_task()
                    .and_then(|task| self.stored.get(&task.id))
                    .map(versioned)
                else {
                    return;
                };
                let args = vec![
                    "approve".into(),
                    task.id.to_string(),
                    format!("--if-version={}", task.version),
                ];
                self.send(screen, Sent::Saved, args);
            }
            _ => {}
        }
    }

    fn open(&mut self, screen: &mut Screen, action: Action) -> Result<Option<Dialog>, String> {
        let target = screen.target(action).map(|task| task.id);
        Ok(match action {
            Action::Add | Action::Insert | Action::Edit => Some(project_view::task_form(
                screen,
                action,
                &self.stored,
                &self.choices,
                &format!("on {}", self.host),
            )),
            Action::Move => {
                target.map(|id| project_view::with_version(forms::move_task(id), &self.stored, id))
            }
            Action::Reject => {
                target.map(|id| project_view::with_version(forms::reject(id), &self.stored, id))
            }
            Action::Stop | Action::Restart | Action::MarkDone | Action::CancelTask => {
                screen.target(action).and_then(|task| {
                    forms::control(action, task)
                        .map(|dialog| project_view::with_version(dialog, &self.stored, task.id))
                })
            }
            Action::Answer => Some(self.answer_form(screen)?),
            Action::Help => Some(forms::help(&format!(
                "This project is on {host}: changes are sent there and its state machine decides. {help}\nConfiguration, tools, storage, project move and delete, reports, fixes, models, supersede, copy, import and export are local-only: do them on {host}.",
                host = self.host,
                help = project_view::HELP
            ))),
            _ => None,
        })
    }

    fn apply(&mut self, screen: &mut Screen, dialog: &mut Dialog) -> Result<bool, String> {
        // Already on its way; the answer closes the form.
        if dialog.ticket.is_some() {
            return Ok(false);
        }
        let Some((sent, args)) = self.command(dialog)? else {
            return Ok(true);
        };
        let action = match sent {
            Sent::Added => Action::Add,
            _ => Action::Edit,
        };
        if let Some(refusal) = screen.remote.as_ref().and_then(|l| l.refusal(action)) {
            return Err(refusal);
        }
        dialog.ticket = Some(self.send(screen, sent, args));
        dialog.error = format!("Sending to {}…", self.host);
        Ok(false)
    }

    fn stats(&self, _screen: &Screen) -> Result<Dialog, String> {
        let body: String = self.ask(Query::Stats)?;
        Ok(Dialog::new(
            DialogKind::Stats,
            &format!("Project statistics · {}", self.host),
            &body,
            vec![],
        ))
    }

    fn tick(&mut self, screen: &mut Screen) -> bool {
        let mut changed = self.finish(screen);
        if let Some(poll) = &self.poll {
            match poll.try_recv() {
                Ok(polled) => {
                    self.poll = None;
                    self.next_poll = Instant::now() + POLL.max(polled.took);
                    changed |= self.receive(screen, polled.answer, polled.contact);
                    changed |= self.announce(screen);
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.poll = None;
                    changed |= self.announce(screen);
                }
            }
        }
        if self.poll.is_none() && Instant::now() >= self.next_poll {
            // Nothing readiness reads can change while the host's supervisor runs.
            let inspect = screen.readiness.is_none()
                || self.known.running.is_none() && self.inspected.elapsed() >= READINESS;
            self.poll = Some(self.start_poll(inspect));
        }
        changed
    }

    fn busy(&self) -> bool {
        !self.waiting.is_empty() || !self.done.is_empty()
    }

    fn resumed(&mut self) {
        // A read left uncollected while parked may describe the host as it was long ago.
        self.poll = None;
        self.next_poll = Instant::now();
    }

    fn inspect_body(&self, _screen: &Screen, dialog: &Dialog) -> String {
        let DialogKind::Inspect {
            id,
            attempt,
            section,
            page,
            chunk,
            artifact,
            step,
            match_sequence,
            watermark,
            ..
        } = dialog.kind
        else {
            return String::new();
        };
        let Some(task) = self.stored.get(&id) else {
            return String::new();
        };
        let Some(record) = task.attempts.get(attempt) else {
            return project_view::inspect_overview(task);
        };
        let query = Query::Evidence {
            attempt: record.id,
            section,
            page,
            chunk,
            artifact,
            step,
            selected_match: match_sequence.and_then(|sequence| {
                dialog
                    .fields
                    .first()
                    .map(|field| (sequence, field.value.clone()))
            }),
            watermark,
        };
        let evidence = self
            .ask::<String>(query)
            .unwrap_or_else(|error| format!("Evidence unavailable: {error}"));
        project_view::inspect_attempt(dialog, task, record, &evidence)
    }

    fn find_label(&self, section: EvidenceSection) -> Option<&'static str> {
        project_view::find_label(section)
    }

    fn watermark(&self, attempt: i64) -> Result<i64, String> {
        self.ask(Query::Watermark { attempt })
    }

    fn artifact_id(&self, attempt: i64, index: usize) -> Option<i64> {
        self.ask(Query::ArtifactId {
            attempt,
            artifact: index,
        })
        .ok()
        .flatten()
    }

    fn artifact_count(&self, attempt: i64) -> Result<usize, String> {
        self.ask(Query::ArtifactCount { attempt })
    }

    fn page_count(
        &self,
        _screen: &Screen,
        at: EvidenceAt,
        section: EvidenceSection,
        artifact: usize,
    ) -> Result<usize, String> {
        self.ask(Query::PageCount {
            attempt: at.attempt,
            watermark: at.watermark,
            transcript: (section != EvidenceSection::Output).then_some(artifact),
        })
    }

    fn chunk_count(&self, at: EvidenceAt, page: usize) -> Result<usize, String> {
        self.ask(Query::ChunkCount {
            attempt: at.attempt,
            page,
            watermark: at.watermark,
        })
    }

    fn step_count(&self, _screen: &Screen, at: EvidenceAt, reviews: bool) -> Result<usize, String> {
        self.ask(Query::StepCount {
            attempt: at.attempt,
            reviews,
        })
    }

    fn search(
        &self,
        _screen: &Screen,
        at: EvidenceAt,
        section: EvidenceSection,
        artifact: usize,
        query: &str,
        cursor: EvidenceCursor,
    ) -> Result<EvidenceSearch, String> {
        self.ask(Query::Search {
            attempt: at.attempt,
            section: section.index(),
            artifact,
            watermark: at.watermark,
            query: query.into(),
            cursor,
        })
    }

    fn stored_task(&self, id: usize) -> Option<&domain::Task> {
        self.stored.get(&id)
    }

    fn label(&self) -> &'static str {
        "linked project"
    }

    fn information(&self, screen: &Screen) -> String {
        let host = &self.host;
        if screen.view != View::Config {
            return format!(
                "{} · {} · {}\n\n{} is a project of the linked host {host}.\nThis computer reads and changes it only over the link: {host}'s own application\ncommands and state machine do the work, and its files and database stay there.",
                crate::PRODUCT,
                env!("CARGO_PKG_VERSION"),
                screen.instance.name(),
                screen.project_name,
            );
        }
        // Nine short lines: the smallest terminal shows no more.
        format!(
            "Local-only: change these on {host}\n• Configuration: run mode, models, profiles, categories, lenses,\n  checks, Git delivery, report limits, settings export and import\n• Tools and dependency setup\n• Storage and build caches\n• Project move and delete\nOver the link: Run, Pause, Stop, Restart, Approve, Reject, Answer,\nAdd, Insert, Edit, Move, Cancel and Mark done.\nRun mode there: {}",
            if screen.auto_advance {
                "Continue queue"
            } else {
                "Run one"
            }
        )
    }

    fn config_controls(&self) -> Vec<(&'static str, Action)> {
        // Listed so that their keys say why; none of them is offered as a button.
        vec![
            ("m Defaults", Action::ProjectModels),
            ("u User defaults", Action::UserDefaults),
            ("f Profiles", Action::Profiles),
            ("w Work categories", Action::Categories),
            ("L Review lenses", Action::Lenses),
            ("c Checks", Action::ProjectChecks),
            ("g Git delivery", Action::Delivery),
            ("k Storage", Action::Storage),
            ("b Build cache", Action::BuildCache),
            ("t Tools", Action::Tools),
            ("i Report limits", Action::ReportLimits),
            ("E Export settings", Action::ExportSettings),
            ("o Import settings", Action::ImportSettings),
            ("e Preferences", Action::Configure),
            ("Esc Back", Action::Back),
        ]
    }

    fn idle_output(&self) -> &'static str {
        "Ready. r starts an attempt on the host; this pane follows its log.\nDefaults and checks are configured on the host."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(offline: Option<Option<i64>>, read_only: bool) -> LinkState {
        LinkState {
            host: "devbox".into(),
            offline,
            read_only,
        }
    }

    /// A stored task pinned to the category and lenses given.
    fn stored(category: &str, lenses: &[&str]) -> domain::Task {
        let lenses: Vec<_> = lenses
            .iter()
            .map(|name| domain::lenses::Lens {
                name: (*name).into(),
                description: String::new(),
                instructions: String::new(),
                scope: domain::categories::Scope::BuiltIn,
            })
            .collect();
        domain::Task {
            id: 3,
            version: 2,
            lifecycle: "ready".into(),
            status: "ready".into(),
            order_key: "a".into(),
            origin_task_id: None,
            origin_attempt_id: None,
            follow_ups: Vec::new(),
            resolution_reason: None,
            resolved_by: None,
            lifecycle_events: Vec::new(),
            revisions: vec![domain::Revision {
                number: 1,
                lifecycle: "ready".into(),
                actor: None,
                category: None,
                lenses,
                definition: Definition {
                    title: "-Dashed".into(),
                    goal: "Goal".into(),
                    kind: domain::Kind::Ai,
                    category: Some(category.into()),
                    lenses: None,
                    criteria: vec!["One".into(), "Two".into()],
                    models: std::array::from_fn(|_| None),
                    attempt_limit: Some(3),
                    timeout_ms: None,
                    stage_timeout_ms: None,
                },
            }],
            attempts: Vec::new(),
            attempts_used: 0,
            counted_from: 0,
            closed_at_ms: None,
        }
    }

    /// The `edit` options of the form opened on `task`, after `change`.
    fn edited(task: &domain::Task, change: &[(&str, &str)]) -> Vec<String> {
        let choices = FormChoices {
            categories: vec!["code".into(), "art".into()],
            lenses: vec!["security".into()],
            lens_defaults: Vec::new(),
        };
        let mut screen = Screen::new(Instance::Development, "web");
        screen.queue = vec![crate::projection::project_task(task).unwrap()];
        let stored = BTreeMap::from([(3, task.clone())]);
        let mut form =
            project_view::task_form(&screen, Action::Edit, &stored, &choices, "on devbox");
        for (label, value) in change {
            form.fields
                .iter_mut()
                .find(|field| field.label == *label)
                .unwrap()
                .value = (*value).into();
        }
        edit_args(&form, task, &choices).unwrap()
    }

    #[test]
    fn an_edit_form_becomes_edit_options_that_keep_what_it_did_not_change() {
        let task = stored("art", &["security"]);
        let kept = [
            "--title=-Dashed",
            "--goal=Goal",
            "--approval=ready",
            "--lens=security",
        ];
        assert_eq!(edited(&task, &[]), kept);
        let changed = edited(&task, &[(forms::CATEGORY, "code")]);
        assert!(changed.contains(&"--category=code".into()), "{changed:?}");
        assert!(!changed.iter().any(|a| a.starts_with("--kind")));
        // The default category is only reachable by naming the kind again.
        let default = edited(&task, &[(forms::CATEGORY, forms::DEFAULT_CATEGORY)]);
        assert!(default.contains(&"--kind=ai".into()), "{default:?}");
        assert!(!default.iter().any(|a| a.starts_with("--category")));
        // So is the project's default review, and the chosen category is named with it.
        let lenses = edited(&task, &[("Review lens 1", lens_ui::PROJECT_DEFAULT)]);
        assert!(lenses.contains(&"--kind=ai".into()), "{lenses:?}");
        assert!(lenses.contains(&"--category=art".into()), "{lenses:?}");
        assert!(!lenses.iter().any(|a| a.starts_with("--lens")));
        let criteria = edited(&task, &[(forms::CHECK, "Three")]);
        assert!(criteria.contains(&"--criterion=Three".into()));
    }

    /// A workspace whose host is never reached: its link files are below `root`, where no
    /// hub runs.
    fn unlinked(root: &std::path::Path) -> RemoteWorkspace {
        let paths = Paths {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
            data: root.join("data"),
            runtime: root.join("runtime"),
            executable: root.join("ktask-dev"),
        };
        RemoteWorkspace::unread(Arc::new(paths), "devbox", 1)
    }

    /// A read that arrived and was not collected yet, listing one queued task.
    fn delivered(took: Duration) -> Receiver<Polled> {
        let view = ProjectView {
            control: true,
            supervisor_alive: false,
            session: None,
            continue_queue: true,
            launch_issue: None,
            choices: FormChoices::default(),
            queue: wire::Section::new(vec![stored("code", &[])], ""),
            history: wire::Section::new(Vec::new(), ""),
            events: wire::Section::new(Vec::new(), ""),
            readiness: None,
        };
        let (send, receive) = mpsc::channel();
        send.send(Polled {
            answer: Ok(view),
            contact: None,
            took,
        })
        .unwrap();
        receive
    }

    #[test]
    fn resuming_a_parked_project_drops_its_old_read_and_reads_at_once() {
        let root = tempfile::tempdir().unwrap();
        let mut workspace = unlinked(root.path());
        let mut screen = Screen::new(Instance::Development, "web");
        screen.remote = Some(link(Some(Some(1)), false));
        workspace.poll = Some(delivered(Duration::ZERO));
        workspace.next_poll = Instant::now() + Duration::from_secs(600);

        workspace.resumed();
        assert!(workspace.poll.is_none());
        assert!(workspace.next_poll <= Instant::now());
        workspace.tick(&mut screen);
        // What the old read said is not shown, and does not end the offline marking.
        assert!(screen.queue.is_empty());
        assert!(screen.remote.unwrap().offline.is_some());
        assert!(workspace.poll.is_some(), "a new read is under way");
    }

    #[test]
    fn the_next_read_waits_by_how_long_the_last_took_not_by_when_it_is_collected() {
        let root = tempfile::tempdir().unwrap();
        for (took, wait) in [
            (Duration::from_millis(10), POLL),
            (Duration::from_secs(30), Duration::from_secs(30)),
        ] {
            let mut workspace = unlinked(root.path());
            let mut screen = Screen::new(Instance::Development, "web");
            screen.remote = Some(link(None, false));
            workspace.poll = Some(delivered(took));
            // Collected well after it arrived.
            thread::sleep(Duration::from_millis(50));
            let before = Instant::now();
            workspace.tick(&mut screen);
            assert_eq!(screen.queue.len(), 1);
            assert!(workspace.next_poll >= before + wait);
            assert!(workspace.next_poll <= Instant::now() + wait);
        }
    }

    #[test]
    fn the_link_refuses_what_stays_on_the_host_and_everything_asked_while_offline() {
        let live = link(None, false);
        assert!(live.refusal(Action::Run).is_none());
        assert!(live.refusal(Action::Inspect).is_none());
        for action in [
            Action::Storage,
            Action::Tools,
            Action::Configure,
            Action::Fix,
        ] {
            assert!(live.refusal(action).unwrap().starts_with("Local-only"));
            assert!(!live.shows_disabled(action));
        }
        let away = link(Some(None), false);
        for action in [
            Action::Run,
            Action::Pause,
            Action::Add,
            Action::Inspect,
            Action::Stats,
        ] {
            let why = away.refusal(action).unwrap();
            assert!(why.contains("devbox is offline · last contact"), "{why}");
            assert!(away.shows_disabled(action));
        }
        // Reading the held screens stays possible.
        for action in [Action::Filter, Action::Search, Action::Events, Action::Help] {
            assert!(away.refusal(action).is_none());
        }
        let read_only = link(None, true);
        assert!(
            read_only
                .refusal(Action::Stop)
                .unwrap()
                .contains("read-only")
        );
        assert!(read_only.refusal(Action::Inspect).is_none());
    }
}
