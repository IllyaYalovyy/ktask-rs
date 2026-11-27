//! Moving a project to another computer: its settings go to one file that import adjusts to
//! the folder there, beside the task export. Each "computer" is a separate instance with its
//! own home and clone; real terminals drive Export and Import settings.
use super::*;
use ktask_rs_mk3::application::{
    categories::{self, Category, Templates},
    configuration::{Defaults, MODEL_ROLES, ProjectCheck},
    lenses::Lens,
    preferences::{Continuation, PlannerApproval, Preferences},
    user_defaults::UserDefaults,
};

/// One computer: a fresh ktask instance with the provider fixture and a clone in a folder
/// named after its user.
struct Computer {
    fixture: Fixture,
    clone: PathBuf,
}

impl Computer {
    /// Clones `origin`, or the fixture's first commit, into `home/USER/PLACE`.
    fn new(user: &str, place: &str, origin: Option<&Path>) -> Result<Self> {
        let fixture = Fixture::new()?;
        // The fixture set up its own project; this computer starts with no ktask data.
        for directory in ["state", "config", "cache"] {
            let path = fixture.scratch.path().join(directory);
            if path.exists() {
                fs::remove_dir_all(path)?;
            }
        }
        let clone = fixture.scratch.path().join("home").join(user).join(place);
        let origin = origin.unwrap_or(&fixture.root).to_owned();
        fs::create_dir_all(clone.parent().unwrap())?;
        let cloned = Command::new("git")
            .args(["clone", "-q"])
            .arg(&origin)
            .arg(&clone)
            .output()?;
        assert!(cloned.status.success(), "{cloned:?}");
        fixture.git_in(&clone, &["config", "user.name", "Project Author"])?;
        fixture.git_in(&clone, &["config", "user.email", "project@example.com"])?;
        Ok(Self { fixture, clone })
    }
    fn ok(&self, args: &[&str]) -> Result<Value> {
        self.fixture.ok_in(&self.clone, args)
    }
    /// Runs a command that must fail and returns its exit code and error message.
    fn refused(&self, args: &[&str]) -> Result<(i32, String)> {
        let output = self
            .fixture
            .process_in(&self.clone)
            .arg("--json")
            .args(args)
            .output()?;
        assert!(!output.status.success(), "{args:?} succeeded");
        let error: Value = serde_json::from_slice(&output.stderr)?;
        Ok((
            output.status.code().unwrap_or_default(),
            error["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .into(),
        ))
    }
    fn paths(&self) -> Paths {
        let root = self.fixture.scratch.path();
        Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join(".local/share/ktask-dev"),
            runtime: root.join("runtime/ktask-dev"),
            executable: BIN.into(),
        }
    }
    fn service(&self) -> Result<Service> {
        Ok(Service::open(self.fixture.db()?, &self.clone, None)?)
    }
    fn file(&self, name: &str) -> String {
        self.fixture
            .scratch
            .path()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
    fn folder(&self) -> String {
        self.clone.to_string_lossy().into_owned()
    }
}

fn model(role: &str) -> Option<ModelConfig> {
    Some(ModelConfig {
        provider: "codex".into(),
        model: role.into(),
        effort: None,
    })
}

/// A source computer with every kind of setting configured, some naming its own folder.
fn configured_source() -> Result<Computer> {
    let source = Computer::new("alice", "work/app", None)?;
    let folder = source.folder();
    fs::create_dir(source.clone.join("scripts"))?;
    fs::write(
        source.clone.join("scripts/check.sh"),
        "#!/bin/sh\nprintf '%s\\n' \"$0\" >> \"$HOME/check-ran\"\ntest ! -f result.txt || grep -q verified result.txt\n",
    )?;
    source.fixture.git_in(&source.clone, &["add", "scripts"])?;
    source
        .fixture
        .git_in(&source.clone, &["commit", "-qm", "check script"])?;
    source.ok(&[
        "check-set",
        "build",
        "--command",
        &format!("sh {folder}/scripts/check.sh"),
        "--timeout-ms",
        "120000",
    ])?;
    let mut service = source.service()?;
    service.save_check(
        &service.project_checks()?,
        &ProjectCheck {
            id: None,
            name: "lint".into(),
            command: format!("/opt/lint/run --root={folder} -I{folder}/include"),
            directory: "scripts".into(),
            timeout_ms: 5000,
            enabled: false,
        },
    )?;
    // Every role's user default, the other new-project settings and the default profile.
    let mut shared: Defaults = std::array::from_fn(|index| model(MODEL_ROLES[index]));
    shared[0] = model("planner");
    let team = service.create_profile("Team", &shared)?;
    let user = service.user_defaults()?;
    service.save_new_project_setup(
        &user,
        &UserDefaults {
            check_timeout_ms: 90_000,
            default_profile: Some(team.name.clone()),
            ..user.clone()
        },
        &shared,
    )?;
    service.select_profile(Some("Team"))?;
    let mut own: Defaults = std::array::from_fn(|_| None);
    own[1] = model("execution");
    service.save_project_overrides(&service.project_overrides()?, &own)?;
    let category = |name: &str, scope| Category {
        name: name.into(),
        description: "Shipping work".into(),
        checks: true,
        templates: Templates {
            execution: format!(
                "Work in {folder}: {{{{title}}}} {{{{goal}}}} {{{{criteria}}}} {{{{context}}}}"
            ),
            review: "Review {{goal}} {{context}}".into(),
            validation: "Validate {{criteria}} {{context}}".into(),
            remediation: "Remediate {{title}} {{context}}".into(),
        },
        scope,
    };
    service.save_category(
        categories::Scope::User,
        None,
        &category("release", categories::Scope::User),
    )?;
    service.save_category(
        categories::Scope::Project,
        None,
        &category("feature", categories::Scope::Project),
    )?;
    let lens = |name: &str, scope| Lens {
        name: name.into(),
        description: "Words".into(),
        instructions: "Judge the wording of {{title}}: {{context}}".into(),
        scope,
    };
    service.save_lens(
        categories::Scope::User,
        None,
        &lens("tone", categories::Scope::User),
    )?;
    service.save_lens(
        categories::Scope::Project,
        None,
        &lens("style", categories::Scope::Project),
    )?;
    drop(service);
    source.ok(&["lens", "defaults", "--set", "style"])?;
    source.ok(&["delivery", "--remote", "upstream", "--base", "trunk"])?;
    source.ok(&["retention", "7", "--max-files", "200000"])?;
    source.ok(&["init", "--agent-network", "on", "--check-before-task", "on"])?;
    source.ok(&["cache", "limits", "--keep-generations", "5"])?;
    source.ok(&["agent", "limits", "--time-limit-ms", "60000"])?;
    source.ok(&["tool", "provision-mode", "automatic"])?;
    let service = source.service()?;
    let config = source.paths().config;
    service.save_preferences(
        &config,
        &service.preferences(&config)?,
        &Preferences {
            version: 0,
            mouse: false,
            planner_approval: PlannerApproval::ReviewRequired,
            continuation: Continuation::Single,
        },
    )?;
    Ok(source)
}

/// What the read commands report, with the project folder written the same way everywhere.
fn observed(computer: &Computer) -> Result<Vec<(String, Value)>> {
    let mut commands: Vec<Vec<&str>> = vec![
        vec!["checks"],
        vec!["defaults"],
        vec!["delivery"],
        vec!["profile", "list"],
        vec!["profile", "show", "Team"],
        vec!["profile", "effective"],
        vec!["category", "list"],
        vec!["category", "show", "feature"],
        vec!["category", "show", "release"],
        vec!["lens", "list"],
        vec!["lens", "show", "style"],
        vec!["lens", "defaults"],
        vec!["agent", "limits"],
        vec!["cache", "limits"],
        vec!["tool", "provision-status"],
        vec!["storage"],
    ];
    commands
        .drain(..)
        .map(|command| {
            let mut data = computer.ok(&command)?;
            // Allocation is this computer's disk use; only the retention policy is a setting.
            if command == ["storage"] {
                data = data["preview"]["policy"].clone();
            }
            let text = data.to_string().replace(&computer.folder(), "<project>");
            Ok((command.join(" "), serde_json::from_str(&text)?))
        })
        .collect()
}

/// Exports the configured source, checking what the command lists and writes.
fn export_settings(source: &Computer, exported: &str) -> Result<Value> {
    let written = source.ok(&["settings-export", exported])?;
    assert_eq!(written["version"], 1);
    for setting in [
        "check build",
        "check lint",
        "model execution",
        "selected profile",
        "shared profile Team review",
        "category feature",
        "shared category release",
        "lens style",
        "shared lens tone",
        "default lenses",
        "delivery remote",
        "retention days",
        "report limits time_limit_ms",
        "automatic provisioning",
        "preferences mouse",
        "shared build cache keep_generations",
        "shared user default check_timeout_ms",
        "shared model planning",
    ] {
        assert!(
            written["written"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["setting"] == setting),
            "{setting}: {written}"
        );
    }
    let text = fs::read_to_string(exported)?;
    let file: Value = serde_json::from_str(&text)?;
    assert_eq!(file["format"], "ktask-settings");
    assert_eq!(file["source_folder"], source.folder());
    // The old folder appears only as that informational value.
    assert_eq!(text.matches(&source.folder()).count(), 1, "{text}");
    assert!(text.contains("sh ${KTASK_PROJECT_ROOT}/scripts/check.sh"));

    // The destination is never replaced without --overwrite.
    let (code, error) = source.refused(&["settings-export", exported])?;
    assert_eq!(code, 4);
    assert!(error.contains("--overwrite"), "{error}");
    assert_eq!(fs::read_to_string(exported)?, text);
    let listed = source
        .fixture
        .process_in(&source.clone)
        .args(["settings-export", exported, "--overwrite"])
        .output()?;
    let listed = String::from_utf8(listed.stdout)?;
    assert!(listed.starts_with("Exported "), "{listed}");
    assert!(listed.contains("delivery remote: upstream"), "{listed}");
    assert_eq!(fs::read_to_string(exported)?, text);
    Ok(file)
}

fn path_notes(report: &Value, key: &str) -> Vec<String> {
    report[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|note| {
            format!(
                "{}: {}",
                note["setting"].as_str().unwrap(),
                note["path"].as_str().unwrap()
            )
        })
        .collect()
}

/// Imports into the empty instance of `target` and checks what moved and where it points.
fn import_into_new_clone(target: &Computer, exported: &str) -> Result {
    let imported = target.ok(&["settings-import", exported])?;
    assert_eq!(imported["applied"], true);
    assert_eq!(imported["registers_project"], true);
    assert_eq!(imported["folder"], target.folder());
    let rewritten = format!("check build: {}/scripts/check.sh", target.folder());
    assert!(
        path_notes(&imported, "rewritten").contains(&rewritten),
        "{imported}"
    );
    assert_eq!(
        path_notes(&imported, "outside"),
        ["check lint: /opt/lint/run"]
    );

    // No setting points at the old folder; commands name this one.
    let checks = target.service()?.project_checks()?;
    assert_eq!(
        checks[1].command,
        format!("/opt/lint/run --root={0} -I{0}/include", target.folder())
    );
    assert!(!checks[1].enabled);
    assert_eq!(checks[1].directory, "scripts");
    Ok(())
}

/// The first imported task succeeds, running the imported check from this clone.
fn first_task_passes_its_checks(target: &Computer) -> Result {
    let ran = target.ok(&["run", "--one"])?;
    let task = target.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{ran}\n{task}");
    let check_steps: Vec<_> = task["attempts"][0]["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|step| step["role"] == "check")
        .collect();
    assert!(
        !check_steps.is_empty() && check_steps.iter().all(|step| step["state"] == "succeeded"),
        "{task}"
    );
    let invoked = fs::read_to_string(target.fixture.scratch.path().join("check-ran"))?;
    let here = format!("{}/scripts/check.sh", target.folder());
    assert!(invoked.lines().all(|line| line == here), "{invoked}");
    Ok(())
}

#[test]
fn a_project_moves_to_another_computer_and_runs_as_it_did() -> Result {
    let source = configured_source()?;
    let exported = source.file("settings.json");
    let file = export_settings(&source, &exported)?;
    source.ok(&[
        "add",
        "--title",
        "Carry result",
        "--goal",
        "Create result.txt containing verified",
        "--criterion",
        "result.txt contains verified",
        "--category",
        "feature",
    ])?;
    let tasks = source.file("tasks.tsv");
    source.ok(&["export", &tasks])?;
    let before = observed(&source)?;

    let target = Computer::new("bob", "src/clones/app", Some(&source.clone))?;
    assert_ne!(target.folder(), source.folder());
    import_into_new_clone(&target, &exported)?;
    let after = observed(&target)?;
    for ((command, expected), (_, actual)) in before.iter().zip(&after) {
        assert_eq!(actual, expected, "ktask {command}");
    }
    let reexported = target.file("again.json");
    target.ok(&["settings-export", &reexported])?;
    let again: Value = serde_json::from_str(&fs::read_to_string(&reexported)?)?;
    assert_eq!(again["project"], file["project"]);
    assert_eq!(again["shared"], file["shared"]);
    assert!(!fs::read_to_string(&reexported)?.contains(&source.folder()));

    // A second import of the same file changes nothing, not even a version.
    let repeated = target.ok(&["settings-import", &exported])?;
    assert_eq!(repeated["registers_project"], false);
    assert!(
        repeated["changes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|change| change["status"] == "unchanged"),
        "{repeated}"
    );
    assert_eq!(observed(&target)?, after);

    // Then the tasks.
    target.ok(&["import", &tasks])?;
    assert_eq!(
        target.ok(&["show", "1"])?["definition"]["category"],
        "feature"
    );
    first_task_passes_its_checks(&target)
}

/// A file that names only some settings.
fn partial(computer: &Computer, name: &str, body: &str) -> Result<String> {
    let file = computer.file(name);
    fs::write(
        &file,
        format!(r#"{{"format":"ktask-settings","version":1,{body}}}"#),
    )?;
    Ok(file)
}

#[test]
fn import_replaces_what_the_file_names_and_keeps_everything_else() -> Result {
    let computer = Computer::new("carol", "app", None)?;
    computer.ok(&["check-set", "build", "--command", "true"])?;
    computer.ok(&["check-set", "keep", "--command", "true"])?;
    for role in ["execution", "review", "validation", "remediation"] {
        computer.ok(&[
            "model-default",
            role,
            "--provider",
            "codex",
            "--model",
            "old",
        ])?;
    }
    computer.ok(&["delivery", "--remote", "mirror"])?;
    computer.ok(&["retention", "9"])?;
    let add = |title: &str| {
        computer.ok(&[
            "add",
            "--title",
            title,
            "--goal",
            "Goal",
            "--criterion",
            "Criterion",
        ])
    };
    add("Before")?;
    add("Finished")?;
    computer.ok(&["done", "2"])?;
    let before = computer.ok(&["show", "1"])?;

    let file = partial(
        &computer,
        "partial.json",
        r#""project":{"checks":[{"name":"build","command":"test -d .","timeout_ms":1000},{"name":"extra","command":"true","timeout_ms":1000}],"models":{"execution":{"provider":"codex","model":"new"}},"delivery":{"base_branch":"release"}}"#,
    )?;
    let report = computer.ok(&["settings-import", &file])?;
    let statuses: Vec<_> = report["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|change| {
            format!(
                "{} {}",
                change["status"].as_str().unwrap(),
                change["setting"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        statuses,
        [
            "changed model execution",
            "changed check build",
            "added check extra",
            "changed delivery base_branch",
        ]
    );
    let checks = computer.ok(&["checks"])?["checks"].clone();
    let commands: Vec<_> = checks
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            format!(
                "{}={}",
                c["name"].as_str().unwrap(),
                c["command"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(commands, ["build=test -d .", "keep=true", "extra=true"]);
    let delivery = computer.ok(&["delivery"])?["delivery"].clone();
    assert_eq!(delivery["remote"], "mirror");
    assert_eq!(delivery["base_branch"], "release");
    assert_eq!(computer.ok(&["storage"])?["preview"]["policy"]["days"], 9);
    // Existing tasks and history keep their revisions; new tasks use the imported model.
    assert_eq!(computer.ok(&["show", "1"])?, before);
    assert_eq!(
        computer.ok(&["list", "--scope", "history"])?["tasks"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
    add("After")?;
    let model = |id: &str| -> Result<Vec<Value>> {
        let models = computer.ok(&["show", id])?["definition"]["models"].clone();
        Ok(vec![
            models["execution"]["model"].clone(),
            models["review"]["model"].clone(),
        ])
    };
    assert_eq!(model("1")?, ["old", "old"]);
    assert_eq!(model("3")?, ["new", "old"]);
    Ok(())
}

#[test]
fn unreadable_files_and_folders_are_refused_and_nothing_is_applied_in_part() -> Result {
    let computer = Computer::new("dave", "app", None)?;
    let state = computer.fixture.scratch.path().join("state");
    // Nothing creates storage for a file or folder that cannot be imported.
    let newer = computer.file("newer.json");
    fs::write(&newer, r#"{"format":"ktask-settings","version":2}"#)?;
    let broken = computer.file("broken.json");
    fs::write(&broken, "{")?;
    for (file, reason) in [
        (&newer, "newer than this ktask reads"),
        (&broken, "Malformed settings file"),
    ] {
        for preview in [None, Some("--preview")] {
            let mut args = vec!["settings-import", file.as_str()];
            args.extend(preview);
            let (code, error) = computer.refused(&args)?;
            assert_eq!(code, 4);
            assert!(error.contains(reason), "{error}");
        }
    }
    let valid = partial(
        &computer,
        "valid.json",
        r#""project":{"agent_network":true}"#,
    )?;
    let plain = computer.fixture.scratch.path().join("plain");
    fs::create_dir(&plain)?;
    let inside = computer.clone.join("sub");
    fs::create_dir(&inside)?;
    for folder in [&plain, &inside] {
        let folder = folder.to_string_lossy();
        let (code, error) = computer.refused(&["--project", &folder, "settings-import", &valid])?;
        assert_eq!(code, 4);
        assert!(
            error.contains("not the root of a Git repository"),
            "{error}"
        );
    }
    assert!(!state.exists());

    // A preview of a fresh instance reports and creates nothing.
    let preview = computer.ok(&["settings-import", &valid, "--preview"])?;
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["registers_project"], true);
    assert_eq!(preview["changes"][0]["status"], "changed");
    assert!(!state.exists());

    // The last setting is invalid: neither the earlier ones nor the registration apply.
    for invalid in [
        r#""project":{"agent_network":true,"checks":[{"name":"ok","command":"true","timeout_ms":1}],"retention":{"days":-1}}"#,
        r#""shared":{"models":{"review":{"provider":"codex","model":"m"}}},"project":{"checks":[{"name":"gone","command":"true","directory":"no-such-directory","timeout_ms":1}]}"#,
        r#""project":{"profile":"Nobody"}"#,
        r#""project":{"lens_defaults":["nothing"]}"#,
        r#""project":{"models":{"review":{"provider":"gemini","model":"m"}}}"#,
        r#""project":{"agent_network":true},"shared":{"build_cache":{"keep_generations":0}}"#,
    ] {
        let file = partial(&computer, "invalid.json", invalid)?;
        for preview in [None, Some("--preview")] {
            let mut args = vec!["settings-import", file.as_str()];
            args.extend(preview);
            assert_eq!(computer.refused(&args)?.0, 4, "{invalid}");
        }
        assert_eq!(computer.ok(&["projects"])?["count"], 0, "{invalid}");
        let rows: i64 = computer.fixture.db()?.connection().query_row(
            "SELECT count(*) FROM default_models",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(rows, 0, "{invalid}");
    }

    // A settings file kept beside the database that cannot be written undoes the others.
    computer.ok(&["settings-import", &valid])?;
    let config = computer.paths().config;
    let preferences = config.join("project-1.json");
    assert!(!preferences.exists());
    fs::create_dir_all(config.join(".build-cache.lock"))?;
    let file = partial(
        &computer,
        "files.json",
        r#""project":{"agent_network":false,"preferences":{"mouse":false}},"shared":{"build_cache":{"grace_secs":1}}"#,
    )?;
    let (code, error) = computer.refused(&["settings-import", &file])?;
    assert_eq!(code, 1, "{error}");
    assert!(!preferences.exists());
    assert!(computer.service()?.agent_network()?);

    // Only the owner enables provisioning.
    let provisioning = partial(
        &computer,
        "provisioning.json",
        r#""project":{"automatic_provisioning":true}"#,
    )?;
    let (_, error) = computer.refused(&[
        "--actor-kind",
        "ai",
        "--actor",
        "bot",
        "settings-import",
        &provisioning,
    ])?;
    assert!(error.contains("Only a human actor"), "{error}");
    Ok(())
}

#[test]
fn a_preview_names_what_is_missing_here_and_changes_nothing() -> Result {
    let computer = Computer::new("erin", "app", None)?;
    computer.ok(&["check-set", "build", "--command", "true"])?;
    let before = computer.fixture.db()?.connection().query_row(
        "SELECT command FROM project_checks",
        [],
        |r| r.get::<_, String>(0),
    )?;
    let file = partial(
        &computer,
        "needs.json",
        r#""project":{"checks":[{"name":"build","command":"no-such-program --all ${KTASK_PROJECT_ROOT}/x","timeout_ms":1000}],"models":{"review":{"provider":"claude","model":"opus"}},"preferences":{"mouse":false}},"tools":[{"name":"android-sdk","version":"36"}]"#,
    )?;
    let output = computer
        .fixture
        .process_in(&computer.clone)
        .args(["settings-import", &file, "--preview"])
        .output()?;
    let text = String::from_utf8(output.stdout)?;
    println!("{text}");
    for expected in [
        "Preview, nothing applied yet: 1 to add, 2 to change, 0 left as they are".to_owned(),
        format!("Folder: {}", computer.folder()),
        format!("- check build: {}/x", computer.folder()),
        "Claude Code CLI (claude) is not on PATH, so these models cannot run: review claude/opus"
            .to_owned(),
        "program no-such-program of check build was not found".to_owned(),
        "tool android-sdk 36 is not bound to this project".to_owned(),
        "- add model review".to_owned(),
        "- change check build".to_owned(),
        "- change preferences mouse: true -> false".to_owned(),
    ] {
        assert!(text.contains(&expected), "{expected}\n{text}");
    }
    let after = computer.fixture.db()?.connection().query_row(
        "SELECT command FROM project_checks",
        [],
        |r| r.get::<_, String>(0),
    )?;
    assert_eq!(after, before);
    assert!(!computer.paths().config.join("project-1.json").exists());
    assert_eq!(
        computer
            .ok(&["profile", "effective"])?
            .to_string()
            .matches("opus")
            .count(),
        0
    );
    Ok(())
}

#[test]
fn export_works_beside_a_live_runner_and_import_waits_for_it_to_stop() -> Result {
    let computer = Computer::new("frank", "app", None)?;
    for role in ["execution", "review", "validation", "remediation"] {
        computer.ok(&[
            "model-default",
            role,
            "--provider",
            "codex",
            "--model",
            role,
        ])?;
    }
    computer.ok(&[
        "check-set",
        "build",
        "--command",
        "touch \"$HOME/check-started\"; sleep 60",
    ])?;
    computer.ok(&[
        "add",
        "--title",
        "Slow",
        "--goal",
        "Goal",
        "--criterion",
        "Criterion",
    ])?;
    computer.ok(&["run", "--background"])?;
    let started = computer.fixture.scratch.path().join("check-started");
    support::eventually("the check is running", || {
        Ok(started.exists().then_some(()))
    })?;

    let exported = computer.file("live.json");
    computer.ok(&["settings-export", &exported])?;
    let checks = partial(
        &computer,
        "checks.json",
        r#""project":{"checks":[{"name":"build","command":"true","timeout_ms":1000}]}"#,
    )?;
    let (code, error) = computer.refused(&["settings-import", &checks])?;
    assert_eq!(code, 4);
    assert!(error.contains("live runner"), "{error}");
    let preview = computer.ok(&["settings-import", &checks, "--preview"])?;
    assert!(
        preview["blocked"].as_str().unwrap().contains("live runner"),
        "{preview}"
    );
    // Unchanged settings and ones no attempt uses do not need the runner stopped.
    computer.ok(&["settings-import", &exported])?;
    let models = partial(
        &computer,
        "models.json",
        r#""project":{"models":{"review":{"provider":"codex","model":"other"}}}"#,
    )?;
    computer.ok(&["settings-import", &models])?;

    computer.ok(&["pause"])?;
    // The paused attempt still owns its checks, as for check-set.
    let (_, error) = computer.refused(&["settings-import", &checks])?;
    assert!(error.contains("live attempt"), "{error}");
    let network = partial(
        &computer,
        "network.json",
        r#""project":{"agent_network":true,"check_before_task":false}"#,
    )?;
    // These switches are not checks: only the stopped runner is needed.
    computer.ok(&["settings-import", &network])?;
    computer.ok(&["stop", "1"])?;
    computer.ok(&["settings-import", &checks])?;
    assert_eq!(computer.ok(&["checks"])?["checks"][0]["command"], "true");
    Ok(())
}

#[test]
fn an_import_applies_only_what_its_preview_showed() -> Result {
    let computer = Computer::new("hana", "app", None)?;
    computer.ok(&["check-set", "build", "--command", "true"])?;
    let data = fs::read(partial(
        &computer,
        "switch.json",
        r#""project":{"agent_network":true,"preferences":{"mouse":false}}"#,
    )?)?;
    let paths = computer.paths();
    let mut service = computer.service()?;
    let previewed = service.preview_settings(&paths, &data)?;
    assert!(!previewed.applied && previewed.changes_anything());
    assert!(!service.agent_network()?);
    // The setting changed elsewhere since the preview.
    computer.service()?.set_agent_network(true)?;
    let error = service
        .import_settings(&paths, &data, &previewed)
        .unwrap_err()
        .to_string();
    assert!(error.contains("changed since the preview"), "{error}");
    assert!(service.preferences(&paths.config)?.mouse);
    let previewed = service.preview_settings(&paths, &data)?;
    let imported = service.import_settings(&paths, &data, &previewed)?;
    assert!(imported.applied);
    assert!(!service.preferences(&paths.config)?.mouse);
    Ok(())
}

const SIZES: [(u16, u16); 2] = [(70, 22), (120, 36)];

fn press(t: &mut Terminal, mouse: bool, key: &str, button: &str) -> Result {
    if mouse { t.click(button) } else { t.send(key) }
}

fn shown(t: &Terminal, (case, mouse): (&str, bool), label: &str, text: &str) -> Result<String> {
    let screen = t.screen(label, |s| s.contains(text))?;
    crate::support::record(&format!("{case}{label}"), mouse, &screen);
    Ok(screen)
}

/// Types `path` into the form's only field, replacing what it holds.
fn type_path(t: &mut Terminal, case: (&str, bool), path: &str) -> Result {
    t.send(&format!("\x01{path}"))?;
    let end = &path[path.len() - 20..];
    let name = path.rsplit('/').next().unwrap_or(path);
    shown(t, case, &format!("PATH {name}"), &format!("{end}▏"))?;
    Ok(())
}

#[test]
fn configuration_exports_and_imports_settings_by_keyboard_and_mouse() -> Result {
    for (columns, rows) in SIZES {
        for mouse in [false, true] {
            let case = ("", mouse);
            let computer = Computer::new("gina", "app", None)?;
            computer.ok(&["check-set", "build", "--command", "test -d ."])?;
            let scratch = computer.fixture.scratch.path();
            let mut t = Terminal::open_project(BIN, scratch, &computer.clone, columns, rows)?;
            t.screen("startup", |s| s.contains("No current task"))?;
            press(&mut t, mouse, "4", "4 Config")?;
            let config = shown(&t, case, "CONFIG", "[E Export settings]")?;
            assert!(config.contains("[o Import settings]"), "{config}");

            // The default destination is beside the project folder, not inside it.
            let file = computer.clone.with_file_name("app-ktask-settings.json");
            press(&mut t, mouse, "E", "E Export settings")?;
            shown(&t, case, "EXPORT FORM", "Export settings · disk")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "EXPORTED", "settings to /")?;
            let exported = fs::read_to_string(&file)?;
            assert!(exported.contains("test -d ."), "{exported}");

            // An existing file is replaced only after confirmation.
            computer.ok(&["check-set", "build", "--command", "false"])?;
            press(&mut t, mouse, "E", "E Export settings")?;
            shown(&t, case, "EXPORT AGAIN", "Export settings · disk")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "CONFIRM OVERWRITE", "Overwrite file?")?;
            press(&mut t, mouse, "\x1b", "Cancel")?;
            t.screen("overwrite cancelled", |s| !s.contains("Overwrite file?"))?;
            assert_eq!(fs::read_to_string(&file)?, exported);

            press(&mut t, mouse, "o", "o Import settings")?;
            shown(&t, case, "IMPORT FORM", "Import settings · disk")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            let preview = shown(&t, case, "PREVIEW", "Import settings preview")?;
            assert!(preview.contains("nothing applied yet"), "{preview}");
            assert!(preview.contains("1 to change"), "{preview}");
            // Changes are listed before the settings left as they are.
            assert!(preview.contains("- change check build"), "{preview}");
            assert_eq!(
                computer.ok(&["checks"])?["checks"][0]["command"],
                "false",
                "a preview changes nothing"
            );
            // The whole preview is reachable, down to the last setting.
            for page in 0..40 {
                if t.current_screen().contains("fix limits time_limit_ms") {
                    break;
                }
                assert!(
                    page < 39,
                    "preview end unreachable:\n{}",
                    t.current_screen()
                );
                let before = t.current_screen();
                press(&mut t, mouse, "\x1b[6~", "Down")?;
                t.screen("preview scrolls", |s| s != before)?;
            }
            shown_still(&t, case, "PREVIEW END", "fix limits time_limit_ms")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "IMPORTED", "Settings imported")?;
            assert_eq!(
                computer.ok(&["checks"])?["checks"][0]["command"],
                "test -d ."
            );

            // A file that cannot be read is reported on the form that named it.
            let newer = computer.file("newer.json");
            fs::write(&newer, r#"{"format":"ktask-settings","version":2}"#)?;
            press(&mut t, mouse, "o", "o Import settings")?;
            t.screen("import form", |s| s.contains("Import settings · disk"))?;
            type_path(&mut t, case, &newer)?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "NEWER FILE", "newer than this")?;
            press(&mut t, mouse, "\x1b", "Cancel")?;
            t.screen("form closed", |s| !s.contains("Import settings · disk"))?;
            t.send("q")?;
            t.finish()?;
        }
    }
    Ok(())
}

fn shown_still(t: &Terminal, (case, mouse): (&str, bool), label: &str, text: &str) -> Result {
    let screen = t.still(label, |s| s.contains(text))?;
    crate::support::record(&format!("{case}{label}"), mouse, &screen);
    Ok(())
}

#[test]
fn demo_settings_move_through_memory_files_without_a_database() -> Result {
    for (columns, rows) in SIZES {
        for mouse in [false, true] {
            let case = ("demo ", mouse);
            let sandbox = support::sandbox()?;
            let mut t = Terminal::open_at(BIN, sandbox.path(), columns, rows)?;
            t.screen("demo", |s| s.contains("MOCK DATA"))?;
            press(&mut t, mouse, "4", "4 Config")?;
            shown(&t, case, "CONFIG", "[E Export settings]")?;

            press(&mut t, mouse, "E", "E Export settings")?;
            shown(&t, case, "EXPORT", "in-memory demo files")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            t.screen("export closed", |s| {
                !s.contains("Export settings · in-memory")
            })?;

            press(&mut t, mouse, "o", "o Import settings")?;
            shown(&t, case, "IMPORT", "/demo/settings.json")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            let preview = shown(&t, case, "PREVIEW", "2 to change, 1 left")?;
            assert!(preview.contains("preview · simulated"), "{preview}");
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "IMPORTED", "Continue queue: Disabled")?;

            // The exported session settings are read back from memory.
            press(&mut t, mouse, "o", "o Import settings")?;
            t.screen("import again", |s| {
                s.contains("Import settings · in-memory")
            })?;
            type_path(&mut t, case, "/demo/my-settings.json")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "PREVIEW BACK", "2 to change, 1 left")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "RESTORED", "Continue queue: Enabled")?;

            press(&mut t, mouse, "o", "o Import settings")?;
            t.screen("import form", |s| s.contains("Import settings · in-memory"))?;
            type_path(&mut t, case, "/demo/settings-newer.json")?;
            press(&mut t, mouse, "\x13", "Apply")?;
            shown(&t, case, "NEWER", "newer than this")?;
            press(&mut t, mouse, "\x1b", "Cancel")?;
            t.screen("form closed", |s| {
                !s.contains("Import settings · in-memory")
            })?;
            t.send("q")?;
            t.finish()?;
            assert!(
                !sandbox
                    .path()
                    .join("state/ktask-dev/ktask.sqlite3")
                    .exists()
            );
            assert!(!sandbox.path().join("demo").exists());
        }
    }
    Ok(())
}
