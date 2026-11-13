use crate::presentation::Task;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Clone)]
pub enum DialogKind {
    Task {
        id: Option<usize>,
        insert: bool,
    },
    Move(usize),
    Stop(usize),
    Restart(usize),
    MarkDone(usize),
    CancelTask(usize),
    Supersede(usize),
    CopyToQueue(usize),
    BreakdownPending,
    BreakdownReview {
        proposal: crate::runner::agent::BreakdownProposal,
    },
    Models(usize),
    Reject(usize),
    Breakdown(usize),
    Answer(usize),
    Readiness,
    Import,
    ImportPreview(Vec<Task>),
    StoredImport(Vec<crate::application::Definition>),
    FileOverwrite {
        path: String,
        data: String,
        expected: Vec<u8>,
    },
    Export,
    Overwrite {
        path: String,
        data: String,
    },
    ProjectModels(Box<crate::application::configuration::Defaults>),
    UserDefaults(crate::application::user_defaults::UserDefaults),
    ApplyDefaults(Vec<crate::application::user_defaults::DefaultsRow>),
    Profiles(Vec<crate::application::profiles::Profile>),
    Profile {
        expected: Option<Box<crate::application::profiles::Profile>>,
    },
    Categories(Vec<crate::application::categories::Entry>),
    Category {
        expected: Option<Box<crate::application::categories::Entry>>,
    },
    Lenses(Vec<crate::application::lenses::Entry>),
    Lens {
        expected: Option<Box<crate::application::lenses::Entry>>,
    },
    LensDefaults(Vec<String>),
    ProjectChecks(Vec<crate::application::configuration::ProjectCheck>),
    ProjectCheck {
        expected: Vec<crate::application::configuration::ProjectCheck>,
        id: Option<i64>,
    },
    Preferences(crate::application::preferences::Preferences),
    Delivery(crate::application::configuration::DeliveryConfig),
    Storage(crate::application::retention::Preview),
    StorageConfirm(crate::application::retention::Preview),
    BuildCache(crate::build_cache::Policy),
    BuildCacheConfirm(BuildCachePlan),
    Config,
    ResetConfig,
    Help,
    Stats,
    Events,
    Inspect {
        id: usize,
        attempt: usize,
        section: usize,
        page: usize,
        chunk: usize,
        artifact: usize,
        step: usize,
        match_sequence: Option<i64>,
        search_active: bool,
        jump_to_match: bool,
        watermark: i64,
        search: crate::application::EvidenceCursor,
        excerpt: String,
    },
}

pub struct Field {
    pub label: String,
    pub value: String,
    pub choices: Vec<String>,
    pub cursor: usize,
}

impl Field {
    pub fn text(label: &str, value: &str) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            choices: Vec::new(),
            cursor: value.chars().count(),
        }
    }
    pub fn choice(label: &str, choices: &[&str], value: &str) -> Self {
        Self {
            choices: choices.iter().map(|s| (*s).into()).collect(),
            ..Self::text(label, value)
        }
    }
    pub fn cycle(&mut self) {
        if self.choices.is_empty() {
            return;
        }
        let pos = self
            .choices
            .iter()
            .position(|s| *s == self.value)
            .unwrap_or(0);
        self.value = self.choices[(pos + 1) % self.choices.len()].clone();
        self.cursor = self.value.chars().count();
    }
    fn key(&mut self, key: KeyEvent) {
        if !self.choices.is_empty() {
            if matches!(
                key.code,
                KeyCode::Char(' ') | KeyCode::Left | KeyCode::Right
            ) {
                self.cycle();
            }
            return;
        }
        let mut chars: Vec<_> = self.value.chars().collect();
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('a') {
                chars.clear();
                self.cursor = 0;
            }
        } else {
            match key.code {
                KeyCode::Char(ch) if !ch.is_control() && chars.len() < 8000 => {
                    chars.insert(self.cursor, ch);
                    self.cursor += 1;
                }
                KeyCode::Backspace if self.cursor > 0 => {
                    self.cursor -= 1;
                    chars.remove(self.cursor);
                }
                KeyCode::Delete if self.cursor < chars.len() => {
                    chars.remove(self.cursor);
                }
                KeyCode::Left => self.cursor = self.cursor.saturating_sub(1),
                KeyCode::Right => self.cursor = (self.cursor + 1).min(chars.len()),
                KeyCode::Home => self.cursor = 0,
                KeyCode::End => self.cursor = chars.len(),
                _ => {}
            }
        }
        self.value = chars.into_iter().collect();
    }
}

pub struct Dialog {
    pub expected: Option<crate::application::VersionedTask>,
    pub kind: DialogKind,
    pub title: String,
    pub body: String,
    pub fields: Vec<Field>,
    pub focus: usize,
    pub error: String,
    pub scroll: usize,
    pub max_scroll: usize,
    pub field_offset: usize,
    pub field_capacity: usize,
    /// At most this many fields show at once, even when more would fit.
    pub field_limit: Option<usize>,
}

impl Dialog {
    pub fn new(kind: DialogKind, title: &str, body: &str, fields: Vec<Field>) -> Self {
        Self {
            expected: None,
            kind,
            title: title.into(),
            body: body.into(),
            fields,
            focus: 0,
            error: String::new(),
            scroll: 0,
            max_scroll: 0,
            field_offset: 0,
            field_capacity: 1,
            field_limit: None,
        }
    }
    /// The trimmed value of the field labelled `label`; every form reads its own labels.
    pub fn get(&self, label: &str) -> &str {
        self.fields
            .iter()
            .find(|field| field.label == label)
            .unwrap_or_else(|| panic!("{} has no field {label:?}", self.title))
            .value
            .trim()
    }
    pub fn key(&mut self, key: KeyEvent) {
        if matches!(
            self.kind,
            DialogKind::Inspect {
                search_active: false,
                ..
            }
        ) {
            match key.code {
                KeyCode::Up | KeyCode::Char('k') => {
                    self.scroll = self.scroll.saturating_sub(1);
                    return;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.scroll = (self.scroll + 1).min(self.max_scroll);
                    return;
                }
                KeyCode::Home => {
                    self.scroll = 0;
                    return;
                }
                KeyCode::End => {
                    self.scroll = self.max_scroll;
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Tab | KeyCode::Down if !self.fields.is_empty() => {
                self.focus = (self.focus + 1) % self.fields.len()
            }
            KeyCode::BackTab | KeyCode::Up if !self.fields.is_empty() => {
                self.focus = (self.focus + self.fields.len() - 1) % self.fields.len()
            }
            KeyCode::Down | KeyCode::Char('j') if self.fields.is_empty() => {
                self.scroll = (self.scroll + 1).min(self.max_scroll)
            }
            KeyCode::Up | KeyCode::Char('k') if self.fields.is_empty() => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::PageDown => self.scroll = (self.scroll + 8).min(self.max_scroll),
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::Home if self.fields.is_empty() => self.scroll = 0,
            KeyCode::End if self.fields.is_empty() => self.scroll = self.max_scroll,
            _ => {
                if let Some(field) = self.fields.get_mut(self.focus) {
                    let before = field.value.clone();
                    field.key(key);
                    if field.value != before {
                        self.error.clear();
                        self.scroll = 0;
                        if let DialogKind::Inspect {
                            match_sequence,
                            search,
                            excerpt,
                            ..
                        } = &mut self.kind
                        {
                            *match_sequence = None;
                            *search = Default::default();
                            excerpt.clear();
                        }
                    }
                }
            }
        }
    }
    pub fn read_only(&self) -> bool {
        matches!(
            self.kind,
            DialogKind::Readiness
                | DialogKind::BreakdownPending
                | DialogKind::Help
                | DialogKind::Stats
                | DialogKind::Events
                | DialogKind::Inspect { .. }
        )
    }

    /// The index of the position field and the task it moves, if the form places a task.
    pub fn position_field(&self) -> Option<(usize, Option<usize>)> {
        let index = self
            .fields
            .iter()
            .position(|field| field.label == crate::forms::BEFORE)?;
        Some((
            index,
            match self.kind {
                DialogKind::Move(id) => Some(id),
                _ => None,
            },
        ))
    }

    pub fn paste(&mut self, text: &str) {
        if !text.is_empty() {
            self.error.clear();
            self.scroll = 0;
            if let DialogKind::Inspect {
                match_sequence,
                search,
                excerpt,
                ..
            } = &mut self.kind
            {
                *match_sequence = None;
                *search = Default::default();
                excerpt.clear();
            }
        }
        if let Some(field) = self
            .fields
            .get_mut(self.focus)
            .filter(|f| f.choices.is_empty())
        {
            for ch in text
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
            {
                field.key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
            }
        }
    }
}

/// What a confirmed Build cache dialog will do.
#[derive(Clone, Debug)]
pub enum BuildCachePlan {
    Clean,
    Adopt(std::path::PathBuf),
}
