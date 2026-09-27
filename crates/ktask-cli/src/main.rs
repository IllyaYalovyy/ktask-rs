//! The `ktask-rs` binary: argument parsing, wiring, exit codes.
//!
//! This is the only place where adapters are chosen and wired to the core.

mod render;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use ktask_adapters::{
    CommandEditor, GitCli, SqliteJournal, SqliteRegistry, SystemClock, journal_path, read_text,
    registry_path,
};
use ktask_core::{
    AddError, CancelError, EditAddError, Edited, ImportError, Placement, Project, RegisterError,
    ResolveError, TaskDraft, TaskId, TaskKind,
};

/// Runs an ordered queue of software tasks through AI coding agents.
#[derive(Debug, Parser)]
#[command(name = "ktask-rs", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The projects the tool knows about
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
    },
    /// Add a task, at the end of the queue unless told where, and print its ID
    ///
    /// Without --title and --criterion the task is written in $EDITOR.
    Add {
        /// One line saying what the task is
        #[arg(long, requires = "criterion")]
        title: Option<String>,
        /// What must be true for the task to be done; repeat for each criterion
        #[arg(long, value_name = "CRITERION", requires = "title")]
        criterion: Vec<String>,
        /// The longer description of the task
        #[arg(long, requires = "title")]
        body: Option<String>,
        /// Who does the task: agent or human [default: agent]
        #[arg(long, value_parser = str::parse::<TaskKind>, requires = "title")]
        kind: Option<TaskKind>,
        /// A related task or page: github:owner/repo#NUMBER or an http(s) URL; repeat for
        /// each link
        #[arg(long, value_name = "REF", requires = "title")]
        link: Vec<String>,
        /// Put the task immediately before the task with this ID
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        before: Option<u64>,
        /// Put the task immediately after the task with this ID
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
    },
    /// Add the tasks of a JSON array, in order and all or none, and print their IDs
    ///
    /// Each task has the authored fields `list --json` prints: title, body, criteria, kind
    /// and links. Only title and criteria are required.
    Import {
        /// The JSON file to read, or - for standard input
        #[arg(value_name = "FILE")]
        file: String,
        /// Put the tasks immediately before the task with this ID
        #[arg(long, value_name = "ID", conflicts_with = "after")]
        before: Option<u64>,
        /// Put the tasks immediately after the task with this ID
        #[arg(long, value_name = "ID")]
        after: Option<u64>,
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
    },
    /// Remove a task from the queue: it is cancelled, and stays in the journal
    Remove {
        /// The ID of the task to remove
        #[arg(value_name = "ID")]
        id: u64,
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
    },
    /// List the queue in order, without the tasks that were removed
    List {
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
        /// Show the tasks that were removed too, with status cancelled
        #[arg(long)]
        all: bool,
        /// Print a JSON array with every field of every task instead of one line per task
        #[arg(long)]
        json: bool,
    },
    /// Open the terminal interface on the project's queue
    Tui {
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectCommand {
    /// List the registered projects
    List {
        /// Print a JSON array instead of one line per project
        #[arg(long)]
        json: bool,
    },
    /// Show the project the current directory belongs to, registering it on first use
    Show {
        /// Work on this registered project instead of the one the current directory is in
        #[arg(long, value_name = "NAME")]
        project: Option<String>,
        /// Print a JSON object instead of one line
        #[arg(long)]
        json: bool,
    },
    /// Register the current directory under a name of your choosing
    Register {
        /// The name to register the project under
        #[arg(long, value_name = "NAME")]
        name: String,
        /// Print a JSON object instead of one line
        #[arg(long)]
        json: bool,
    },
}

/// Why a command failed, and the exit code to report it with.
#[derive(Debug)]
struct Failure {
    message: String,
    code: u8,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self { message, code: 1 }
    }
}

impl From<ResolveError> for Failure {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::UnknownProject(_) => Self {
                message: format!("{error}; `ktask-rs project list` shows the registered projects"),
                code: 2,
            },
            ResolveError::NameTaken { ref path, .. } => Self {
                message: format!(
                    "{error}; to register {} under a different name, run in that directory: \
                     ktask-rs project register --name <NAME>",
                    path.display()
                ),
                code: 2,
            },
            _ => Self::from(error.to_string()),
        }
    }
}

impl From<RegisterError> for Failure {
    fn from(error: RegisterError) -> Self {
        match error {
            RegisterError::Registry(_) | RegisterError::Git(_) => Self::from(error.to_string()),
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<AddError> for Failure {
    fn from(error: AddError) -> Self {
        match error {
            AddError::Journal(_) => Self::from(error.to_string()),
            AddError::UnknownTask(_) | AddError::CancelledTask(_) => Self {
                message: format!(
                    "{error}; `ktask-rs list` shows the tasks a new one can be placed next to"
                ),
                code: 2,
            },
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<CancelError> for Failure {
    fn from(error: CancelError) -> Self {
        match error {
            CancelError::Journal(_) => Self::from(error.to_string()),
            CancelError::UnknownTask(_) | CancelError::AlreadyCancelled(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
        }
    }
}

impl From<EditAddError> for Failure {
    fn from(error: EditAddError) -> Self {
        match error {
            EditAddError::Add(error) => Self::from(error),
            EditAddError::Editor(_) | EditAddError::Invalid(_) => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<ImportError> for Failure {
    fn from(error: ImportError) -> Self {
        match error {
            ImportError::Add(error) => Self::from(error),
            ImportError::Malformed(_) | ImportError::NotAnArray | ImportError::Invalid(_) => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let mut stdout = io::stdout().lock();
    match run(&cli.command, &mut stdout) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // Nowhere left to report a failure to write to standard error.
            let _ = writeln!(io::stderr(), "ktask-rs: {}", failure.message);
            ExitCode::from(failure.code)
        }
    }
}

fn run(command: &Command, stdout: &mut impl Write) -> Result<(), Failure> {
    match command {
        Command::Project {
            command: ProjectCommand::List { json },
        } => {
            let registry = open_registry()?;
            let projects = ktask_core::list_projects(&registry).map_err(|e| e.to_string())?;
            Ok(render::projects(&projects, *json, stdout)?)
        }
        Command::Project {
            command: ProjectCommand::Show { project, json },
        } => {
            let registry = open_registry()?;
            let project = resolve(&registry, project.as_deref())?;
            Ok(render::project(&project, *json, stdout)?)
        }
        Command::Tui { project } => tui(project.as_deref()),
        Command::Add {
            title,
            criterion,
            body,
            kind,
            link,
            before,
            after,
            project,
        } => {
            // Checked before anything is registered or opened, so that a missing editor
            // changes nothing.
            let content = match title {
                Some(title) => Content::Written(TaskDraft {
                    title: title.clone(),
                    body: body.clone().unwrap_or_default(),
                    criteria: criterion.clone(),
                    kind: kind.unwrap_or_default(),
                    links: link.clone(),
                }),
                None => Content::Editor(editor_from_environment()?),
            };
            let journal = open_queue(project.as_deref())?;
            let placement = placement(*before, *after);
            let task = match content {
                Content::Written(draft) => {
                    ktask_core::add_task(&journal, &SystemClock, &draft, placement)?
                }
                Content::Editor(editor) => {
                    match ktask_core::add_task_in_editor(
                        &journal,
                        &SystemClock,
                        &editor,
                        placement,
                    )? {
                        Edited::Added(task) => task,
                        Edited::Unchanged => {
                            return Ok(render::nothing_added("left unchanged", &mut io::stderr())?);
                        }
                        Edited::Emptied => {
                            return Ok(render::nothing_added("emptied", &mut io::stderr())?);
                        }
                    }
                }
            };
            Ok(render::added(&task, stdout)?)
        }
        Command::Import {
            file,
            before,
            after,
            project,
        } => {
            // Read before anything is registered or opened, so that a missing file changes
            // nothing.
            let json = read_text(file).map_err(|message| Failure { message, code: 2 })?;
            let journal = open_queue(project.as_deref())?;
            let tasks = ktask_core::import_tasks(
                &journal,
                &SystemClock,
                &json,
                placement(*before, *after),
            )?;
            tasks
                .iter()
                .try_for_each(|task| render::added(task, stdout))
                .map_err(Failure::from)
        }
        Command::Remove { id, project } => {
            let journal = open_queue(project.as_deref())?;
            ktask_core::remove_task(&journal, &SystemClock, TaskId(*id))?;
            Ok(render::removed(TaskId(*id), stdout)?)
        }
        Command::List { project, all, json } => {
            let journal = open_queue(project.as_deref())?;
            let tasks = if *all {
                ktask_core::list_all_tasks(&journal)
            } else {
                ktask_core::list_tasks(&journal)
            }
            .map_err(|e| e.to_string())?;
            Ok(render::tasks(&tasks, *json, stdout)?)
        }
        Command::Project {
            command: ProjectCommand::Register { name, json },
        } => {
            let registry = open_registry()?;
            let cwd = current_dir()?;
            let project =
                ktask_core::register_project(&registry, &GitCli, &SystemClock, &cwd, name)?;
            Ok(render::project(&project, *json, stdout)?)
        }
    }
}

/// Where `--before` and `--after` put new tasks.
fn placement(before: Option<u64>, after: Option<u64>) -> Placement {
    match (before, after) {
        (Some(id), _) => Placement::Before(TaskId(id)),
        (None, Some(id)) => Placement::After(TaskId(id)),
        (None, None) => Placement::End,
    }
}

/// The journal of the project a command works on.
fn open_queue(selected: Option<&str>) -> Result<SqliteJournal, Failure> {
    let registry = open_registry()?;
    let project = resolve(&registry, selected)?;
    Ok(open_journal(&project)?)
}

/// The project a command works on, telling on standard error when that registered it.
/// Opens the terminal interface on the queue of the project selected, or the current one.
fn tui(selected: Option<&str>) -> Result<(), Failure> {
    if !io::stdout().is_terminal() {
        return Err(Failure {
            message: "the terminal interface needs a terminal; \
                      `ktask-rs list` shows the queue without one"
                .to_owned(),
            code: 2,
        });
    }
    let registry = open_registry()?;
    let project = resolve(&registry, selected)?;
    let journal = open_journal(&project)?;
    Ok(ktask_tui::run(|show_cancelled| {
        ktask_core::queue_view(project.clone(), &journal, show_cancelled).map_err(|e| e.to_string())
    })?)
}

fn resolve(registry: &SqliteRegistry, selected: Option<&str>) -> Result<Project, Failure> {
    let cwd = current_dir()?;
    let resolution = ktask_core::resolve_project(registry, &GitCli, &SystemClock, &cwd, selected)?;
    if resolution.registered {
        render::registered(&resolution.project, &mut io::stderr())?;
    }
    Ok(resolution.project)
}

/// Where a new task's content comes from.
enum Content {
    /// The command line gave it.
    Written(TaskDraft),
    /// The person writes it in this editor.
    Editor(CommandEditor),
}

/// The editor `$EDITOR` names.
fn editor_from_environment() -> Result<CommandEditor, Failure> {
    match std::env::var("EDITOR") {
        Ok(command) if !command.trim().is_empty() => Ok(CommandEditor::new(command)),
        _ => Err(Failure {
            message: "$EDITOR is not set, so there is nothing to write the task in; set it \
                      (for example EDITOR=vi) or give the task with --title and --criterion"
                .to_owned(),
            code: 2,
        }),
    }
}

fn open_registry() -> Result<SqliteRegistry, String> {
    let path = registry_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")).ok_or(
        "cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path",
    )?;
    SqliteRegistry::open(&path).map_err(|e| e.to_string())
}

fn open_journal(project: &Project) -> Result<SqliteJournal, String> {
    let path = journal_path(
        std::env::var_os("XDG_STATE_HOME"),
        std::env::var_os("HOME"),
        &project.name,
    )
    .ok_or("cannot locate the state directory: set XDG_STATE_HOME or HOME to an absolute path")?;
    SqliteJournal::open(&path).map_err(|e| e.to_string())
}

fn current_dir() -> Result<PathBuf, String> {
    std::env::current_dir().map_err(|e| format!("cannot find the current directory: {e}"))
}
