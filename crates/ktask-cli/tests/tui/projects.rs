//! `p` on the queue screen: it opens the list of registered projects — the same one
//! `ktask-rs project list` prints — the current one marked, and selecting one replaces the
//! queue screen with that project's queue, every action from then on applying to it. `d` there
//! forgets the selected project, after asking, the same as `ktask-rs project forget --yes`.

use std::path::PathBuf;

use super::pty::{Terminal, lines_inside_frame};
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

const ROWS: u16 = 24;
const COLS: u16 = 90;
const DOWN: &str = "\x1b[B";
const ENTER: &str = "\r";
const ESC: &str = "\x1b";
const SUBMIT: &str = "\x13";

/// A sandbox with two registered git repositories: `my-app`, with one task, registered first,
/// and `other-app`, with one task of its own, registered second.
struct Fixture {
    sandbox: Sandbox,
    my_app: PathBuf,
    other_app: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let my_app = git_repository(&sandbox, &work, "my-app")?;
        let other_app = git_repository(&sandbox, &work, "other-app")?;
        let fixture = Self {
            sandbox,
            my_app,
            other_app,
            _keep: keep,
        };
        // Registers `my-app` first and `other-app` second: `ktask-rs project list` and the
        // picker both list oldest registration first.
        fixture.add(&fixture.my_app, "alpha")?;
        fixture.add(&fixture.other_app, "bravo")?;
        Ok(fixture)
    }

    fn add(&self, repository: &std::path::Path, title: &str) -> Result<()> {
        let outcome = self.sandbox.run(
            repository,
            &["add", "--title", title, "--criterion", "it works"],
        )?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(())
    }

    /// What `ktask-rs project list` prints, as text.
    fn cli_project_list(&self) -> Result<String> {
        let outcome = self.sandbox.run(&self.my_app, &["project", "list"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }

    /// What `ktask-rs list` prints for `repository`'s project, as text.
    fn cli_list(&self, repository: &std::path::Path) -> Result<String> {
        let outcome = self.sandbox.run(repository, &["list"])?;
        assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
        Ok(outcome.stdout)
    }

    /// Opens the queue screen on `my-app` and waits until it is drawn whole, with its task
    /// selected.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.my_app, &["tui"], ROWS, COLS)?;
        terminal.wait_for("the queue with its task selected", |screen| {
            let contents = screen.contents();
            contents.ends_with('┘') && contents.contains(">1  #1  pending  agent  alpha")
        })?;
        Ok(terminal)
    }

    /// Opens the queue screen and the project picker over it.
    fn open_picker(&self) -> Result<Terminal> {
        let mut terminal = self.open()?;
        terminal.send("p")?;
        terminal.wait_for("the project picker", |screen| {
            screen.contents().contains("Projects")
        })?;
        Ok(terminal)
    }

    /// Where the binary keeps `name`'s journal under this fixture's `XDG_STATE_HOME`.
    fn journal_file(&self, name: &str) -> PathBuf {
        self.sandbox.state_dir().join(name).join("journal.db")
    }
}

#[test]
fn p_opens_the_list_project_list_prints_with_name_and_path_the_current_one_marked() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert_eq!(lines[1], "Projects");

    let listed = fixture.cli_project_list()?;
    let mut cli_rows = listed.lines();
    let (my_app_name, my_app_path) = cli_rows.next().and_then(|l| l.split_once('\t')).unwrap();
    let (other_app_name, other_app_path) =
        cli_rows.next().and_then(|l| l.split_once('\t')).unwrap();
    assert_eq!(my_app_name, "my-app");
    assert_eq!(other_app_name, "other-app");
    assert!(cli_rows.next().is_none(), "{listed}");

    // The picker lists them in the same order, `my-app` marked as the one on show.
    assert!(
        lines[3].starts_with('>')
            && lines[3].contains(my_app_name)
            && lines[3].contains(my_app_path)
            && lines[3].ends_with("(current)"),
        "{lines:?}"
    );
    assert!(
        lines[4].starts_with(' ')
            && lines[4].contains(other_app_name)
            && lines[4].contains(other_app_path)
            && !lines[4].contains("(current)"),
        "{lines:?}"
    );
    assert!(
        lines[ROWS as usize - 1]
            .starts_with("└ j, k select · Enter switch · d forget · Esc cancel"),
        "{lines:?}"
    );

    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn esc_closes_the_picker_and_switches_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    terminal.send(ESC)?;

    terminal.wait_for("the queue back, without the picker", |screen| {
        let contents = screen.contents();
        !contents.contains("Projects") && contents.contains("my-app")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn selecting_a_project_replaces_the_queue_screen_and_every_action_then_applies_to_it() -> Result<()>
{
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    terminal.send(DOWN)?;
    terminal.send(ENTER)?;

    // The queue screen now shows `other-app`'s queue, not `my-app`'s.
    terminal.wait_for("other-app's queue, with its own task selected", |screen| {
        let lines = lines_inside_frame(&screen.contents());
        lines.get(1).is_some_and(|line| line == "other-app")
            && lines
                .get(5)
                .is_some_and(|line| line == ">1  #1  pending  agent  bravo")
    })?;

    // Adding a task from here applies to `other-app`, not `my-app`.
    terminal.send("n")?;
    terminal.wait_for("the form", |screen| screen.contents().contains("New task"))?;
    terminal.send("charlie")?;
    terminal.send("\t\t\t\tit works")?;
    terminal.send(SUBMIT)?;
    terminal.wait_for("charlie added to the queue on show", |screen| {
        screen.contents().contains("charlie")
    })?;

    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    let other_app_list = fixture.cli_list(&fixture.other_app)?;
    assert!(other_app_list.contains("bravo"), "{other_app_list}");
    assert!(other_app_list.contains("charlie"), "{other_app_list}");
    let my_app_list = fixture.cli_list(&fixture.my_app)?;
    assert!(my_app_list.contains("alpha"), "{my_app_list}");
    assert!(!my_app_list.contains("charlie"), "{my_app_list}");
    Ok(())
}

#[test]
fn d_asks_to_confirm_forgetting_the_selected_project() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;

    terminal.send("d")?;
    terminal.wait_for("the forget question", |screen| {
        screen.contents().contains("Forget project \"my-app\"?")
    })?;
    let screen = terminal.screen();
    let lines = lines_inside_frame(&screen);
    assert!(
        lines[ROWS as usize - 1].starts_with("└ y forget · n, Esc keep it"),
        "{lines:?}"
    );

    terminal.send(ESC)?;
    terminal.wait_for("the plain picker, the question gone", |screen| {
        !screen.contents().contains("Forget project")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn question_mark_shows_the_pickers_own_keys_and_esc_closes_it_back_to_the_picker() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;
    let before = terminal.screen();

    terminal.send("?")?;
    let screen = terminal.wait_for("the picker's key map", |screen| {
        screen.contents().contains("Keys")
    })?;
    let lines = lines_inside_frame(&screen);
    assert!(
        lines.contains(&"j, k   select the next or previous project".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"Enter  switch to the selected project".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(&"d      forget the selected project, after asking".to_owned()),
        "{lines:?}"
    );
    assert!(lines.contains(&"Esc    cancel".to_owned()), "{lines:?}");
    assert!(!screen.contains("my-app"), "{screen}");

    // Other keys, including d and Enter, do nothing while the key map is up.
    terminal.send("d")?;
    terminal.send(ENTER)?;

    terminal.send(ESC)?;
    let screen = terminal.wait_for("the picker back", |screen| {
        !screen.contents().contains("Keys")
    })?;
    assert_eq!(screen, before);

    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn question_mark_shows_the_forget_questions_own_keys_while_it_is_asking() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open_picker()?;
    terminal.send("d")?;
    let asked = terminal.wait_for("the forget question", |screen| {
        screen.contents().contains("Forget project")
    })?;

    terminal.send("?")?;
    let screen = terminal.wait_for("the forget question's key map", |screen| {
        screen.contents().contains("Keys")
    })?;
    let lines = lines_inside_frame(&screen);
    assert!(lines.contains(&"y       forget it".to_owned()), "{lines:?}");
    assert!(lines.contains(&"n, Esc  keep it".to_owned()), "{lines:?}");
    assert!(!screen.contains("Forget project"), "{screen}");

    // y does nothing while the key map is up: the project is not forgotten.
    terminal.send("y")?;

    terminal.send(ESC)?;
    let screen = terminal.wait_for("the question back", |screen| {
        screen.contents().contains("Forget project")
    })?;
    assert_eq!(screen, asked);

    terminal.send(ESC)?;
    terminal.wait_for("the plain picker, the question gone", |screen| {
        !screen.contents().contains("Forget project")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    assert!(fixture.cli_project_list()?.contains("my-app"));
    Ok(())
}

#[test]
fn a_project_name_too_long_to_fit_is_cut_with_an_ellipsis_but_the_forget_keys_never_are()
-> Result<()> {
    let sandbox = Sandbox::new()?;
    let (_keep, work) = scratch()?;
    let long_name = "a-very-long-project-name-that-will-not-fit-next-to-the-question-keys";
    let repository = git_repository(&sandbox, &work, long_name)?;
    let outcome = sandbox.run(&repository, &["project", "register", "--name", long_name])?;
    assert_eq!(outcome.code, Some(0), "{}", outcome.stderr);
    let added = sandbox.run(
        &repository,
        &["add", "--title", "t", "--criterion", "it works"],
    )?;
    assert_eq!(added.code, Some(0), "{}", added.stderr);

    let mut terminal = Terminal::launch(&sandbox, &repository, &["tui"], ROWS, COLS)?;
    terminal.wait_for("the queue with its task selected", |screen| {
        screen.contents().ends_with('┘') && screen.contents().contains(">1")
    })?;
    terminal.send("p")?;
    terminal.wait_for("the project picker", |screen| {
        screen.contents().contains("Projects")
    })?;

    terminal.send("d")?;
    let screen = terminal.wait_for("the forget question", |screen| {
        screen.contents().contains("Forget project")
    })?;
    let lines = lines_inside_frame(&screen);
    let question = lines
        .iter()
        .find(|line| line.starts_with("Forget project"))
        .unwrap_or_else(|| panic!("no forget question in {lines:?}"));
    assert!(
        question.ends_with("\"? Its journal stays on disk. y to forget · n or Esc to keep it"),
        "{question:?}"
    );
    assert!(!question.contains(long_name), "{question:?}");

    terminal.send(ESC)?;
    terminal.wait_for("the plain picker, the question gone", |screen| {
        !screen.contents().contains("Forget project")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back, without the picker", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}

#[test]
fn n_and_esc_drop_the_forget_question_and_forget_nothing() -> Result<()> {
    let fixture = Fixture::new()?;
    for answer in ["n", ESC] {
        let mut terminal = fixture.open_picker()?;
        terminal.send("d")?;
        terminal.wait_for("the forget question", |screen| {
            screen.contents().contains("Forget project")
        })?;

        terminal.send(answer)?;
        terminal.wait_for(
            "the picker back, with both projects still listed",
            |screen| {
                let contents = screen.contents();
                !contents.contains("Forget project")
                    && contents.contains("my-app")
                    && contents.contains("other-app")
            },
        )?;

        terminal.send(ESC)?;
        terminal.wait_for("the queue back, without the picker", |screen| {
            !screen.contents().contains("Projects")
        })?;
        terminal.send("q")?;
        assert_eq!(terminal.wait_for_exit()?, 0);
    }
    assert!(fixture.cli_project_list()?.contains("my-app"));
    Ok(())
}

#[test]
fn y_forgets_the_selected_project_leaving_its_journal_on_disk() -> Result<()> {
    let fixture = Fixture::new()?;
    let journal = fixture.journal_file("my-app");
    assert!(journal.is_file(), "{}", journal.display());
    let mut terminal = fixture.open_picker()?;

    terminal.send("d")?;
    terminal.wait_for("the forget question", |screen| {
        screen.contents().contains("Forget project")
    })?;
    terminal.send("y")?;

    // The picker stays open, now showing only `other-app`.
    terminal.wait_for("the picker with my-app gone", |screen| {
        let contents = screen.contents();
        contents.contains("Projects")
            && !contents.contains("my-app")
            && contents.contains("other-app")
    })?;
    terminal.send(ESC)?;
    terminal.wait_for("the queue back", |screen| {
        !screen.contents().contains("Projects")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);

    // Forgetting through the terminal interface gives the same result `ktask-rs project
    // forget --yes my-app` would: gone from the registry, its journal left where it was.
    assert_eq!(
        fixture.cli_project_list()?,
        format!("other-app\t{}\n", fixture.other_app.display())
    );
    assert!(journal.is_file(), "{}", journal.display());
    Ok(())
}

#[test]
fn the_queue_key_map_lists_p() -> Result<()> {
    let fixture = Fixture::new()?;
    let mut terminal = fixture.open()?;
    terminal.send("?")?;
    let screen = terminal.wait_for("the key map", |screen| screen.contents().contains("Keys"))?;
    assert!(
        screen.contains("p        work on another registered project's queue"),
        "{screen}"
    );
    terminal.send(ESC)?;
    terminal.wait_for("the key map closed", |screen| {
        !screen.contents().contains("Keys")
    })?;
    terminal.send("q")?;
    assert_eq!(terminal.wait_for_exit()?, 0);
    Ok(())
}
