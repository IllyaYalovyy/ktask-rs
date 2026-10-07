//! The instruction files on the real binary: every agent's prompt opens with the inlined
//! `VISION.md` and then its own role's file — `CODER.md`, `REVIEWER.md`, `TESTER.md` or
//! `RESOLVER.md` — from the `instructions-dir` setting's directory, and a file that is missing
//! refuses the run before any attempt begins, naming the file and the setting.

#[path = "support/repo.rs"]
mod repo;
#[path = "support/run_cleanup.rs"]
mod run_cleanup;
mod support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use repo::{INSTRUCTION_FILES, git_repository, scratch, write_instruction_files};
use support::{Outcome, Result, Sandbox};

/// Puts the directory of the `ktask-rs` under test on `command`'s `PATH`, so a task's own
/// bash block can call back into `ktask-rs report`.
fn with_nested_ktask_rs_on_path(command: &mut Command) {
    let mut paths = Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
        .parent()
        .map(Path::to_path_buf)
        .into_iter()
        .collect::<Vec<_>>();
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    if let Ok(joined) = std::env::join_paths(paths) {
        command.env("PATH", joined);
    }
}

/// What the file `name` of the scratch project's instruction files holds.
fn text_of(name: &str) -> &'static str {
    INSTRUCTION_FILES
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, text)| *text)
        .unwrap_or_default()
}

/// A sandbox with a git repository called `my-app` whose agent steps copy the prompt file they
/// are given (`$7`) into `prompts/<step>.prompt` before reporting.
struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    prompts: PathBuf,
    _keep: tempfile::TempDir,
}

/// However a test above left its `run`, nothing of it survives the test itself.
impl Drop for Fixture {
    fn drop(&mut self) {
        run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let prompts = work.join("prompts");
        std::fs::create_dir_all(&prompts)?;
        let set = sandbox.run(&repository, &["settings", "set", "max-attempts", "2"])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(Self {
            sandbox,
            repository,
            prompts,
            _keep: keep,
        })
    }

    fn run(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox.run(&self.repository, args)
    }

    fn run_the_queue(&self, args: &[&str]) -> Result<Outcome> {
        self.sandbox
            .run_with(&self.repository, args, with_nested_ktask_rs_on_path)
    }

    /// Adds an agent task whose script copies its prompt file to `prompts/<step>.prompt`, then
    /// reports `implementation` for the implementation step (`done` or `failed`), approves the
    /// review, accepts the test and stops the resolver.
    fn add_task(&self, title: &str, implementation: &str) -> Result<()> {
        let body = format!(
            "```bash\ncp \"$7\" \"{}/$3.prompt\"\ncase \"$3\" in\n  review) ktask-rs report --token \"$1\" approved ;;\n  testing) ktask-rs report --token \"$1\" accepted ;;\n  resolve) ktask-rs report --token \"$1\" stop --reason \"enough\" ;;\n  *) ktask-rs report --token \"$1\" {implementation} ;;\nesac\n```\n",
            self.prompts.display()
        );
        let added = self.run(&[
            "add",
            "--title",
            title,
            "--criterion",
            "it works",
            "--body",
            &body,
        ])?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(())
    }

    /// The prompt the script saw for `step`.
    fn prompt_of(&self, step: &str) -> Result<String> {
        Ok(std::fs::read_to_string(
            self.prompts.join(format!("{step}.prompt")),
        )?)
    }

    /// How many prompts the scripts have copied so far.
    fn prompts_seen(&self) -> Result<usize> {
        Ok(std::fs::read_dir(&self.prompts)?.count())
    }

    /// Task `id`'s current status, as `list --json` shows it.
    fn task_status(&self, id: u64) -> Result<String> {
        let outcome = self.run(&["list", "--json"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        let tasks: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
        let task = tasks
            .as_array()
            .ok_or("not a JSON array")?
            .iter()
            .find(|task| task["id"].as_u64() == Some(id))
            .ok_or("no such task")?;
        Ok(task["status"]
            .as_str()
            .ok_or("status is not a string")?
            .to_owned())
    }

    fn set_instructions_dir(&self, dir: &str) -> Result<()> {
        let set = self.run(&["settings", "set", "instructions-dir", dir])?;
        assert_eq!(set.code, Some(0), "{}", set.stderr);
        Ok(())
    }
}

/// Asserts that `prompt` begins with the whole of the vision file's content, immediately
/// followed by the whole of the role file's content.
fn assert_opens_with(prompt: &str, vision: &str, role: &str, step: &str) {
    let opening = format!("{vision}{role}");
    assert!(
        prompt.starts_with(&opening),
        "the {step} prompt does not begin with the vision and the role file:\n{prompt}"
    );
    assert!(
        prompt.len() > opening.len(),
        "the {step} prompt holds nothing after its opening:\n{prompt}"
    );
}

/// The steps a pipeline of agents runs, and the role file each one reads.
const ROLES: [(&str, &str); 4] = [
    ("implementation", "CODER.md"),
    ("review", "REVIEWER.md"),
    ("testing", "TESTER.md"),
    ("resolve", "RESOLVER.md"),
];

/// Runs one task that goes through implementation, review and testing and another whose
/// implementation fails so the resolver runs, so every role has been given a prompt.
fn run_every_role(fixture: &Fixture) -> Result<()> {
    fixture.add_task("passes", "done")?;
    fixture.add_task("fails", "failed --reason boom")?;
    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    Ok(())
}

#[test]
fn every_agent_prompt_begins_with_the_vision_and_then_its_own_roles_file() -> Result<()> {
    let fixture = Fixture::new()?;
    run_every_role(&fixture)?;

    for (step, role_file) in ROLES {
        let prompt = fixture.prompt_of(step)?;
        assert_opens_with(&prompt, text_of("VISION.md"), text_of(role_file), step);
    }
    Ok(())
}

#[test]
fn the_prompt_never_asks_the_agent_to_go_and_read_a_file() -> Result<()> {
    let fixture = Fixture::new()?;
    run_every_role(&fixture)?;

    for (step, _) in ROLES {
        let prompt = fixture.prompt_of(step)?.to_lowercase();
        for phrase in ["go and read", "go read", "read docs/", "read the file"] {
            assert!(!prompt.contains(phrase), "{step}: {phrase}\n{prompt}");
        }
    }
    Ok(())
}

#[test]
fn another_instructions_directory_is_the_one_used() -> Result<()> {
    let fixture = Fixture::new()?;
    let handbook = fixture.repository.join("handbook");
    std::fs::OpenOptions::new()
        .append(true)
        .open(fixture.repository.join(".git/info/exclude"))?
        .write_all(b"/handbook/\n")?;
    write_instruction_files(&handbook)?;
    for (name, _) in INSTRUCTION_FILES {
        std::fs::write(
            handbook.join(name),
            format!("{name} from the handbook directory\n"),
        )?;
    }
    fixture.set_instructions_dir("handbook")?;
    run_every_role(&fixture)?;

    for (step, role_file) in ROLES {
        let prompt = fixture.prompt_of(step)?;
        assert_opens_with(
            &prompt,
            "VISION.md from the handbook directory\n",
            &format!("{role_file} from the handbook directory\n"),
            step,
        );
    }
    Ok(())
}

#[test]
fn an_absolute_instructions_directory_is_used_as_it_is() -> Result<()> {
    let fixture = Fixture::new()?;
    let (_elsewhere, elsewhere_path) = scratch()?;
    let outside = elsewhere_path.join("guides");
    write_instruction_files(&outside)?;
    std::fs::write(outside.join("VISION.md"), "the vision from far away\n")?;
    fixture.set_instructions_dir(&outside.display().to_string())?;
    run_every_role(&fixture)?;

    for (step, role_file) in ROLES {
        let prompt = fixture.prompt_of(step)?;
        assert_opens_with(
            &prompt,
            "the vision from far away\n",
            text_of(role_file),
            step,
        );
    }
    Ok(())
}

/// The scratch project's instruction file `name` is removed, then the queue is run: nothing
/// ran, the task is still pending and the refusal names `path` and `instructions-dir`.
fn assert_refused_without(fixture: &Fixture, path: &str) -> Result<()> {
    fixture.add_task("a", "done")?;
    let outcome = fixture.run_the_queue(&["run"])?;
    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    assert!(outcome.stdout.contains(path), "{}", outcome.stdout);
    assert!(
        outcome.stdout.contains("instructions-dir"),
        "{}",
        outcome.stdout
    );
    assert!(
        outcome.stdout.contains("was not started"),
        "{}",
        outcome.stdout
    );
    assert_eq!(fixture.prompts_seen()?, 0, "an agent was run");
    assert_eq!(fixture.task_status(1)?, "pending");
    let status = fixture.run(&["status"])?;
    assert!(
        !status.stdout.contains("attempt 1"),
        "an attempt was begun:\n{}",
        status.stdout
    );
    Ok(())
}

#[test]
fn any_missing_instruction_file_refuses_the_run_naming_the_file_and_the_setting() -> Result<()> {
    for (name, _) in INSTRUCTION_FILES {
        let fixture = Fixture::new()?;
        std::fs::remove_file(fixture.repository.join("docs").join(name))?;
        assert_refused_without(&fixture, &format!("docs/{name}"))?;
    }
    Ok(())
}

#[test]
fn a_missing_file_in_another_instructions_directory_names_that_directory() -> Result<()> {
    let fixture = Fixture::new()?;
    write_instruction_files(&fixture.repository.join("handbook"))?;
    std::fs::remove_file(fixture.repository.join("handbook/TESTER.md"))?;
    fixture.set_instructions_dir("handbook")?;
    assert_refused_without(&fixture, "handbook/TESTER.md")
}

#[test]
fn an_instructions_directory_that_does_not_exist_refuses_the_run() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.set_instructions_dir("nowhere")?;
    assert_refused_without(&fixture, "nowhere/VISION.md")
}

#[test]
fn the_refusal_is_in_the_json_report_too() -> Result<()> {
    let fixture = Fixture::new()?;
    std::fs::remove_file(fixture.repository.join("docs/CODER.md"))?;
    fixture.add_task("a", "done")?;

    let outcome = fixture.run_the_queue(&["run", "--json"])?;

    assert_eq!(outcome.code, Some(1), "{}", outcome.stderr);
    let report: serde_json::Value = serde_json::from_str(&outcome.stdout)?;
    assert_eq!(report["end"]["kind"], "instructions_unreadable");
    assert_eq!(report["end"]["id"], 1);
    assert_eq!(report["end"]["path"], "docs/CODER.md");
    assert_eq!(report["attempted"], serde_json::json!([]));
    Ok(())
}

#[test]
fn restoring_the_file_lets_the_same_task_run_afterwards() -> Result<()> {
    let fixture = Fixture::new()?;
    let coder = fixture.repository.join("docs/CODER.md");
    std::fs::remove_file(&coder)?;
    fixture.add_task("a", "done")?;
    let refused = fixture.run_the_queue(&["run"])?;
    assert_eq!(refused.code, Some(1), "{}", refused.stderr);

    std::fs::write(&coder, text_of("CODER.md"))?;
    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}

#[test]
fn a_step_that_is_switched_off_does_not_need_its_role_file() -> Result<()> {
    let fixture = Fixture::new()?;
    for (setting, file) in [
        ("step-review", "REVIEWER.md"),
        ("step-testing", "TESTER.md"),
    ] {
        std::fs::remove_file(fixture.repository.join("docs").join(file))?;
        let off = fixture.run(&["settings", "set", setting, "off"])?;
        assert_eq!(off.code, Some(0), "{}", off.stderr);
    }
    fixture.add_task("a", "done")?;

    let outcome = fixture.run_the_queue(&["run"])?;

    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    assert_eq!(fixture.task_status(1)?, "done");
    Ok(())
}
