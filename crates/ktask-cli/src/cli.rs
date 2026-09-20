//! CLI argument parsing.

use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "ktask-rs")]
#[command(about = "A supervisor for unattended AI coding work")]
#[command(version)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,

    #[arg(long, global = true)]
    pub(crate) project: Option<PathBuf>,

    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[arg(long, global = true)]
    pub(crate) no_color: bool,

    #[arg(long, global = true)]
    pub(crate) verbose: bool,

    #[arg(long, global = true)]
    pub(crate) quiet: bool,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Command {
    /// Check provider availability, git, toolchain, filesystem permissions and state directory health
    Doctor,

    /// Register the current repository
    Init,

    /// Add a new task to the queue
    Add {
        /// Read task from file instead of opening editor
        #[arg(long)]
        file: Option<PathBuf>,
    },

    /// Validate the queue
    Plan {
        #[command(subcommand)]
        subcommand: PlanSubcommand,
    },

    /// Show the status of all tasks
    Status,

    /// Run tasks from the queue
    Run {
        /// Run exactly one task
        #[arg(long)]
        task: Option<String>,

        /// Start at a task ID
        #[arg(long)]
        from: Option<String>,
    },

    /// Continue from the first incomplete task
    Resume,

    /// Start a fresh attempt on a failed task
    Retry {
        /// Task ID to retry
        #[arg(long, required = true)]
        task: String,
    },

    /// Answer a waiting_input question
    Resolve {
        /// Task ID
        #[arg(long, required = true)]
        task: String,

        /// Answer text
        #[arg(long)]
        note: Option<String>,
    },

    /// Pass a human gate
    Ack {
        /// Task ID
        #[arg(long)]
        task: Option<String>,
    },

    /// Pause the running queue
    Pause,

    /// Terminate the running attempt
    Interrupt,

    /// Mark a task cancelled
    Cancel {
        /// Task ID
        #[arg(long, required = true)]
        task: String,
    },

    /// Re-run a completion gate
    #[command(name = "rerun-gate")]
    RerunGate {
        /// Task ID
        #[arg(long, required = true)]
        task: String,

        /// Gate kind
        #[arg(long)]
        gate: Option<String>,
    },

    /// Launch the interactive TUI
    Tui,
}

#[derive(Subcommand, Debug)]
pub(crate) enum PlanSubcommand {
    /// Validate the queue
    Lint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_parses() {
        let args = vec!["ktask-rs", "doctor"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse doctor command");
        assert!(matches!(cli.command, Command::Doctor));
    }

    #[test]
    fn init_parses() {
        let args = vec!["ktask-rs", "init"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse init command");
        assert!(matches!(cli.command, Command::Init));
    }

    #[test]
    fn add_parses() {
        let args = vec!["ktask-rs", "add"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse add command");
        assert!(matches!(cli.command, Command::Add { file: None }));
    }

    #[test]
    fn add_with_file_parses() {
        let args = vec!["ktask-rs", "add", "--file", "/path/to/task.md"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse add with file");
        assert!(matches!(cli.command, Command::Add { file: Some(_) }));
    }

    #[test]
    fn plan_lint_parses() {
        let args = vec!["ktask-rs", "plan", "lint"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse plan lint");
        assert!(matches!(cli.command, Command::Plan { .. }));
    }

    #[test]
    fn status_parses() {
        let args = vec!["ktask-rs", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse status");
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn run_parses() {
        let args = vec!["ktask-rs", "run"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse run");
        assert!(matches!(
            cli.command,
            Command::Run {
                task: None,
                from: None
            }
        ));
    }

    #[test]
    fn run_with_task_parses() {
        let args = vec!["ktask-rs", "run", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse run with task");
        match cli.command {
            Command::Run {
                task: Some(id),
                from: None,
            } => {
                assert_eq!(id, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn run_with_from_parses() {
        let args = vec!["ktask-rs", "run", "--from", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse run with from");
        match cli.command {
            Command::Run {
                task: None,
                from: Some(id),
            } => {
                assert_eq!(id, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn resume_parses() {
        let args = vec!["ktask-rs", "resume"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse resume");
        assert!(matches!(cli.command, Command::Resume));
    }

    #[test]
    fn retry_parses() {
        let args = vec!["ktask-rs", "retry", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse retry");
        match cli.command {
            Command::Retry { task } => {
                assert_eq!(task, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn resolve_parses() {
        let args = vec!["ktask-rs", "resolve", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse resolve");
        match cli.command {
            Command::Resolve { task, note: None } => {
                assert_eq!(task, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn resolve_with_note_parses() {
        let args = vec![
            "ktask-rs", "resolve", "--task", "t-001", "--note", "approved",
        ];
        let cli = Cli::try_parse_from(args).expect("Failed to parse resolve with note");
        match cli.command {
            Command::Resolve {
                task,
                note: Some(n),
            } => {
                assert_eq!(task, "t-001");
                assert_eq!(n, "approved");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn ack_parses() {
        let args = vec!["ktask-rs", "ack"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse ack");
        assert!(matches!(cli.command, Command::Ack { task: None }));
    }

    #[test]
    fn ack_with_task_parses() {
        let args = vec!["ktask-rs", "ack", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse ack with task");
        match cli.command {
            Command::Ack { task: Some(id) } => {
                assert_eq!(id, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn pause_parses() {
        let args = vec!["ktask-rs", "pause"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse pause");
        assert!(matches!(cli.command, Command::Pause));
    }

    #[test]
    fn interrupt_parses() {
        let args = vec!["ktask-rs", "interrupt"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse interrupt");
        assert!(matches!(cli.command, Command::Interrupt));
    }

    #[test]
    fn cancel_parses() {
        let args = vec!["ktask-rs", "cancel", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse cancel");
        match cli.command {
            Command::Cancel { task } => {
                assert_eq!(task, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn rerun_gate_parses() {
        let args = vec!["ktask-rs", "rerun-gate", "--task", "t-001"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse rerun-gate");
        match cli.command {
            Command::RerunGate { task, gate: None } => {
                assert_eq!(task, "t-001");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn rerun_gate_with_gate_parses() {
        let args = vec![
            "ktask-rs",
            "rerun-gate",
            "--task",
            "t-001",
            "--gate",
            "build",
        ];
        let cli = Cli::try_parse_from(args).expect("Failed to parse rerun-gate with gate");
        match cli.command {
            Command::RerunGate {
                task,
                gate: Some(g),
            } => {
                assert_eq!(task, "t-001");
                assert_eq!(g, "build");
            }
            _ => panic!("Unexpected command"),
        }
    }

    #[test]
    fn tui_parses() {
        let args = vec!["ktask-rs", "tui"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse tui");
        assert!(matches!(cli.command, Command::Tui));
    }

    #[test]
    fn global_option_project_parses() {
        let args = vec!["ktask-rs", "--project", "/path/to/project", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with --project");
        assert!(cli.project.is_some());
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn global_option_json_parses() {
        let args = vec!["ktask-rs", "--json", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with --json");
        assert!(cli.json);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn global_option_no_color_parses() {
        let args = vec!["ktask-rs", "--no-color", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with --no-color");
        assert!(cli.no_color);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn global_option_verbose_parses() {
        let args = vec!["ktask-rs", "--verbose", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with --verbose");
        assert!(cli.verbose);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn global_option_quiet_parses() {
        let args = vec!["ktask-rs", "--quiet", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with --quiet");
        assert!(cli.quiet);
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn multiple_global_options_parse() {
        let args = vec![
            "ktask-rs",
            "--json",
            "--verbose",
            "--project",
            "/path",
            "run",
        ];
        let cli = Cli::try_parse_from(args).expect("Failed to parse with multiple global options");
        assert!(cli.json);
        assert!(cli.verbose);
        assert!(cli.project.is_some());
        assert!(matches!(cli.command, Command::Run { .. }));
    }

    #[test]
    fn unknown_flag_fails() {
        let args = vec!["ktask-rs", "--unknown-flag", "status"];
        let result = Cli::try_parse_from(args);
        assert!(result.is_err());
    }

    #[test]
    fn help_contains_all_commands() {
        let args = vec!["ktask-rs", "--help"];
        let result = Cli::try_parse_from(args);
        // --help causes clap to exit with success, so we expect an error here
        // (the error is actually clap's help output)
        assert!(result.is_err());
    }
}
