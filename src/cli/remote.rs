//! Task commands on a linked host. The parent forwards the command line it was given and
//! shows what came back, never opening anything of the child's. The child runs that line as
//! a command of its own, with its own checks, after deciding here that it accepts it.
use super::{
    Failure, Result,
    args::{Cli, Command},
    validate,
};
use crate::{
    application::Service,
    instance::{Instance, Paths},
    link::{self, Called},
    storage::Database,
};
use std::{
    ffi::OsString,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// The host is offline or incompatible: nothing was sent.
const UNAVAILABLE: u8 = 5;
/// The command was sent and its answer never came.
const UNKNOWN: u8 = 6;

/// A command a parent may run on its child.
pub(super) struct Routed {
    pub name: &'static str,
    pub task: Option<i64>,
    /// Whether it changes anything, which a read-only link refuses.
    pub change: bool,
}

/// The one list of what crosses a link. Everything else, project deletion and moves,
/// shutdown, tools and provisioning included, is refused on both sides.
fn routed(cli: &Cli) -> Option<Routed> {
    if cli.paths || cli.demo || cli.worker_token.is_some() || cli.project_name.is_some() {
        return None;
    }
    let (name, task, change) = match cli.command.as_ref()? {
        Command::List { .. } => ("list", None, false),
        Command::Current => ("current", None, false),
        Command::Show { id } => ("show", Some(*id), false),
        Command::Events { .. } => ("events", None, false),
        Command::Stats => ("stats", None, false),
        Command::Add(_) => ("add", None, true),
        Command::Edit {
            target, changes, ..
        } => ("edit", Some(target.id), !changes.preview_project_defaults),
        Command::Move { target, .. } => ("move", Some(target.id), true),
        // The reads of a parent's TUI.
        Command::Screen { .. } => ("screen", None, false),
        Command::Approve { target } => ("approve", Some(target.id), true),
        Command::Reject { target, .. } => ("reject", Some(target.id), true),
        // The hidden flags belong to a supervisor that this host starts itself.
        Command::Run {
            notify: false,
            continue_session: false,
            ..
        } => ("run", None, true),
        Command::Pause { all: false } => ("pause", None, true),
        Command::Stop { target, .. } => ("stop", Some(target.id), true),
        Command::Restart { target, .. } => ("restart", Some(target.id), true),
        Command::Done { target, .. } => ("done", Some(target.id), true),
        Command::Cancel { target, .. } => ("cancel", Some(target.id), true),
        Command::Answer { target, .. } => ("answer", Some(target.id), true),
        _ => return None,
    };
    Some(Routed { name, task, change })
}

fn not_accepted() -> Failure {
    Failure::usage(
        "Only list, current, show, events, add, edit, move, approve, reject, run, pause (not \
         --all), stop, restart, done, cancel, answer and stats run on another host; project-delete, \
         project-move, shutdown, tool and provisioning commands are never accepted remotely",
    )
}

fn no_project() -> Failure {
    Failure::usage("--host requires --project with the project's ID or name on that host")
}

fn failure(status: u8, code: &'static str, message: String) -> Failure {
    Failure {
        status,
        code,
        message,
    }
}

/// What a child decided about a command from its parent.
pub(super) struct Admitted {
    /// The folder of the project the command named.
    pub root: PathBuf,
    /// The configured parent, recorded with whoever acted.
    pub parent: String,
    /// Whether the link may change anything here.
    pub control: bool,
    pub request: Routed,
}

impl Admitted {
    /// Who the parent says acted, and where the command came from.
    pub fn attribute(&self, actor: &mut crate::application::Actor) {
        actor.name = format!("{} via {}", actor.name, self.parent);
    }

    /// What the project's events say was asked for.
    pub fn summary(&self) -> String {
        match self.request.task {
            Some(task) => format!("{} #{task}", self.request.name),
            None => self.request.name.to_owned(),
        }
    }
}

/// The child's side, before anything the command names is touched.
pub(super) fn admit(instance: Instance, cli: &Cli, parent: &str) -> Result<Admitted> {
    let paths = instance.paths()?;
    let config = link::config(&paths)?
        .filter(|config| config.parent == parent)
        .ok_or_else(|| {
            failure(
                4,
                "refused",
                "This host accepts control only from the parent it is configured with".into(),
            )
        })?;
    let request = routed(cli).ok_or_else(not_accepted)?;
    if request.change && !config.control {
        return Err(failure(
            4,
            "read_only",
            format!(
                "The link to {} is read-only (ktask link control off there); {} was not run",
                config.name, request.name
            ),
        ));
    }
    let project = cli.project.as_deref().ok_or_else(no_project)?;
    Ok(Admitted {
        root: project_root(&paths, &config.name, project)?,
        parent: config.parent,
        control: config.control,
        request,
    })
}

/// The folder of the project with this ID or, failing that, this exact name.
fn project_root(paths: &Paths, host: &str, project: &Path) -> Result<PathBuf> {
    let registrations = match Database::open_existing(paths)? {
        Some(db) => Service::project_registrations(&db)?,
        None => Vec::new(),
    };
    let wanted = project.to_string_lossy();
    let id = wanted.parse::<i64>().ok();
    registrations
        .iter()
        .find(|(known, ..)| Some(*known) == id)
        .or_else(|| registrations.iter().find(|(_, name, _)| *name == wanted))
        .map(|(.., root)| root.clone())
        .ok_or_else(|| {
            Failure::missing(format!(
                "{host} has no project with the ID or name {wanted}; see ktask projects"
            ))
        })
}

/// The parent's side: sends the command line once and ends as the child's command ended.
pub(super) fn send(
    instance: Instance,
    cli: &Cli,
    host: &str,
    arguments: &[OsString],
) -> Result<()> {
    let request = routed(cli).ok_or_else(not_accepted)?;
    validate(cli)?;
    if cli.project.is_none() {
        return Err(no_project());
    }
    let arguments = arguments
        .iter()
        .skip(1)
        .map(|argument| argument.to_str().map(str::to_owned))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| Failure::usage("--host requires UTF-8 arguments"))?;
    let name = request.name;
    match link::call(&instance.paths()?, host, arguments)? {
        Called::Ran {
            stdout,
            stderr,
            status,
        } => {
            // A closed error stream loses diagnostics only, as for a local command.
            let _ = show(io::stderr().lock(), &stderr, cli.json);
            show(io::stdout().lock(), &stdout, cli.json).map_err(|error| match error.kind() {
                io::ErrorKind::BrokenPipe => Failure::closed_output(),
                _ => error.into(),
            })?;
            match status {
                0 => Ok(()),
                status => Err(Failure::silent(status)),
            }
        }
        Called::Unknown => Err(Failure::missing(format!(
            "No linked host is named {host}; see ktask link status"
        ))),
        Called::Offline(why) => Err(failure(
            UNAVAILABLE,
            "host_offline",
            format!("{host} is offline ({why}); {name} was not sent"),
        )),
        Called::Incompatible(why) => Err(failure(
            UNAVAILABLE,
            "host_incompatible",
            format!("{host} cannot be controlled: {why}; {name} was not sent"),
        )),
        Called::Refused(why) => Err(failure(
            4,
            "refused",
            format!("{host} did not run {name}: {why}"),
        )),
        Called::Lost => Err(failure(
            UNKNOWN,
            "outcome_unknown",
            format!(
                "Lost the connection to {host} during {name}; its outcome is unknown and \
                 nothing is retried. Read the task and events there before running it again"
            ),
        )),
    }
}

/// Writes the child's output as it printed it. A command's own output holds no other control
/// characters, so any that arrive are shown escaped instead of reaching the terminal.
fn show(mut to: impl Write, text: &str, json: bool) -> io::Result<()> {
    to.write_all(printable(text, json).as_bytes())?;
    to.flush()
}

fn printable(text: &str, json: bool) -> String {
    // JSON escapes the ASCII controls itself and leaves the others in its strings.
    let allowed = |c: char| {
        !c.is_control() || matches!(c, '\n' | '\t') || (json && (c == '\u{7f}' || !c.is_ascii()))
    };
    let mut shown = String::with_capacity(text.len());
    for c in text.chars() {
        if allowed(c) {
            shown.push(c);
        } else {
            shown.extend(c.escape_default());
        }
    }
    shown
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn route(line: &str) -> Option<(&'static str, bool)> {
        let cli = Cli::try_parse_from(line.split(' ')).expect(line);
        routed(&cli).map(|routed| (routed.name, routed.change))
    }

    #[test]
    fn only_task_commands_cross_a_link() {
        for (line, name) in [
            ("k list", "list"),
            ("k current", "current"),
            ("k show 1", "show"),
            ("k events", "events"),
            ("k stats", "stats"),
            ("k screen {}", "screen"),
        ] {
            assert_eq!(route(line), Some((name, false)), "{line}");
        }
        for (line, name) in [
            ("k add --title t --goal g --criterion c", "add"),
            ("k edit 1 --title t", "edit"),
            ("k approve 1", "approve"),
            ("k reject 1 --reason r", "reject"),
            ("k run", "run"),
            ("k resume --background", "run"),
            ("k pause", "pause"),
            ("k stop 1", "stop"),
            ("k restart 1", "restart"),
            ("k restart 1 --background", "restart"),
            ("k move 1 --before 2", "move"),
            ("k done 1", "done"),
            ("k cancel 1", "cancel"),
            ("k answer 1 --text yes", "answer"),
        ] {
            assert_eq!(route(line), Some((name, true)), "{line}");
        }
        assert_eq!(
            route("k edit 1 --preview-project-defaults"),
            Some(("edit", false))
        );
        for line in [
            "k project-delete 1 --confirm p",
            "k project-move 1 /tmp",
            "k shutdown",
            "k pause --all",
            "k tool list",
            "k tool provision-status",
            "k tool provision-mode automatic",
            "k init",
            "k check-set c --command true",
            "k link control on",
            "k link parent --clear",
            "k projects",
            "k mcp",
            "k evidence /etc/passwd",
            "k run --notify",
            "k --paths",
            "k --demo list",
            "k --project-name other add --title t --goal g --criterion c",
            "k agent list",
            "k storage --apply",
            "k delivery --mode pr",
        ] {
            assert_eq!(route(line), None, "{line}");
        }
    }

    #[test]
    fn a_child_cannot_drive_the_parents_terminal() {
        let hostile = "a\u{1b}[2J\r\u{9b}b\n\tc";
        assert_eq!(printable(hostile, false), "a\\u{1b}[2J\\r\\u{9b}b\n\tc");
        // Valid JSON of the child keeps its bytes.
        let json = serde_json::json!({"title":"del\u{7f} csi\u{9b}"}).to_string();
        assert_eq!(printable(&json, true), json);
    }
}
