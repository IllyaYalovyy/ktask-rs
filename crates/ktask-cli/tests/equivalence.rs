//! Equivalence of the two interfaces (docs/CONTRACT.md section 0, rule 1):
//! every operation the TUI performs is a CLI command, and every CLI command
//! that changes what the supervisor does is an operation the TUI offers.
//! Neither interface can drift ahead of the other without this file failing.
//!
//! The TUI's operations are two types: [`Action`] changes what the
//! supervisor does and needs a command; [`ViewOp`] changes only what the
//! interface shows and needs none. The split is the type system's, so a view
//! operation cannot be added to the table and an action cannot hide among the
//! view operations.
//!
//! `ktask-cli` has no library target, so the command set is read from the
//! compiled binary's own `--help`, which `clap` generates from the same
//! definition that parses arguments.

use std::collections::BTreeSet;
use std::io;
use std::process::Command;

use ktask_core::{GateKind, TaskId};
use ktask_tui::{Action, ViewOp};

/// How many variants [`Action`] has. [`slot`] is an exhaustive `match`, so a
/// new variant stops this file compiling until it is given a slot; the
/// count then has to be raised, and [`table`] needs a row for the new slot.
const ACTION_SLOTS: usize = 8;

/// The slot an [`Action`] variant occupies, `0..ACTION_SLOTS`. Deliberately
/// has no wildcard arm.
fn slot(action: &Action) -> usize {
    match action {
        Action::Pause => 0,
        Action::Interrupt => 1,
        Action::Resume => 2,
        Action::Retry { .. } => 3,
        Action::Resolve { .. } => 4,
        Action::Acknowledge { .. } => 5,
        Action::Cancel { .. } => 6,
        Action::RerunGate { .. } => 7,
    }
}

/// Every TUI action, paired with the `ktask-rs` command that performs it.
/// The command names are written out here rather than read from
/// [`Action::command`], so the TUI's own answer is checked against them.
fn table() -> Vec<(Action, &'static str)> {
    let task = TaskId::new(1);
    vec![
        (Action::Pause, "pause"),
        (Action::Interrupt, "interrupt"),
        (Action::Resume, "resume"),
        (Action::Retry { task }, "retry"),
        (
            Action::Resolve {
                task,
                note: "use sqlite".into(),
            },
            "resolve",
        ),
        (Action::Acknowledge { task: None }, "ack"),
        (Action::Cancel { task }, "cancel"),
        (
            Action::RerunGate {
                task,
                gate: Some(GateKind::Lint),
            },
            "rerun-gate",
        ),
    ]
}

/// The commands that exist for a script and have no TUI action: they start,
/// inspect or set up the supervisor rather than steer a run in progress. The
/// TUI shows their results (`status`, `doctor`) or is them (`tui`).
const CLI_ONLY: &[&str] = &["doctor", "init", "add", "plan", "status", "run", "tui"];

/// Every view operation, by name.
fn view_ops() -> Vec<ViewOp> {
    vec![
        ViewOp::Attach,
        ViewOp::OpenDiff {
            task: TaskId::new(1),
        },
    ]
}

/// The command names in the "Commands:" section of `ktask-rs --help`,
/// without clap's own `help` subcommand.
fn commands_in(help: &str) -> BTreeSet<String> {
    help.lines()
        .skip_while(|line| line.trim_end() != "Commands:")
        .skip(1)
        .take_while(|line| line.starts_with(' '))
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

/// Everything wrong with the pairing of `actions` (each with the command that
/// performs it) against `commands`, the CLI's command set, once `cli_only`
/// commands are set aside. Empty when the interfaces are equivalent.
fn drift(
    actions: &[(String, String)],
    commands: &BTreeSet<String>,
    cli_only: &[&str],
) -> Vec<String> {
    let mut problems = Vec::new();
    for (action, command) in actions {
        if !commands.contains(command) {
            problems.push(format!(
                "action {action} names command {command:?}, which the CLI does not have"
            ));
        }
        if cli_only.contains(&command.as_str()) {
            problems.push(format!(
                "command {command:?} is listed as CLI-only but action {action} performs it"
            ));
        }
    }
    let performed: BTreeSet<&str> = actions.iter().map(|(_, c)| c.as_str()).collect();
    for command in commands {
        if !performed.contains(command.as_str()) && !cli_only.contains(&command.as_str()) {
            problems.push(format!(
                "command {command:?} has no TUI action and is not listed as CLI-only"
            ));
        }
    }
    for name in cli_only {
        if !commands.contains(*name) {
            problems.push(format!(
                "CLI-only command {name:?} does not exist in the CLI"
            ));
        }
    }
    problems
}

/// The CLI's command set, from the compiled binary.
fn cli_commands() -> io::Result<BTreeSet<String>> {
    let output = Command::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .arg("--help")
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other("ktask-rs --help failed"));
    }
    let help = String::from_utf8(output.stdout).map_err(io::Error::other)?;
    let commands = commands_in(&help);
    if commands.len() < 2 {
        return Err(io::Error::other(format!(
            "no commands found in --help output:\n{help}"
        )));
    }
    Ok(commands)
}

fn rows() -> Vec<(String, String)> {
    table()
        .into_iter()
        .map(|(action, command)| (format!("{action:?}"), command.to_owned()))
        .collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

fn row(action: &str, command: &str) -> (String, String) {
    (action.to_owned(), command.to_owned())
}

#[test]
fn equivalence_table_has_one_row_for_every_action() {
    let mut seen = vec![0_usize; ACTION_SLOTS];
    for (action, _) in table() {
        seen[slot(&action)] += 1;
    }
    assert_eq!(
        seen,
        vec![1; ACTION_SLOTS],
        "each Action variant needs exactly one row in the table"
    );
}

#[test]
fn equivalence_table_agrees_with_the_tui_about_each_command() {
    for (action, command) in table() {
        assert_eq!(action.command(), command, "{action:?}");
    }
}

#[test]
fn equivalence_every_action_is_a_command_and_every_command_is_classified() {
    let problems = drift(
        &rows(),
        &cli_commands().expect("read the CLI's command set"),
        CLI_ONLY,
    );
    assert!(problems.is_empty(), "interfaces drifted: {problems:#?}");
}

#[test]
fn equivalence_every_listed_command_answers_help() {
    let mut listed: BTreeSet<&str> = table().iter().map(|(_, c)| *c).collect();
    listed.extend(CLI_ONLY.iter().copied());
    for command in listed {
        let output = Command::new(env!("CARGO_BIN_EXE_ktask-rs"))
            .args([command, "--help"])
            .output()
            .expect("run ktask-rs <command> --help");
        assert!(
            output.status.success(),
            "`ktask-rs {command} --help` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn equivalence_view_operations_are_not_commands() {
    let commands = cli_commands().expect("read the CLI's command set");
    for op in view_ops() {
        assert!(
            !commands.contains(op.name()),
            "view operation {} has a CLI command; it is either an action or the command is stray",
            op.name()
        );
    }
}

#[test]
fn equivalence_an_action_without_a_command_is_drift() {
    let mut actions = rows();
    actions.push(row("Snooze", "snooze"));
    let problems = drift(
        &actions,
        &cli_commands().expect("read the CLI's command set"),
        CLI_ONLY,
    );
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("Snooze"), "{problems:#?}");
    assert!(problems[0].contains("snooze"), "{problems:#?}");
}

#[test]
fn equivalence_a_command_without_an_action_is_drift() {
    let mut commands = cli_commands().expect("read the CLI's command set");
    commands.insert("snooze".to_owned());
    let problems = drift(&rows(), &commands, CLI_ONLY);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("snooze"), "{problems:#?}");
}

#[test]
fn equivalence_an_action_whose_command_was_removed_is_drift() {
    let mut commands = cli_commands().expect("read the CLI's command set");
    commands.remove("cancel");
    let problems = drift(&rows(), &commands, CLI_ONLY);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("Cancel"), "{problems:#?}");
}

#[test]
fn equivalence_a_stale_cli_only_entry_is_drift() {
    let problems = drift(
        &rows(),
        &cli_commands().expect("read the CLI's command set"),
        &[
            "doctor", "init", "add", "plan", "status", "run", "tui", "vanished",
        ],
    );
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("vanished"), "{problems:#?}");
}

#[test]
fn equivalence_a_command_cannot_be_both_an_action_and_cli_only() {
    let problems = drift(&[row("Pause", "pause")], &set(&["pause"]), &["pause"]);
    assert_eq!(problems.len(), 1, "{problems:#?}");
    assert!(problems[0].contains("CLI-only"), "{problems:#?}");
}

#[test]
fn equivalence_consistent_sets_have_no_drift() {
    let problems = drift(
        &[row("Pause", "pause")],
        &set(&["pause", "status"]),
        &["status"],
    );
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn equivalence_help_parsing_reads_only_the_commands_section() {
    let help = "Usage: x\n\nCommands:\n  one    First\n  two    Second\n  help   Print\n\nOptions:\n  --json  Machine\n";
    assert_eq!(commands_in(help), set(&["one", "two"]));
}
