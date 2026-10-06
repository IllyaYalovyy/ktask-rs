//! The registered-projects picker: the same list `ktask-rs project list` prints, the ability
//! to switch to one, and a question to confirm forgetting one. Covers the queue while it is
//! open.

use ktask_core::Project;
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

use crate::widgets::{elide, key_map};

/// What the project picker's frame says at the bottom.
const PROJECTS_KEYS: &str = " j, k select · Enter switch · d forget · Esc cancel ";

/// What the project picker's frame says at the bottom while it asks to confirm forgetting a
/// project.
const FORGET_KEYS: &str = " y forget · n, Esc keep it ";

/// The keys shown by `?` while the picker is open, not asking to forget a project.
const PICKER_KEYS: [(&str, &str); 5] = [
    ("j, k", "select the next or previous project"),
    ("Enter", "switch to the selected project"),
    ("d", "forget the selected project, after asking"),
    ("?", "show or hide this key map"),
    ("Esc", "cancel"),
];

/// The keys shown by `?` while the forget question is asking.
const FORGET_QUESTION_KEYS: [(&str, &str); 2] = [("y", "forget it"), ("n, Esc", "keep it")];

/// What a key on the project picker asks the rest of the application to do, when it is not
/// something the picker answers entirely by itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Request {
    /// Close the picker.
    Close,
    /// Switch to the project called this.
    Switch(String),
    /// Forget the project called this, confirmed already.
    Forget(String),
}

/// The project picker's own state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ProjectsScreen {
    projects: Vec<Project>,
    selection: usize,
    problem: Option<String>,
    /// The name of the project a forget question is asking to confirm, while one is.
    forgetting: Option<String>,
    help: bool,
}

impl ProjectsScreen {
    /// The picker open on `projects`, the selection on the one named `current`, or the first
    /// when none matches.
    pub(crate) fn open(projects: Vec<Project>, current: Option<&str>) -> Self {
        let selection = projects
            .iter()
            .position(|project| Some(project.name.as_str()) == current)
            .unwrap_or(0);
        Self {
            projects,
            selection,
            problem: None,
            forgetting: None,
            help: false,
        }
    }

    /// Why the picker's last submission changed nothing, when it did not.
    #[cfg(test)]
    pub(crate) fn problem(&self) -> Option<&str> {
        self.problem.as_deref()
    }

    /// The name of the selected project, when there is one.
    fn selected_name(&self) -> Option<String> {
        self.projects
            .get(self.selection)
            .map(|project| project.name.clone())
    }

    /// The picker once its submission did not switch the project, for this reason.
    pub(crate) fn switch_failed(self, message: String) -> Self {
        Self {
            problem: Some(message),
            ..self
        }
    }

    /// The picker once the forget question is carried out: the fresh registered projects
    /// replace the ones it showed, the selection stays inside them, and the question closes.
    pub(crate) fn forgotten(self, projects: Vec<Project>) -> Self {
        let last = projects.len().saturating_sub(1);
        Self {
            selection: self.selection.min(last),
            projects,
            forgetting: None,
            problem: None,
            ..self
        }
    }

    /// The picker once forgetting the project the question named did not happen, for this
    /// reason.
    pub(crate) fn forget_failed(self, message: String) -> Self {
        Self {
            forgetting: None,
            problem: Some(message),
            ..self
        }
    }

    /// A key on the project picker: routed to the forget question while it is asking, to the
    /// key map while it is open, or to the picker's own keys otherwise.
    pub(crate) fn key(self, key: KeyCode) -> (Self, Option<Request>) {
        if self.forgetting.is_some() {
            return self.forget_confirm_key(key);
        }
        if self.help {
            return (self.help_key(key), None);
        }
        self.plain_key(key)
    }

    /// A key while the picker's own key map is open: only `?` or Esc, to close it, answer.
    fn help_key(self, key: KeyCode) -> Self {
        match key {
            KeyCode::Esc | KeyCode::Char('?') => Self {
                help: false,
                ..self
            },
            _ => self,
        }
    }

    /// A key while asking to confirm forgetting a project: while its own key map is open,
    /// only `?` or Esc, to close it back onto the question, answer.
    fn forget_confirm_key(self, key: KeyCode) -> (Self, Option<Request>) {
        if self.help {
            return (self.help_key(key), None);
        }
        match key {
            KeyCode::Char('y') => {
                let name = self.forgetting.clone();
                (
                    Self {
                        forgetting: None,
                        ..self
                    },
                    name.map(Request::Forget),
                )
            }
            KeyCode::Char('n') | KeyCode::Esc => (
                Self {
                    forgetting: None,
                    ..self
                },
                None,
            ),
            KeyCode::Char('?') => (Self { help: true, ..self }, None),
            _ => (self, None),
        }
    }

    /// A key on the plain picker: no forget question or key map in the way.
    fn plain_key(self, key: KeyCode) -> (Self, Option<Request>) {
        match key {
            KeyCode::Esc => (self, Some(Request::Close)),
            KeyCode::Char('j') | KeyCode::Down => (self.move_selection(1), None),
            KeyCode::Char('k') | KeyCode::Up => (self.move_selection(-1), None),
            KeyCode::Enter => {
                let name = self.selected_name();
                (self, name.map(Request::Switch))
            }
            KeyCode::Char('d') => (self.ask_forget(), None),
            KeyCode::Char('?') => (Self { help: true, ..self }, None),
            _ => (self, None),
        }
    }

    /// The picker with the selection asking to confirm forgetting it, when there is one.
    fn ask_forget(self) -> Self {
        let Some(name) = self.selected_name() else {
            return self;
        };
        Self {
            forgetting: Some(name),
            problem: None,
            ..self
        }
    }

    /// The picker's selection moved by `delta`, staying inside the list.
    fn move_selection(self, delta: isize) -> Self {
        let Some(last) = self.projects.len().checked_sub(1) else {
            return self;
        };
        Self {
            selection: self.selection.saturating_add_signed(delta).min(last),
            ..self
        }
    }

    /// What the frame's bottom border says while the picker is showing.
    pub(crate) fn footer_keys(&self) -> &'static str {
        if self.forgetting.is_some() {
            FORGET_KEYS
        } else {
            PROJECTS_KEYS
        }
    }

    /// The keys `?` shows right now: the forget question's own keys while it is asking, the
    /// picker's own keys otherwise.
    fn help_keys(&self) -> &'static [(&'static str, &'static str)] {
        if self.forgetting.is_some() {
            &FORGET_QUESTION_KEYS
        } else {
            &PICKER_KEYS
        }
    }

    /// Draws the picker over the whole of `area`: name and path per project — the one whose
    /// queue is on show, named by `current`, marked, and the selection marked and shown
    /// reversed.
    pub(crate) fn draw(&self, current: Option<&str>, area: Rect, buf: &mut Buffer) {
        if self.help {
            key_map(self.help_keys(), area, buf);
            return;
        }
        let mut lines = self.header_lines(usize::from(area.width));
        if self.projects.is_empty() {
            lines.push(Line::from("No projects are registered."));
        }
        let name_width = self
            .projects
            .iter()
            .map(|project| project.name.chars().count())
            .max()
            .unwrap_or(0);
        lines.extend(self.projects.iter().enumerate().map(|(index, project)| {
            let selected = index == self.selection;
            let is_current = Some(project.name.as_str()) == current;
            project_row(project, selected, is_current, name_width)
        }));
        Paragraph::new(lines).render(area, buf);
    }

    /// The picker's title, its problem line when its last submission changed nothing, and its
    /// forget question while one is open — everything above the project rows themselves. The
    /// forget question's name is cut with `…` to fit `width` when it is long, so its keys are
    /// never pushed off screen.
    fn header_lines(&self, width: usize) -> Vec<Line<'static>> {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::styled("Projects", bold)];
        if let Some(problem) = &self.problem {
            lines.push(Line::styled(format!("! {problem}"), bold));
        }
        if let Some(name) = &self.forgetting {
            let prefix = "Forget project \"";
            let suffix = "\"? Its journal stays on disk. y to forget · n or Esc to keep it";
            let budget = width.saturating_sub(prefix.chars().count() + suffix.chars().count());
            lines.push(Line::styled(
                format!("{prefix}{}{suffix}", elide(name, budget)),
                bold,
            ));
        }
        lines.push(Line::default());
        lines
    }
}

/// One row of the picker: `project`'s name, padded to `name_width`, and its path — marked
/// `>` and shown reversed when it is `selected`, and suffixed `(current)` when it is the one
/// whose queue is on show.
fn project_row(
    project: &Project,
    selected: bool,
    current: bool,
    name_width: usize,
) -> Line<'static> {
    let marker = if selected { '>' } else { ' ' };
    let mut style = Style::new();
    if selected {
        style = style.add_modifier(Modifier::REVERSED);
    }
    let current_mark = if current { " (current)" } else { "" };
    Line::styled(
        format!(
            "{marker} {:<name_width$}  {}{current_mark}",
            project.name,
            project.path.display(),
        ),
        style,
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::SystemTime;

    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::*;

    fn project(name: &str) -> Project {
        Project {
            name: name.to_owned(),
            path: PathBuf::from(format!("/work/{name}")),
            registered_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn projects() -> Vec<Project> {
        vec![project("app"), project("other")]
    }

    fn press(screen: ProjectsScreen, keys: &[KeyCode]) -> ProjectsScreen {
        keys.iter().fold(screen, |screen, key| screen.key(*key).0)
    }

    #[test]
    fn opens_with_the_current_project_selected() {
        let screen = ProjectsScreen::open(projects(), Some("other"));
        assert_eq!(screen.selection, 1);
        let screen = ProjectsScreen::open(projects(), None);
        assert_eq!(screen.selection, 0);
    }

    #[test]
    fn esc_asks_to_close_the_picker() {
        let (_, request) = ProjectsScreen::open(projects(), None).key(KeyCode::Esc);
        assert_eq!(request, Some(Request::Close));
    }

    #[test]
    fn j_and_k_move_the_selection_and_stay_inside_the_list() {
        let screen = ProjectsScreen::open(projects(), None);
        let screen = press(screen, &[KeyCode::Char('j'), KeyCode::Char('j')]);
        assert_eq!(screen.selection, 1);
        let screen = press(screen, &[KeyCode::Char('k'), KeyCode::Char('k')]);
        assert_eq!(screen.selection, 0);
    }

    #[test]
    fn while_the_key_map_is_open_only_it_answers_keys() {
        let open = ProjectsScreen::open(projects(), None);
        let mapped = press(open.clone(), &[KeyCode::Char('?')]);
        assert!(mapped.help);
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('d'),
            KeyCode::Enter,
            KeyCode::Char('x'),
        ] {
            assert_eq!(press(mapped.clone(), &[key]), mapped);
        }
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let closed = press(mapped.clone(), &[close]);
            assert!(!closed.help);
            assert_eq!(closed.selection, open.selection);
        }
    }

    #[test]
    fn enter_asks_to_switch_to_the_selected_projects_name() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('j')],
        );
        let (screen, request) = screen.key(KeyCode::Enter);
        assert_eq!(request, Some(Request::Switch("other".to_owned())));
        assert_eq!(screen.projects, projects());
    }

    #[test]
    fn switch_failed_shows_why_and_keeps_the_picker_open() {
        let screen =
            ProjectsScreen::open(projects(), None).switch_failed("cannot open it".to_owned());
        assert_eq!(screen.problem.as_deref(), Some("cannot open it"));
    }

    #[test]
    fn d_asks_to_confirm_forgetting_the_selected_project() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('j'), KeyCode::Char('d')],
        );
        assert_eq!(screen.forgetting.as_deref(), Some("other"));
    }

    #[test]
    fn while_the_forget_question_is_open_only_y_n_esc_and_the_key_map_answer_it() {
        let asked = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('d')],
        );
        for key in [
            KeyCode::Char('j'),
            KeyCode::Char('k'),
            KeyCode::Enter,
            KeyCode::Char('q'),
        ] {
            assert_eq!(press(asked.clone(), &[key]), asked);
        }

        let mapped = press(asked.clone(), &[KeyCode::Char('?')]);
        assert!(mapped.help);
        assert_eq!(mapped.forgetting, asked.forgetting);
        for key in [KeyCode::Char('y'), KeyCode::Char('n'), KeyCode::Char('x')] {
            assert_eq!(press(mapped.clone(), &[key]), mapped);
        }
        for close in [KeyCode::Esc, KeyCode::Char('?')] {
            let closed = press(mapped.clone(), &[close]);
            assert!(!closed.help);
            assert_eq!(closed.forgetting, asked.forgetting);
        }
    }

    #[test]
    fn y_confirms_the_forget_question_leaving_the_name_for_the_loop_to_forget() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('d')],
        );
        let (screen, request) = screen.key(KeyCode::Char('y'));
        assert_eq!(request, Some(Request::Forget("app".to_owned())));
        assert_eq!(screen.forgetting, None);
    }

    #[test]
    fn n_and_esc_drop_the_forget_question_and_forget_nothing() {
        let asked = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('d')],
        );
        for key in [KeyCode::Char('n'), KeyCode::Esc] {
            let (screen, request) = asked.clone().key(key);
            assert_eq!(screen.forgetting, None);
            assert_eq!(request, None);
        }
    }

    #[test]
    fn forgotten_shows_the_fresh_list_and_closes_the_question() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('d')],
        );
        let remaining = vec![project("other")];
        let screen = screen.forgotten(remaining.clone());
        assert_eq!(screen.projects, remaining);
        assert_eq!(screen.forgetting, None);
        assert_eq!(screen.selection, 0);
    }

    #[test]
    fn forgotten_clamps_the_selection_when_it_ran_past_the_fresh_list() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('j'), KeyCode::Char('d')],
        );
        let screen = screen.forgotten(vec![project("app")]);
        assert_eq!(screen.selection, 0);
    }

    #[test]
    fn forget_failed_keeps_the_picker_open_and_shows_why() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('d')],
        );
        let screen = screen.forget_failed("unknown project".to_owned());
        assert_eq!(screen.problem.as_deref(), Some("unknown project"));
        assert_eq!(screen.forgetting, None);
    }

    fn drawn(
        screen: &ProjectsScreen,
        current: Option<&str>,
        width: u16,
        height: u16,
    ) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        screen.draw(current, area, &mut buf);
        (0..height)
            .map(|y| (0..width).map(|x| buf[(x, y)].symbol()).collect())
            .collect()
    }

    fn row(rows: &[String], y: usize) -> &str {
        rows[y].trim_end()
    }

    #[test]
    fn shows_name_and_path_with_the_current_one_marked() {
        let screen = ProjectsScreen::open(projects(), Some("app"));
        let rows = drawn(&screen, Some("app"), 60, 6);
        assert_eq!(row(&rows, 0), "Projects");
        assert_eq!(row(&rows, 2), "> app    /work/app (current)");
        assert_eq!(row(&rows, 3), "  other  /work/other");
    }

    #[test]
    fn moving_the_selection_marks_the_row_it_lands_on() {
        let screen = press(
            ProjectsScreen::open(projects(), Some("app")),
            &[KeyCode::Char('j')],
        );
        let rows = drawn(&screen, Some("app"), 60, 6);
        assert_eq!(row(&rows, 2), "  app    /work/app (current)");
        assert_eq!(row(&rows, 3), "> other  /work/other");
    }

    #[test]
    fn a_failed_switch_shows_the_reason_above_the_list() {
        let screen =
            ProjectsScreen::open(projects(), None).switch_failed("cannot open it".to_owned());
        let rows = drawn(&screen, None, 60, 6);
        assert_eq!(row(&rows, 1), "! cannot open it");
    }

    #[test]
    fn the_key_map_replaces_the_picker() {
        let screen = press(
            ProjectsScreen::open(projects(), None),
            &[KeyCode::Char('?')],
        );
        let rows = drawn(&screen, None, 60, 10);
        let text = rows.join("\n");
        assert!(text.contains("Enter"), "{text}");
        assert!(!text.contains("app"), "{text}");
    }
}
