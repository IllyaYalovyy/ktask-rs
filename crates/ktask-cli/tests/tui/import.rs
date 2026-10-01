//! `i` on the queue screen: it asks for a file path and imports the tasks of that file
//! exactly as `ktask-rs import` does, showing the same result, or the same refusal, in the
//! same words, above the task list, which stays visible under it.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 100;
const SUBMIT: &str = "\x13";
const ESC: &str = "\x1b";

/// A sandbox with a git repository called `my-app` and a scratch directory a file of tasks
/// can be written to, outside the repository.
struct Fixture {
    sandbox: Sandbox,
    work: PathBuf,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        Ok(Self {
            sandbox,
            work,
            repository,
            _keep: keep,
        })
    }

    fn add(&self, title: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            &self.repository,
            &["add", "--title", title, "--criterion", "it works"],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// Writes `text` to a file called `name` outside the repository and returns its path.
    fn file(&self, name: &str, text: &str) -> Result<String> {
        let path = self.work.join(name);
        std::fs::write(&path, text)?;
        Ok(path.to_string_lossy().into_owned())
    }

    /// Opens the queue screen and waits until it is drawn whole.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }

    /// Opens the import form over the queue screen.
    fn open_form(&self) -> Result<Terminal> {
        let mut terminal = self.open()?;
        terminal.send("i")?;
        terminal.wait_for("the import form", |screen| {
            screen.contents().contains("Import tasks")
        })?;
        Ok(terminal)
    }
}

/// The row at `index`, counting from the top of where an import's results are shown, one per
/// line, above the task list.
fn result_line(screen: &str, index: usize) -> String {
    lines_inside_frame(screen)
        .get(4 + index)
        .cloned()
        .unwrap_or_default()
}

/// Whether `screen` still shows a row of the task list — one with `#`, the mark of a task's
/// own ID column, that no line of an import's own report ever carries — or the empty queue's
/// own placeholder line, proving the list was not replaced by whatever the import has to say.
fn list_still_shown(screen: &str) -> bool {
    lines_inside_frame(screen)
        .iter()
        .any(|line| line.contains('#') || line.contains("The queue is empty."))
}

#[test]
fn i_opens_the_import_form_asking_for_a_file_path() -> Result<()> {
    let fixture = Fixture::new()?;

    let terminal = fixture.open_form()?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Import tasks");
    assert_eq!(lines[3], "File path:");
    assert_eq!(lines[4], ">");
    assert!(
        screen
            .lines()
            .next_back()
            .is_some_and(|row| row.starts_with("└ Ctrl-S import · Esc cancel")),
        "{screen}"
    );
    Ok(())
}

#[test]
fn esc_closes_the_import_form_and_imports_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;
    let mut terminal = fixture.open_form()?;
    terminal.send("/does/not/matter")?;

    terminal.send(ESC)?;

    let screen = terminal.wait_for("the queue screen back", |screen| {
        lines_inside_frame(&screen.contents())
            .get(4)
            .is_some_and(|line| line.contains("Existing"))
    })?;
    assert!(!screen.contains("Import tasks"), "{screen}");
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn ctrl_s_imports_the_tasks_of_the_typed_path_and_shows_how_many_and_their_ids() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;
    let file = fixture.file(
        "tasks.json",
        r#"[{"title": "x", "criteria": ["c"]}, {"title": "y", "criteria": ["c"], "kind": "human"}]"#,
    )?;
    let mut terminal = fixture.open_form()?;

    terminal.send(&file)?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for(
        "the imported count and ids, not the form still showing the path",
        |screen| {
            let contents = screen.contents();
            !contents.contains("Import tasks") && result_line(&contents, 0) == "2 tasks added: 2, 3"
        },
    )?;
    assert_eq!(result_line(&screen, 0), "2 tasks added: 2, 3");
    assert!(!screen.contains("Import tasks"), "{screen}");
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("x")?;
    let screen = terminal.wait_for("the reloaded queue with the imported tasks", |screen| {
        screen.contents().contains(" y")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 3 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    // The first task the import added, "x", is selected.
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with('>') && line.ends_with("agent  x")),
        "{screen}"
    );
    assert!(
        lines.iter().any(|line| line.ends_with("human  y")),
        "{screen}"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn an_invalid_task_is_refused_in_the_same_words_ktask_rs_import_would_give() -> Result<()> {
    let fixture = Fixture::new()?;
    fixture.add("Existing")?;
    let file = fixture.file("tasks.json", r#"[{"title": " ", "criteria": []}]"#)?;
    let mut terminal = fixture.open_form()?;

    terminal.send(&file)?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for_text("1 task is invalid")?;
    assert_eq!(
        result_line(&screen, 0),
        "1 task is invalid, so nothing was imported"
    );
    assert_eq!(
        result_line(&screen, 1),
        "  - task 1: the title is empty: a task needs a title"
    );
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("x")?;
    let screen = terminal.wait_for("the queue back with nothing new added", |screen| {
        !screen.contents().contains("task is invalid")
    })?;
    let lines = lines_inside_frame(&screen);
    assert_eq!(
        lines[2],
        "pending 1 running 0 done 0 failed 0 blocked 0 unknown 0 cancelled 0 skipped 0"
    );
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_file_that_cannot_be_read_is_refused_naming_it() -> Result<()> {
    let fixture = Fixture::new()?;
    let missing = fixture.work.join("missing.json");
    let missing = missing.to_string_lossy().into_owned();
    let mut terminal = fixture.open_form()?;

    terminal.send(&missing)?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for_text("cannot read")?;
    assert!(result_line(&screen, 0).contains(&missing), "{screen}");
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn a_cancelled_task_in_the_file_is_skipped_saying_how_many() -> Result<()> {
    let fixture = Fixture::new()?;
    let file = fixture.file(
        "tasks.json",
        r#"[{"title": "kept", "criteria": ["c"]},
            {"title": "gone", "criteria": ["c"], "status": "cancelled"}]"#,
    )?;
    let mut terminal = fixture.open_form()?;

    terminal.send(&file)?;
    terminal.send(SUBMIT)?;

    let screen = terminal.wait_for_text("cancelled task was skipped")?;
    assert_eq!(result_line(&screen, 0), "1 task added: 1");
    assert_eq!(result_line(&screen, 1), "1 cancelled task was skipped");
    assert!(list_still_shown(&screen), "{screen}");

    terminal.send("x")?;
    let screen = terminal.wait_for("the reloaded queue with the kept task selected", |screen| {
        let contents = screen.contents();
        !contents.contains("cancelled task was skipped")
            && lines_inside_frame(&contents)
                .iter()
                .any(|line| line.starts_with('>'))
    })?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with('>') && line.ends_with("agent  kept")),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn the_key_map_lists_i() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;

    terminal.send("?")?;

    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        screen.contains("ktask-rs import") || screen.contains("JSON file"),
        "{screen}"
    );
    assert!(
        lines_inside_frame(&screen)
            .iter()
            .any(|line| line.trim_start().starts_with('i')),
        "{screen}"
    );

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
