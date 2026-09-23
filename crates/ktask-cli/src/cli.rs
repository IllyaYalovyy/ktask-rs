//! The command-line argument model: every global option and every command
//! documented in `docs/CONTRACT.md` sections 2 and 3, as a `clap` derive.
//!
//! This module only parses. Dispatching a parsed [`Command`] to behavior is
//! later work; the contract this module is responsible for is "the whole
//! command surface parses" — `--help` lists every command, every documented
//! invocation parses into the fields it names, and an unknown flag is a
//! usage error (exit 2, enforced by `clap` itself).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use ktask_core::{GateKind, TaskId};
use serde::Serialize;

/// `ktask-rs`: a supervisor for unattended AI coding work.
///
/// Global options are accepted by every command and may appear before or
/// after it (docs/CONTRACT.md section 2).
#[derive(Debug, Parser, Serialize)]
#[command(name = "ktask-rs", version, about, propagate_version = true)]
pub(crate) struct Cli {
    /// Operate on this project instead of discovering one.
    #[arg(long, global = true, value_name = "PATH")]
    pub(crate) project: Option<PathBuf>,

    /// How results are rendered, flattened so neither this struct nor
    /// [`OutputOptions`] crosses clippy's excessive-bools threshold.
    #[command(flatten)]
    pub(crate) output: OutputOptions,

    /// Diagnostic detail on stderr.
    #[arg(long, global = true)]
    pub(crate) verbose: bool,

    /// Suppress non-essential stderr.
    #[arg(long, global = true)]
    pub(crate) quiet: bool,

    /// The command to run.
    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The two flags that change how a result is rendered rather than what is
/// done.
#[derive(Debug, Args, Serialize)]
pub(crate) struct OutputOptions {
    /// Machine-readable output on stdout.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Disable styling (also honors `NO_COLOR`).
    #[arg(long, global = true)]
    pub(crate) no_color: bool,
}

/// Every command in `docs/CONTRACT.md` section 3.
#[derive(Debug, Subcommand, Serialize)]
#[command(rename_all = "kebab-case")]
pub(crate) enum Command {
    /// Checks provider availability, git, toolchain, filesystem permissions
    /// and state directory health.
    Doctor,

    /// Registers the current repository.
    Init,

    /// Opens `$EDITOR` with a task template, or reads one from `--file`.
    Add {
        /// Read the task from this file instead of opening `$EDITOR`.
        #[arg(long)]
        file: Option<PathBuf>,
    },

    /// Validates the whole queue without running anything.
    Plan {
        /// The `plan` subcommand.
        #[command(subcommand)]
        command: PlanCommand,
    },

    /// The headless dashboard: one line per task, then a summary by state.
    Status,

    /// Drains the queue in order, strictly serially.
    Run {
        /// Run exactly one task.
        #[arg(long, value_parser = parse_task_id, conflicts_with = "from")]
        task: Option<TaskId>,

        /// Start at this task id.
        #[arg(long, value_parser = parse_task_id)]
        from: Option<TaskId>,
    },

    /// Continues from the first task that is not done.
    Resume,

    /// Starts a fresh remediation attempt seeded with the failure bundle.
    Retry {
        /// The task to retry.
        #[arg(long, value_parser = parse_task_id)]
        task: TaskId,
    },

    /// Answers a `waiting_input` question.
    Resolve {
        /// The task that is waiting for input.
        #[arg(long, value_parser = parse_task_id)]
        task: TaskId,

        /// The answer. Without this, opens `$EDITOR`.
        #[arg(long)]
        note: Option<String>,
    },

    /// Passes a human gate.
    Ack {
        /// The task whose gate to acknowledge.
        #[arg(long, value_parser = parse_task_id)]
        task: Option<TaskId>,
    },

    /// Stops the queue after the current task reaches a safe boundary.
    Pause,

    /// Terminates the running attempt now, leaving durable resumable state.
    Interrupt,

    /// Marks a task cancelled so the queue may proceed past it.
    Cancel {
        /// The task to cancel.
        #[arg(long, value_parser = parse_task_id)]
        task: TaskId,
    },

    /// Re-runs a gate against the current worktree, discarding any cached
    /// result.
    RerunGate {
        /// The task whose worktree to run the gate against.
        #[arg(long, value_parser = parse_task_id)]
        task: TaskId,

        /// The gate to run. Without this, runs the whole completion set.
        #[arg(long, value_parser = parse_gate_kind)]
        gate: Option<GateKind>,
    },

    /// Launches the TUI. Requires a terminal.
    Tui,
}

/// The `plan` subcommands.
#[derive(Debug, Subcommand, Serialize)]
#[command(rename_all = "kebab-case")]
pub(crate) enum PlanCommand {
    /// Required sections present, `Verify` commands parseable, referenced
    /// paths existing, no duplicate ids.
    Lint,
}

/// Parses a `--task`/`--from` value into a [`TaskId`].
fn parse_task_id(raw: &str) -> Result<TaskId, String> {
    raw.parse::<u32>()
        .map(TaskId::new)
        .map_err(|e| format!("invalid task id {raw:?}: {e}"))
}

/// Parses a `--gate` value into a [`GateKind`].
fn parse_gate_kind(raw: &str) -> Result<GateKind, String> {
    match raw.to_ascii_lowercase().as_str() {
        "baseline" => Ok(GateKind::Baseline),
        "targeted" => Ok(GateKind::Targeted),
        "verify" => Ok(GateKind::Verify),
        "lint" => Ok(GateKind::Lint),
        "format" => Ok(GateKind::Format),
        "build" => Ok(GateKind::Build),
        "privacy" => Ok(GateKind::Privacy),
        other => Err(format!(
            "invalid gate kind {other:?}: expected one of baseline, targeted, \
             verify, lint, format, build, privacy"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use std::path::Path;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("ktask-rs").chain(args.iter().copied()))
            .unwrap_or_else(|e| panic!("expected {args:?} to parse, got: {e}"))
    }

    fn parse_err(args: &[&str]) -> clap::Error {
        Cli::try_parse_from(std::iter::once("ktask-rs").chain(args.iter().copied()))
            .expect_err("expected a parse error")
    }

    #[test]
    fn help_lists_every_command() {
        let help = Cli::command().render_long_help().to_string();
        for name in [
            "doctor",
            "init",
            "add",
            "plan",
            "status",
            "run",
            "resume",
            "retry",
            "resolve",
            "ack",
            "pause",
            "interrupt",
            "cancel",
            "rerun-gate",
            "tui",
        ] {
            assert!(
                help.contains(name),
                "help text missing command {name:?}:\n{help}"
            );
        }
    }

    #[test]
    fn plan_help_lists_lint() {
        let mut plan = Cli::command()
            .find_subcommand("plan")
            .expect("plan subcommand registered")
            .clone();
        let help = plan.render_long_help().to_string();
        assert!(help.contains("lint"), "plan --help missing lint:\n{help}");
    }

    #[test]
    fn doctor_parses() {
        let cli = parse(&["doctor"]);
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn init_parses() {
        let cli = parse(&["init"]);
        assert!(matches!(cli.command, Command::Init));
    }

    #[test]
    fn add_parses_without_file() {
        let cli = parse(&["add"]);
        assert!(matches!(cli.command, Command::Add { file: None }));
    }

    #[test]
    fn add_parses_with_file() {
        let cli = parse(&["add", "--file", "task.md"]);
        match cli.command {
            Command::Add { file } => assert_eq!(file, Some(PathBuf::from("task.md"))),
            other => panic!("expected Add, got {other:?}"),
        }
    }

    #[test]
    fn plan_lint_parses() {
        let cli = parse(&["plan", "lint"]);
        assert!(matches!(
            cli.command,
            Command::Plan {
                command: PlanCommand::Lint
            }
        ));
    }

    #[test]
    fn status_parses() {
        let cli = parse(&["status"]);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn run_parses_with_no_flags() {
        let cli = parse(&["run"]);
        match cli.command {
            Command::Run { task, from } => {
                assert_eq!(task, None);
                assert_eq!(from, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_with_task() {
        let cli = parse(&["run", "--task", "3"]);
        match cli.command {
            Command::Run { task, from } => {
                assert_eq!(task, Some(TaskId::new(3)));
                assert_eq!(from, None);
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_parses_with_from() {
        let cli = parse(&["run", "--from", "5"]);
        match cli.command {
            Command::Run { task, from } => {
                assert_eq!(task, None);
                assert_eq!(from, Some(TaskId::new(5)));
            }
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn run_rejects_task_and_from_together() {
        let err = parse_err(&["run", "--task", "1", "--from", "2"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resume_parses() {
        let cli = parse(&["resume"]);
        assert!(matches!(cli.command, Command::Resume));
    }

    #[test]
    fn retry_parses() {
        let cli = parse(&["retry", "--task", "7"]);
        match cli.command {
            Command::Retry { task } => assert_eq!(task, TaskId::new(7)),
            other => panic!("expected Retry, got {other:?}"),
        }
    }

    #[test]
    fn retry_requires_task() {
        let err = parse_err(&["retry"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_parses_with_note() {
        let cli = parse(&["resolve", "--task", "2", "--note", "use plan B"]);
        match cli.command {
            Command::Resolve { task, note } => {
                assert_eq!(task, TaskId::new(2));
                assert_eq!(note.as_deref(), Some("use plan B"));
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn resolve_parses_without_note() {
        let cli = parse(&["resolve", "--task", "2"]);
        match cli.command {
            Command::Resolve { task, note } => {
                assert_eq!(task, TaskId::new(2));
                assert_eq!(note, None);
            }
            other => panic!("expected Resolve, got {other:?}"),
        }
    }

    #[test]
    fn ack_parses_without_task() {
        let cli = parse(&["ack"]);
        assert!(matches!(cli.command, Command::Ack { task: None }));
    }

    #[test]
    fn ack_parses_with_task() {
        let cli = parse(&["ack", "--task", "4"]);
        match cli.command {
            Command::Ack { task } => assert_eq!(task, Some(TaskId::new(4))),
            other => panic!("expected Ack, got {other:?}"),
        }
    }

    #[test]
    fn pause_parses() {
        let cli = parse(&["pause"]);
        assert!(matches!(cli.command, Command::Pause));
    }

    #[test]
    fn interrupt_parses() {
        let cli = parse(&["interrupt"]);
        assert!(matches!(cli.command, Command::Interrupt));
    }

    #[test]
    fn cancel_parses() {
        let cli = parse(&["cancel", "--task", "9"]);
        match cli.command {
            Command::Cancel { task } => assert_eq!(task, TaskId::new(9)),
            other => panic!("expected Cancel, got {other:?}"),
        }
    }

    #[test]
    fn cancel_requires_task() {
        let err = parse_err(&["cancel"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn rerun_gate_parses_without_gate() {
        let cli = parse(&["rerun-gate", "--task", "1"]);
        match cli.command {
            Command::RerunGate { task, gate } => {
                assert_eq!(task, TaskId::new(1));
                assert_eq!(gate, None);
            }
            other => panic!("expected RerunGate, got {other:?}"),
        }
    }

    #[test]
    fn rerun_gate_parses_with_gate() {
        let cli = parse(&["rerun-gate", "--task", "1", "--gate", "verify"]);
        match cli.command {
            Command::RerunGate { task, gate } => {
                assert_eq!(task, TaskId::new(1));
                assert_eq!(gate, Some(GateKind::Verify));
            }
            other => panic!("expected RerunGate, got {other:?}"),
        }
    }

    #[test]
    fn rerun_gate_rejects_an_unknown_gate_kind() {
        let err = parse_err(&["rerun-gate", "--task", "1", "--gate", "nonsense"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn tui_parses() {
        let cli = parse(&["tui"]);
        assert!(matches!(cli.command, Command::Tui));
    }

    #[test]
    fn global_options_parse_before_the_subcommand() {
        let cli = parse(&[
            "--project",
            "/tmp/proj",
            "--json",
            "--no-color",
            "--verbose",
            "--quiet",
            "status",
        ]);
        assert_eq!(cli.project.as_deref(), Some(Path::new("/tmp/proj")));
        assert!(cli.output.json);
        assert!(cli.output.no_color);
        assert!(cli.verbose);
        assert!(cli.quiet);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn global_options_parse_after_the_subcommand() {
        let cli = parse(&["status", "--json", "--verbose"]);
        assert!(cli.output.json);
        assert!(cli.verbose);
    }

    #[test]
    fn missing_project_is_none() {
        let cli = parse(&["status"]);
        assert_eq!(cli.project, None);
    }

    #[test]
    fn an_unknown_flag_exits_with_the_usage_error_code() {
        let err = parse_err(&["status", "--bogus"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn an_unknown_command_exits_with_the_usage_error_code() {
        let err = parse_err(&["not-a-command"]);
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn a_non_numeric_task_id_exits_with_the_usage_error_code() {
        let err = parse_err(&["retry", "--task", "abc"]);
        assert_eq!(err.exit_code(), 2);
    }
}
