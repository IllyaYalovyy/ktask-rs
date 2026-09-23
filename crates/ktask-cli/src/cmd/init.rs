//! `ktask-rs init`: registers the current repository.
//!
//! `main` already resolves (and, for `init`, registers) the project before
//! dispatch reaches here — including rejecting a directory that is not a
//! git repository, which [`ktask_core::Project::register`] itself now
//! enforces — so by the time [`run`] is called the project exists. All that
//! is left for `init` itself to do is report the registration: the project
//! id and where its state lives. Since [`ktask_core::Project::register`] is
//! already idempotent, running `init` again against an already-registered
//! repository reaches this same code with the same `project` and reports
//! the same thing, satisfying `docs/CONTRACT.md` section 3's "re-running
//! prints the existing registration and exits 0".

use crate::render;
use ktask_core::{Config, Project, RunOutcome};

/// Prints `project`'s id and state directory, per `docs/CONTRACT.md`
/// section 3. Always [`RunOutcome::Drained`]: by the time this runs,
/// registration has already succeeded.
pub(crate) fn run(project: &Project, _config: &Config) -> RunOutcome {
    render::out(format_args!("project: {}", project.id));
    render::out(format_args!("state: {}", project.state_dir.display()));
    RunOutcome::Drained
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::path::PathBuf;
    use std::process::Command;

    fn fixture_project() -> Project {
        Project {
            root: PathBuf::from("/nonexistent/ktask-init-fixture/root"),
            id: "init-fixture-id".to_string(),
            state_dir: PathBuf::from("/nonexistent/ktask-init-fixture/state"),
        }
    }

    #[test]
    fn run_reports_the_queue_as_drained() {
        let outcome = run(&fixture_project(), &Config::default());
        assert_eq!(outcome, RunOutcome::Drained);
    }

    /// Spawns this test binary re-executed as `child_name`, the pattern
    /// `render.rs` and `json.rs` also use to observe real, separate stdout
    /// and stderr for a process boundary this crate has no library target
    /// to unit test against directly.
    fn run_child(child_name: &str) -> (String, String) {
        let exe = env::current_exe().expect("current test exe");
        let output = Command::new(exe)
            .args(["--exact", "--ignored", "--nocapture", child_name])
            .output()
            .expect("spawn child");
        (
            String::from_utf8(output.stdout).expect("stdout is utf8"),
            String::from_utf8(output.stderr).expect("stderr is utf8"),
        )
    }

    #[test]
    fn run_prints_the_project_id_and_state_directory_to_stdout_only() {
        let (stdout, stderr) = run_child("cmd::init::tests::emit_fixture_registration");

        assert!(
            stdout.contains("init-fixture-id"),
            "missing project id on stdout: {stdout:?}"
        );
        assert!(
            stdout.contains("/nonexistent/ktask-init-fixture/state"),
            "missing state directory on stdout: {stdout:?}"
        );
        assert!(
            !stderr.contains("init-fixture-id"),
            "registration details leaked onto stderr: {stderr:?}"
        );
    }

    #[test]
    #[ignore = "invoked directly as a child process by \
                run_prints_the_project_id_and_state_directory_to_stdout_only"]
    fn emit_fixture_registration() {
        run(&fixture_project(), &Config::default());
    }
}
