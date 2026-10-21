use crate::{
    dialog::Dialog,
    instance::Instance,
    mock,
    presentation::{Kind, Status, Task},
};
use ratatui::{
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    layout::Rect,
    widgets::ListState,
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum View {
    Current,
    Queue,
    History,
    Config,
    About,
}
impl View {
    pub const ALL: [Self; 5] = [
        Self::Current,
        Self::Queue,
        Self::History,
        Self::Config,
        Self::About,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::Queue => "Queue",
            Self::History => "History",
            Self::Config => "Config",
            Self::About => "About",
        }
    }
}

#[derive(Clone, Copy)]
pub enum Action {
    View(View),
    Back,
    Quit,
    Help,
    Copy,
    Run,
    Recover,
    Readiness,
    Pause,
    Retry,
    Diagnose,
    Approve,
    Reject,
    Models,
    Resolve,
    Add,
    Insert,
    Edit,
    Delete,
    Undo,
    Move,
    Breakdown,
    Answer,
    Abandon,
    Output,
    Acknowledge,
    Stats,
    Events,
    Import,
    Export,
    Inspect,
    InspectSection(usize),
    Filter,
    Search,
    Clear,
    Follow,
    Submit,
    Cancel,
    Field(usize),
    Select(usize),
    PreviousAttempt,
    NextAttempt,
    EvidenceSection(i32),
    EvidencePage(i32),
    EvidenceChunk(i32),
    EvidenceArtifact(i32),
    EvidenceStep(i32),
    EvidenceOpen(usize),
    FindEvidence,
    RefreshEvidence,
    Configure,
    ProjectModels,
    ProjectChecks,
    Storage,
    Scroll(i32),
    Position(i32),
    Fields(i32),
}

#[derive(Clone, Default)]
pub struct Catalog {
    pub state: ListState,
    pub query: String,
    pub filter: usize,
    pub selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum EvidenceSection {
    #[default]
    Output,
    Results,
    Transcripts,
    Summary,
    Reviews,
    Failures,
}
impl EvidenceSection {
    pub fn from_index(index: usize) -> Self {
        match index {
            0 => Self::Output,
            1 => Self::Results,
            2 => Self::Transcripts,
            3 => Self::Summary,
            4 => Self::Reviews,
            _ => Self::Failures,
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Output => 0,
            Self::Results => 1,
            Self::Transcripts => 2,
            Self::Summary => 3,
            Self::Reviews => 4,
            Self::Failures => 5,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EvidenceAttemptKey {
    pub view: View,
    pub task: usize,
    pub attempt: i64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct EvidenceSourceKey {
    pub attempt: EvidenceAttemptKey,
    pub section: EvidenceSection,
    pub artifact: Option<i64>,
    pub step: Option<usize>,
}
#[derive(Clone, Default)]
pub struct EvidencePosition {
    pub page: usize,
    pub chunk: usize,
    pub scroll: usize,
    pub match_sequence: Option<i64>,
    pub query: String,
    pub search: crate::application::EvidenceCursor,
    pub excerpt: String,
}
#[derive(Clone, Default)]
pub struct EvidenceAttemptState {
    pub section: EvidenceSection,
    pub artifact: usize,
    pub step: usize,
    pub review_step: usize,
    pub failure_step: usize,
    pub watermark: i64,
}
impl EvidenceAttemptState {
    pub fn select_section(&mut self, section: EvidenceSection) {
        self.section = section;
        self.step = match section {
            EvidenceSection::Reviews => self.review_step,
            EvidenceSection::Failures => self.failure_step,
            _ => self.step,
        };
    }
    pub fn select_step(&mut self, step: usize) {
        self.step = step;
        match self.section {
            EvidenceSection::Reviews => self.review_step = step,
            EvidenceSection::Failures => self.failure_step = step,
            _ => {}
        }
    }
}

pub struct App {
    pub service: Option<crate::application::Service>,
    pub supervisor_alive: bool,
    pub(crate) proposal_job: Option<crate::persistent::ProposalJob>,
    pub stored: BTreeMap<usize, crate::application::Task>,
    pub durable_undo: Option<crate::application::VersionedTask>,
    pub project_name: String,
    pub instance: Instance,
    pub view: View,
    pub previous: View,
    pub queue: Vec<Task>,
    pub history: Vec<Task>,
    pub queue_catalog: Catalog,
    pub history_catalog: Catalog,
    pub dialog: Option<Dialog>,
    pub search_before: Option<Catalog>,
    pub notice: String,
    pub events: Vec<String>,
    /// Events about the current task only (SQLite); mirrors `events` in demo mode.
    pub task_events: Vec<String>,
    pub files: BTreeMap<String, String>,
    pub next_id: usize,
    pub undo: Option<(usize, Task)>,
    pub mouse_enabled: bool,
    pub review_required: bool,
    pub cleanup_fail_once: bool,
    pub clean_retry: bool,
    pub auto_advance: bool,
    pub(crate) execution_session: Option<crate::application::session::ExecutionSession>,
    pub launch_issue: Option<String>,
    pub readiness: Option<crate::readiness::Readiness>,
    pub last_workspace: Option<usize>,
    pub tick_ms: u64,
    pub hits: Vec<(Rect, Action)>,
    pub list_rect: Rect,
    pub log_rect: Rect,
    pub selection: crate::selection::Selection,
    pub output_cache: crate::ui::OutputCache,
    pub evidence_positions: BTreeMap<EvidenceSourceKey, EvidencePosition>,
    pub evidence_attempts: BTreeMap<EvidenceAttemptKey, EvidenceAttemptState>,
    pub inspected_attempts: BTreeMap<(View, usize), i64>,
    pub copy_pending: Option<String>,
    copy_pressed: bool,
    pub page_size: usize,
    pub log_offset: usize,
    pub log_rows: usize,
    pub log_height: usize,
    pub follow: bool,
    log_follow_before_mouse_down: Option<bool>,
    /// Show the raw log instead of the attempt report when a report exists.
    pub show_log: bool,
    pub quit: bool,
    pub last_tick: Instant,
}

impl App {
    pub fn new(instance: Instance) -> Self {
        Self::build(instance, true)
    }
    pub(crate) fn empty(instance: Instance) -> Self {
        Self::build(instance, false)
    }
    fn build(instance: Instance, demo: bool) -> Self {
        let (queue, history) = if demo {
            mock::dataset("Large")
        } else {
            (Vec::new(), Vec::new())
        };
        Self {
            service: None,
            supervisor_alive: false,
            proposal_job: None,
            stored: BTreeMap::new(),
            durable_undo: None,
            project_name: "demo workspace".into(),
            instance,
            view: View::Current,
            previous: View::Current,
            queue,
            history,
            queue_catalog: Catalog {
                selected: demo.then_some(1),
                ..Default::default()
            },
            history_catalog: Catalog {
                selected: demo.then_some(121),
                ..Default::default()
            },
            dialog: None,
            search_before: None,
            notice: "Mock workspace · changes last only for this session".into(),
            events: if demo {
                vec![
                    "Check failed · #1".into(),
                    "Execution stopped".into(),
                    "Ready for diagnosis".into(),
                ]
            } else {
                Vec::new()
            },
            task_events: if demo {
                vec![
                    "Check failed · #1".into(),
                    "Execution stopped".into(),
                    "Ready for diagnosis".into(),
                ]
            } else {
                Vec::new()
            },
            files: if demo { mock::files() } else { BTreeMap::new() },
            next_id: 241,
            undo: None,
            mouse_enabled: true,
            review_required: false,
            cleanup_fail_once: false,
            clean_retry: false,
            auto_advance: true,
            execution_session: None,
            launch_issue: None,
            readiness: None,
            last_workspace: Some(1),
            tick_ms: 400,
            hits: Vec::new(),
            list_rect: Rect::default(),
            log_rect: Rect::default(),
            selection: crate::selection::Selection::default(),
            output_cache: crate::ui::OutputCache::default(),
            evidence_positions: BTreeMap::new(),
            evidence_attempts: BTreeMap::new(),
            inspected_attempts: BTreeMap::new(),
            copy_pending: None,
            copy_pressed: false,
            page_size: 1,
            log_offset: 0,
            log_rows: 0,
            log_height: 1,
            follow: true,
            log_follow_before_mouse_down: None,
            show_log: false,
            quit: false,
            last_tick: Instant::now(),
        }
    }
    pub fn current(&self) -> Option<&Task> {
        self.queue.first()
    }
    pub fn catalog(&self) -> &Catalog {
        if self.view == View::History {
            &self.history_catalog
        } else {
            &self.queue_catalog
        }
    }
    pub fn catalog_mut(&mut self) -> &mut Catalog {
        if self.view == View::History {
            &mut self.history_catalog
        } else {
            &mut self.queue_catalog
        }
    }
    pub fn source(&self) -> &[Task] {
        if self.view == View::History {
            &self.history
        } else {
            &self.queue
        }
    }
    pub fn visible(&self) -> Vec<usize> {
        let catalog = self.catalog();
        let query = catalog.query.to_lowercase();
        self.source()
            .iter()
            .filter(|task| {
                (catalog.filter == 0
                    || (catalog.filter == 1 && task.definition().kind == Kind::Ai)
                    || (catalog.filter == 2 && task.definition().kind == Kind::Human))
                    && format!(
                        "#{} {} {}",
                        task.id,
                        task.definition().title,
                        task.status().label()
                    )
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|task| task.id)
            .collect()
    }
    pub fn task(&self, id: usize) -> Option<&Task> {
        self.queue.iter().chain(&self.history).find(|t| t.id == id)
    }
    pub fn selected_task(&self) -> Option<&Task> {
        if self.view == View::Current {
            self.current()
        } else {
            self.catalog().selected.and_then(|id| self.task(id))
        }
    }
    pub fn reconcile(&mut self) {
        let ids = self.visible();
        if !self.catalog().selected.is_some_and(|id| ids.contains(&id)) {
            self.catalog_mut().selected = ids.first().copied();
        }
    }
    pub fn switch(&mut self, view: View) {
        if self.view != view {
            self.selection.clear();
            self.previous = self.view;
            self.view = view;
        }
        self.search_before = None;
        self.reconcile();
    }
    pub fn message(&mut self, text: impl Into<String>) {
        self.notice = text.into();
    }
    pub(crate) fn copy_selection(&mut self) {
        if let Some(text) = self.selection.text() {
            self.message(format!(
                "Sent {} characters via OSC 52 · F7: native selection fallback",
                text.chars().count()
            ));
            self.copy_pending = Some(text);
        } else {
            self.message("Drag text, or F2 then Shift-arrows; F6 copies · F7: native selection");
        }
    }
    pub fn record(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.notice.clone_from(&text);
        self.events.insert(0, text.clone());
        self.events.truncate(30);
        self.task_events.insert(0, text);
        self.task_events.truncate(30);
    }
    pub fn key(&mut self, key: KeyEvent, fits: bool) {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if !fits {
            if key.code == KeyCode::Char('q') {
                self.quit = true;
            }
            return;
        }
        if key.code == KeyCode::F(7) {
            self.mouse_enabled = !self.mouse_enabled;
            self.message(if self.mouse_enabled {
                "Mouse capture on · drag selects text"
            } else {
                "Mouse capture off · use the terminal's native text selection"
            });
            return;
        }
        if key.code == KeyCode::F(6) {
            self.copy_selection();
            return;
        }
        if key.code == KeyCode::Esc && self.selection.active() {
            self.selection.clear();
            return;
        }
        if self.selection.keyboard_key(key) {
            if self.dialog.is_none()
                && self.view == View::Current
                && self.selection.keyboard_in(self.log_rect)
            {
                self.follow = false;
            }
            if self.selection.copy_requested() {
                self.copy_selection();
            }
            return;
        }
        if self.dialog.is_some() {
            self.dialog_key(key);
            return;
        }
        if self.search_before.is_some() {
            self.search_key(key);
            return;
        }
        let action = match key.code {
            KeyCode::Char('q') => Some(Action::Quit),
            KeyCode::Char('?') => Some(Action::Help),
            KeyCode::Char(ch @ '1'..='5') => {
                Some(Action::View(View::ALL[(ch as u8 - b'1') as usize]))
            }
            KeyCode::Tab | KeyCode::BackTab => {
                let n = View::ALL.iter().position(|v| *v == self.view).unwrap_or(0);
                Some(Action::View(
                    View::ALL[(n + if key.code == KeyCode::Tab { 1 } else { 4 }) % 5],
                ))
            }
            KeyCode::Esc if matches!(self.view, View::About | View::Config) => Some(Action::Back),
            KeyCode::Esc => Some(Action::Clear),
            KeyCode::Char('s') => Some(Action::Stats),
            KeyCode::Char('l') => Some(Action::Events),
            KeyCode::Char('a') => Some(Action::Add),
            code => self.view_key(code),
        };
        if let Some(action) = action {
            self.act(action);
        }
    }
    fn view_key(&mut self, code: KeyCode) -> Option<Action> {
        match self.view {
            View::Current => match code {
                KeyCode::Char('u') => Some(Action::Readiness),
                KeyCode::Char('r') => Some(Action::Run),
                KeyCode::Char('m') => Some(Action::Configure),
                KeyCode::Char('R') => Some(Action::Recover),
                KeyCode::Char('p') => Some(Action::Pause),
                KeyCode::Char('t') => Some(Action::Retry),
                KeyCode::Char('d') => Some(Action::Diagnose),
                KeyCode::Char('v') => Some(Action::Approve),
                KeyCode::Char('n') => Some(Action::Reject),
                KeyCode::Char('c') => Some(Action::Models),
                KeyCode::Char('z') => Some(Action::Resolve),
                KeyCode::Char('y') => Some(Action::Acknowledge),
                KeyCode::Char('w') => Some(Action::Answer),
                KeyCode::Char('x') => Some(Action::Abandon),
                KeyCode::Char('o') => Some(Action::Output),
                KeyCode::Char('e') => Some(Action::Edit),
                KeyCode::Char('b') => Some(Action::Breakdown),
                KeyCode::Enter => Some(Action::Inspect),
                KeyCode::End | KeyCode::Char('G') => Some(Action::Follow),
                _ => {
                    self.navigate(code, true);
                    None
                }
            },
            View::History if code == KeyCode::Char('V') => Some(Action::InspectSection(4)),
            View::History if code == KeyCode::Char('F') => Some(Action::InspectSection(5)),
            View::Queue | View::History => match code {
                KeyCode::Char('r') if self.view == View::Queue => Some(Action::Run),
                KeyCode::Char('/') => Some(Action::Search),
                KeyCode::Char('f') => Some(Action::Filter),
                KeyCode::Char('e') => Some(Action::Edit),
                KeyCode::Char('d') => Some(Action::Delete),
                KeyCode::Char('u') => Some(Action::Undo),
                KeyCode::Char('m') => Some(Action::Move),
                KeyCode::Char('i') => Some(Action::Insert),
                KeyCode::Char('b') => Some(Action::Breakdown),
                KeyCode::Char('o') => Some(Action::Import),
                KeyCode::Char('v') => Some(Action::Approve),
                KeyCode::Char('n') => Some(Action::Reject),
                KeyCode::Char('c') => Some(Action::Models),
                KeyCode::Char('x') => Some(Action::Export),
                KeyCode::Enter => Some(Action::Inspect),
                _ => {
                    self.navigate(code, false);
                    None
                }
            },
            View::Config if self.service.is_some() && code == KeyCode::Char('m') => {
                Some(Action::ProjectModels)
            }
            View::Config if self.service.is_some() && code == KeyCode::Char('c') => {
                Some(Action::ProjectChecks)
            }
            View::Config if self.service.is_some() && code == KeyCode::Char('k') => {
                Some(Action::Storage)
            }
            View::Config if matches!(code, KeyCode::Enter | KeyCode::Char('e')) => {
                Some(Action::Configure)
            }
            _ => None,
        }
    }
    fn dialog_key(&mut self, key: KeyEvent) {
        if self
            .dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.kind, crate::dialog::DialogKind::Readiness))
        {
            let action = match key.code {
                KeyCode::Char('c') => Some(Action::ProjectChecks),
                KeyCode::Char('m') => Some(Action::Models),
                KeyCode::Char('v') => Some(Action::Approve),
                KeyCode::Char('w') => Some(Action::Answer),
                KeyCode::Char('y') => Some(Action::Acknowledge),
                KeyCode::Char('p') => Some(Action::Pause),
                KeyCode::Char('R') => Some(Action::Recover),
                KeyCode::Char('r') => Some(Action::Run),
                KeyCode::Char('t') => Some(Action::Retry),
                KeyCode::Char('u') => Some(Action::Readiness),
                _ => None,
            };
            if let Some(action) = action {
                self.act(action);
                return;
            }
        }
        let readonly = self.dialog.as_ref().is_some_and(Dialog::read_only);
        if self.inspect_search_key(key) {
            return;
        }
        if self.inspect_key(key) {
            return;
        }
        if key.code == KeyCode::Esc || (readonly && matches!(key.code, KeyCode::Char('q' | '?'))) {
            self.act(Action::Cancel);
        } else if key.code == KeyCode::Enter
            || (key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.act(Action::Submit);
        } else if key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Up | KeyCode::Down)
        {
            self.act(Action::Position(if key.code == KeyCode::Up {
                -1
            } else {
                1
            }));
        } else if readonly && key.code == KeyCode::Char('[') {
            self.act(Action::PreviousAttempt);
        } else if readonly && key.code == KeyCode::Char(']') {
            self.act(Action::NextAttempt);
        } else if let Some(dialog) = &mut self.dialog {
            dialog.key(key);
        }
    }
    fn inspect_search_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(crate::dialog::DialogKind::Inspect {
                search_active: true,
                ..
            })
        ) {
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                if let Some(crate::dialog::Dialog {
                    kind: crate::dialog::DialogKind::Inspect { search_active, .. },
                    ..
                }) = &mut self.dialog
                {
                    *search_active = false;
                }
            }
            KeyCode::Enter => self.act(Action::FindEvidence),
            _ => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.key(key);
                }
            }
        }
        true
    }
    fn inspect_key(&mut self, key: KeyEvent) -> bool {
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(crate::dialog::DialogKind::Inspect { .. })
        ) {
            return false;
        }
        let action = match key.code {
            KeyCode::Enter => Some(Action::FindEvidence),
            KeyCode::Char('[') => Some(Action::PreviousAttempt),
            KeyCode::Char(']') => Some(Action::NextAttempt),
            KeyCode::Left => Some(Action::EvidenceSection(-1)),
            KeyCode::Right => Some(Action::EvidenceSection(1)),
            KeyCode::Char('>') => Some(Action::EvidencePage(1)),
            KeyCode::Char('<') => Some(Action::EvidencePage(-1)),
            KeyCode::Char('}') => Some(Action::EvidenceChunk(1)),
            KeyCode::Char('{') => Some(Action::EvidenceChunk(-1)),
            KeyCode::Char('.') => Some(Action::EvidenceArtifact(1)),
            KeyCode::Char(',') => Some(Action::EvidenceArtifact(-1)),
            KeyCode::Char('n') => Some(Action::EvidenceStep(1)),
            KeyCode::Char('p') => Some(Action::EvidenceStep(-1)),
            KeyCode::Char('s') => Some(Action::EvidenceOpen(3)),
            KeyCode::Char('v') => Some(Action::EvidenceOpen(4)),
            KeyCode::Char('f') => Some(Action::EvidenceOpen(5)),
            KeyCode::Char('g') => Some(Action::EvidenceOpen(1)),
            KeyCode::Char('o') => Some(Action::EvidenceOpen(0)),
            KeyCode::Char('t') => Some(Action::EvidenceOpen(2)),
            KeyCode::Char('r') => Some(Action::RefreshEvidence),
            KeyCode::Char('/') => {
                if let Some(crate::dialog::Dialog {
                    kind: crate::dialog::DialogKind::Inspect { search_active, .. },
                    ..
                }) = &mut self.dialog
                {
                    *search_active = true;
                }
                return true;
            }
            _ => None,
        };
        if let Some(action) = action {
            self.act(action);
            true
        } else {
            false
        }
    }
    fn search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                if let Some(catalog) = self.search_before.take() {
                    *self.catalog_mut() = catalog;
                }
            }
            KeyCode::Enter => {
                self.search_before = None;
            }
            KeyCode::Backspace => {
                self.catalog_mut().query.pop();
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.catalog_mut().query.clear()
            }
            KeyCode::Char(ch) if !ch.is_control() => self.catalog_mut().query.push(ch),
            _ => {}
        }
        self.reconcile();
    }

    pub fn paste(&mut self, text: &str, fits: bool) {
        if !fits {
            return;
        }
        if let Some(dialog) = &mut self.dialog {
            dialog.paste(text);
        } else if self.search_before.is_some() {
            self.catalog_mut().query.extend(
                text.chars()
                    .map(|ch| if ch.is_control() { ' ' } else { ch }),
            );
            self.reconcile();
        }
    }
    fn navigate(&mut self, code: KeyCode, logs: bool) {
        let ids = self.visible();
        let pos = if logs {
            self.log_offset
        } else {
            ids.iter()
                .position(|id| Some(*id) == self.catalog().selected)
                .unwrap_or(0)
        };
        let page = if logs {
            self.log_height
        } else {
            self.page_size
        };
        let last = if logs {
            self.log_rows.saturating_sub(self.log_height)
        } else {
            ids.len().saturating_sub(1)
        };
        let next = match code {
            KeyCode::Up | KeyCode::Char('k') => pos.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (pos + 1).min(last),
            KeyCode::PageUp => pos.saturating_sub(page),
            KeyCode::PageDown => (pos + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return,
        };
        if logs {
            self.log_offset = next;
            self.follow = false;
        } else {
            self.catalog_mut().selected = ids.get(next).copied();
        }
    }
    pub fn mouse(&mut self, event: MouseEvent, fits: bool) {
        if !self.mouse_enabled || !fits {
            return;
        }
        let point = (event.column, event.row).into();
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.copy_pressed = self
                    .hits
                    .iter()
                    .any(|(rect, action)| rect.contains(point) && matches!(action, Action::Copy));
                if self.copy_pressed {
                    return;
                }
                self.selection.mouse_down(event.column, event.row);
                self.log_follow_before_mouse_down = (self.dialog.is_none()
                    && self.view == View::Current
                    && self.log_rect.contains(point))
                .then_some(self.follow);
                if self.log_follow_before_mouse_down.is_some() {
                    self.follow = false;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.selection.mouse_drag(event.column, event.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if std::mem::take(&mut self.copy_pressed) {
                    if self.hits.iter().any(|(rect, action)| {
                        rect.contains(point) && matches!(action, Action::Copy)
                    }) {
                        self.act(Action::Copy);
                    }
                    return;
                }
                let dragged = self.selection.mouse_up(event.column, event.row);
                if let Some(previous) = self.log_follow_before_mouse_down.take()
                    && !dragged
                {
                    self.follow = previous;
                }
                if dragged {
                    return;
                }
                if let Some((_, action)) = self
                    .hits
                    .iter()
                    .rev()
                    .find(|(rect, _)| rect.contains(point))
                {
                    self.act(*action);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.selection.clear();
                let delta = if event.kind == MouseEventKind::ScrollUp {
                    -3
                } else {
                    3
                };
                if self.dialog.is_some() {
                    self.act(Action::Scroll(delta));
                } else if self.log_rect.contains(point) || self.list_rect.contains(point) {
                    for _ in 0..3 {
                        self.navigate(
                            if delta < 0 {
                                KeyCode::Up
                            } else {
                                KeyCode::Down
                            },
                            self.view == View::Current,
                        );
                    }
                }
            }
            _ => {}
        }
    }
    fn refresh_persistent(&mut self) -> bool {
        match self
            .service
            .as_ref()
            .expect("SQLite mode")
            .externally_changed()
        {
            Ok(true) => {
                if let Err(error) = self.reload() {
                    self.message(error);
                }
            }
            Ok(false) => {
                if self.current().is_some_and(|t| {
                    matches!(
                        t.status(),
                        Status::Running
                            | Status::Paused
                            | Status::Waiting
                            | Status::Question
                            | Status::Abandoned
                    )
                }) {
                    if let Err(error) = self.reload() {
                        self.message(error);
                    }
                    return true;
                }
                return false;
            }
            Err(error) => self.message(error.to_string()),
        }
        true
    }
    pub fn tick(&mut self) -> bool {
        if self.poll_remediation() {
            return true;
        }
        if self.service.is_some() {
            if self.dialog.is_some() || self.last_tick.elapsed() < Duration::from_millis(500) {
                return false;
            }
            let changed = self.refresh_persistent();
            // A slow snapshot must not make every queued input trigger another
            // snapshot. Leave an input-processing interval after refresh finishes.
            self.last_tick = Instant::now();
            return changed;
        }
        if self.last_tick.elapsed() < Duration::from_millis(self.tick_ms) {
            return false;
        }
        self.last_tick = Instant::now();
        if let Some(task) = self.queue.first_mut()
            && task.status() == Status::Waiting
        {
            let attempt = task.attempts.last_mut().expect("waiting attempt");
            if attempt.resume_at.is_some_and(|at| Instant::now() >= at) {
                // A task that regained current after another workspace must pass handoff checks.
                attempt.state = Status::Paused;
                attempt.resume_at = None;
                self.start_current();
                if self
                    .current()
                    .is_some_and(|t| t.status() == Status::Running)
                {
                    self.record("Rate-limit wait ended · same attempt/session · no budget charged");
                }
            }
            return true;
        }
        let Some(task) = self
            .queue
            .first_mut()
            .filter(|t| t.status() == Status::Running)
        else {
            return false;
        };
        let attempt = task.attempts.last_mut().expect("running task has attempt");
        attempt.step += 1;
        attempt.elapsed_ms += self.tick_ms;
        attempt.cost_cents += 1;
        if attempt.revision.settings.timeout_ms > 0
            && attempt.elapsed_ms >= attempt.revision.settings.timeout_ms
        {
            attempt.state = Status::Failed;
            attempt.outcome = "Failed: active time limit reached".into();
            attempt.logs.push(attempt.outcome.clone());
            self.record("Active time limit reached · increase limit in Models or resolve current");
            return true;
        }
        if task.needs_rate_limit && attempt.step == 2 {
            attempt.state = Status::Waiting;
            attempt.resume_at = Some(Instant::now() + Duration::from_secs(3));
            attempt.outcome = "Waiting: rate limit (3 seconds)".into();
            attempt.logs.push(
                "RATE LIMIT: wait 3 seconds; retain attempt/session and active-time budget".into(),
            );
            self.record("Provider rate limit · scheduled resume in 3 seconds (mock)");
            return true;
        }
        if task.needs_question && task.answer.is_empty() && attempt.step == 2 {
            attempt.state = Status::Question;
            attempt.outcome = "Waiting for answer".into();
            attempt.question = Some("Which export format should be used?".into());
            attempt
                .logs
                .push("QUESTION: Which export format should be used?".into());
            self.record("Current task needs your answer");
            return true;
        }
        attempt.logs.push(
            match attempt.step {
                1 => "Executor: inspected the task",
                2 => "Executor: implementing",
                3 => "Checks: running",
                4 => "Review: checking the result",
                5 => "Validation: checking the goal",
                _ => "Checks: finishing",
            }
            .into(),
        );
        if attempt.step >= 6 {
            if task.id == 1 && !attempt.revision.repaired {
                attempt.state = Status::Failed;
                attempt.outcome = "Failed: boundary error".into();
                attempt
                    .logs
                    .push("FAIL: final row omitted. Request remediation.".into());
                self.record("Check failed · later tasks remain pending");
            } else {
                self.complete_current("Completed; checks and review passed");
            }
        }
        true
    }
    pub fn complete_current(&mut self, result: &str) {
        let mut task = self.queue.remove(0);
        task.resolution = Some(Status::Done);
        if task.attempts.is_empty() {
            task.attempts.push(crate::presentation::Attempt::start(
                task.definition().clone(),
                task.id,
            ));
        }
        if let Some(attempt) = task.attempts.last_mut() {
            attempt.state = Status::Done;
            attempt.outcome = result.into();
            attempt.logs.push(result.into());
        }
        let repaired = task.repairs;
        if let Some(target) = repaired.and_then(|id| self.queue.iter_mut().find(|t| t.id == id)) {
            let mut revision = target.definition().clone();
            revision.repaired = true;
            revision.actor = "AI: remediator".into();
            target.revise(revision);
        }
        self.record(format!(
            "#{} completed · {}",
            task.id,
            if self.queue.is_empty() {
                "queue complete"
            } else {
                "next task ready"
            }
        ));
        self.history_catalog.selected = Some(task.id);
        self.history.insert(0, task);
        self.follow = true;
        self.show_log = false;
        self.log_offset = 0;
        self.reconcile();
        if (self.auto_advance || repaired.is_some())
            && self.current().is_some_and(|t| {
                t.definition().kind == Kind::Ai
                    && (t.status() == Status::Pending
                        || (repaired.is_some() && t.status() == Status::Failed))
            })
        {
            self.start_current();
        }
    }
}
