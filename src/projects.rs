//! Every project of this instance in one terminal: the Projects screen's list, and the
//! workspaces opened from it. One project is open at a time; the others keep their screen
//! (view, selection, scroll) parked until reopened. Each project still owns its queue and
//! its background supervisor.
use crate::{
    app::{Action, App, Screen, View},
    application::{
        Service,
        overview::{Attention, Project},
        registry,
    },
    backend::Backend,
    dialog::{Dialog, DialogKind, Field},
    instance::{Instance, Paths},
    persistent::StoredWorkspace,
    storage::Database,
};
use ratatui::{crossterm::event::KeyCode, widgets::ListState};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};

pub(crate) const PROJECT_NAME: &str = "Project name";
pub(crate) const NEW_FOLDER: &str = "New folder";
pub(crate) const CONFIRM_NAME: &str = "Type the project name to delete it";

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub project: Project,
    pub runner_alive: bool,
}

impl Row {
    pub fn attention(&self) -> Attention {
        self.project.attention(self.runner_alive)
    }
}

type Workspace = (Box<dyn Backend>, Screen);

struct Source {
    db: Database,
    paths: Paths,
}

pub(crate) struct Hub {
    pub rows: Vec<Row>,
    pub selected: Option<i64>,
    pub state: ListState,
    /// The open project; None in `--demo`, which has one simulated workspace.
    pub active: Option<i64>,
    /// The folder the TUI started in when it is not a registered project.
    pub unregistered: Option<PathBuf>,
    /// The project a Move or Delete form is about.
    target: Option<i64>,
    /// Why the last refresh failed; the list then shows what it had.
    pub error: Option<String>,
    source: Option<Source>,
    parked: BTreeMap<i64, Workspace>,
    refreshed: Instant,
}

impl Hub {
    /// The simulator has no stored projects and never opens the database.
    pub fn demo() -> Self {
        Self {
            rows: Vec::new(),
            selected: None,
            state: ListState::default(),
            active: None,
            unregistered: None,
            target: None,
            error: None,
            source: None,
            parked: BTreeMap::new(),
            refreshed: Instant::now(),
        }
    }

    pub fn open(
        instance: Instance,
        active: i64,
        unregistered: Option<PathBuf>,
    ) -> Result<Self, String> {
        let paths = instance.paths().map_err(|e| e.to_string())?;
        let db = Database::open(&paths).map_err(|e| e.to_string())?;
        let mut hub = Self {
            active: Some(active),
            selected: Some(active),
            unregistered,
            source: Some(Source { db, paths }),
            ..Self::demo()
        };
        hub.refresh();
        Ok(hub)
    }

    pub fn stored(&self) -> bool {
        self.source.is_some()
    }

    /// Reads the list again; true when it changed.
    fn refresh(&mut self) -> bool {
        self.refreshed = Instant::now();
        let Some(source) = &self.source else {
            return false;
        };
        let listed = crate::runner::registration::listed(&source.paths, &source.db);
        let (rows, error) = match listed {
            Ok(listed) => (
                listed
                    .into_iter()
                    .map(|(project, runner_alive)| Row {
                        project,
                        runner_alive,
                    })
                    .collect(),
                None,
            ),
            Err(error) => (self.rows.clone(), Some(error.to_string())),
        };
        let changed = rows != self.rows || error != self.error;
        self.rows = rows;
        self.error = error;
        if self.position().is_none() {
            self.selected = self
                .active
                .filter(|id| self.rows.iter().any(|row| row.project.id == *id))
                .or_else(|| self.rows.first().map(|row| row.project.id));
        }
        changed
    }

    /// Refreshes on a timer, faster while the list is on screen.
    fn tick(&mut self, visible: bool) -> bool {
        let interval = Duration::from_millis(if visible { 1000 } else { 3000 });
        self.refreshed.elapsed() >= interval && self.refresh()
    }

    /// Parked workspaces advance only while they finish something they started.
    fn tick_parked(&mut self) {
        for (backend, screen) in self.parked.values_mut() {
            if backend.busy() {
                backend.tick(screen);
            }
        }
    }

    pub fn position(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|row| Some(row.project.id) == self.selected)
    }

    pub fn selected_row(&self) -> Option<&Row> {
        self.position().map(|index| &self.rows[index])
    }

    /// Projects with a live runner, and projects whose current task waits for the owner.
    pub fn counts(&self) -> (usize, usize) {
        let count = |wanted| {
            self.rows
                .iter()
                .filter(|row| row.attention() == wanted)
                .count()
        };
        (count(Attention::Running), count(Attention::NeedsOwner))
    }

    fn navigate(&mut self, code: KeyCode, page: usize) {
        let last = self.rows.len().saturating_sub(1);
        let position = self.position().unwrap_or(0);
        let next = match code {
            KeyCode::Up | KeyCode::Char('k') => position.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => (position + 1).min(last),
            KeyCode::PageUp => position.saturating_sub(page),
            KeyCode::PageDown => (position + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => 0,
            KeyCode::End | KeyCode::Char('G') => last,
            _ => return,
        };
        self.selected = self.rows.get(next).map(|row| row.project.id);
    }

    /// The parked workspace of `id`, or a newly opened one.
    fn workspace(&mut self, instance: Instance, id: i64) -> Result<Workspace, String> {
        if let Some(parked) = self.parked.remove(&id) {
            return Ok(parked);
        }
        let row = self
            .rows
            .iter()
            .find(|row| row.project.id == id)
            .ok_or("That project is no longer registered")?;
        if !row.project.root.is_dir() {
            return Err(format!(
                "{}: folder is missing · restore {} to open this project",
                row.project.name,
                row.project.root.display()
            ));
        }
        let source = self.source.as_ref().ok_or("No stored projects")?;
        let db = Database::open(&source.paths).map_err(|e| e.to_string())?;
        let service =
            Service::registered(db, id, row.project.name.clone()).map_err(|e| e.to_string())?;
        stored_workspace(instance, service)
    }

    /// The project a Move or Delete form applies to, once nothing of it is on screen or
    /// still finishing in a parked workspace.
    fn managed(&self) -> Result<(i64, Database, &Paths), String> {
        let id = self.target.ok_or("No project selected")?;
        if self
            .parked
            .get(&id)
            .is_some_and(|(backend, _)| backend.busy())
        {
            return Err("That project is finishing an operation · try again in a moment".into());
        }
        let source = self.source.as_ref().ok_or("No stored projects")?;
        let db = Database::open(&source.paths).map_err(|e| e.to_string())?;
        Ok((id, db, &source.paths))
    }

    fn relocate(&mut self, folder: &str) -> Result<PathBuf, String> {
        let (id, mut db, paths) = self.managed()?;
        let root = registry::relocate(&mut db, paths, id, std::path::Path::new(folder.trim()))
            .map_err(|e| e.to_string())?;
        if self.unregistered.as_ref() == Some(&root) {
            self.unregistered = None;
        }
        Ok(root)
    }

    fn remove(&mut self, confirm: &str) -> Result<registry::Removed, String> {
        let (id, mut db, paths) = self.managed()?;
        if Some(id) == self.active {
            return Err("The open project cannot be deleted · open another one first".into());
        }
        let removed = registry::remove(&mut db, paths, id, confirm).map_err(|e| e.to_string())?;
        self.parked.remove(&id);
        Ok(removed)
    }

    fn register(&mut self, instance: Instance, name: &str) -> Result<(i64, Workspace), String> {
        let root = self
            .unregistered
            .clone()
            .ok_or("This folder is already a project")?;
        let source = self.source.as_ref().ok_or("No stored projects")?;
        let db = Database::open(&source.paths).map_err(|e| e.to_string())?;
        let service = Service::open(db, &root, Some(name)).map_err(|e| e.to_string())?;
        let id = service.project_id;
        let workspace = stored_workspace(instance, service)?;
        self.unregistered = None;
        Ok((id, workspace))
    }
}

fn stored_workspace(instance: Instance, service: Service) -> Result<Workspace, String> {
    let mut screen = Screen::new(instance, &service.project_name);
    let backend = StoredWorkspace::open(&mut screen, service)?;
    Ok((Box::new(backend), screen))
}

impl App {
    pub(crate) fn tick_projects(&mut self) -> bool {
        self.hub.tick_parked();
        self.hub.tick(self.view == View::Projects)
    }

    pub(crate) fn navigate_projects(&mut self, code: KeyCode) {
        let page = self.page_size;
        self.hub.navigate(code, page);
    }

    /// Open the selected project; the one left keeps its screen until it is reopened.
    pub(crate) fn open_project(&mut self) {
        let Some(id) = self.hub.selected else {
            return;
        };
        if Some(id) == self.hub.active {
            let previous = self.previous;
            self.switch(previous);
            return;
        }
        match self.hub.workspace(self.instance, id) {
            Ok(workspace) => self.activate(id, workspace),
            Err(error) => self.message(error),
        }
    }

    pub(crate) fn register_form(&mut self) {
        let Some(root) = &self.hub.unregistered else {
            self.message("This folder is already a project");
            return;
        };
        let name = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
        self.dialog = Some(Dialog::new(
            DialogKind::RegisterProject,
            "Add this folder as a project",
            &format!(
                "Folder: {}\nThe project starts with your user defaults and its own empty queue.\nNothing in the folder changes.",
                root.display()
            ),
            vec![Field::text(PROJECT_NAME, &name)],
        ));
    }

    /// Apply the register form: Ok closes it and opens the new project.
    pub(crate) fn register_project(&mut self, dialog: &Dialog) -> Result<(), String> {
        let (id, workspace) = self.hub.register(self.instance, dialog.get(PROJECT_NAME))?;
        self.hub.refresh();
        self.activate(id, workspace);
        Ok(())
    }

    /// The selected project when it can be moved or deleted now.
    fn manageable(&mut self, verb: &str) -> Option<Row> {
        let row = self.hub.selected_row()?.clone();
        if row.runner_alive {
            self.message(format!(
                "{} is running · pause it before you {verb} it",
                row.project.name
            ));
            return None;
        }
        self.hub.target = Some(row.project.id);
        Some(row)
    }

    pub(crate) fn move_form(&mut self) {
        let Some(row) = self.manageable("move") else {
            return;
        };
        let suggestion = self.hub.unregistered.as_ref().unwrap_or(&row.project.root);
        self.dialog = Some(Dialog::new(
            DialogKind::MoveProject,
            &format!("Move project {}", row.project.name),
            &format!(
                "Now: {}\nktask moves no files. Move or clone the repository yourself, then enter its folder here. Tasks, history and settings follow the project.",
                row.project.root.display()
            ),
            vec![Field::text(NEW_FOLDER, &suggestion.to_string_lossy())],
        ));
    }

    pub(crate) fn move_project(&mut self, dialog: &Dialog) -> Result<(), String> {
        let root = self.hub.relocate(dialog.get(NEW_FOLDER))?;
        self.hub.refresh();
        self.message(format!("Project moved to {}", root.display()));
        Ok(())
    }

    pub(crate) fn delete_form(&mut self) {
        let Some(row) = self.manageable("delete") else {
            return;
        };
        if Some(row.project.id) == self.hub.active {
            self.message(
                "The open project cannot be deleted · open another one first, or use ktask project-delete",
            );
            return;
        }
        self.dialog = Some(Dialog::new(
            DialogKind::DeleteProject,
            &format!("DANGER · Delete project {}", row.project.name),
            &format!(
                "This cannot be undone. It permanently deletes {} unfinished and {} finished tasks with their attempts, output, evidence, usage and settings, and ktask's own files for this project (logs, evidence, preferences, caches).\n\nNothing in {} is touched: the repository, its commits and stashes stay as they are.",
                row.project.queued,
                row.project.finished,
                row.project.root.display()
            ),
            vec![Field::text(CONFIRM_NAME, "")],
        ));
    }

    pub(crate) fn delete_project(&mut self, dialog: &Dialog) -> Result<(), String> {
        let removed = self.hub.remove(dialog.get(CONFIRM_NAME))?;
        self.hub.refresh();
        let kept = if removed.leftovers.is_empty() {
            String::new()
        } else {
            format!(
                " · {} ktask file(s) could not be removed: {}",
                removed.leftovers.len(),
                removed.leftovers[0]
            )
        };
        self.message(format!(
            "Deleted project {}: {} tasks, {} attempts · its folder was not touched{kept}",
            removed.name, removed.tasks, removed.attempts
        ));
        Ok(())
    }

    fn activate(&mut self, id: i64, (backend, screen): Workspace) {
        let mut left = (
            std::mem::replace(&mut self.backend, backend),
            std::mem::replace(&mut self.screen, screen),
        );
        // Reopening returns to the screen the owner left for Projects.
        if left.1.view == View::Projects {
            left.1.view = match left.1.previous {
                View::Projects => View::Current,
                previous => previous,
            };
        }
        left.1.selection.clear();
        if let Some(previous) = self.hub.active.replace(id) {
            self.hub.parked.insert(previous, left);
        }
        self.hub.selected = Some(id);
        self.screen.reconcile();
        let name = self.project_name.clone();
        self.message(format!(
            "Opened {name} · 6 Projects lists every project; runs continue in the background"
        ));
    }

    pub(crate) fn project_controls(&self) -> Vec<(&'static str, Action)> {
        let mut controls = Vec::new();
        if !self.hub.rows.is_empty() {
            controls.push(("Enter Open", Action::OpenProject));
        }
        if self.hub.unregistered.is_some() {
            controls.push(("n Add this folder", Action::RegisterProject));
        }
        if self.hub.selected_row().is_some() {
            controls.push(("m Move", Action::MoveProject));
            controls.push(("D Delete", Action::DeleteProject));
        }
        controls
    }
}
