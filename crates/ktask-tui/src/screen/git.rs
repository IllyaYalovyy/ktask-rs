//! The git screen: the repository state of the selected task, where the work
//! happens.
//!
//! It shows, for the task selected on the queue screen: the files the task
//! changed against the commit its attempt began from, the diff of the selected
//! file, the commits the task made, whether the task is published, and how its
//! `HEAD` compares with the remote mainline.
//!
//! The screen keeps two things from the journal and two read from git.
//! [`fold`] keeps, per task, the base commit of its latest attempt and how far
//! publication got, so publication is shown even when the worktree is gone.
//! What git says is a `Snapshot` (the changed files, the commits and the
//! remote comparison) and a `DiffPage` (a window of the selected file's
//! diff). [`fold`] adds nothing to either; it only notes that the journal has
//! moved on, so both are stale and [`wanted`] asks for a fresh read.
//!
//! [`update`](crate::update) does no I/O, so reading is the shell's part, as
//! for the history: after a turn it calls [`backfill`], which does what
//! [`wanted`] asks for. Files and commits are capped ([`FILE_CAP`],
//! [`COMMIT_CAP`]), and a diff is never held whole: [`file_diff`] returns the
//! diff as one string, which is counted and cut to at most [`DIFF_PAGE`] lines
//! around the view and then dropped. Scrolling the diff therefore costs a
//! `git diff`, never a growing page. The comparison uses the remote-tracking
//! ref, so it is as fresh as the last fetch; viewing the screen never touches
//! the network.
//!
//! Only committed work is shown: the worktree must be clean at verification
//! (VISION.md §10), and a task's commits are what publication pushes. A task
//! whose worktree has been removed shows its publication state and says so.
//!
//! Everything git printed may carry text an agent wrote (paths, subjects, the
//! diff), so it is sanitized and cut to one line when it is read.

use crate::app::App;
use crate::keys::{KeyAction, lookup};
use crate::layout::{LayoutPlan, layout_for};
use crate::sanitize::sanitize;
use crate::screen::history::viewport;
use crate::screen::queue::selected_row;
use crate::text::truncate_to_width;
use crate::types::Screen;
use crossterm::event::{KeyCode, KeyEvent};
use ktask_core::{Event, EventKind, TaskId, file_diff, git as run_git};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The most changed files a snapshot lists.
#[cfg(not(test))]
pub const FILE_CAP: usize = 1_000;

/// Smaller under test, so the cap can be reached with a handful of files.
#[cfg(test)]
pub const FILE_CAP: usize = 6;

/// The most commits a snapshot lists.
#[cfg(not(test))]
pub const COMMIT_CAP: usize = 100;

/// Smaller under test, for the same reason.
#[cfg(test)]
pub const COMMIT_CAP: usize = 4;

/// The most diff lines a page holds, however long the diff is.
#[cfg(not(test))]
pub const DIFF_PAGE: usize = 1_024;

/// Smaller under test, so a large diff is a few hundred lines.
#[cfg(test)]
pub const DIFF_PAGE: usize = 64;

/// How many lines a page reaches either side of the line it is centred on.
const HALF_PAGE: usize = DIFF_PAGE / 2;

/// The longest line kept from git's output.
const MAX_LINE_CHARS: usize = 512;

/// What replaces the line breaks inside a line of git's output.
const BREAK: &str = " ⏎ ";

/// How many characters of a commit hash are shown.
const SHORT_SHA: usize = 7;

/// The marker column that points at the selected file.
const MARKER: &str = "> ";

/// The marker column of a file that is not selected.
const NO_MARKER: &str = "  ";

/// What separates the parts of a line.
const SEPARATOR: &str = " · ";

/// What separates the entries of the key bar.
const BAR_GAP: &str = "  ";

/// The fewest body rows that get the full layout: three status lines, three
/// section headings, the key bar and a row for each section.
const FULL_MIN_HEIGHT: usize = 10;

/// The status lines at the top: the heading, the publication and the remote.
const STATUS_LINES: usize = 3;

/// What the body shows when the queue has no task.
const NO_TASK: &str = "No task is selected";

/// What the body shows before the task's first attempt has a base commit.
const NO_BASE: &str = "The task has not started an attempt, so it has no changes yet";

/// What the body shows before the snapshot has been read.
const NOT_LOADED: &str = "Reading the repository…";

/// What the files section shows when the task changed nothing.
const NO_FILES: &str = "No changed files";

/// What the commits section shows when the task made no commit.
const NO_COMMITS: &str = "No commits";

/// What the diff section shows before the page has been read.
const DIFF_NOT_LOADED: &str = "Reading the diff…";

/// What the diff section shows for a file whose diff is empty.
const NO_DIFF: &str = "No differences";

/// Where a task's repository is, as far as the interface can tell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    /// The task's worktree; the diff is taken there.
    pub worktree: PathBuf,
    /// The remote the task publishes to.
    pub remote: String,
    /// The mainline branch on that remote.
    pub branch: String,
}

/// How far publication got, from the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Publication {
    /// The commit is being pushed and checked against the remote.
    Publishing {
        /// The commit being published.
        candidate: String,
    },
    /// The remote's tip was observed to be the commit.
    Verified {
        /// The published commit.
        commit: String,
        /// The commit as observed on the remote.
        remote: String,
    },
}

/// What the journal says about a task that this screen uses.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct TaskFacts {
    /// The commit the latest attempt began from.
    base: Option<String>,
    /// How far publication got; `None` before it began.
    publication: Option<Publication>,
}

/// How a file differs from the base.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
}

impl FileStatus {
    fn letter(self) -> char {
        match self {
            FileStatus::Added => 'A',
            FileStatus::Modified => 'M',
            FileStatus::Deleted => 'D',
            FileStatus::Renamed => 'R',
        }
    }

    fn style(self) -> Style {
        match self {
            FileStatus::Added => Style::new().fg(Color::Green),
            FileStatus::Modified => Style::new().fg(Color::Yellow),
            FileStatus::Deleted => Style::new().fg(Color::Red),
            FileStatus::Renamed => Style::new().fg(Color::Cyan),
        }
    }
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileEntry {
    status: FileStatus,
    /// The path as git printed it, which is what the diff is asked for.
    path: String,
    /// Where a renamed file was.
    from: Option<String>,
    binary: bool,
}

/// One commit the task made.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Commit {
    sha: String,
    subject: String,
}

/// How the task's `HEAD` compares with the remote mainline.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Remote {
    /// The commits only `HEAD` has, and the commits only the remote has.
    Compared {
        name: String,
        ahead: usize,
        behind: usize,
    },
    /// The comparison could not be made, and why.
    Unavailable { name: String, reason: String },
}

/// What git said about the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Contents {
    files: Vec<FileEntry>,
    /// How many files changed; more than `files` holds past [`FILE_CAP`].
    files_total: usize,
    commits: Vec<Commit>,
    /// How many commits the task made; more than `commits` holds past
    /// [`COMMIT_CAP`].
    commits_total: usize,
    remote: Remote,
}

/// A read of one task's repository, or why there is none.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Snapshot {
    task: TaskId,
    /// [`GitView::revision`] when it was read.
    revision: u64,
    contents: Result<Contents, String>,
}

/// A run of consecutive lines of one file's diff.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DiffPage {
    task: TaskId,
    path: String,
    /// [`GitView::revision`] when it was read.
    revision: u64,
    /// The number of the first line held.
    start: usize,
    /// How many lines the whole diff has.
    total: usize,
    lines: Vec<String>,
    /// Why the diff could not be read, in place of lines.
    failure: Option<String>,
}

impl DiffPage {
    /// Where the page ends: the number after its last line.
    fn end(&self) -> usize {
        self.start + self.lines.len()
    }
}

/// What the git screen keeps: what the journal said, the selected file and
/// diff position, and what git said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitView {
    /// How many events have been folded that the screen may show.
    revision: u64,
    tasks: BTreeMap<TaskId, TaskFacts>,
    /// The selected file, as an index into the snapshot's files.
    selected: usize,
    /// The first diff line the view shows.
    top: usize,
    snapshot: Option<Snapshot>,
    diff: Option<DiffPage>,
}

impl GitView {
    /// How many lines of diff are held in memory.
    #[must_use]
    pub fn held(&self) -> usize {
        self.diff.as_ref().map_or(0, |page| page.lines.len())
    }

    /// How many lines the selected file's diff has, as far as the page read
    /// says.
    #[must_use]
    pub fn diff_len(&self) -> usize {
        self.diff.as_ref().map_or(0, |page| page.total)
    }

    /// How far publication of `task` got.
    #[must_use]
    pub fn publication(&self, task: TaskId) -> Option<&Publication> {
        self.tasks.get(&task)?.publication.as_ref()
    }

    /// How many files the snapshot lists, or 0 before it is read or when it
    /// failed.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.contents().map_or(0, |contents| contents.files.len())
    }

    /// The index of the selected file within the files listed.
    #[must_use]
    pub fn selected_file(&self) -> usize {
        self.selected.min(self.file_count().saturating_sub(1))
    }

    fn contents(&self) -> Option<&Contents> {
        self.snapshot.as_ref()?.contents.as_ref().ok()
    }

    fn selected_entry(&self) -> Option<&FileEntry> {
        self.contents()?.files.get(self.selected_file())
    }
}

/// `text` sanitized, on one line and no longer than [`MAX_LINE_CHARS`].
fn one_line(text: &str) -> String {
    let clean = sanitize(text);
    let joined = clean.trim_end_matches('\n').replace('\n', BREAK);
    match joined.char_indices().nth(MAX_LINE_CHARS) {
        Some((end, _)) => format!("{}…", joined.get(..end).unwrap_or_default()),
        None => joined,
    }
}

/// The first `SHORT_SHA` characters of `sha`.
fn short(sha: &str) -> &str {
    sha.get(..SHORT_SHA).unwrap_or(sha)
}

/// The changed files in `name_status`, the output of `git diff --name-status
/// -z`, with the ones `numstat` (`--numstat -z`) reports no line counts for
/// marked binary.
fn parse_files(name_status: &str, numstat: &str) -> Vec<FileEntry> {
    let mut binary = std::collections::HashSet::new();
    let mut tokens = numstat.split('\0').filter(|token| !token.is_empty());
    while let Some(token) = tokens.next() {
        let mut fields = token.splitn(3, '\t');
        let (Some(added), Some(deleted), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // A rename has no path in the counts' own field: both follow.
        let path = if path.is_empty() {
            tokens.next();
            tokens.next().unwrap_or_default()
        } else {
            path
        };
        if added == "-" && deleted == "-" {
            binary.insert(path.to_owned());
        }
    }
    let mut files = Vec::new();
    let mut tokens = name_status.split('\0').filter(|token| !token.is_empty());
    while let Some(code) = tokens.next() {
        let status = match code.chars().next() {
            Some('A') => FileStatus::Added,
            Some('D') => FileStatus::Deleted,
            Some('R' | 'C') => FileStatus::Renamed,
            _ => FileStatus::Modified,
        };
        let (from, path) = if status == FileStatus::Renamed {
            (tokens.next(), tokens.next())
        } else {
            (None, tokens.next())
        };
        let Some(path) = path else { break };
        files.push(FileEntry {
            status,
            binary: binary.contains(path),
            from: from.map(str::to_owned),
            path: path.to_owned(),
        });
    }
    files
}

/// The commits in `log`, the output of `git log --format=%H%x1f%s`.
fn parse_commits(log: &str) -> Vec<Commit> {
    log.lines()
        .filter_map(|line| {
            let (sha, subject) = line.split_once('\u{1f}')?;
            Some(Commit {
                sha: sha.to_owned(),
                subject: one_line(subject),
            })
        })
        .collect()
}

/// How `HEAD` compares with `remote`'s `branch`, by the remote-tracking ref
/// as the last fetch left it.
fn compare(dir: &Path, remote: &str, branch: &str) -> Remote {
    let name = format!("{remote}/{branch}");
    let range = format!("HEAD...refs/remotes/{name}");
    let counts = run_git(dir, &["rev-list", "--left-right", "--count", &range]);
    let unavailable = |reason: String| Remote::Unavailable {
        name: name.clone(),
        reason,
    };
    match counts {
        Ok(text) => {
            let mut numbers = text.split_whitespace().map(str::parse::<usize>);
            match (numbers.next(), numbers.next()) {
                (Some(Ok(ahead)), Some(Ok(behind))) => Remote::Compared {
                    name: name.clone(),
                    ahead,
                    behind,
                },
                _ => unavailable(format!("unexpected output: {}", one_line(&text))),
            }
        }
        Err(ktask_core::Error::Git { stderr, .. }) => unavailable(one_line(&stderr)),
        Err(other) => unavailable(one_line(&other.to_string())),
    }
}

/// Reads the changed files, the commits and the remote comparison of the
/// worktree of `repo` against `base`.
fn read_contents(repo: &Repo, base: &str) -> ktask_core::Result<Contents> {
    let dir = repo.worktree.as_path();
    let names = run_git(
        dir,
        &[
            "diff",
            "--name-status",
            "-z",
            "--find-renames",
            base,
            "HEAD",
        ],
    )?;
    let counts = run_git(
        dir,
        &["diff", "--numstat", "-z", "--find-renames", base, "HEAD"],
    )?;
    let mut files = parse_files(&names, &counts);
    let files_total = files.len();
    files.truncate(FILE_CAP);
    let range = format!("{base}..HEAD");
    let limit = COMMIT_CAP.to_string();
    let commits = parse_commits(&run_git(
        dir,
        &["log", "--format=%H%x1f%s", "-n", &limit, &range],
    )?);
    let commits_total = run_git(dir, &["rev-list", "--count", &range])?
        .parse()
        .unwrap_or(commits.len());
    Ok(Contents {
        files,
        files_total,
        commits,
        commits_total,
        remote: compare(dir, &repo.remote, &repo.branch),
    })
}

/// Reads the diff of `path` and keeps the [`DIFF_PAGE`] lines around `top`.
///
/// [`file_diff`] returns the diff as one string; it is counted, cut to the
/// window and dropped here, so no more than the window is kept.
fn read_diff(
    repo: &Repo,
    base: &str,
    path: &str,
    top: usize,
) -> ktask_core::Result<(usize, usize, Vec<String>)> {
    let text = file_diff(&repo.worktree, base, Path::new(path))?;
    let total = text.lines().count();
    let start = top
        .saturating_sub(HALF_PAGE)
        .min(total.saturating_sub(DIFF_PAGE));
    let lines = text
        .lines()
        .skip(start)
        .take(DIFF_PAGE)
        .map(one_line)
        .collect();
    Ok((start, total, lines))
}

/// What [`wanted`] asks the shell to read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// The changed files, commits and remote comparison of a task.
    Snapshot {
        /// The task to read.
        task: TaskId,
    },
    /// A window of the diff of one of the task's files.
    Diff {
        /// The task the file belongs to.
        task: TaskId,
        /// The file, as the snapshot lists it.
        path: String,
        /// The line the view is on; the page is centred on it.
        top: usize,
    },
}

/// What the plan of the body gives each part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    /// Whether the section headings, the diff and the key bar are drawn.
    full: bool,
    files: usize,
    commits: usize,
    diff: usize,
}

/// The rows of each part in a body `height` rows tall, for a task with
/// `files` changed files and `commits` commits.
///
/// A body too short for every part is compact: the status lines and the file
/// list. Otherwise each list gets a share, at least a row (which is where
/// "no files" is said), and the diff gets the rest.
fn plan(height: usize, files: usize, commits: usize) -> Plan {
    if height < FULL_MIN_HEIGHT {
        return Plan {
            full: false,
            files: height.saturating_sub(STATUS_LINES),
            commits: 0,
            diff: 0,
        };
    }
    // The status lines, three headings and the key bar.
    let budget = height - STATUS_LINES - 4;
    let files = files.clamp(1, (budget / 3).max(1));
    let commits = commits.clamp(1, (budget / 5).max(1));
    Plan {
        full: true,
        files,
        commits,
        diff: budget - files - commits,
    }
}

/// The task the git screen shows: the one selected on the queue screen.
fn task_of(app: &App) -> Option<TaskId> {
    Some(app.tasks.get(selected_row(app)?)?.id)
}

/// The rows the layout of `app` gives the body.
fn body_height(app: &App) -> usize {
    let (columns, rows) = app.size;
    usize::from(layout_for(Rect::new(0, 0, columns, rows)).body.height)
}

/// How many diff lines the view of `app` has room for; at most [`HALF_PAGE`].
fn diff_height(app: &App) -> usize {
    let commits = app.git.contents().map_or(0, |c| c.commits.len());
    plan(body_height(app), app.git.file_count(), commits)
        .diff
        .min(HALF_PAGE)
}

/// The diff line numbers the view shows: `height` lines from `top`, never
/// past the end.
fn diff_view(top: usize, total: usize, height: usize) -> std::ops::Range<usize> {
    let top = top.min(total.saturating_sub(height));
    top..(top + height).min(total)
}

/// What the shell should read to make the screen show what git says, or
/// `None` when it already does, when this screen is not showing, or when
/// there is nothing to read (no task, no attempt yet, no file, a read that
/// failed).
///
/// The snapshot is wanted when none has been read, when it is of another
/// task, and when the journal has moved on since it was read. The diff is
/// wanted for the same reasons, when the selected file is not the one it is
/// of, and when the view has scrolled out of it.
#[must_use]
pub fn wanted(app: &App) -> Option<Request> {
    if app.screen != Screen::Git {
        return None;
    }
    let task = task_of(app)?;
    let view = &app.git;
    view.tasks.get(&task)?.base.as_ref()?;
    let current = |revision: u64| revision == view.revision;
    let fresh = view
        .snapshot
        .as_ref()
        .is_some_and(|s| s.task == task && current(s.revision));
    if !fresh {
        return Some(Request::Snapshot { task });
    }
    let path = view.selected_entry()?.path.clone();
    let height = diff_height(app);
    if height == 0 {
        return None;
    }
    let request = Request::Diff {
        task,
        path: path.clone(),
        top: view.top,
    };
    let Some(page) = view
        .diff
        .as_ref()
        .filter(|p| p.task == task && p.path == path && current(p.revision))
    else {
        return Some(request);
    };
    let shown = diff_view(view.top, page.total, height);
    let covered = page.failure.is_some() || (page.start <= shown.start && shown.end <= page.end());
    (!covered).then_some(request)
}

/// The reason given when a task has no worktree to read.
const NO_WORKTREE: &str = "The task's worktree is gone, so its changes cannot be shown";

/// Does what [`wanted`] asks for, reading from the repository `resolve` finds
/// for the task, and stores the result, replacing the previous one. Returns
/// whether it read.
///
/// The shell calls this after a turn; it is the one place this screen does
/// I/O. A read that fails is stored as the reason it failed and shown, so it
/// is not asked for again until the journal moves on.
pub fn backfill(app: &mut App, resolve: &dyn Fn(TaskId) -> Option<Repo>) -> bool {
    let Some(request) = wanted(app) else {
        return false;
    };
    let revision = app.git.revision;
    match request {
        Request::Snapshot { task } => {
            let base = app
                .git
                .tasks
                .get(&task)
                .and_then(|facts| facts.base.clone());
            let contents = match (resolve(task), base) {
                (Some(repo), Some(base)) => {
                    read_contents(&repo, &base).map_err(|err| one_line(&err.to_string()))
                }
                _ => Err(NO_WORKTREE.to_owned()),
            };
            if app.git.snapshot.as_ref().is_none_or(|s| s.task != task) {
                app.git.selected = 0;
                app.git.top = 0;
            }
            app.git.snapshot = Some(Snapshot {
                task,
                revision,
                contents,
            });
        }
        Request::Diff { task, path, top } => {
            let base = app
                .git
                .tasks
                .get(&task)
                .and_then(|facts| facts.base.clone());
            let read = match (resolve(task), base) {
                (Some(repo), Some(base)) => {
                    read_diff(&repo, &base, &path, top).map_err(|err| one_line(&err.to_string()))
                }
                _ => Err(NO_WORKTREE.to_owned()),
            };
            let (start, total, lines, failure) = match read {
                Ok((start, total, lines)) => (start, total, lines, None),
                Err(reason) => (0, 0, Vec::new(), Some(reason)),
            };
            app.git.diff = Some(DiffPage {
                task,
                path,
                revision,
                start,
                total,
                lines,
                failure,
            });
        }
    }
    true
}

/// Notes what the journal says about publication and the base commit, and
/// that git's state may have moved on. The commits and files themselves are
/// not kept: the next read finds them in the repository.
pub fn fold(app: &mut App, event: &Event) {
    if matches!(event.kind, EventKind::AgentOutput { .. }) {
        return;
    }
    app.git.revision = app.git.revision.wrapping_add(1);
    let Some(task) = event.task_id else { return };
    match &event.kind {
        EventKind::AttemptStarted { base_sha, .. } => {
            let facts = app.git.tasks.entry(task).or_default();
            facts.base = Some(base_sha.clone());
            facts.publication = None;
        }
        EventKind::RetryStarted { .. } => {
            app.git.tasks.entry(task).or_default().publication = None;
        }
        EventKind::PublishStarted { candidate_sha, .. } => {
            app.git.tasks.entry(task).or_default().publication = Some(Publication::Publishing {
                candidate: candidate_sha.clone(),
            });
        }
        EventKind::PublishVerified { commit, remote_sha } => {
            app.git.tasks.entry(task).or_default().publication = Some(Publication::Verified {
                commit: commit.clone(),
                remote: remote_sha.clone(),
            });
        }
        _ => {}
    }
}

/// Whether the git screen has the keys: it is showing and nothing is over it.
fn has_focus(app: &App) -> bool {
    app.screen == Screen::Git && app.overlay.is_none()
}

/// Handles the git screen's keys: `j`, `k`, the arrows, `g` and `G` select a
/// file, the previous, the next, the first or the last, and `PageUp` and
/// `PageDown` scroll the diff of the selected file a screenful. Does nothing
/// on other screens or under an overlay.
///
/// Selecting stops at the ends rather than wrapping, and shows the new file's
/// diff from its first line.
pub fn handle_key(app: &mut App, key: &KeyEvent) {
    if !has_focus(app) {
        return;
    }
    let files = app.git.file_count();
    if files == 0 {
        return;
    }
    let action = lookup(app.screen, key).map(|binding| binding.action);
    let current = app.git.selected_file();
    let target = match action {
        Some(KeyAction::MoveUp) => current.saturating_sub(1),
        Some(KeyAction::MoveDown) => (current + 1).min(files - 1),
        Some(KeyAction::First) => 0,
        Some(KeyAction::Last) => files - 1,
        _ => return scroll(app, key.code),
    };
    if target != current {
        app.git.selected = target;
        app.git.top = 0;
    }
}

/// Scrolls the diff a screenful for `PageUp` and `PageDown`, within its ends.
fn scroll(app: &mut App, code: KeyCode) {
    let height = diff_height(app);
    let total = app.git.diff_len();
    let last_top = total.saturating_sub(height);
    let current = app.git.top.min(last_top);
    app.git.top = match code {
        KeyCode::PageUp => current.saturating_sub(height.max(1)),
        KeyCode::PageDown => current.saturating_add(height.max(1)).min(last_top),
        _ => return,
    };
}

/// The style a line of a diff is drawn in.
fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff ") {
        Style::new().add_modifier(Modifier::BOLD)
    } else if line.starts_with('+') {
        Style::new().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::new().fg(Color::Red)
    } else if line.starts_with("@@") {
        Style::new().fg(Color::Cyan)
    } else {
        Style::new()
    }
}

/// The text of the publication line: how far publication of the task got.
fn publication_text(publication: Option<&Publication>) -> String {
    match publication {
        None => "Publication: not published".to_owned(),
        Some(Publication::Publishing { candidate }) => {
            format!("Publication: publishing {}", short(candidate))
        }
        Some(Publication::Verified { commit, remote }) => format!(
            "Publication: verified{SEPARATOR}commit {}{SEPARATOR}remote {}",
            short(commit),
            short(remote)
        ),
    }
}

/// The text of the line that compares `HEAD` with the remote mainline.
fn remote_text(remote: &Remote) -> String {
    match remote {
        Remote::Compared {
            name,
            ahead: 0,
            behind: 0,
        } => format!("Remote {name}: identical to HEAD (as of the last fetch)"),
        Remote::Compared {
            name,
            ahead,
            behind,
        } => {
            format!("Remote {name}: HEAD is {ahead} ahead, {behind} behind (as of the last fetch)")
        }
        Remote::Unavailable { name, reason } => format!("Remote {name}: unavailable, {reason}"),
    }
}

/// The text of a file's row after its marker.
fn file_text(file: &FileEntry) -> String {
    let path = match &file.from {
        Some(from) => format!("{} → {}", one_line(from), one_line(&file.path)),
        None => one_line(&file.path),
    };
    let binary = if file.binary { " (binary)" } else { "" };
    format!("{} {path}{binary}", file.status.letter())
}

/// `count` of `total` when the list was cut, otherwise `total`.
fn counted(count: usize, total: usize) -> String {
    if count < total {
        format!("{count} of {total}")
    } else {
        total.to_string()
    }
}

/// A line of `text` in `style`, cut at `width`.
fn styled(text: &str, style: Style, width: usize) -> Line<'static> {
    Line::styled(truncate_to_width(text, width), style)
}

/// The three status lines: the heading, the publication and `third`, which is
/// the remote comparison or the message that stands in for it.
fn status_lines(
    task: TaskId,
    view: &GitView,
    contents: Option<&Contents>,
    third: &str,
    width: usize,
) -> Vec<Line<'static>> {
    let facts = view.tasks.get(&task);
    let heading = match contents {
        Some(contents) => format!(
            "Git{SEPARATOR}task {task}{SEPARATOR}{} files{SEPARATOR}{} commits",
            counted(contents.files.len(), contents.files_total),
            counted(contents.commits.len(), contents.commits_total)
        ),
        None => format!("Git{SEPARATOR}task {task}"),
    };
    vec![
        styled(&heading, Style::new().add_modifier(Modifier::BOLD), width),
        styled(
            &publication_text(facts.and_then(|f| f.publication.as_ref())),
            Style::new(),
            width,
        ),
        styled(third, Style::new().add_modifier(Modifier::DIM), width),
    ]
}

/// The rows of the files section: the window of files around the selected
/// one, or the message that there are none.
fn file_rows(view: &GitView, contents: &Contents, rows: usize, width: usize) -> Vec<Line<'static>> {
    if contents.files.is_empty() {
        return vec![styled(
            NO_FILES,
            Style::new().add_modifier(Modifier::DIM),
            width,
        )];
    }
    let selected = view.selected_file();
    viewport(Some(selected), contents.files.len(), rows)
        .filter_map(|at| {
            let file = contents.files.get(at)?;
            let marker = if at == selected { MARKER } else { NO_MARKER };
            let style = if at == selected {
                file.status.style().add_modifier(Modifier::BOLD)
            } else {
                file.status.style()
            };
            Some(styled(
                &format!("{marker}{}", file_text(file)),
                style,
                width,
            ))
        })
        .collect()
}

/// The rows of the commits section: the newest commits, or the message that
/// there are none.
fn commit_rows(contents: &Contents, rows: usize, width: usize) -> Vec<Line<'static>> {
    if contents.commits.is_empty() {
        return vec![styled(
            NO_COMMITS,
            Style::new().add_modifier(Modifier::DIM),
            width,
        )];
    }
    contents
        .commits
        .iter()
        .take(rows)
        .map(|commit| {
            styled(
                &format!("{NO_MARKER}{} {}", short(&commit.sha), commit.subject),
                Style::new(),
                width,
            )
        })
        .collect()
}

/// The heading and rows of the diff section, `rows` rows in all.
fn diff_section(
    view: &GitView,
    task: TaskId,
    file: &FileEntry,
    rows: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let dim = Style::new().add_modifier(Modifier::DIM);
    let title = format!("Diff{SEPARATOR}{}", one_line(&file.path));
    let page = view
        .diff
        .as_ref()
        .filter(|p| p.task == task && p.path == file.path);
    let Some(page) = page else {
        return vec![
            styled(&title, bold, width),
            styled(DIFF_NOT_LOADED, dim, width),
        ];
    };
    if let Some(reason) = &page.failure {
        return vec![
            styled(&title, bold, width),
            styled(reason, Style::new().fg(Color::Red), width),
        ];
    }
    if page.total == 0 {
        return vec![styled(&title, bold, width), styled(NO_DIFF, dim, width)];
    }
    let shown = diff_view(view.top, page.total, rows.saturating_sub(1).min(HALF_PAGE));
    let heading = format!(
        "{title}{SEPARATOR}lines {}–{} of {}",
        shown.start + 1,
        shown.end,
        page.total
    );
    let mut lines = vec![styled(&heading, bold, width)];
    lines.extend(shown.filter_map(|at| {
        let line = page.lines.get(at.checked_sub(page.start)?)?;
        Some(styled(line, diff_style(line), width))
    }));
    lines
}

/// The line of keys shown under the diff.
fn key_bar() -> String {
    [
        "j/k file",
        "g/G first/last file",
        "PgUp/PgDn scroll the diff",
    ]
    .join(BAR_GAP)
}

/// Every line of the body, `height` rows tall and `width` columns wide.
fn body_lines(app: &App, height: usize, width: usize) -> Vec<Line<'static>> {
    let view = &app.git;
    let dim = Style::new().add_modifier(Modifier::DIM);
    let Some(task) = task_of(app) else {
        return vec![
            styled("Git", Style::new().add_modifier(Modifier::BOLD), width),
            styled(NO_TASK, dim, width),
        ];
    };
    let has_base = view.tasks.get(&task).is_some_and(|f| f.base.is_some());
    let snapshot = view.snapshot.as_ref().filter(|s| s.task == task);
    let ready = match (has_base, snapshot) {
        (false, _) => Err(NO_BASE.to_owned()),
        (true, None) => Err(NOT_LOADED.to_owned()),
        (true, Some(snapshot)) => snapshot.contents.as_ref().map_err(String::clone),
    };
    let contents = match ready {
        Ok(contents) => contents,
        Err(message) => return status_lines(task, view, None, &message, width),
    };
    let plan = plan(height, contents.files.len(), contents.commits.len());
    let mut lines = status_lines(
        task,
        view,
        Some(contents),
        &remote_text(&contents.remote),
        width,
    );
    if !plan.full {
        lines.extend(file_rows(view, contents, plan.files, width));
        return lines;
    }
    let bold = Style::new().add_modifier(Modifier::BOLD);
    lines.push(styled("Changed files", bold, width));
    lines.extend(file_rows(view, contents, plan.files, width));
    lines.push(styled("Commits", bold, width));
    lines.extend(commit_rows(contents, plan.commits, width));
    let diff = match view.selected_entry() {
        Some(file) => diff_section(view, task, file, plan.diff + 1, width),
        None => vec![styled("Diff", bold, width)],
    };
    lines.extend(diff);
    lines.resize_with(height.saturating_sub(1), Line::default);
    lines.push(styled(&key_bar(), Style::new(), width));
    lines
}

/// Draws the git screen into the body of `plan`: the task's changed files,
/// the diff of the selected one, its commits, its publication state and the
/// comparison with the remote mainline. Where the body is too short for all of
/// it, the status lines and the file list are drawn.
pub fn render(app: &App, plan: &LayoutPlan, frame: &mut Frame<'_>) {
    let body = plan.body;
    if body.is_empty() {
        return;
    }
    let lines = body_lines(app, usize::from(body.height), usize::from(body.width));
    frame.render_widget(Paragraph::new(lines), body);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::update;
    use crate::event::AppEvent;
    use crate::testing::Harness;
    use crate::types::TaskView;
    use crossterm::event::KeyModifiers;
    use ktask_core::{AttemptId, EventSeq, Stream};
    use time::macros::datetime;

    // ---- fixtures ----

    fn g(dir: &Path, args: &[&str]) -> String {
        run_git(dir, args).unwrap_or_else(|err| panic!("git {args:?}: {err}"))
    }

    /// A clone with a bare remote, and the commit the task's work starts from.
    struct Fixture {
        dir: tempfile::TempDir,
        work: PathBuf,
        base: String,
    }

    fn commit(work: &Path, message: &str) {
        g(work, &["add", "-A"]);
        g(
            work,
            &[
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                message,
            ],
        );
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        let remote = root.join("remote.git");
        let work = root.join("work");
        g(root, &["init", "--quiet", "--bare", "remote.git"]);
        g(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        g(root, &["init", "--quiet", "work"]);
        g(&work, &["config", "user.email", "test@example.com"]);
        g(&work, &["config", "user.name", "Test"]);
        g(&work, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        g(
            &work,
            &["remote", "add", "origin", &remote.to_string_lossy()],
        );
        std::fs::write(work.join("modified.txt"), "one\ntwo\nthree\n").expect("write");
        std::fs::write(work.join("deleted.txt"), "going\naway\n").expect("write");
        std::fs::write(work.join("image.bin"), [0u8, 1, 2, 3]).expect("write");
        commit(&work, "Seed");
        g(&work, &["push", "--quiet", "origin", "main"]);
        let base = g(&work, &["rev-parse", "HEAD"]);
        Fixture { dir, work, base }
    }

    impl Fixture {
        fn repo(&self) -> Repo {
            Repo {
                worktree: self.work.clone(),
                remote: "origin".into(),
                branch: "main".into(),
            }
        }

        fn write(&self, name: &str, contents: &str) {
            std::fs::write(self.work.join(name), contents).expect("write");
        }

        /// The task's work: an added, a modified, a deleted and a binary file,
        /// in two commits.
        fn change_everything(&self) {
            self.write("added.txt", "brand new\nfile\n");
            self.write("modified.txt", "one\n2\nthree\nfour\n");
            commit(&self.work, "Add added.txt, edit modified.txt");
            std::fs::remove_file(self.work.join("deleted.txt")).expect("remove");
            std::fs::write(self.work.join("image.bin"), [0u8, 9, 9, 9]).expect("write");
            commit(&self.work, "Delete deleted.txt, change image.bin");
        }

        fn head(&self) -> String {
            g(&self.work, &["rev-parse", "HEAD"])
        }

        /// A second clone of the remote, to move mainline from.
        fn other_clone(&self) -> PathBuf {
            let other = self.dir.path().join("other");
            let remote = self.dir.path().join("remote.git");
            g(
                self.dir.path(),
                &["clone", "--quiet", &remote.to_string_lossy(), "other"],
            );
            g(&other, &["config", "user.email", "other@example.com"]);
            g(&other, &["config", "user.name", "Other"]);
            other
        }
    }

    fn event(task: Option<u32>, kind: EventKind) -> Event {
        Event {
            seq: EventSeq::new(1),
            ts: datetime!(2026-09-23 10:30:05 UTC),
            task_id: task.map(TaskId::new),
            kind,
        }
    }

    fn attempt(task: u32, base: &str) -> Event {
        event(
            Some(task),
            EventKind::AttemptStarted {
                attempt: AttemptId::new(1),
                protocol: "tdd".into(),
                pid: 7,
                base_sha: base.into(),
            },
        )
    }

    fn task_view(id: u32) -> TaskView {
        TaskView {
            id: TaskId::new(id),
            title: format!("Task number {id}"),
            state: "Queued".into(),
            protocol: String::new(),
            phase: None,
            attempts: 0,
            elapsed: None,
        }
    }

    fn app_on_git(size: (u16, u16), tasks: u32) -> App {
        let mut app = App {
            screen: Screen::Git,
            ..App::new(size)
        };
        app.tasks = (1..=tasks).map(task_view).collect();
        app
    }

    fn feed(app: App, events: &[Event]) -> App {
        events
            .iter()
            .cloned()
            .fold(app, |app, event| update(app, AppEvent::Core(event)))
    }

    fn press(app: App, code: KeyCode) -> App {
        update(app, AppEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn key(app: App, c: char) -> App {
        press(app, KeyCode::Char(c))
    }

    /// Reads until [`wanted`] has nothing left to ask for.
    fn settle(app: &mut App, repo: &Repo) {
        for _ in 0..8 {
            if !backfill(app, &|_| Some(repo.clone())) {
                return;
            }
        }
        panic!("the screen kept asking for reads: {:?}", wanted(app));
    }

    /// An app on the git screen for task 1, whose attempt began at the
    /// fixture's base, with everything read.
    fn loaded(fixture: &Fixture, size: (u16, u16)) -> App {
        let mut app = feed(app_on_git(size, 1), &[attempt(1, &fixture.base)]);
        settle(&mut app, &fixture.repo());
        app
    }

    fn screen(app: &App) -> Vec<String> {
        Harness::from_app(app.clone())
            .text()
            .lines()
            .map(|line| line.trim_end().to_owned())
            .collect()
    }

    /// The body of the screen: everything between the header and the footer,
    /// which a terminal smaller than the full layout does not draw.
    fn body(app: &App) -> Vec<String> {
        let lines = screen(app);
        let full =
            app.size.0 >= crate::layout::FULL_WIDTH && app.size.1 >= crate::layout::FULL_HEIGHT;
        let end = lines.len().saturating_sub(usize::from(full));
        lines.get(1..end).unwrap_or_default().to_vec()
    }

    /// [`body`] without the blank rows that pad it to the key bar.
    fn content(app: &App) -> Vec<String> {
        let mut lines = body(app);
        while lines.last().is_some_and(String::is_empty) {
            lines.pop();
        }
        lines
    }

    /// The commits the fixture's `change_everything` made, newest first, as
    /// the screen lists them.
    fn commit_lines(f: &Fixture) -> [String; 2] {
        let newest = g(&f.work, &["rev-parse", "--short=7", "HEAD"]);
        let older = g(&f.work, &["rev-parse", "--short=7", "HEAD~1"]);
        [
            format!("  {newest} Delete deleted.txt, change image.bin"),
            format!("  {older} Add added.txt, edit modified.txt"),
        ]
    }

    /// The full body of the screen at 80x24 with the task's four changed
    /// files, `selected` of them selected, and `diff` the diff pane's rows.
    fn expected_body(f: &Fixture, selected: usize, diff: &[&str]) -> Vec<String> {
        let files = [
            "A added.txt",
            "D deleted.txt",
            "M image.bin (binary)",
            "M modified.txt",
        ];
        let mut lines = vec![
            "Git · task 1 · 4 files · 2 commits".to_owned(),
            "Publication: not published".to_owned(),
            "Remote origin/main: HEAD is 2 ahead, 0 behind (as of the last fetch)".to_owned(),
            "Changed files".to_owned(),
        ];
        for (at, file) in files.iter().enumerate() {
            let marker = if at == selected { "> " } else { "  " };
            lines.push(format!("{marker}{file}"));
        }
        lines.push("Commits".to_owned());
        lines.extend(commit_lines(f));
        lines.extend(diff.iter().map(|line| (*line).to_owned()));
        lines.resize(21, String::new());
        lines.push("j/k file  g/G first/last file  PgUp/PgDn scroll the diff".to_owned());
        lines
    }

    // ---- what git said, and how it is drawn ----

    #[test]
    fn git_screen_shows_an_added_file_and_its_diff() {
        let f = fixture();
        f.change_everything();
        let app = loaded(&f, (80, 24));
        assert_eq!(
            body(&app),
            expected_body(
                &f,
                0,
                &[
                    "Diff · added.txt · lines 1–8 of 8",
                    "diff --git a/added.txt b/added.txt",
                    "new file mode 100644",
                    "index 0000000..39963cd",
                    "--- /dev/null",
                    "+++ b/added.txt",
                    "@@ -0,0 +1,2 @@",
                    "+brand new",
                    "+file",
                ]
            )
        );
        assert_eq!(screen(&app).first().map(String::as_str), Some("8 Git"));
    }

    #[test]
    fn git_screen_shows_a_deleted_file_and_its_diff() {
        let f = fixture();
        f.change_everything();
        let app = key(loaded(&f, (80, 24)), 'j');
        let mut app = app;
        settle(&mut app, &f.repo());
        assert_eq!(
            body(&app),
            expected_body(
                &f,
                1,
                &[
                    "Diff · deleted.txt · lines 1–8 of 8",
                    "diff --git a/deleted.txt b/deleted.txt",
                    "deleted file mode 100644",
                    "index 8334ea1..0000000",
                    "--- a/deleted.txt",
                    "+++ /dev/null",
                    "@@ -1,2 +0,0 @@",
                    "-going",
                    "-away",
                ]
            )
        );
    }

    #[test]
    fn git_screen_shows_a_binary_file_by_name_without_its_bytes() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 24));
        app = key(key(app, 'j'), 'j');
        settle(&mut app, &f.repo());
        assert_eq!(
            body(&app),
            expected_body(
                &f,
                2,
                &[
                    "Diff · image.bin · lines 1–3 of 3",
                    "diff --git a/image.bin b/image.bin",
                    "index eaf36c1..75218e8 100644",
                    "Binary files a/image.bin and b/image.bin differ",
                ]
            )
        );
    }

    #[test]
    fn git_screen_shows_a_modified_file_and_its_diff() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 24));
        app = key(app, 'G');
        settle(&mut app, &f.repo());
        assert_eq!(
            body(&app),
            expected_body(
                &f,
                3,
                &[
                    "Diff · modified.txt · lines 1–9 of 10",
                    "diff --git a/modified.txt b/modified.txt",
                    "index 4cb29ea..ea14db2 100644",
                    "--- a/modified.txt",
                    "+++ b/modified.txt",
                    "@@ -1,3 +1,4 @@",
                    " one",
                    "-two",
                    "+2",
                    " three",
                ]
            )
        );
    }

    // ---- reading what git printed ----

    fn entry(status: FileStatus, path: &str, from: Option<&str>, binary: bool) -> FileEntry {
        FileEntry {
            status,
            path: path.into(),
            from: from.map(str::to_owned),
            binary,
        }
    }

    #[test]
    fn git_parse_files_reads_every_status_and_pairs_a_rename_with_its_old_path() {
        let names = "A\0added.txt\0M\0edit.txt\0D\0gone.txt\0R100\0old.txt\0new.txt\0T\0mode.txt\0";
        let counts = "2\t0\tadded.txt\0";
        assert_eq!(
            parse_files(names, counts),
            [
                entry(FileStatus::Added, "added.txt", None, false),
                entry(FileStatus::Modified, "edit.txt", None, false),
                entry(FileStatus::Deleted, "gone.txt", None, false),
                entry(FileStatus::Renamed, "new.txt", Some("old.txt"), false),
                entry(FileStatus::Modified, "mode.txt", None, false),
            ]
        );
    }

    #[test]
    fn git_parse_files_marks_a_file_binary_only_when_both_counts_are_dashes() {
        let names = "M\0a.bin\0M\0b.txt\0R090\0old.bin\0new.bin\0A\0c.txt\0";
        let counts = "-\t-\ta.bin\0-\t3\tb.txt\0-\t-\t\0old.bin\0new.bin\u{0}1\t1\tc.txt\0";
        let files = parse_files(names, counts);
        let binary: Vec<(&str, bool)> = files.iter().map(|f| (f.path.as_str(), f.binary)).collect();
        assert_eq!(
            binary,
            [
                ("a.bin", true),
                ("b.txt", false),
                ("new.bin", true),
                ("c.txt", false)
            ]
        );
    }

    #[test]
    fn git_parse_files_stops_at_a_cut_record_and_reads_nothing_from_nothing() {
        assert_eq!(parse_files("", ""), []);
        assert_eq!(parse_files("A\0", ""), []);
        assert_eq!(
            parse_files("M\0kept.txt\0R100\0old.txt", ""),
            [entry(FileStatus::Modified, "kept.txt", None, false)]
        );
    }

    #[test]
    fn git_parse_commits_reads_hash_and_subject_and_skips_lines_without_both() {
        let log = "aaaa111\u{1f}First\nnot a commit\nbbbb222\u{1f}\ncccc333\u{1f}Has \u{1f} inside";
        assert_eq!(
            parse_commits(log),
            [
                Commit {
                    sha: "aaaa111".into(),
                    subject: "First".into()
                },
                Commit {
                    sha: "bbbb222".into(),
                    subject: String::new()
                },
                Commit {
                    sha: "cccc333".into(),
                    subject: "Has \u{2426} inside".into()
                },
            ]
        );
    }

    #[test]
    fn git_short_keeps_seven_characters_of_a_hash() {
        assert_eq!(short("0123456789abcdef"), "0123456");
        assert_eq!(short("0123456"), "0123456");
        assert_eq!(short("012"), "012");
        assert_eq!(short(""), "");
    }

    #[test]
    fn git_one_line_sanitizes_joins_lines_and_cuts_at_the_limit() {
        assert_eq!(one_line("\u{1b}[31mred\u{1b}[0m"), "red");
        assert_eq!(one_line("a\tb"), "a       b");
        assert_eq!(one_line("one\ntwo\n"), "one ⏎ two");
        assert_eq!(one_line("spin\rspun"), "spin ⏎ spun");
        assert_eq!(
            one_line(&"x".repeat(MAX_LINE_CHARS)),
            "x".repeat(MAX_LINE_CHARS)
        );
        assert_eq!(
            one_line(&"x".repeat(MAX_LINE_CHARS + 1)),
            format!("{}…", "x".repeat(MAX_LINE_CHARS))
        );
    }

    // ---- the plan of the body ----

    #[test]
    fn git_plan_is_compact_below_the_full_height_and_gives_the_files_what_is_left() {
        let compact = |files| Plan {
            full: false,
            files,
            commits: 0,
            diff: 0,
        };
        assert_eq!(plan(0, 5, 5), compact(0));
        assert_eq!(plan(3, 5, 5), compact(0));
        assert_eq!(plan(4, 5, 5), compact(1));
        assert_eq!(plan(9, 5, 5), compact(6));
    }

    #[test]
    fn git_plan_full_uses_every_row_and_gives_each_list_a_bounded_share() {
        assert_eq!(
            plan(10, 9, 9),
            Plan {
                full: true,
                files: 1,
                commits: 1,
                diff: 1
            }
        );
        // 22 rows less the status lines, three headings and the key bar.
        assert_eq!(
            plan(22, 9, 9),
            Plan {
                full: true,
                files: 5,
                commits: 3,
                diff: 7
            }
        );
        assert_eq!(
            plan(22, 2, 1),
            Plan {
                full: true,
                files: 2,
                commits: 1,
                diff: 12
            }
        );
        // An empty list still has a row to say so.
        assert_eq!(
            plan(22, 0, 0),
            Plan {
                full: true,
                files: 1,
                commits: 1,
                diff: 13
            }
        );
        for height in FULL_MIN_HEIGHT..80 {
            let plan = plan(height, 50, 50);
            assert_eq!(
                plan.files + plan.commits + plan.diff + STATUS_LINES + 4,
                height,
                "height {height}"
            );
            assert!(plan.diff >= 1, "height {height}");
        }
    }

    #[test]
    fn git_diff_view_shows_a_height_from_the_top_within_the_ends() {
        assert_eq!(diff_view(0, 100, 10), 0..10);
        assert_eq!(diff_view(40, 100, 10), 40..50);
        assert_eq!(diff_view(95, 100, 10), 90..100);
        assert_eq!(diff_view(500, 100, 10), 90..100);
        assert_eq!(diff_view(3, 4, 10), 0..4);
        assert_eq!(diff_view(0, 0, 10), 0..0);
    }

    // ---- what the journal says ----

    fn publish_started(task: u32, sha: &str) -> Event {
        event(
            Some(task),
            EventKind::PublishStarted {
                attempt: AttemptId::new(1),
                candidate_sha: sha.into(),
            },
        )
    }

    fn publish_verified(task: u32, commit: &str, remote: &str) -> Event {
        event(
            Some(task),
            EventKind::PublishVerified {
                commit: commit.into(),
                remote_sha: remote.into(),
            },
        )
    }

    #[test]
    fn git_fold_remembers_the_base_and_how_far_publication_got_per_task() {
        let task = TaskId::new(1);
        let mut app = app_on_git((80, 24), 2);
        fold(&mut app, &attempt(1, "base1"));
        assert_eq!(
            app.git.tasks.get(&task).and_then(|f| f.base.as_deref()),
            Some("base1")
        );
        assert_eq!(app.git.publication(task), None);
        fold(&mut app, &publish_started(1, "cand"));
        assert_eq!(
            app.git.publication(task),
            Some(&Publication::Publishing {
                candidate: "cand".into()
            })
        );
        fold(&mut app, &publish_verified(1, "cand", "cand"));
        assert_eq!(
            app.git.publication(task),
            Some(&Publication::Verified {
                commit: "cand".into(),
                remote: "cand".into()
            })
        );
        assert_eq!(app.git.publication(TaskId::new(2)), None);
    }

    #[test]
    fn git_fold_a_new_attempt_or_a_retry_forgets_that_the_task_was_published() {
        let task = TaskId::new(1);
        let mut app = app_on_git((80, 24), 1);
        fold(&mut app, &attempt(1, "base1"));
        fold(&mut app, &publish_verified(1, "c", "c"));
        fold(
            &mut app,
            &event(
                Some(1),
                EventKind::RetryStarted {
                    attempt: AttemptId::new(2),
                },
            ),
        );
        assert_eq!(app.git.publication(task), None);
        fold(&mut app, &publish_started(1, "c2"));
        fold(&mut app, &attempt(1, "base2"));
        assert_eq!(app.git.publication(task), None);
        assert_eq!(
            app.git.tasks.get(&task).and_then(|f| f.base.as_deref()),
            Some("base2")
        );
    }

    #[test]
    fn git_fold_marks_the_read_stale_except_for_agent_output() {
        let mut app = app_on_git((80, 24), 1);
        let output = event(
            Some(1),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "hi".into(),
            },
        );
        fold(&mut app, &output);
        assert_eq!(app.git, GitView::default());
        fold(&mut app, &event(None, EventKind::Resumed));
        assert_eq!(app.git.revision, 1);
        assert!(app.git.tasks.is_empty(), "no task was named");
        fold(&mut app, &event(Some(1), EventKind::Resumed));
        assert_eq!(app.git.revision, 2);
        assert!(app.git.tasks.is_empty(), "nothing this screen uses changed");
    }

    // ---- what is read, and when ----

    #[test]
    fn git_wanted_asks_for_nothing_off_the_screen_without_a_task_or_before_an_attempt() {
        let f = fixture();
        let started = feed(app_on_git((80, 24), 1), &[attempt(1, &f.base)]);
        assert_eq!(
            wanted(&started),
            Some(Request::Snapshot {
                task: TaskId::new(1)
            })
        );
        let elsewhere = App {
            screen: Screen::Queue,
            ..started.clone()
        };
        assert_eq!(wanted(&elsewhere), None);
        assert_eq!(wanted(&app_on_git((80, 24), 0)), None);
        assert_eq!(
            wanted(&app_on_git((80, 24), 1)),
            None,
            "no attempt, no base"
        );
    }

    #[test]
    fn git_wanted_asks_for_the_snapshot_then_the_selected_files_diff_then_nothing() {
        let f = fixture();
        f.change_everything();
        let mut app = feed(app_on_git((80, 24), 1), &[attempt(1, &f.base)]);
        assert!(backfill(&mut app, &|_| Some(f.repo())));
        assert_eq!(
            wanted(&app),
            Some(Request::Diff {
                task: TaskId::new(1),
                path: "added.txt".into(),
                top: 0
            })
        );
        assert!(backfill(&mut app, &|_| Some(f.repo())));
        assert_eq!(wanted(&app), None);
        assert!(!backfill(&mut app, &|_| Some(f.repo())));
    }

    #[test]
    fn git_a_journal_event_makes_the_snapshot_and_the_diff_stale() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 24));
        assert_eq!(wanted(&app), None);
        app = feed(app, &[event(Some(1), EventKind::Resumed)]);
        assert_eq!(
            wanted(&app),
            Some(Request::Snapshot {
                task: TaskId::new(1)
            })
        );
        assert!(backfill(&mut app, &|_| Some(f.repo())));
        assert!(matches!(wanted(&app), Some(Request::Diff { .. })));
    }

    #[test]
    fn git_a_new_commit_shows_after_the_next_event_and_output_alone_does_not_reread() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 24));
        f.write("later.txt", "later\n");
        commit(&f.work, "A later commit");
        let output = event(
            Some(1),
            EventKind::AgentOutput {
                attempt: AttemptId::new(1),
                stream: Stream::Stdout,
                text: "working".into(),
            },
        );
        app = feed(app, &[output]);
        assert_eq!(wanted(&app), None);
        app = feed(app, &[event(Some(1), EventKind::Resumed)]);
        settle(&mut app, &f.repo());
        assert_eq!(app.git.contents().map(|c| c.files.len()), Some(5));
        assert_eq!(app.git.contents().map(|c| c.commits_total), Some(3));
        assert!(body(&app).contains(&"Git · task 1 · 5 files · 3 commits".to_owned()));
    }

    #[test]
    fn git_the_snapshot_lists_files_commits_and_the_comparison_with_the_remote() {
        let f = fixture();
        f.change_everything();
        let app = loaded(&f, (80, 24));
        let contents = app.git.contents().expect("read");
        assert_eq!(
            contents.files,
            [
                entry(FileStatus::Added, "added.txt", None, false),
                entry(FileStatus::Deleted, "deleted.txt", None, false),
                entry(FileStatus::Modified, "image.bin", None, true),
                entry(FileStatus::Modified, "modified.txt", None, false),
            ]
        );
        assert_eq!(contents.files_total, 4);
        let subjects: Vec<&str> = contents
            .commits
            .iter()
            .map(|c| c.subject.as_str())
            .collect();
        assert_eq!(
            subjects,
            [
                "Delete deleted.txt, change image.bin",
                "Add added.txt, edit modified.txt"
            ]
        );
        assert_eq!(
            contents.commits.first().map(|c| c.sha.clone()),
            Some(f.head())
        );
        assert_eq!(contents.commits_total, 2);
        assert_eq!(
            contents.remote,
            Remote::Compared {
                name: "origin/main".into(),
                ahead: 2,
                behind: 0
            }
        );
    }

    #[test]
    fn git_a_rename_is_one_file_shown_with_its_old_path() {
        let f = fixture();
        g(&f.work, &["mv", "modified.txt", "renamed.txt"]);
        commit(&f.work, "Rename it");
        let mut app = loaded(&f, (80, 24));
        assert_eq!(
            app.git.contents().map(|c| c.files.clone()),
            Some(vec![entry(
                FileStatus::Renamed,
                "renamed.txt",
                Some("modified.txt"),
                false
            )])
        );
        assert!(screen(&app).contains(&"> R modified.txt → renamed.txt".to_owned()));
        settle(&mut app, &f.repo());
        assert!(matches!(&app.git.diff, Some(page) if page.path == "renamed.txt"));
    }

    #[test]
    fn git_the_remote_comparison_shows_drift_and_identity() {
        let f = fixture();
        f.change_everything();
        let other = f.other_clone();
        std::fs::write(other.join("elsewhere.txt"), "x\n").expect("write");
        commit(&other, "Someone else's work");
        g(&other, &["push", "--quiet", "origin", "main"]);
        g(&f.work, &["fetch", "--quiet", "origin"]);
        let app = loaded(&f, (80, 24));
        assert_eq!(
            app.git.contents().map(|c| c.remote.clone()),
            Some(Remote::Compared {
                name: "origin/main".into(),
                ahead: 2,
                behind: 1
            })
        );
        assert!(body(&app).contains(
            &"Remote origin/main: HEAD is 2 ahead, 1 behind (as of the last fetch)".to_owned()
        ));
        g(
            &f.work,
            &["push", "--quiet", "origin", "HEAD:refs/heads/side"],
        );
        g(&f.work, &["reset", "--quiet", "--hard", "origin/main"]);
        let app = loaded(&f, (80, 24));
        assert_eq!(
            app.git.contents().map(|c| c.remote.clone()),
            Some(Remote::Compared {
                name: "origin/main".into(),
                ahead: 0,
                behind: 0
            })
        );
        assert!(
            body(&app).contains(
                &"Remote origin/main: identical to HEAD (as of the last fetch)".to_owned()
            )
        );
    }

    #[test]
    fn git_a_missing_remote_branch_is_said_not_failed_on() {
        let f = fixture();
        f.change_everything();
        let mut repo = f.repo();
        repo.branch = "nowhere".into();
        let mut app = feed(app_on_git((80, 24), 1), &[attempt(1, &f.base)]);
        settle(&mut app, &repo);
        let Some(Remote::Unavailable { name, reason }) =
            app.git.contents().map(|c| c.remote.clone())
        else {
            panic!("expected the comparison to be unavailable");
        };
        assert_eq!(name, "origin/nowhere");
        assert!(reason.contains("refs/remotes/origin/nowhere"), "{reason}");
        assert!(body(&app)[2].starts_with("Remote origin/nowhere: unavailable, "));
        assert_eq!(app.git.file_count(), 4, "the rest is still shown");
    }

    #[test]
    fn git_a_task_that_changed_nothing_says_so() {
        let f = fixture();
        let app = loaded(&f, (80, 24));
        let lines = body(&app);
        assert_eq!(
            lines.first().map(String::as_str),
            Some("Git · task 1 · 0 files · 0 commits")
        );
        assert!(lines.contains(&"No changed files".to_owned()));
        assert!(lines.contains(&"No commits".to_owned()));
        assert!(lines.contains(&"Diff".to_owned()));
        assert!(
            lines.contains(
                &"Remote origin/main: identical to HEAD (as of the last fetch)".to_owned()
            )
        );
        assert_eq!(app.git.held(), 0);
    }

    #[test]
    fn git_files_and_commits_past_their_caps_are_counted_but_not_listed() {
        let f = fixture();
        for n in 0..FILE_CAP + 2 {
            f.write(&format!("file{n:02}.txt"), "x\n");
            commit(&f.work, &format!("Commit {n}"));
        }
        let app = loaded(&f, (80, 24));
        let contents = app.git.contents().expect("read");
        assert_eq!(contents.files.len(), FILE_CAP);
        assert_eq!(contents.files_total, FILE_CAP + 2);
        assert_eq!(contents.commits.len(), COMMIT_CAP);
        assert_eq!(contents.commits_total, FILE_CAP + 2);
        let heading = format!(
            "Git · task 1 · {FILE_CAP} of {} files · {COMMIT_CAP} of {} commits",
            FILE_CAP + 2,
            FILE_CAP + 2
        );
        assert_eq!(body(&app).first(), Some(&heading));
        let newest = contents.commits.first().map(|c| c.subject.clone());
        assert_eq!(newest, Some(format!("Commit {}", FILE_CAP + 1)));
    }

    #[test]
    fn git_without_a_worktree_the_reason_is_shown_beside_the_journals_publication_state() {
        let f = fixture();
        let mut app = feed(
            app_on_git((80, 24), 1),
            &[
                attempt(1, &f.base),
                publish_verified(1, "0123456789", "0123456789"),
            ],
        );
        assert!(backfill(&mut app, &|_| None));
        assert_eq!(wanted(&app), None, "a failed read is not asked for again");
        assert_eq!(
            content(&app),
            [
                "Git · task 1",
                "Publication: verified · commit 0123456 · remote 0123456",
                NO_WORKTREE,
            ]
        );
        app = feed(app, &[event(Some(1), EventKind::Resumed)]);
        assert!(wanted(&app).is_some(), "the journal moving on retries it");
    }

    #[test]
    fn git_a_base_that_git_does_not_know_is_shown_as_the_failure_it_is() {
        let f = fixture();
        let mut app = feed(app_on_git((80, 24), 1), &[attempt(1, "deadbeef")]);
        settle(&mut app, &f.repo());
        let lines = body(&app);
        assert!(lines[2].contains("deadbeef"), "{lines:?}");
        assert_eq!(app.git.file_count(), 0);
    }

    #[test]
    fn git_before_the_first_attempt_and_before_the_first_read_it_says_what_it_waits_for() {
        let f = fixture();
        let app = app_on_git((80, 24), 1);
        assert_eq!(
            content(&app),
            ["Git · task 1", "Publication: not published", NO_BASE]
        );
        let app = feed(app, &[attempt(1, &f.base)]);
        assert_eq!(
            content(&app),
            ["Git · task 1", "Publication: not published", NOT_LOADED]
        );
        assert_eq!(content(&app_on_git((80, 24), 0)), ["Git", NO_TASK]);
    }

    #[test]
    fn git_the_screen_follows_the_task_selected_on_the_queue() {
        let f = fixture();
        f.change_everything();
        let mut app = feed(
            app_on_git((80, 24), 2),
            &[attempt(1, &f.base), attempt(2, &f.base)],
        );
        settle(&mut app, &f.repo());
        app = key(app, 'j');
        assert_eq!(app.git.selected_file(), 1);
        app.selected.insert(Screen::Queue, 1);
        assert_eq!(
            wanted(&app),
            Some(Request::Snapshot {
                task: TaskId::new(2)
            })
        );
        assert!(body(&app)[0].starts_with("Git · task 2"));
        settle(&mut app, &f.repo());
        assert_eq!(
            app.git.selected_file(),
            0,
            "another task starts at its first file"
        );
        assert_eq!(app.git.diff.as_ref().map(|p| p.task), Some(TaskId::new(2)));
    }

    // ---- a very large diff ----

    /// The fixture with `lines` lines added to a file of their own.
    fn with_large_file(lines: usize) -> Fixture {
        let f = fixture();
        let numbered: Vec<String> = (1..=lines).map(|n| format!("line {n}")).collect();
        f.write("large.txt", &format!("{}\n", numbered.join("\n")));
        commit(&f.work, "A very large file");
        f
    }

    #[test]
    fn git_a_very_large_diff_is_held_as_a_window_not_whole() {
        let lines = 5 * DIFF_PAGE;
        let f = with_large_file(lines);
        let app = loaded(&f, (80, 24));
        // Five header lines, the hunk header and one line per added line.
        assert_eq!(app.git.diff_len(), lines + 6);
        assert_eq!(app.git.held(), DIFF_PAGE);
        let shown = body(&app);
        assert!(shown.contains(&format!("Diff · large.txt · lines 1–13 of {}", lines + 6)));
        assert!(shown.contains(&"+line 1".to_owned()));
        assert!(!shown.contains(&"+line 40".to_owned()));
    }

    #[test]
    fn git_scrolling_a_large_diff_reads_the_window_it_moves_into() {
        let lines = 5 * DIFF_PAGE;
        let f = with_large_file(lines);
        let mut app = loaded(&f, (80, 24));
        let total = lines + 6;
        let height = diff_height(&app);
        assert_eq!(height, 13);
        app = press(app, KeyCode::PageDown);
        assert_eq!(wanted(&app), None, "the first page is still under the view");
        for _ in 0..(total / height) {
            app = press(app, KeyCode::PageDown);
            settle(&mut app, &f.repo());
            assert!(app.git.held() <= DIFF_PAGE);
        }
        let shown = body(&app);
        assert!(
            shown.contains(&format!(
                "Diff · large.txt · lines {}–{total} of {total}",
                total - height + 1
            )),
            "{shown:?}"
        );
        assert!(shown.contains(&format!("+line {lines}")));
        assert!(app.git.held() <= DIFF_PAGE);
        // Back to the start.
        for _ in 0..=(total / height) {
            app = press(app, KeyCode::PageUp);
            settle(&mut app, &f.repo());
        }
        assert!(body(&app).contains(&"+line 1".to_owned()));
        assert!(app.git.held() <= DIFF_PAGE);
    }

    #[test]
    fn git_a_diff_shorter_than_the_view_after_it_moved_is_not_read_forever() {
        let f = with_large_file(3 * DIFF_PAGE);
        let mut app = loaded(&f, (80, 24));
        app.git.top = 3 * DIFF_PAGE;
        settle(&mut app, &f.repo());
        g(&f.work, &["reset", "--quiet", "--hard", "HEAD~1"]);
        f.write("large.txt", "tiny\n");
        commit(&f.work, "Now tiny");
        app = feed(app, &[event(Some(1), EventKind::Resumed)]);
        settle(&mut app, &f.repo());
        assert!(body(&app).contains(&"+tiny".to_owned()));
    }

    // ---- keys ----

    #[test]
    fn git_j_k_g_and_the_arrows_select_a_file_and_stop_at_the_ends() {
        let f = fixture();
        f.change_everything();
        let app = loaded(&f, (80, 24));
        let selected = |app: &App| app.git.selected_file();
        assert_eq!(selected(&app), 0);
        let app = key(app, 'k');
        assert_eq!(selected(&app), 0, "no wrapping above the first");
        let app = key(app, 'j');
        assert_eq!(selected(&app), 1);
        let app = press(app, KeyCode::Down);
        assert_eq!(selected(&app), 2);
        let app = press(app, KeyCode::Up);
        assert_eq!(selected(&app), 1);
        let app = key(app, 'G');
        assert_eq!(selected(&app), 3);
        let app = key(app, 'j');
        assert_eq!(selected(&app), 3, "no wrapping below the last");
        let app = key(app, 'g');
        assert_eq!(selected(&app), 0);
    }

    #[test]
    fn git_selecting_another_file_shows_its_diff_from_the_first_line() {
        let f = with_large_file(100);
        f.write("a.txt", "first\n");
        commit(&f.work, "A file that sorts first");
        let mut app = loaded(&f, (80, 24));
        app = key(app, 'G');
        assert_eq!(app.git.selected_file(), 1);
        settle(&mut app, &f.repo());
        app = press(app, KeyCode::PageDown);
        assert!(app.git.top > 0);
        app = key(app, 'k');
        assert_eq!(app.git.selected_file(), 0);
        assert_eq!(app.git.top, 0);
        app = key(app, 'G');
        assert_eq!(app.git.top, 0);
        settle(&mut app, &f.repo());
        app = press(app, KeyCode::PageDown);
        let scrolled = app.git.top;
        assert!(scrolled > 0);
        app = key(app, 'j');
        assert_eq!(
            app.git.top, scrolled,
            "staying on the last file keeps the view"
        );
    }

    #[test]
    fn git_page_keys_scroll_the_diff_a_screenful_within_its_ends() {
        let f = with_large_file(40);
        let mut app = loaded(&f, (80, 24));
        app = key(app, 'G');
        settle(&mut app, &f.repo());
        let total = app.git.diff_len();
        let height = diff_height(&app);
        assert!(total > 2 * height);
        app = press(app, KeyCode::PageUp);
        assert_eq!(app.git.top, 0);
        app = press(app, KeyCode::PageDown);
        assert_eq!(app.git.top, height);
        for _ in 0..10 {
            app = press(app, KeyCode::PageDown);
        }
        assert_eq!(app.git.top, total - height);
        app = press(app, KeyCode::PageUp);
        assert_eq!(app.git.top, total - 2 * height);
    }

    #[test]
    fn git_keys_do_nothing_without_files_on_other_screens_or_under_an_overlay() {
        let f = fixture();
        f.change_everything();
        let app = loaded(&f, (80, 24));
        for code in [KeyCode::Char('j'), KeyCode::Char('G'), KeyCode::PageDown] {
            let elsewhere = App {
                screen: Screen::History,
                ..app.clone()
            };
            assert_eq!(
                press(elsewhere.clone(), code).git,
                elsewhere.git,
                "{code:?}"
            );
            let covered = App {
                overlay: Some(crate::types::Overlay::KeyMap),
                ..app.clone()
            };
            assert_eq!(press(covered.clone(), code).git, covered.git, "{code:?}");
            let empty = feed(app_on_git((80, 24), 1), &[attempt(1, &f.base)]);
            assert_eq!(press(empty.clone(), code).git, empty.git, "{code:?}");
        }
        let unknown = press(app.clone(), KeyCode::Char('x'));
        assert_eq!(unknown.git, app.git);
        let home = press(app.clone(), KeyCode::Home);
        assert_eq!(home.git, app.git);
    }

    #[test]
    fn git_number_keys_still_change_screens() {
        let f = fixture();
        let app = loaded(&f, (80, 24));
        assert_eq!(key(app, '1').screen, Screen::Queue);
    }

    // ---- publication ----

    #[test]
    fn git_the_publication_line_says_how_far_publication_got() {
        assert_eq!(publication_text(None), "Publication: not published");
        assert_eq!(
            publication_text(Some(&Publication::Publishing {
                candidate: "0123456789".into()
            })),
            "Publication: publishing 0123456"
        );
        assert_eq!(
            publication_text(Some(&Publication::Verified {
                commit: "0123456789".into(),
                remote: "0123456789".into()
            })),
            "Publication: verified · commit 0123456 · remote 0123456"
        );
    }

    #[test]
    fn git_a_published_task_shows_it_on_the_screen() {
        let f = fixture();
        f.change_everything();
        let head = f.head();
        let mut app = feed(
            app_on_git((80, 24), 1),
            &[attempt(1, &f.base), publish_verified(1, &head, &head)],
        );
        settle(&mut app, &f.repo());
        let expected = format!(
            "Publication: verified · commit {0} · remote {0}",
            short(&head)
        );
        assert_eq!(body(&app).get(1), Some(&expected));
    }

    #[test]
    fn git_the_remote_line_reads_ahead_behind_identical_and_unavailable() {
        let name = || "origin/main".to_owned();
        assert_eq!(
            remote_text(&Remote::Compared {
                name: name(),
                ahead: 3,
                behind: 0
            }),
            "Remote origin/main: HEAD is 3 ahead, 0 behind (as of the last fetch)"
        );
        assert_eq!(
            remote_text(&Remote::Compared {
                name: name(),
                ahead: 0,
                behind: 2
            }),
            "Remote origin/main: HEAD is 0 ahead, 2 behind (as of the last fetch)"
        );
        assert_eq!(
            remote_text(&Remote::Compared {
                name: name(),
                ahead: 0,
                behind: 0
            }),
            "Remote origin/main: identical to HEAD (as of the last fetch)"
        );
        assert_eq!(
            remote_text(&Remote::Unavailable {
                name: name(),
                reason: "no ref".into()
            }),
            "Remote origin/main: unavailable, no ref"
        );
    }

    #[test]
    fn git_counted_says_of_only_when_the_list_was_cut() {
        assert_eq!(counted(3, 3), "3");
        assert_eq!(counted(0, 0), "0");
        assert_eq!(counted(3, 7), "3 of 7");
    }

    // ---- stress ----

    #[test]
    fn git_hostile_paths_and_diff_text_cannot_reach_the_terminal() {
        let f = fixture();
        f.write(
            "esc\u{1b}[31m.txt",
            "a\u{1b}]0;title\u{7}b\r\rc\td\n\u{9b}31mrow\n",
        );
        commit(&f.work, "Escape \u{1b}[2J the subject");
        let mut app = loaded(&f, (80, 24));
        settle(&mut app, &f.repo());
        let text = Harness::from_app(app).text();
        assert!(!text.contains('\u{1b}'), "{text}");
        assert!(!text.contains('\u{7}'));
        assert!(!text.contains('\r'));
        assert!(text.contains("+a"), "{text}");
    }

    #[test]
    fn git_wide_characters_in_paths_are_cut_by_columns_not_characters() {
        let f = fixture();
        let name = format!("{}.txt", "日本語".repeat(20));
        f.write(&name, "x\n");
        commit(&f.work, "Wide");
        let app = loaded(&f, (80, 24));
        let lines = body_lines(&app, 22, 80);
        assert!(lines.iter().all(|line| line.width() <= 80));
        let row = lines.get(4).expect("the file row");
        // 80 columns less the marker and "A ", spent on whole two-column characters.
        assert_eq!(row.width(), 80);
        assert!(row.to_string().starts_with("> A 日本語日本語"));
    }

    #[test]
    fn git_every_size_down_to_nothing_draws_without_panicking_and_fits() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 24));
        for width in [0u16, 1, 2, 7, 20, 40, 80, 200] {
            for height in [0u16, 1, 2, 3, 5, 9, 10, 11, 14, 24, 60] {
                app.size = (width, height);
                let text = Harness::from_app(app.clone()).text();
                let lines: Vec<&str> = text.lines().collect();
                if width > 0 && height > 0 {
                    assert_eq!(lines.len(), usize::from(height), "{width}x{height}");
                }
                for line in lines {
                    assert!(crate::text::display_width(line) <= usize::from(width));
                }
            }
        }
    }

    #[test]
    fn git_a_short_body_shows_the_status_and_the_selected_file() {
        let f = fixture();
        f.change_everything();
        let mut app = loaded(&f, (80, 10));
        app = key(key(app, 'j'), 'j');
        assert_eq!(
            body(&app),
            [
                "Git · task 1 · 4 files · 2 commits",
                "Publication: not published",
                "Remote origin/main: HEAD is 2 ahead, 0 behind (as of the last fetch)",
                "  A added.txt",
                "  D deleted.txt",
                "> M image.bin (binary)",
                "  M modified.txt",
                "",
                "",
            ]
        );
        let taller = loaded(&f, (80, 11));
        assert!(
            body(&taller).contains(&"Changed files".to_owned()),
            "full from ten body rows"
        );
    }

    #[test]
    fn git_diff_lines_are_coloured_by_what_they_do() {
        let bold = Style::new().add_modifier(Modifier::BOLD);
        assert_eq!(diff_style("+added"), Style::new().fg(Color::Green));
        assert_eq!(diff_style("-removed"), Style::new().fg(Color::Red));
        assert_eq!(diff_style("@@ -1 +1 @@"), Style::new().fg(Color::Cyan));
        assert_eq!(diff_style("+++ b/x"), bold);
        assert_eq!(diff_style("--- a/x"), bold);
        assert_eq!(diff_style("diff --git a/x b/x"), bold);
        assert_eq!(diff_style(" context"), Style::new());
        for (status, colour) in [
            (FileStatus::Added, Color::Green),
            (FileStatus::Modified, Color::Yellow),
            (FileStatus::Deleted, Color::Red),
            (FileStatus::Renamed, Color::Cyan),
        ] {
            assert_eq!(status.style(), Style::new().fg(colour));
        }
        assert_eq!(
            ['A', 'M', 'D', 'R'],
            [
                FileStatus::Added.letter(),
                FileStatus::Modified.letter(),
                FileStatus::Deleted.letter(),
                FileStatus::Renamed.letter()
            ]
        );
    }

    #[test]
    fn git_the_selected_file_is_bold_and_the_others_are_not() {
        let f = fixture();
        f.change_everything();
        let harness = Harness::from_app(loaded(&f, (80, 24)));
        let buffer = harness.buffer();
        // Row 5 is the first file (selected), row 6 the second.
        let bold = |y: u16| buffer[(3, y)].modifier.contains(Modifier::BOLD);
        assert!(bold(5));
        assert!(!bold(6));
        assert_eq!(buffer[(3, 5)].fg, Color::Green);
        assert_eq!(buffer[(3, 6)].fg, Color::Red);
    }
}
