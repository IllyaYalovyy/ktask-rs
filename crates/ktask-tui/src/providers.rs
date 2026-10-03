//! The read-only provider catalogue screen.

use ktask_core::{ProviderCheck, ProviderView, provider_field_source, provider_fields};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::widgets::{Paragraph, Widget, Wrap};

/// The catalogue, first as a list and then as one selected definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProvidersScreen {
    providers: Vec<ProviderView>,
    selected: usize,
    showing: bool,
    offset: usize,
    check: Option<ProviderCheck>,
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
        }
    }
    /// Applies a key: Enter shows the selected definition, Esc returns to the list or queue.
    pub(crate) fn key(mut self, key: KeyCode) -> (Self, bool, Option<String>) {
        match (self.showing, key) {
            (_, KeyCode::Esc) if self.showing => {
                self.showing = false;
                self.offset = 0;
                (self, false, None)
            }
            (_, KeyCode::Esc) => (self, true, None),
            (false, KeyCode::Char('j') | KeyCode::Down) => {
                self.selected = (self.selected + 1).min(self.providers.len().saturating_sub(1));
                (self, false, None)
            }
            (false, KeyCode::Char('k') | KeyCode::Up) => {
                self.selected = self.selected.saturating_sub(1);
                (self, false, None)
            }
            (false, KeyCode::Enter) => {
                self.showing = true;
                self.offset = 0;
                self.check = None;
                (self, false, None)
            }
            (true, KeyCode::Char('c')) => {
                let name = self
                    .providers
                    .get(self.selected)
                    .map(|provider| provider.name.clone());
                (self, false, name)
            }
            (true, KeyCode::Char('j') | KeyCode::Down) => {
                self.offset = self.offset.saturating_add(1);
                (self, false, None)
            }
            (true, KeyCode::Char('k') | KeyCode::Up) => {
                self.offset = self.offset.saturating_sub(1);
                (self, false, None)
            }
            _ => (self, false, None),
        }
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
            " c check · j/k scroll · Esc list "
        } else {
            " j/k select · Enter show · Esc back "
        }
    }
    /// Draws the list or the full selected definition.
    pub(crate) fn draw(&self, area: Rect, buf: &mut Buffer) {
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
