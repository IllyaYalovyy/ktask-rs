//! The read-only provider catalogue screen.

use ktask_core::{ProviderCheck, ProviderView, provider_field_source, provider_fields};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

use crate::widgets::key_map;

const LIST_KEYS: [(&str, &str); 4] = [
    ("j, Down / k, Up", "select the next / previous provider"),
    ("Enter", "show the selected provider's definition"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map, or go back to the queue"),
];

const DEFINITION_KEYS: [(&str, &str); 4] = [
    ("c", "check the provider is ready to use"),
    ("j, Down / k, Up", "scroll the definition"),
    ("?", "show or hide this key map"),
    ("Esc", "close this key map, or go back to the list"),
];

/// The catalogue, first as a list and then as one selected definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvidersScreen {
    providers: Vec<ProviderView>,
    selected: usize,
    showing: bool,
    offset: usize,
    check: Option<ProviderCheck>,
    help: bool,
}

impl ProvidersScreen {
    /// Opens the provider list, with its first name selected.
    pub(crate) fn new(providers: Vec<ProviderView>) -> Self {
        Self {
            providers,
            selected: 0,
            showing: false,
            offset: 0,
            check: None,
            help: false,
        }
    }
    /// Applies a key: `?` shows or hides the key map, Enter shows the selected definition, Esc
    /// returns to the list or queue.
    pub(crate) fn key(mut self, key: KeyCode) -> (Self, bool, Option<String>) {
        if self.help {
            self.help = !matches!(key, KeyCode::Esc | KeyCode::Char('?'));
            return (self, false, None);
        }
        if key == KeyCode::Char('?') {
            self.help = true;
            return (self, false, None);
        }
        if self.showing {
            self.definition_key(key)
        } else {
            self.list_key(key)
        }
    }

    fn list_key(mut self, key: KeyCode) -> (Self, bool, Option<String>) {
        match key {
            KeyCode::Esc => return (self, true, None),
            KeyCode::Char('j') | KeyCode::Down => {
                self.selected = (self.selected + 1).min(self.providers.len().saturating_sub(1));
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
            }
            KeyCode::Enter => {
                self.showing = true;
                self.offset = 0;
                self.check = None;
            }
            _ => {}
        }
        (self, false, None)
    }

    fn definition_key(mut self, key: KeyCode) -> (Self, bool, Option<String>) {
        match key {
            KeyCode::Esc => {
                self.showing = false;
                self.offset = 0;
            }
            KeyCode::Char('c') => {
                let name = self
                    .providers
                    .get(self.selected)
                    .map(|provider| provider.name.clone());
                return (self, false, name);
            }
            KeyCode::Char('j') | KeyCode::Down => self.offset = self.offset.saturating_add(1),
            KeyCode::Char('k') | KeyCode::Up => self.offset = self.offset.saturating_sub(1),
            _ => {}
        }
        (self, false, None)
    }
    /// Keeps a completed check below the selected provider definition.
    #[must_use]
    pub(crate) fn checked(mut self, check: ProviderCheck) -> Self {
        self.check = Some(check);
        self.offset = 0;
        self
    }
    /// The footer appropriate to the list or definition view.
    pub(crate) fn footer_keys(&self) -> &'static str {
        if self.showing {
            " c check · j/k scroll · ? keys · Esc list "
        } else {
            " j/k select · Enter show · ? keys · Esc back "
        }
    }
    /// Draws the list or the full selected definition.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
        if self.help {
            let keys = if self.showing {
                &DEFINITION_KEYS
            } else {
                &LIST_KEYS
            };
            key_map(keys, area, buf);
            return;
        }
        Paragraph::new(self.lines().join("\n"))
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }
    fn lines(&self) -> Vec<String> {
        if !self.showing {
            let mut lines = vec!["Providers".to_owned(), String::new()];
            lines.extend(self.providers.iter().enumerate().map(|(index, provider)| {
                format!(
                    "{} {}",
                    if index == self.selected { ">" } else { " " },
                    provider.name
                )
            }));
            return lines;
        }
        let Some(provider) = self.providers.get(self.selected) else {
            return vec!["Providers".to_owned()];
        };
        let mut lines = vec![format!("Provider: {}", provider.name), String::new()];
        lines.extend(provider_fields(provider).into_iter().map(|(field, value)| {
            format!(
                "{field}: {value} ({})",
                provider_field_source(provider, field)
            )
        }));
        if let Some(check) = &self.check
            && check.provider == provider.name
        {
            lines.push(String::new());
            lines.push("Readiness".to_owned());
            lines.extend(crate::presentation::provider_check_lines(check));
        }
        let last = lines.len().saturating_sub(1);
        lines.into_iter().skip(self.offset.min(last)).collect()
    }
}
