//! Headless command-line entry point.
//!
//! Writing to stdout and stderr is this crate's purpose: it is the process
//! boundary where results become text. The workspace denies direct printing
//! everywhere else, so that library code returns values and errors instead of
//! emitting them, and the supervisor stays testable without capturing output.
#![allow(clippy::print_stdout, clippy::print_stderr)]

mod cli;
mod cmd;
mod exit;
mod json;
mod render;

use clap::Parser;
use cli::Cli;
use ktask_core::{RunOutcome, discover};
use std::path::Path;

fn main() {
    let cli = Cli::parse();

    if cli.verbose && cli.quiet {
        render::progress(format_args!(
            "error: --verbose and --quiet cannot be used together"
        ));
        std::process::exit(exit::code_for(&RunOutcome::Usage {
            detail: "conflicting options: --verbose and --quiet".to_string(),
        }));
    }

    render::init(cli.no_color);
    render::init_verbosity(cli.quiet, cli.verbose);

    let outcome = run_cli(cli);
    let code = exit::code_for(&outcome);
    std::process::exit(code);
}

fn run_cli(cli: Cli) -> RunOutcome {
    let project = resolve_project(cli.project.as_ref());

    match project {
        Ok(proj) => {
            let config = ktask_core::load_for(&proj).ok();
            cmd::dispatch(cli.command, Some(proj), config, cli.json)
        }
        Err(e) => {
            // Commands that don't need a project: doctor, init
            match cli.command {
                cli::Command::Doctor => cmd::doctor::run(cli.json),
                cli::Command::Init => cmd::init::run(),
                _ => {
                    render::progress(format_args!(
                        "error: no project found\nrun 'ktask-rs init' to register this repository"
                    ));
                    RunOutcome::Usage {
                        detail: format!("{e}"),
                    }
                }
            }
        }
    }
}

fn resolve_project(
    project_path: Option<&std::path::PathBuf>,
) -> ktask_core::Result<ktask_core::Project> {
    match project_path {
        Some(path) => ktask_core::register(path),
        None => discover(Path::new(".")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;

    #[test]
    fn global_options_are_honoured() {
        assert!(
            Cli::try_parse_from(vec!["ktask-rs", "--json", "status"])
                .expect("Failed to parse")
                .json
        );

        let cli = Cli::try_parse_from(vec!["ktask-rs", "--project", "/tmp", "status"])
            .expect("Failed to parse");
        assert!(cli.project.is_some());

        let cli =
            Cli::try_parse_from(vec!["ktask-rs", "--quiet", "status"]).expect("Failed to parse");
        assert!(cli.quiet);
        assert!(!cli.verbose);

        let cli =
            Cli::try_parse_from(vec!["ktask-rs", "--verbose", "status"]).expect("Failed to parse");
        assert!(cli.verbose);
        assert!(!cli.quiet);

        assert!(
            Cli::try_parse_from(vec!["ktask-rs", "--no-color", "status"])
                .expect("Failed to parse")
                .no_color
        );
    }

    #[test]
    fn project_flag_overrides_discovery() {
        let cli = Cli::try_parse_from(vec!["ktask-rs", "--project", "/explicit/path", "status"])
            .expect("Failed to parse");
        assert_eq!(cli.project.as_ref().unwrap(), Path::new("/explicit/path"));
    }

    #[test]
    fn json_flag_is_honoured() {
        let args = vec!["ktask-rs", "--json", "status"];
        let cli = Cli::try_parse_from(args).expect("Failed to parse");
        assert!(cli.json);
        assert!(matches!(cli.command, cli::Command::Status));
    }

    #[test]
    fn quiet_flag_is_honoured() {
        let cli = Cli::try_parse_from(vec!["ktask-rs", "--quiet", "run"]).expect("Failed to parse");
        assert!(cli.quiet);
        assert!(!cli.verbose);
    }

    #[test]
    fn verbose_flag_is_honoured() {
        let cli =
            Cli::try_parse_from(vec!["ktask-rs", "--verbose", "run"]).expect("Failed to parse");
        assert!(cli.verbose);
        assert!(!cli.quiet);
    }

    #[test]
    fn no_color_flag_is_honoured() {
        let cli =
            Cli::try_parse_from(vec!["ktask-rs", "--no-color", "status"]).expect("Failed to parse");
        assert!(cli.no_color);
    }
}
