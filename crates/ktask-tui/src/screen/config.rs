//! The configuration screen: effective configuration and doctor results.
//!
//! Two sections, one scrollable document. The doctor comes first so a failing
//! check is on screen without scrolling: each result is the line `ktask-rs
//! doctor` prints ([`CheckResult::render_line`], the remedy included), wrapped
//! rather than cut, so a remedy is never lost to a narrow terminal. Below it is
//! every configuration key with the layer that resolved it (default, global
//! file, project file, environment or flag, from [`Config::provenance`] by way
//! of [`Config::settings`]) and its effective value.
//!
//! What the screen holds is one [`Snapshot`], read by the shell, not by
//! [`update`](crate::update), which does no I/O. [`wanted`] says when a read is
//! due: on first showing, and again after `r`. [`backfill`] does it, calling
//! `load` (usually [`load`], which reads the files as they are now and runs the
//! checks) and storing what came back. A configuration that cannot be read is
//! stored as the reason and shown with the way out. Nothing here follows the
//! journal: configuration and the environment do not move with it, so the
//! screen is as fresh as the last `r`.
//!
//! Everything shown may carry text from a file or the environment, so it is
//! sanitized and put on one line before it is wrapped.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::{LayoutPlan, layout_for};
use crate::sanitize::sanitize;
use crate::text::{display_width, truncate_to_width};
use crate::types::Screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ktask_core::{
    CheckResult, CheckStatus, Config, Project, Setting, Source, load_for, run_checks,
};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation;

/// What replaces the line breaks inside a piece of text.
const BREAK: &str = " ⏎ ";

/// What the body shows before the first read.
const NOT_LOADED: &str = "Reading the configuration and running the checks…";

/// What a key nothing has set shows as its value.
const UNSET: &str = "(unset)";

/// The width of the source column: the longest label, `project file`.
const SOURCE_WIDTH: usize = 12;

/// The widest the key column gets, however long a key is.
const KEY_WIDTH: usize = 32;

/// What separates the columns of a setting's row.
const GAP: &str = " ";

/// How far the continuation lines of a wrapped line are indented.
const INDENT: usize = 2;

/// The fewest columns the value column may have before the value is moved to
/// lines of its own under the key.
const MIN_VALUE_WIDTH: usize = 16;

/// What separates the entries of the key bar.
const BAR_GAP: &str = "  ";

/// What one read of the configuration and the environment found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Every configuration key with its effective value and source.
    pub settings: Vec<Setting>,
    /// What `ktask-rs doctor` reports, in the order it prints it.
    pub checks: Vec<CheckResult>,
}

/// The screen's state: the last read, whether another has been asked for, and
/// how far the document is scrolled.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConfigView {
    /// The last read, or the reason it failed; `None` before the first.
    loaded: Option<Result<Snapshot, String>>,
    /// Whether `r` asked for a fresh read that has not happened yet.
    refresh: bool,
    /// The first line of the document that is shown.
    top: usize,
}

impl ConfigView {
    /// The last read, or `None` before the first one and while it failed.
    #[must_use]
    pub fn snapshot(&self) -> Option<&Snapshot> {
        self.loaded.as_ref()?.as_ref().ok()
    }

    /// Why the last read failed, or `None` when it did not or has not happened.
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.loaded.as_ref()?.as_ref().err().map(String::as_str)
    }

    /// Whether a fresh read has been asked for and has not happened yet.
    #[must_use]
    pub fn refreshing(&self) -> bool {
        self.refresh
    }
}

/// Reads the effective configuration of `config`'s project and runs the
/// doctor's checks against it.
#[must_use]
pub fn snapshot(config: &Config, project: &Project) -> Snapshot {
    Snapshot {
        settings: config.settings(),
        checks: run_checks(project, config),
    }
}

/// Reads `project`'s configuration as it is on disk and in the environment now
/// and runs the checks, the same way `ktask-rs doctor` does.
///
/// # Errors
///
/// The reason the configuration could not be loaded: a file that cannot be
/// read or parsed, an unknown key or a value of the wrong type. The doctor
/// cannot say anything about a configuration that is not there, so no checks
/// are run.
pub fn load(project: &Project) -> Result<Snapshot, String> {
    let config = load_for(project).map_err(|err| err.to_string())?;
    Ok(snapshot(&config, project))
}

/// Whether the shell should read now: the screen is showing, and nothing has
/// been read yet or `r` asked for another read.
#[must_use]
pub fn wanted(app: &App) -> bool {
    app.screen == Screen::Config && (app.config.loaded.is_none() || app.config.refresh)
}

/// Does what [`wanted`] asks for, calling `load` and storing what it returns,
/// replacing the previous read. Returns whether it read.
///
/// The shell calls this after a turn; it is the one place this screen does
/// I/O. A failed read is stored as the reason it failed, so it is not asked
/// for again until `r`.
pub fn backfill(app: &mut App, load: &dyn Fn() -> Result<Snapshot, String>) -> bool {
    if !wanted(app) {
        return false;
    }
    app.config.loaded = Some(load());
    app.config.refresh = false;
    true
}

/// Whether the screen has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Config && app.overlay.is_none()
}

/// Handles the configuration screen's keys: `r` asks for a fresh read, `j`,
/// `k` and the arrows scroll a line, `g` and `G` go to the top and the bottom,
/// and `PageUp` and `PageDown` scroll a screenful. Does nothing on other
/// screens or under an overlay.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    if key.code == KeyCode::Char('r') && (key.modifiers - KeyModifiers::SHIFT).is_empty() {
        app.config.refresh = true;
        return;
    }
    let height = rows(app);
    let last_top = document(&app.config, usize::from(app.size.0))
        .len()
        .saturating_sub(height);
    let current = app.config.top.min(last_top);
    let action = lookup(app.screen, key).map(|binding| binding.action);
    let top = match (action, key.code) {
        (Some(KeyAction::MoveUp), _) => current.saturating_sub(1),
        (Some(KeyAction::MoveDown), _) => (current + 1).min(last_top),
        (Some(KeyAction::First), _) => 0,
        (Some(KeyAction::Last), _) => last_top,
        (_, KeyCode::PageUp) => current.saturating_sub(height.max(1)),
        (_, KeyCode::PageDown) => current.saturating_add(height.max(1)).min(last_top),
        _ => return,
    };
    app.config.top = top;
}

/// The rows of the body that show the document: all but the key bar, which
/// takes the last row when there is more than one.
fn rows(app: &App) -> usize {
    let (columns, height) = app.size;
    let body = layout_for(Rect::new(0, 0, columns, height)).body;
    match usize::from(body.height) {
        0 => 0,
        1 => 1,
        rows => rows - 1,
    }
}

/// `text` sanitized and on one line.
fn one_line(text: &str) -> String {
    sanitize(text).trim_end_matches('\n').replace('\n', BREAK)
}

/// `text` cut into lines of at most `width` columns, at grapheme boundaries,
/// without dropping or changing a character: the first line has the whole
/// width and each later one is indented by [`INDENT`], so removing the
/// indentation and joining the lines gives `text` back.
///
/// A cluster wider than the room it has is put on a line of its own rather
/// than dropped. Nothing is returned for a `width` of zero.
fn wrap(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }
    let indent = if width > INDENT + 1 { INDENT } else { 0 };
    let mut lines = vec![String::new()];
    let mut used = 0;
    for cluster in text.graphemes(true) {
        let columns = display_width(cluster);
        let limit = if lines.len() == 1 {
            width
        } else {
            width - indent
        };
        if used > 0 && used + columns > limit {
            lines.push(" ".repeat(indent));
            used = 0;
        }
        if let Some(line) = lines.last_mut() {
            line.push_str(cluster);
        }
        used += columns;
    }
    lines
}

/// The label of the layer that resolved a value.
fn source_label(source: Source) -> &'static str {
    match source {
        Source::Default => "default",
        Source::GlobalFile => "global file",
        Source::ProjectFile => "project file",
        Source::Env => "environment",
        Source::Flag => "flag",
    }
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

/// The style a check's line is drawn in.
fn check_style(check: &CheckResult) -> Style {
    match check.status {
        CheckStatus::Pass => Style::new(),
        CheckStatus::Fail => Style::new().fg(Color::Red),
    }
}

/// The style of a source label: a value that did not come from the defaults
/// stands out, since it is the one somebody chose.
fn source_style(source: Source) -> Style {
    match source {
        Source::Default => dim(),
        _ => Style::new().fg(Color::Cyan),
    }
}

/// The heading of the doctor section: how the checks came out.
fn doctor_heading(checks: &[CheckResult], refreshing: bool) -> String {
    let failed = checks.iter().filter(|check| !check.passed()).count();
    let outcome = match (checks.len(), failed) {
        (0, _) => "no checks".to_owned(),
        (total, 0) => format!("all {total} checks pass"),
        (total, failed) => format!("{failed} of {total} checks fail"),
    };
    let suffix = if refreshing { " · refreshing…" } else { "" };
    format!("Doctor · {outcome}{suffix}")
}

/// The lines of the doctor section: the heading, then each check as the line
/// the command prints, wrapped.
fn doctor_lines(checks: &[CheckResult], refreshing: bool, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        truncate_to_width(&doctor_heading(checks, refreshing), width),
        bold(),
    )];
    for check in checks {
        let style = check_style(check);
        lines.extend(
            wrap(&one_line(&check.render_line()), width)
                .into_iter()
                .map(|line| Line::styled(line, style)),
        );
    }
    lines
}

/// The lines of the configuration section: the heading, then a row for each
/// setting with its key, its source and its value, which wraps when it is
/// long. Where the columns leave the value too little room, it goes on lines
/// of its own under the key and the source.
fn settings_lines(settings: &[Setting], width: usize) -> Vec<Line<'static>> {
    let overridden = settings
        .iter()
        .filter(|setting| setting.source != Source::Default)
        .count();
    let heading = format!(
        "Configuration · {} settings · {overridden} not from the defaults",
        settings.len()
    );
    let mut lines = vec![Line::styled(truncate_to_width(&heading, width), bold())];
    let key_width = settings
        .iter()
        .map(|setting| display_width(&setting.key))
        .max()
        .unwrap_or(0)
        .min(KEY_WIDTH);
    let value_column = key_width + GAP.len() + SOURCE_WIDTH + GAP.len();
    let inline = width >= value_column + MIN_VALUE_WIDTH;
    for setting in settings {
        let value = setting.value.as_deref().map_or_else(
            || (UNSET.to_owned(), dim()),
            |value| (one_line(value), Style::new()),
        );
        let key = truncate_to_width(&setting.key, key_width);
        let label = source_label(setting.source);
        let source_style = source_style(setting.source);
        if inline {
            let mut chunks = wrap(&value.0, width - value_column).into_iter();
            let first = chunks.next().unwrap_or_default();
            let key_pad = key_width - display_width(&key);
            let source_pad = SOURCE_WIDTH - display_width(label);
            lines.push(Line::from(vec![
                Span::raw(format!("{key}{}{GAP}", " ".repeat(key_pad))),
                Span::styled(
                    format!("{label}{}{GAP}", " ".repeat(source_pad)),
                    source_style,
                ),
                Span::styled(first, value.1),
            ]));
            lines.extend(chunks.map(|chunk| {
                Line::from(vec![
                    Span::raw(" ".repeat(value_column)),
                    Span::styled(chunk, value.1),
                ])
            }));
        } else {
            lines.push(Line::from(vec![
                Span::raw(truncate_to_width(&format!("{key}{GAP}"), width)),
                Span::styled(truncate_to_width(label, width), source_style),
            ]));
            lines.extend(
                wrap(&value.0, width.saturating_sub(INDENT))
                    .into_iter()
                    .map(|chunk| {
                        Line::from(vec![
                            Span::raw(" ".repeat(INDENT.min(width))),
                            Span::styled(chunk, value.1),
                        ])
                    }),
            );
        }
    }
    lines
}

/// What the body shows when the configuration could not be read: the reason,
/// wrapped, and what to do about it.
fn failure_lines(reason: &str, width: usize) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled(
        truncate_to_width("The configuration could not be loaded", width),
        Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    )];
    lines.extend(
        wrap(&one_line(reason), width)
            .into_iter()
            .map(|line| Line::styled(line, Style::new().fg(Color::Red))),
    );
    lines.push(Line::default());
    lines.push(Line::raw(truncate_to_width(
        "Fix the file it names, then press r to read it again.",
        width,
    )));
    lines
}

/// The whole scrollable document at `width` columns: the doctor, then the
/// configuration, or the reason there is none, or the note that it has not
/// been read.
fn document(view: &ConfigView, width: usize) -> Vec<Line<'static>> {
    match &view.loaded {
        None => vec![Line::styled(truncate_to_width(NOT_LOADED, width), dim())],
        Some(Err(reason)) => failure_lines(reason, width),
        Some(Ok(snapshot)) => {
            let mut lines = doctor_lines(&snapshot.checks, view.refresh, width);
            lines.push(Line::default());
            lines.extend(settings_lines(&snapshot.settings, width));
            lines
        }
    }
}

/// The line of keys shown under the document, with where the view is in it
/// when it does not all fit.
fn key_bar(top: usize, shown: usize, total: usize) -> String {
    let mut entries = vec![
        "r refresh".to_owned(),
        "j/k scroll".to_owned(),
        "g/G top/bottom".to_owned(),
        "PgUp/PgDn page".to_owned(),
    ];
    if shown < total {
        entries.push(format!("{}-{} of {total}", top + 1, top + shown));
    }
    entries.join(BAR_GAP)
}

/// Draws the configuration screen into the body of `plan`: the doctor's
/// results, then the effective configuration with the source of each value,
/// scrolled to the view, and under them the keys.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let width = usize::from(body.width);
    let lines = document(&app.config, width);
    if app.config.loaded.is_none() {
        frame.render_widget(Paragraph::new(lines), body);
        return;
    }
    let height = usize::from(body.height);
    let shown = if height > 1 { height - 1 } else { height };
    let top = app.config.top.min(lines.len().saturating_sub(shown));
    let total = lines.len();
    let mut visible: Vec<Line<'static>> = lines.into_iter().skip(top).take(shown).collect();
    if height > 1 {
        let shown_lines = visible.len();
        visible.resize_with(shown, Line::default);
        visible.push(Line::raw(truncate_to_width(
            &key_bar(top, shown_lines, total),
            width,
        )));
    }
    frame.render_widget(Paragraph::new(visible), body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::Overlay;
    use ktask_core::{Error, Setting};
    use std::cell::Cell;

    // ---- fixtures ----

    fn app_at(size: (u16, u16)) -> App {
        App {
            screen: Screen::Config,
            ..App::new(size)
        }
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn key(app: App, c: char) -> App {
        press(app, KeyCode::Char(c))
    }

    fn check(
        name: &'static str,
        status: CheckStatus,
        detail: &str,
        remedy: Option<&str>,
    ) -> CheckResult {
        CheckResult {
            check: name,
            status,
            detail: detail.to_owned(),
            remedy: remedy.map(str::to_owned),
        }
    }

    fn passing(name: &'static str) -> CheckResult {
        check(name, CheckStatus::Pass, "fine", None)
    }

    fn failing(name: &'static str, remedy: &str) -> CheckResult {
        check(name, CheckStatus::Fail, "broken", Some(remedy))
    }

    fn setting(key: &str, value: Option<&str>, source: Source) -> Setting {
        Setting {
            key: key.to_owned(),
            value: value.map(str::to_owned),
            source,
        }
    }

    fn snap(settings: Vec<Setting>, checks: Vec<CheckResult>) -> Snapshot {
        Snapshot { settings, checks }
    }

    fn loaded(size: (u16, u16), snapshot: Snapshot) -> App {
        let mut app = app_at(size);
        let once = Cell::new(Some(snapshot));
        let read = backfill(&mut app, &|| {
            once.take().ok_or_else(|| "read twice".to_owned())
        });
        assert!(read);
        app
    }

    fn shown(app: &App) -> Vec<String> {
        Harness::from_app(app.clone())
            .text()
            .split('\n')
            .map(|row| row.trim_end().to_owned())
            .collect()
    }

    fn has_line(app: &App, wanted: &str) -> bool {
        shown(app).iter().any(|line| line == wanted)
    }

    /// A configuration with a value from every layer `Config::load` produces.
    fn layered() -> Config {
        let dir = tempfile::tempdir().expect("tempdir");
        let global = dir.path().join("global.toml");
        let project = dir.path().join("project.toml");
        std::fs::write(&global, "provider = \"claude\"\nretention_days = 30\n").expect("write");
        std::fs::write(
            &project,
            "retention_days = 45\nverify_command = [\"make\", \"test\"]\n",
        )
        .expect("write");
        Config::load(Some(&global), Some(&project), &|name| {
            (name == "KTASK_MODEL").then(|| "opus".to_owned())
        })
        .expect("load")
    }

    /// The settings of `layered()` as a snapshot with no checks, so no provider
    /// is run.
    fn layered_snapshot() -> (Config, Snapshot) {
        let config = layered();
        let snapshot = snap(config.settings(), Vec::new());
        (config, snapshot)
    }

    fn broken_project() -> Project {
        Project {
            root: "/nonexistent/ktask-config-screen/root".into(),
            id: "config-screen-fixture".to_owned(),
            state_dir: "/nonexistent/ktask-config-screen/state".into(),
        }
    }

    // ---- wrap ----

    #[test]
    fn wrap_leaves_text_that_fits_on_one_line() {
        assert_eq!(wrap("hello", 5), ["hello"]);
        assert_eq!(wrap("hello", 80), ["hello"]);
    }

    #[test]
    fn wrap_breaks_at_the_width_and_indents_the_continuation() {
        assert_eq!(wrap("abcdefghij", 6), ["abcdef", "  ghij"]);
        // The continuation has the width less its indent.
        assert_eq!(
            wrap("abcdefghijklmnop", 6),
            ["abcdef", "  ghij", "  klmn", "  op"]
        );
    }

    #[test]
    fn wrap_gives_nothing_back_for_no_width() {
        assert!(wrap("anything", 0).is_empty());
    }

    #[test]
    fn wrap_without_room_for_an_indent_does_not_indent() {
        assert_eq!(wrap("abcd", 2), ["ab", "cd"]);
        assert_eq!(wrap("abcd", 3), ["abc", "d"]);
    }

    #[test]
    fn wrap_loses_no_character_and_no_line_is_too_wide() {
        let text = "FAIL provider: unknown provider `x` (remedy: fix `provider` in the config) 日本語 🚀 e\u{301}";
        for width in [1, 2, 3, 4, 7, 20, 40, 80] {
            let lines = wrap(text, width);
            let rebuilt: String = lines
                .iter()
                .enumerate()
                .map(|(n, line)| {
                    if n == 0 {
                        line.as_str()
                    } else {
                        line.get(if width > 3 { INDENT } else { 0 }..).unwrap_or("")
                    }
                })
                .collect();
            assert_eq!(rebuilt, text, "width {width}");
            for line in &lines {
                // Only a cluster wider than the room may overflow it, alone.
                assert!(
                    display_width(line) <= width || line.trim_start().graphemes(true).count() == 1,
                    "{line:?} at {width}"
                );
            }
        }
    }

    #[test]
    fn wrap_does_not_split_a_wide_character_across_lines() {
        assert_eq!(wrap("日本語", 5), ["日本", "  語"]);
    }

    // ---- reading ----

    #[test]
    fn config_is_wanted_on_first_showing_and_not_on_other_screens() {
        assert!(wanted(&app_at((80, 24))));
        assert!(!wanted(&App::new((80, 24))));
        let mut elsewhere = App::new((80, 24));
        elsewhere.screen = Screen::Git;
        assert!(!wanted(&elsewhere));
    }

    #[test]
    fn config_backfill_reads_once_and_stores_what_was_read() {
        let mut app = app_at((80, 24));
        let reads = Cell::new(0);
        let snapshot = snap(
            vec![setting("provider", Some("\"dummy\""), Source::Default)],
            vec![passing("git")],
        );
        let load = || {
            reads.set(reads.get() + 1);
            Ok(snapshot.clone())
        };
        assert!(backfill(&mut app, &load));
        assert_eq!(app.config.snapshot(), Some(&snapshot));
        assert!(!wanted(&app));
        assert!(!backfill(&mut app, &load));
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn config_backfill_does_not_read_for_a_screen_that_is_not_showing() {
        let mut app = App::new((80, 24));
        assert!(!backfill(&mut app, &|| panic!("must not read")));
        assert_eq!(app.config, ConfigView::default());
    }

    #[test]
    fn config_backfill_stores_a_failed_read_as_its_reason_and_does_not_retry_it() {
        let mut app = app_at((80, 24));
        assert!(backfill(&mut app, &|| Err("bad key".to_owned())));
        assert_eq!(app.config.failure(), Some("bad key"));
        assert_eq!(app.config.snapshot(), None);
        assert!(!wanted(&app));
    }

    #[test]
    fn config_snapshot_of_a_config_holds_its_settings_and_the_doctors_checks() {
        let config = Config::default();
        let project = broken_project();
        let snapshot = snapshot(&config, &project);
        assert_eq!(snapshot.settings, config.settings());
        assert_eq!(snapshot.checks, run_checks(&project, &config));
        assert_eq!(snapshot.checks.len(), 5);
    }

    #[test]
    fn config_load_reports_a_configuration_that_cannot_be_read_as_a_reason() {
        // No HOME and no config files: whichever way the environment goes, the
        // result is a snapshot or a reason, never a panic.
        match load(&broken_project()) {
            Ok(snapshot) => assert_eq!(snapshot.checks.len(), 5),
            Err(reason) => assert!(!reason.is_empty()),
        }
    }

    // ---- r ----

    #[test]
    fn config_r_asks_for_a_fresh_read_and_the_next_backfill_replaces_the_last() {
        let first = snap(
            vec![setting("provider", Some("\"dummy\""), Source::Default)],
            vec![passing("git")],
        );
        let second = snap(
            vec![setting("provider", Some("\"claude\""), Source::ProjectFile)],
            vec![failing("git", "install git")],
        );
        let mut app = loaded((80, 24), first);
        assert!(!wanted(&app));
        app = key(app, 'r');
        assert!(app.config.refreshing());
        assert!(wanted(&app));
        assert!(backfill(&mut app, &|| Ok(second.clone())));
        assert_eq!(app.config.snapshot(), Some(&second));
        assert!(!app.config.refreshing());
        assert!(!wanted(&app));
    }

    #[test]
    fn config_r_after_a_failed_read_asks_again() {
        let mut app = app_at((80, 24));
        assert!(backfill(&mut app, &|| Err("bad".to_owned())));
        app = key(app, 'r');
        assert!(wanted(&app));
        let good = snap(Vec::new(), vec![passing("git")]);
        assert!(backfill(&mut app, &|| Ok(good.clone())));
        assert_eq!(app.config.failure(), None);
        assert_eq!(app.config.snapshot(), Some(&good));
    }

    #[test]
    fn config_r_is_ignored_on_other_screens_under_an_overlay_and_with_modifiers() {
        let base = loaded((80, 24), snap(Vec::new(), vec![passing("git")]));
        let mut elsewhere = base.clone();
        elsewhere.screen = Screen::Logs;
        assert!(!key(elsewhere, 'r').config.refreshing());
        let mut covered = base.clone();
        covered.overlay = Some(Overlay::KeyMap);
        assert!(!key(covered, 'r').config.refreshing());
        let ctrl = update(
            base,
            AppEvent::Key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)),
        );
        assert!(!ctrl.config.refreshing());
    }

    #[test]
    fn config_the_doctor_heading_says_it_is_refreshing_until_the_read_happens() {
        let mut app = loaded((80, 24), snap(Vec::new(), vec![passing("git")]));
        assert!(has_line(&app, "Doctor · all 1 checks pass"));
        app = key(app, 'r');
        assert!(has_line(&app, "Doctor · all 1 checks pass · refreshing…"));
        assert!(backfill(&mut app, &|| Ok(snap(
            Vec::new(),
            vec![passing("git")]
        ))));
        assert!(has_line(&app, "Doctor · all 1 checks pass"));
    }

    // ---- provenance ----

    #[test]
    fn config_shows_every_key_with_its_value_and_the_layer_that_resolved_it() {
        let (config, snapshot) = layered_snapshot();
        let app = loaded((200, 90), snapshot);
        let lines = shown(&app);
        for (key, source) in config.provenance() {
            let row = lines
                .iter()
                .find(|line| line.split_whitespace().next() == Some(key.as_str()))
                .unwrap_or_else(|| panic!("no row for {key}: {lines:#?}"));
            let label = source_label(source);
            assert!(row.contains(label), "{key} should say {label}: {row:?}");
        }
    }

    /// The key, the source and the value of the row of `key`, read from the
    /// columns the screen lays out: the key column is as wide as the longest
    /// key, then a space, twelve columns of source and a space.
    fn columns(app: &App, key: &str) -> (String, String, String) {
        let lines = shown(app);
        let row = lines
            .iter()
            .find(|line| line.split_whitespace().next() == Some(key))
            .unwrap_or_else(|| panic!("no row for {key}"));
        let key_width = "circuit_breaker_threshold".len();
        let cut = |from: usize, to: usize| row.get(from..to).unwrap_or_default().trim().to_owned();
        let source_at = key_width + 1;
        let value_at = source_at + SOURCE_WIDTH + 1;
        (
            cut(0, key_width),
            cut(source_at, value_at),
            row.get(value_at..).unwrap_or_default().to_owned(),
        )
    }

    #[test]
    fn config_rows_name_the_winning_layer_of_each_value() {
        let (_, snapshot) = layered_snapshot();
        let app = loaded((200, 90), snapshot);
        let row = |key: &str| columns(&app, key);
        assert_eq!(
            row("provider"),
            ("provider".into(), "global file".into(), "\"claude\"".into())
        );
        assert_eq!(
            row("retention_days"),
            ("retention_days".into(), "project file".into(), "45".into())
        );
        assert_eq!(
            row("model"),
            ("model".into(), "environment".into(), "\"opus\"".into())
        );
        assert_eq!(
            row("max_attempts"),
            ("max_attempts".into(), "default".into(), "2".into())
        );
        assert_eq!(
            row("verify_command"),
            (
                "verify_command".into(),
                "project file".into(),
                "[\"make\", \"test\"]".into()
            )
        );
        assert_eq!(
            row("flake_command"),
            ("flake_command".into(), "default".into(), "(unset)".into())
        );
    }

    #[test]
    fn config_a_flag_is_labelled_as_one() {
        let app = loaded(
            (80, 24),
            snap(
                vec![setting("provider", Some("\"x\""), Source::Flag)],
                Vec::new(),
            ),
        );
        assert!(
            shown(&app)
                .iter()
                .any(|line| line.starts_with("provider") && line.contains("flag"))
        );
    }

    #[test]
    fn config_a_key_nothing_set_says_so() {
        let app = loaded(
            (80, 24),
            snap(vec![setting("model", None, Source::Default)], Vec::new()),
        );
        assert!(
            shown(&app)
                .iter()
                .any(|line| line.starts_with("model") && line.contains("(unset)"))
        );
    }

    #[test]
    fn config_the_heading_counts_the_settings_and_those_not_from_the_defaults() {
        let config = layered();
        let settings = config.settings();
        let elsewhere = settings
            .iter()
            .filter(|s| s.source != Source::Default)
            .count();
        assert_eq!(elsewhere, 4);
        let app = loaded((200, 60), snapshot(&config, &broken_project()));
        assert!(has_line(
            &app,
            &format!(
                "Configuration · {} settings · {elsewhere} not from the defaults",
                settings.len()
            )
        ));
    }

    #[test]
    fn config_a_long_value_wraps_under_its_column_without_losing_any_of_it() {
        let value = format!(
            "[{}]",
            (0..30)
                .map(|n| format!("\"pattern-{n}\""))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let app = loaded(
            (80, 60),
            snap(
                vec![setting(
                    "secret_patterns",
                    Some(&value),
                    Source::ProjectFile,
                )],
                Vec::new(),
            ),
        );
        let lines = shown(&app);
        let at = lines
            .iter()
            .position(|l| l.starts_with("secret_patterns"))
            .expect("row");
        let column = "secret_patterns".len() + GAP.len() + SOURCE_WIDTH + GAP.len();
        let column = column.max(lines[at].find('[').expect("value starts"));
        let mut rebuilt = String::new();
        for (n, line) in lines[at..].iter().enumerate() {
            let part = line.get(column..).unwrap_or("");
            if n > 0 {
                assert!(
                    line.trim_start().is_empty() || line.starts_with(&" ".repeat(column)),
                    "{line:?}"
                );
                if part.is_empty() {
                    break;
                }
            }
            rebuilt.push_str(part.strip_prefix("  ").filter(|_| n > 0).unwrap_or(part));
        }
        assert_eq!(rebuilt, value);
    }

    #[test]
    fn config_a_narrow_terminal_puts_the_value_under_the_key() {
        let app = loaded(
            (40, 30),
            snap(
                vec![setting("mainline_branch", Some("\"main\""), Source::Env)],
                Vec::new(),
            ),
        );
        let lines = shown(&app);
        let at = lines
            .iter()
            .position(|l| l.starts_with("mainline_branch"))
            .expect("row");
        assert_eq!(lines[at], "mainline_branch environment");
        assert_eq!(lines[at + 1], "  \"main\"");
    }

    // ---- doctor ----

    #[test]
    fn config_a_failing_check_shows_the_line_the_command_prints_remedy_included() {
        let results = run_checks(&broken_project(), &Config::default());
        assert!(results.iter().any(|result| !result.passed()));
        let app = loaded((300, 40), snap(Vec::new(), results.clone()));
        let lines = shown(&app);
        for result in &results {
            let line = result.render_line();
            assert!(
                lines.contains(&line),
                "{line:?} is not on screen: {lines:#?}"
            );
        }
    }

    #[test]
    fn config_a_long_remedy_is_wrapped_not_cut_and_reads_the_same_as_the_command() {
        let results = run_checks(&broken_project(), &Config::default());
        let app = loaded((80, 40), snap(Vec::new(), results.clone()));
        let lines = shown(&app);
        for result in &results {
            let expected = result.render_line();
            let at = lines
                .iter()
                .position(|line| {
                    line.starts_with(&format!(
                        "{} {}:",
                        if result.passed() { "PASS" } else { "FAIL" },
                        result.check
                    ))
                })
                .unwrap_or_else(|| panic!("{} is not shown", result.check));
            let mut rebuilt = lines[at].clone();
            for line in &lines[at + 1..] {
                match line.strip_prefix("  ") {
                    Some(rest) => rebuilt.push_str(rest),
                    None => break,
                }
            }
            assert_eq!(rebuilt, expected);
        }
    }

    #[test]
    fn config_failing_checks_are_red_and_passing_ones_are_not() {
        let app = loaded(
            (80, 24),
            snap(
                Vec::new(),
                vec![passing("git"), failing("toolchain", "install rust")],
            ),
        );
        let harness = Harness::from_app(app);
        let rows: Vec<String> = harness.text().split('\n').map(str::to_owned).collect();
        let row_of = |prefix: &str| {
            rows.iter()
                .position(|row| row.starts_with(prefix))
                .expect("row")
        };
        let buffer = harness.buffer();
        let colour = |row: usize| buffer[(0, u16::try_from(row).expect("row"))].fg;
        assert_eq!(colour(row_of("FAIL toolchain")), Color::Red);
        assert_ne!(colour(row_of("PASS git")), Color::Red);
    }

    #[test]
    fn config_the_doctor_heading_counts_the_failures() {
        let app = loaded(
            (80, 24),
            snap(
                Vec::new(),
                vec![
                    passing("git"),
                    failing("toolchain", "x"),
                    failing("journal", "y"),
                ],
            ),
        );
        assert!(has_line(&app, "Doctor · 2 of 3 checks fail"));
        let none = loaded((80, 24), snap(Vec::new(), Vec::new()));
        assert!(has_line(&none, "Doctor · no checks"));
    }

    #[test]
    fn config_text_from_the_environment_cannot_move_the_cursor_or_break_a_line() {
        let hostile = check(
            "state_dir",
            CheckStatus::Fail,
            "no \x1b[2J\x1b]0;pwned\x07 dir\nsecond line\rover",
            Some("fix it"),
        );
        let app = loaded(
            (80, 24),
            snap(
                vec![setting("provider", Some("\"a\x1b[31mb\""), Source::Env)],
                vec![hostile],
            ),
        );
        let text = shown(&app).join("\n");
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(!text.contains('\r'));
        assert!(text.contains("second line"));
        assert!(text.contains("⏎"));
        assert!(!text.contains("pwned"));
    }

    // ---- states ----

    #[test]
    fn config_before_the_first_read_says_it_is_reading() {
        assert!(has_line(&app_at((80, 24)), NOT_LOADED));
    }

    #[test]
    fn config_a_configuration_that_cannot_be_loaded_is_shown_with_the_way_out() {
        let reason = Error::Config {
            key: "/home/x/.config/ktask-rs/config.toml".to_owned(),
            detail: "unknown field `bogus`".to_owned(),
        }
        .to_string();
        let mut app = app_at((80, 24));
        assert!(backfill(&mut app, &|| Err(reason.clone())));
        let text = shown(&app).join("\n");
        assert!(text.contains("The configuration could not be loaded"));
        assert!(text.contains("bogus"), "{text}");
        assert!(text.contains("press r to read it again"));
        assert!(!text.contains("Doctor"));
    }

    // ---- scrolling ----

    fn many(count: usize) -> Snapshot {
        snap(
            (0..count)
                .map(|n| setting(&format!("key_{n:02}"), Some("1"), Source::Default))
                .collect(),
            vec![passing("git")],
        )
    }

    fn top_row(app: &App) -> String {
        shown(app).get(1).cloned().unwrap_or_default()
    }

    #[test]
    fn config_j_and_k_scroll_a_line_within_the_document() {
        let mut app = loaded((80, 24), many(40));
        assert_eq!(top_row(&app), "Doctor · all 1 checks pass");
        app = key(app, 'j');
        assert_eq!(top_row(&app), "PASS git: fine");
        app = press(app, KeyCode::Down);
        assert_eq!(top_row(&app), "");
        app = key(app, 'k');
        assert_eq!(top_row(&app), "PASS git: fine");
        app = press(app, KeyCode::Up);
        app = key(app, 'k');
        assert_eq!(top_row(&app), "Doctor · all 1 checks pass");
    }

    #[test]
    fn config_g_and_capital_g_go_to_the_top_and_the_bottom() {
        let mut app = loaded((80, 24), many(40));
        app = key(app, 'G');
        let rows = shown(&app);
        assert_eq!(rows[rows.len() - 3], "key_39 default      1");
        app = key(app, 'g');
        assert_eq!(top_row(&app), "Doctor · all 1 checks pass");
    }

    #[test]
    fn config_page_keys_scroll_a_screenful_and_stop_at_the_ends() {
        let mut app = loaded((80, 24), many(40));
        let page = usize::from(app.size.1) - 3;
        // Document: heading, 1 check, blank, heading, 40 rows.
        let total = 4 + 40;
        app = press(app, KeyCode::PageDown);
        assert_eq!(app.config.top, page);
        for _ in 0..5 {
            app = press(app, KeyCode::PageDown);
        }
        assert_eq!(app.config.top, total - page);
        app = press(app, KeyCode::PageUp);
        assert_eq!(app.config.top, total - 2 * page);
        for _ in 0..5 {
            app = press(app, KeyCode::PageUp);
        }
        assert_eq!(app.config.top, 0);
    }

    #[test]
    fn config_scrolling_past_the_bottom_then_back_up_moves_one_line() {
        let mut app = loaded((80, 24), many(40));
        app = key(app, 'G');
        let bottom = app.config.top;
        app = key(app, 'j');
        assert_eq!(app.config.top, bottom);
        app = key(app, 'k');
        assert_eq!(app.config.top, bottom - 1);
    }

    #[test]
    fn config_a_short_document_does_not_scroll() {
        let mut app = loaded((80, 24), many(2));
        for code in ['j', 'G', 'k', 'g'] {
            app = key(app, code);
            assert_eq!(app.config.top, 0);
        }
        app = press(app, KeyCode::PageDown);
        assert_eq!(app.config.top, 0);
    }

    #[test]
    fn config_scroll_keys_do_nothing_on_other_screens_or_under_an_overlay() {
        let base = loaded((80, 24), many(40));
        let mut elsewhere = base.clone();
        elsewhere.screen = Screen::Logs;
        assert_eq!(key(elsewhere.clone(), 'j').config, elsewhere.config);
        let mut covered = base;
        covered.overlay = Some(Overlay::KeyMap);
        assert_eq!(key(covered.clone(), 'G').config, covered.config);
    }

    #[test]
    fn config_unbound_keys_change_nothing() {
        let app = loaded((80, 24), many(40));
        for c in ['x', 'a', 'z', 'R'] {
            assert_eq!(key(app.clone(), c).config, app.config, "{c}");
        }
    }

    #[test]
    fn config_the_key_bar_lists_the_keys_and_where_the_view_is() {
        let mut app = loaded((80, 24), many(40));
        let rows = shown(&app);
        assert_eq!(
            rows.last().map(String::as_str),
            Some("Press ? for the key map")
        );
        let bar = &rows[rows.len() - 2];
        assert_eq!(
            bar,
            "r refresh  j/k scroll  g/G top/bottom  PgUp/PgDn page  1-21 of 44"
        );
        app = key(app, 'G');
        let rows = shown(&app);
        assert!(rows[rows.len() - 2].ends_with("24-44 of 44"), "{rows:#?}");
    }

    #[test]
    fn config_a_document_that_fits_shows_the_keys_without_a_position() {
        let app = loaded((80, 24), many(2));
        let rows = shown(&app);
        assert_eq!(
            rows[rows.len() - 2],
            "r refresh  j/k scroll  g/G top/bottom  PgUp/PgDn page"
        );
    }

    #[test]
    fn config_a_short_terminal_is_drawn_within_its_size() {
        for size in [
            (0, 0),
            (1, 1),
            (3, 2),
            (20, 5),
            (80, 3),
            (10, 24),
            (200, 60),
        ] {
            for snapshot in [many(40), snap(Vec::new(), Vec::new())] {
                let mut app = loaded(size, snapshot);
                for code in ['j', 'G', 'j', 'r'] {
                    app = key(app, code);
                }
                let harness = Harness::from_app(app);
                let text = harness.text();
                let rows: Vec<&str> = text.split('\n').collect();
                if size.1 > 0 {
                    assert_eq!(rows.len(), usize::from(size.1), "{size:?}");
                }
                assert!(
                    rows.iter()
                        .all(|row| row.chars().count() == usize::from(size.0))
                );
            }
        }
    }

    // ---- styles and boundaries ----

    /// The foreground and modifiers of the first cell of the row of `app` that
    /// starts with `prefix`, and of the cell `column` columns along it.
    fn style_at(app: &App, prefix: &str, column: u16) -> Style {
        let harness = Harness::from_app(app.clone());
        let text = harness.text();
        let row = text
            .split('\n')
            .position(|row| row.starts_with(prefix))
            .unwrap_or_else(|| panic!("no row starting {prefix:?}"));
        let cell = &harness.buffer()[(column, u16::try_from(row).expect("row"))];
        cell.style()
    }

    #[test]
    fn config_headings_are_bold_and_a_source_that_is_not_the_default_stands_out() {
        let app = loaded(
            (80, 24),
            snap(
                vec![
                    setting("provider", Some("\"claude\""), Source::ProjectFile),
                    setting("max_attempts", Some("2"), Source::Default),
                ],
                vec![passing("git")],
            ),
        );
        let bold = |style: Style| style.add_modifier.contains(Modifier::BOLD);
        assert!(bold(style_at(&app, "Doctor", 0)));
        assert!(bold(style_at(&app, "Configuration", 0)));
        // The source column starts after the key column and its gap.
        let source = u16::try_from("max_attempts".len() + 1).expect("column");
        assert_eq!(style_at(&app, "provider", source).fg, Some(Color::Cyan));
        assert!(
            style_at(&app, "max_attempts", source)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(
            style_at(&app, "max_attempts", source).fg,
            Some(Color::Reset)
        );
        // The key and the value are not styled like the source.
        assert_eq!(style_at(&app, "provider", 0).fg, Some(Color::Reset));
    }

    #[test]
    fn config_an_unset_value_is_dim_and_a_set_one_is_not() {
        let app = loaded(
            (80, 24),
            snap(
                vec![
                    setting("model", None, Source::Default),
                    setting("provider", Some("\"x\""), Source::Default),
                ],
                Vec::new(),
            ),
        );
        let value = u16::try_from("provider".len() + 1 + SOURCE_WIDTH + 1).expect("column");
        assert!(
            style_at(&app, "model", value)
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert!(
            !style_at(&app, "provider", value)
                .add_modifier
                .contains(Modifier::DIM)
        );
    }

    #[test]
    fn config_the_reason_a_configuration_could_not_load_is_red() {
        let mut app = app_at((80, 24));
        assert!(backfill(&mut app, &|| Err("bad key".to_owned())));
        assert_eq!(
            style_at(&app, "The configuration could not", 0).fg,
            Some(Color::Red)
        );
        assert_eq!(style_at(&app, "bad key", 0).fg, Some(Color::Red));
        assert_eq!(style_at(&app, "Fix the file", 0).fg, Some(Color::Reset));
    }

    #[test]
    fn config_the_value_column_needs_its_minimum_room_or_the_value_moves_under_the_key() {
        let settings = vec![setting("mainline_branch", Some("\"main\""), Source::Env)];
        let value_column = "mainline_branch".len() + GAP.len() + SOURCE_WIDTH + GAP.len();
        let enough = u16::try_from(value_column + MIN_VALUE_WIDTH).expect("width");
        let inline = loaded((enough, 30), snap(settings.clone(), Vec::new()));
        assert!(
            shown(&inline)
                .iter()
                .any(|line| line.starts_with("mainline_branch environment  \"main\""))
        );
        let short = loaded((enough - 1, 30), snap(settings, Vec::new()));
        assert!(shown(&short).iter().any(|line| line == "  \"main\""));
    }

    #[test]
    fn config_a_body_of_one_row_shows_a_line_of_the_document_and_no_key_bar() {
        let app = loaded((80, 2), many(3));
        assert_eq!(
            shown(&app),
            ["9 Configuration and doctor", "Doctor · all 1 checks pass"]
        );
        let scrolled = key(app, 'j');
        assert_eq!(shown(&scrolled)[1], "PASS git: fine");
    }

    #[test]
    fn config_a_body_of_no_rows_draws_nothing_and_scrolls_nowhere() {
        let app = loaded((80, 1), many(40));
        assert_eq!(shown(&app), ["9 Configuration and doctor"]);
        assert_eq!(key(app, 'G').config.top, 40 + 4);
    }
}
