//! One terminal for every project of an instance: the Projects screen, switching between
//! projects, starting outside a project, and `ktask projects`.
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{Definition, Kind, Service, VersionedTask, overview},
    storage::Database,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");

fn definition(title: &str) -> Definition {
    Definition {
        title: title.into(),
        goal: "Show this project among the others".into(),
        criteria: vec!["Listed on Projects".into()],
        kind: Kind::Ai,
        category: None,
        lenses: None,
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: Some(600_000),
        stage_timeout_ms: None,
    }
}

fn state(root: &Path) -> PathBuf {
    root.join("state/ktask-dev")
}

fn project(root: &Path, name: &str) -> Result<Service> {
    let folder = root.join(name);
    fs::create_dir_all(&folder)?;
    Ok(Service::open(
        Database::open_in(&state(root))?,
        &folder,
        None,
    )?)
}

/// The list row of project `name`, without the details pane beside it.
fn row<'a>(screen: &'a str, name: &str) -> &'a str {
    screen
        .lines()
        .filter_map(|line| line.split("││").next())
        .find(|row| row.starts_with('│') && row.contains(name))
        .unwrap_or("")
}

/// alpha-app: 110 queued and 110 finished; beta-lib: a draft awaiting approval;
/// gamma-docs: nothing queued; delta-gone: its folder was removed.
fn four_projects(root: &Path) -> Result<Service> {
    let mut alpha = project(root, "alpha-app")?;
    let tasks: Vec<_> = (1..=220)
        .map(|n| definition(&format!("Projects fixture {n}")))
        .collect();
    let ids = alpha.create(&tasks, None, true)?;
    for id in &ids[..110] {
        alpha.mark_done(
            VersionedTask {
                id: *id,
                version: 1,
            },
            "fixture history",
        )?;
    }
    project(root, "beta-lib")?.create(&[definition("Approve the beta plan")], None, false)?;
    let gamma = project(root, "gamma-docs")?;
    project(root, "delta-gone")?;
    fs::remove_dir(root.join("delta-gone"))?;
    Ok(gamma)
}

#[test]
fn projects_screen_lists_live_states_and_switches_projects_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        let mut gamma = four_projects(root)?;
        let mut t = Terminal::open_project(DEV, root, &root.join("alpha-app"), 110, 30)?;
        let current = t.screen("the folder's own project opens on Current", |s| {
            s.contains("/ alpha-app")
                && s.contains("Current #111 · Pending")
                && s.contains("Projects: 1 need you · 0 running")
        })?;
        println!("STARTUP IN A PROJECT mouse={mouse}\n{current}");

        t.send("2")?;
        t.screen("alpha queue", |s| s.contains("Queue · 1 / 110"))?;
        t.send("jjj")?;
        t.screen("alpha queue selection", |s| s.contains("Queue · 4 / 110"))?;

        if mouse {
            t.click("6 Projects")?;
        } else {
            t.send("6")?;
        }
        let listed = t.screen("projects listed", |s| {
            s.contains("Projects · 4 registered · 0 running · 1 need you")
                && row(s, "gamma-docs").contains("Idle")
        })?;
        println!("PROJECTS mouse={mouse}\n{listed}");
        assert!(row(&listed, "alpha-app").contains("* alpha-app"));
        assert!(row(&listed, "alpha-app").contains("Ready"));
        assert!(row(&listed, "alpha-app").contains("110 queued · #111"));
        assert!(row(&listed, "beta-lib").contains("Needs approval"));
        assert!(row(&listed, "beta-lib").contains("1 queued · #221 Approve"));
        assert!(row(&listed, "delta-gone").contains("Missing folder"));
        assert!(!listed.contains("p Pause") && !listed.contains("a Add"));

        // Another process changes a project that is not open: the list follows.
        gamma.create(&[definition("Write the gamma guide")], None, true)?;
        let ready = t.screen("list follows another project's queue", |s| {
            row(s, "gamma-docs").contains("Ready") && row(s, "gamma-docs").contains("#222")
        })?;
        println!("LIVE CHANGE mouse={mouse}\n{ready}");
        let runners = state(root).join("runners");
        fs::create_dir_all(&runners)?;
        let lease = fs::File::create(runners.join(format!("{}.lock", gamma.project_id)))?;
        lease.lock()?;
        let running = t.screen("list follows a runner lease", |s| {
            row(s, "gamma-docs").contains("Running")
                && s.contains("Projects: 1 need you · 1 running")
        })?;
        println!("LIVE RUNNER mouse={mouse}\n{running}");
        drop(lease);
        t.screen("runner gone", |s| row(s, "gamma-docs").contains("Ready"))?;

        if mouse {
            t.click("beta-lib")?;
        } else {
            t.send("j")?;
        }
        t.screen("beta selected", |s| {
            s.contains("Waiting for you · Enter opens it.")
        })?;
        if mouse {
            t.click("Enter Open")?;
        } else {
            t.send("\r")?;
        }
        let beta = t.screen("beta opened", |s| {
            s.contains("/ beta-lib")
                && s.contains("Current #221 · Needs approval")
                && s.contains("Opened beta-lib")
        })?;
        println!("OPENED ANOTHER PROJECT mouse={mouse}\n{beta}");

        if mouse {
            t.click("6 Projects")?;
        } else {
            t.send("6")?;
        }
        t.screen("back on projects", |s| {
            row(s, "beta-lib").contains("* beta-lib")
        })?;
        if mouse {
            t.click("delta-gone")?;
        } else {
            t.send("j")?;
        }
        t.screen("missing selected", |s| {
            s.contains("Restore the folder to open this project")
        })?;
        if mouse {
            t.click("Enter Open")?;
        } else {
            t.send("\r")?;
        }
        let missing = t.screen("a missing folder does not open", |s| {
            s.contains("delta-gone: folder") && s.contains("is missing")
        })?;
        println!("MISSING FOLDER mouse={mouse}\n{missing}");
        assert!(missing.contains("/ beta-lib"));

        if mouse {
            t.click("alpha-app")?;
        } else {
            t.send("kk")?;
        }
        t.screen("alpha selected", |s| s.contains("Queue: 110 unfinished"))?;
        if mouse {
            // The details pane opens the selected project, as Queue's opens task details.
            t.click("Queue: 110 unfinished")?;
        } else {
            t.send("\r")?;
        }
        let returned = t.screen("alpha reopens where it was left", |s| {
            s.contains("/ alpha-app") && s.contains("Queue · 4 / 110")
        })?;
        println!("RETURNED TO THE FIRST PROJECT mouse={mouse}\n{returned}");

        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn starting_outside_a_project_opens_projects_and_adds_the_folder_only_on_request() -> Result {
    for mouse in [false, true] {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        project(root, "alpha-app")?.create(&[definition("Only alpha task")], None, true)?;
        let fresh = root.join("fresh-folder");
        fs::create_dir(&fresh)?;
        let mut t = Terminal::open_project(DEV, root, &fresh, 110, 30)?;
        let started = t.screen("projects at startup", |s| {
            s.contains("Projects · 1 registered")
                && s.contains("is not a project · n adds it")
                && s.contains("n Add this folder")
        })?;
        println!("STARTUP OUTSIDE A PROJECT mouse={mouse}\n{started}");
        assert!(started.contains("/ alpha-app"));
        assert_eq!(
            overview::projects(&Database::open_in(&state(root))?)?.len(),
            1,
            "starting elsewhere registered a project"
        );

        if mouse {
            t.click("n Add this folder")?;
        } else {
            t.send("n")?;
        }
        let form = t.screen("register form", |s| {
            s.contains("Add this folder as a project") && s.contains("Project name")
        })?;
        println!("ADD THIS FOLDER mouse={mouse}\n{form}");
        assert!(form.contains("fresh-folder"));
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let opened = t.screen("the new project opens", |s| {
            s.contains("/ fresh-folder")
                && s.contains("No current task")
                && s.contains("Opened fresh-folder")
        })?;
        println!("ADDED AND OPENED mouse={mouse}\n{opened}");
        let names: Vec<_> = overview::projects(&Database::open_in(&state(root))?)?
            .into_iter()
            .map(|project| project.name)
            .collect();
        assert_eq!(names, ["alpha-app", "fresh-folder"]);

        t.send("6")?;
        let listed = t.screen("both listed", |s| {
            s.contains("Projects · 2 registered")
                && row(s, "fresh-folder").contains("* fresh-folder")
        })?;
        assert!(!listed.contains("n Add this folder"));
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn projects_command_lists_every_project_without_registering_the_folder() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Ok(Command::new(DEV)
            .args(args)
            .env_clear()
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .current_dir(root)
            .output()?)
    };
    let empty = run(&["projects"])?;
    assert!(empty.status.success());
    assert_eq!(String::from_utf8(empty.stdout)?, "No projects.\n");
    assert!(!state(root).exists(), "projects created storage");

    drop(four_projects(root)?);
    // The overview names the same current task as the `current_tasks` view.
    let db = Database::open_in(&state(root))?;
    for project in overview::projects(&db)? {
        let viewed: Option<i64> = db
            .connection()
            .query_row(
                "SELECT id FROM current_tasks WHERE project_id = ?1",
                [project.id],
                |row| row.get(0),
            )
            .ok();
        assert_eq!(
            project.current.map(|task| task.id),
            viewed,
            "{}",
            project.name
        );
    }
    drop(db);
    let output = run(&["projects", "--json"])?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    println!("PROJECTS JSON\n{reply:#}");
    assert_eq!(reply["schema_version"], 1);
    assert_eq!(reply["data"]["count"], 4);
    let projects = reply["data"]["projects"].as_array().unwrap();
    let field = |name: &str| -> Vec<String> {
        projects
            .iter()
            .map(|project| project[name].to_string())
            .collect()
    };
    assert_eq!(
        field("name"),
        [
            "\"alpha-app\"",
            "\"beta-lib\"",
            "\"delta-gone\"",
            "\"gamma-docs\""
        ]
    );
    assert_eq!(
        field("state"),
        ["\"ready\"", "\"needs_owner\"", "\"missing\"", "\"idle\""]
    );
    assert_eq!(field("queued"), ["110", "1", "0", "0"]);
    assert_eq!(field("finished"), ["110", "0", "0", "0"]);
    assert_eq!(projects[0]["current"]["id"], 111);
    assert_eq!(projects[1]["current"]["state"], "needs_approval");
    assert_eq!(projects[1]["runner_alive"], false);
    assert!(projects[3]["current"].is_null());

    let text = String::from_utf8(run(&["projects"])?.stdout)?;
    println!("PROJECTS TEXT\n{text}");
    assert!(text.contains("\tneeds_owner\t1\tbeta-lib\t"));
    assert!(text.contains("#221 Needs approval · Approve the beta plan"));

    let scoped = run(&["projects", "--project", "."])?;
    assert_eq!(scoped.status.code(), Some(2));
    Ok(())
}

#[test]
fn projects_fit_the_smallest_terminal_and_the_simulator_lists_no_real_project() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    drop(four_projects(root)?);
    project(root, "a-project-with-a-name-longer-than-its-column")?;
    let mut t = Terminal::open_project(DEV, root, &root.join("beta-lib"), 70, 22)?;
    t.screen("beta current", |s| s.contains("/ beta-lib"))?;
    // Shift-Tab from the first screen wraps to the last one.
    t.send("\x1b[Z")?;
    let narrow = t.screen("projects at 70 x 22", |s| {
        s.contains("Projects · 5 registered") && row(s, "gamma-docs").contains("Idle")
    })?;
    println!("PROJECTS AT 70 x 22\n{narrow}");
    assert!(row(&narrow, "beta-lib").contains("* beta-lib"));
    assert!(narrow.contains("[Enter Open]") && narrow.contains("[q Quit]"));
    let (column, line) = support::location(&narrow, "gamma-docs")?;
    // One notch moves three rows: from the third project to the last.
    t.wheel(true, column as u16, line as u16)?;
    t.screen("the wheel moves the selection", |s| {
        s.contains("Projects · 5 / 5")
    })?;
    t.wheel(false, column as u16, line as u16)?;
    t.screen("and back", |s| s.contains("Projects · 2 / 5"))?;
    t.send("\t")?;
    t.screen("Tab wraps to Current", |s| {
        s.contains("Current #221 · Needs approval") && s.contains("Pending revision")
    })?;
    t.send("q")?;
    t.finish()?;

    let mut demo = Terminal::open_at(DEV, root, 70, 22)?;
    demo.screen("demo", |s| s.contains("MOCK DATA"))?;
    demo.click("6 Projects")?;
    let simulated = demo.screen("demo projects", |s| {
        s.contains("The simulator has one mock workspace")
    })?;
    println!("PROJECTS IN THE SIMULATOR\n{simulated}");
    assert!(!simulated.contains("alpha-app") && !simulated.contains("Enter Open"));
    demo.send("\r")?;
    demo.send("1")?;
    demo.screen("demo current", |s| {
        !s.contains("The simulator has one mock workspace")
    })?;
    demo.send("q")?;
    demo.finish()?;
    Ok(())
}

/// ktask's own files of project `id`, one of each kind outside the database.
fn ktask_files(root: &Path, id: i64) -> Result<Vec<PathBuf>> {
    let files = vec![
        state(root).join(format!("runner-{id}.log")),
        root.join(format!("config/ktask-dev/project-{id}.json")),
        root.join(format!("cache/ktask-dev/project-runtime/{id}/tool")),
    ];
    for file in &files {
        fs::create_dir_all(file.parent().unwrap())?;
        fs::write(file, "ktask")?;
    }
    Ok(files)
}

#[test]
fn a_project_is_moved_and_deleted_from_projects_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        drop(four_projects(root)?);
        let ids: std::collections::BTreeMap<_, _> =
            overview::projects(&Database::open_in(&state(root))?)?
                .into_iter()
                .map(|project| (project.name, project.id))
                .collect();
        let beta_files = ktask_files(root, ids["beta-lib"])?;
        let gamma_files = ktask_files(root, ids["gamma-docs"])?;
        fs::write(root.join("beta-lib/source.rs"), "kept")?;
        // The owner restored delta-gone's repository elsewhere and starts ktask there.
        let restored = root.join("delta-restored");
        fs::create_dir(&restored)?;
        let mut t = Terminal::open_project(DEV, root, &restored, 110, 30)?;
        t.screen("projects at startup", |s| {
            s.contains("Projects · 4 registered") && s.contains("m Move") && s.contains("D Delete")
        })?;

        t.send("jj")?;
        t.screen("the missing project is selected", |s| {
            s.contains("Projects · 3 / 4") && s.contains("Restore the folder")
        })?;
        if mouse {
            t.click("m Move")?;
        } else {
            t.send("m")?;
        }
        let form = t.screen("move form", |s| {
            s.contains("Move project delta-gone") && s.contains("New folder")
        })?;
        println!("MOVE FORM mouse={mouse}\n{form}");
        assert!(form.contains("ktask moves no files"));
        assert!(form.contains("delta-restored"));
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let moved = t.screen("the project follows its folder", |s| {
            s.contains("Project moved to")
                && row(s, "delta-gone").contains("Idle")
                && !s.contains("n Add this folder")
        })?;
        println!("MOVED mouse={mouse}\n{moved}");
        assert!(moved.contains("delta-restored"));

        // The open project is never deleted from under its own screens.
        t.send("g")?;
        t.screen("the open project is selected", |s| {
            s.contains("Projects · 1 / 4")
        })?;
        t.send("D")?;
        let refused = t.screen("deleting the open project is refused", |s| {
            s.contains("The open project cannot be deleted")
        })?;
        assert!(!refused.contains("DANGER"));

        t.send("j")?;
        t.screen("beta selected", |s| s.contains("Projects · 2 / 4"))?;
        if mouse {
            t.click("D Delete")?;
        } else {
            t.send("D")?;
        }
        let danger = t.screen("delete form", |s| {
            s.contains("DANGER · Delete project beta-lib") && s.contains("Type the project name")
        })?;
        println!("DELETE FORM mouse={mouse}\n{danger}");
        assert!(danger.contains("This cannot be undone"));
        assert!(danger.contains("1 unfinished and 0 finished"));
        assert!(danger.contains("is touched"));

        // A wrong name deletes nothing and keeps the form open.
        t.send("beta")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let kept = t.screen("a wrong name is refused", |s| s.contains("exact name"))?;
        println!("WRONG NAME mouse={mouse}\n{kept}");
        assert!(kept.contains("DANGER · Delete project beta-lib"));
        assert!(beta_files.iter().all(|file| file.exists()));

        t.send("-lib")?;
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\r")?;
        }
        let deleted = t.screen("the project is gone", |s| {
            s.contains("Projects · 3 registered")
                && s.contains("Deleted project beta-lib: 1 tasks, 0 attempts")
                && row(s, "beta-lib").is_empty()
        })?;
        println!("DELETED mouse={mouse}\n{deleted}");
        assert!(deleted.contains("Projects: 0 need you"));
        assert!(beta_files.iter().all(|file| !file.exists()));
        assert!(gamma_files.iter().all(|file| file.exists()));
        assert_eq!(fs::read_to_string(root.join("beta-lib/source.rs"))?, "kept");

        let db = Database::open_in(&state(root))?;
        let left: Vec<_> = overview::projects(&db)?
            .into_iter()
            .map(|project| (project.name, project.queued + project.finished))
            .collect();
        assert_eq!(
            left,
            [
                ("alpha-app".to_string(), 220),
                ("delta-gone".to_string(), 0),
                ("gamma-docs".to_string(), 0)
            ]
        );
        let beta_tasks: i64 = db.connection().query_row(
            "SELECT count(*) FROM tasks WHERE project_id = ?1",
            [ids["beta-lib"]],
            |row| row.get(0),
        )?;
        assert_eq!(beta_tasks, 0);

        // A running project is managed only after its runner is paused.
        let runners = state(root).join("runners");
        fs::create_dir_all(&runners)?;
        let lease = fs::File::create(runners.join(format!("{}.lock", ids["gamma-docs"])))?;
        lease.lock()?;
        t.send("G")?;
        t.screen("gamma runs", |s| {
            s.contains("Projects · 3 / 3") && s.contains("Runner: working in the background")
        })?;
        t.send("D")?;
        let running = t.screen("a running project is not deleted", |s| {
            s.contains("gamma-docs is running · pause it before you delete it")
        })?;
        assert!(!running.contains("DANGER"));
        drop(lease);
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

#[test]
fn project_commands_move_and_delete_by_id_and_refuse_without_confirmation() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let run = |args: &[&str]| -> Result<std::process::Output> {
        Ok(Command::new(DEV)
            .args(args)
            .env_clear()
            .env("HOME", root)
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .current_dir(root)
            .output()?)
    };
    let nothing = run(&["project-delete", "1", "--confirm", "alpha-app"])?;
    assert!(!nothing.status.success());
    assert!(!state(root).exists(), "project-delete created storage");

    drop(four_projects(root)?);
    let id = |name: &str| -> Result<String> {
        Ok(overview::projects(&Database::open_in(&state(root))?)?
            .into_iter()
            .find(|project| project.name == name)
            .ok_or("fixture project")?
            .id
            .to_string())
    };
    let (alpha, delta) = (id("alpha-app")?, id("delta-gone")?);
    let files = ktask_files(root, alpha.parse()?)?;

    fs::create_dir(root.join("delta-back"))?;
    let moved = run(&["--json", "project-move", &delta, "delta-back"])?;
    assert!(
        moved.status.success(),
        "{}",
        String::from_utf8_lossy(&moved.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&moved.stdout)?;
    println!("PROJECT-MOVE JSON\n{reply}");
    assert_eq!(
        reply["data"]["root"],
        root.join("delta-back")
            .canonicalize()?
            .to_string_lossy()
            .as_ref()
    );
    let taken = run(&["project-move", &delta, "alpha-app"])?;
    assert!(!taken.status.success());
    assert!(String::from_utf8_lossy(&taken.stderr).contains("already project alpha-app"));

    for refused in [
        run(&["project-delete", &alpha])?,
        run(&["project-delete", &alpha, "--confirm", "alpha"])?,
        run(&[
            "project-delete",
            &alpha,
            "--confirm",
            "alpha-app",
            "--project",
            ".",
        ])?,
    ] {
        assert!(!refused.status.success());
    }
    assert!(files.iter().all(|file| file.exists()));

    let deleted = run(&["--json", "project-delete", &alpha, "--confirm", "alpha-app"])?;
    assert!(
        deleted.status.success(),
        "{}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    let reply: serde_json::Value = serde_json::from_slice(&deleted.stdout)?;
    println!("PROJECT-DELETE JSON\n{reply}");
    assert_eq!(reply["data"]["name"], "alpha-app");
    assert_eq!(reply["data"]["tasks"], 220);
    assert_eq!(reply["data"]["leftovers"], serde_json::json!([]));
    assert!(files.iter().all(|file| !file.exists()));
    assert!(root.join("alpha-app").is_dir());

    let text = String::from_utf8(run(&["projects"])?.stdout)?;
    println!("PROJECTS AFTER\n{text}");
    assert!(!text.contains("alpha-app"));
    assert!(text.contains("\tidle\t0\tdelta-gone\t"));
    assert!(text.contains("delta-back"));
    Ok(())
}
