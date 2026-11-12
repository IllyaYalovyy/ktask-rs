use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{
        ModelConfig, Service,
        configuration::{Defaults, DeliveryConfig, MODEL_ROLES, ProjectCheck},
        preferences::{Continuation, PlannerApproval},
        user_defaults::UserDefaults,
    },
    storage::Database,
};
use std::path::Path;
const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

#[test]
fn any_model_name_saves_and_only_structure_is_rejected_atomically() -> Result {
    use ktask_rs_mk3::application::{Definition, Kind, ROLES, VersionedTask};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut service = service(root)?;
    let defaults = service.project_defaults()?;
    let model = |provider: &str, model: &str, effort: Option<&str>| ModelConfig {
        provider: provider.into(),
        model: model.into(),
        effort: effort.map(Into::into),
    };
    // Names ktask has never heard of are the owner's choice, in every role and provider.
    for (index, role) in MODEL_ROLES.iter().enumerate() {
        for chosen in [
            model("codex", "typo-model", Some("high")),
            model("claude", "fable", Some("xhigh")),
            model("opencode", "any/vendor/model-9", Some("max")),
            model("claude", "claude-whatever-2099", None),
        ] {
            let mut values = defaults.clone();
            values[index] = Some(chosen.clone());
            service.save_project_overrides(&service.project_overrides()?, &values)?;
            assert_eq!(service.project_defaults()?[index], Some(chosen.clone()));
            service.save_new_project_defaults(&values)?;
            assert_eq!(service.new_project_defaults()?[index], Some(chosen.clone()));
            service.set_default(role, &chosen, true)?;
        }
        service.save_project_overrides(&service.project_overrides()?, &defaults)?;
    }
    // Structure is still checked, and a rejected save changes nothing.
    let before = service.project_overrides()?;
    let before_new = service.new_project_defaults()?;
    for (index, role) in MODEL_ROLES.iter().enumerate() {
        for (bad, needle) in [
            (model("gemini", "pro", None), "unsupported provider"),
            (
                model("codex", "  ", None),
                "provider and model are required",
            ),
            (
                model("", "typo-model", None),
                "provider and model are required",
            ),
            (
                model("codex", "typo-model", Some("hight")),
                "accepted values",
            ),
            (model("opencode", "x/y", Some("ultra")), "accepted values"),
            (model("claude", "fable", Some(" ")), "must be nonempty"),
        ] {
            let mut values = before.clone();
            values[index] = Some(bad.clone());
            for error in [
                service
                    .save_project_overrides(&before, &values)
                    .unwrap_err(),
                service.save_new_project_defaults(&values).unwrap_err(),
                service.set_default(role, &bad, true).unwrap_err(),
            ] {
                let message = error.to_string();
                assert!(
                    message.contains(role) && message.contains(needle),
                    "{message}"
                );
            }
            assert_eq!(service.project_overrides()?, before);
            assert_eq!(service.new_project_defaults()?, before_new);
        }
    }
    let mut definition = Definition {
        title: "Atomic models".into(),
        goal: "Validate structure only".into(),
        kind: Kind::Ai,
        category: Some("code".into()),
        criteria: vec!["No partial writes".into()],
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(2),
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    let id = service.create(&[definition.clone()], None, true)?[0];
    for (index, role) in ROLES.iter().enumerate() {
        definition.models[index] = Some(model("codex", "typo-model", Some("hight")));
        let before = service.task(id)?.unwrap();
        let target = VersionedTask {
            id,
            version: before.version,
        };
        let error = service
            .revise(target, &definition, true)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(role) && error.contains("accepted values"),
            "{error}"
        );
        let error = service
            .create(
                &[before.definition().definition.clone(), definition.clone()],
                None,
                true,
            )
            .unwrap_err()
            .to_string();
        assert!(error.contains(role), "{error}");
        let after = service.task(id)?.unwrap();
        assert_eq!(after.version, before.version);
        assert_eq!(after.revisions.len(), before.revisions.len());
        assert_eq!(service.snapshot()?.queue.len(), 1);
        // The same unknown name with a valid effort is accepted as written.
        definition.models[index] = Some(model("codex", "typo-model", Some("high")));
        service.revise(target, &definition, true)?;
        assert_eq!(
            service.task(id)?.unwrap().definition().definition.models[index],
            definition.models[index]
        );
        definition.models[index] = None;
    }
    Ok(())
}

#[test]
fn real_git_delivery_form_supports_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut t = Terminal::open_real(BIN, root, 80, 24)?;
        screen(&t, "No current task")?;
        open(&mut t, mouse, "4", "4 Config", "Storage: SQLite")?;
        activate(&mut t, mouse, "g", "g Git delivery")?;
        let opened = t.screen("Git delivery form", |s| s.contains("╭ Git delivery"))?;
        println!("GIT DELIVERY FORM mouse={mouse}\n{opened}");
        if mouse {
            t.click("< Local >")?;
            t.click("Git remote")?;
            t.send("\x01upstream")?;
            t.click("Base branch")?;
            t.send("\x01trunk")?;
            t.click("Apply")?;
        } else {
            t.send(" \t\x01upstream\t\x01trunk\x13")?;
        }
        let saved = screen(&t, "Delivery: direct via upstream/trunk")?;
        println!("GIT DELIVERY CONFIG mouse={mouse}\n{saved}");
        let config = service(root)?.delivery_config()?;
        assert_eq!(
            (
                config.mode.as_str(),
                config.remote.as_str(),
                config.base_branch.as_str()
            ),
            ("direct", "upstream", "trunk")
        );
        quit(&mut t)?;
    }
    Ok(())
}
fn service(root: &Path) -> Result<Service> {
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        root,
        None,
    )?)
}
fn screen(t: &Terminal, label: &str) -> Result<String> {
    let rendered = t.screen(label, |s| s.contains(label))?;
    println!("{label}\n{rendered}");
    Ok(rendered)
}
/// The value line under a form field's `label`.
fn field<'a>(s: &'a str, label: &str) -> &'a str {
    let mut lines = s.lines().skip_while(|line| !line.contains(label)).skip(1);
    lines.next().unwrap_or_default()
}
fn activate(t: &mut Terminal, mouse: bool, key: &str, label: &str) -> Result {
    if mouse {
        t.click(label)?;
    } else {
        t.send(key)?;
    }
    Ok(())
}
fn open(t: &mut Terminal, mouse: bool, key: &str, label: &str, title: &str) -> Result {
    activate(t, mouse, key, label)?;
    screen(t, title)?;
    Ok(())
}
fn apply(t: &mut Terminal, mouse: bool) -> Result {
    activate(t, mouse, "\x13", "Apply")?;
    let rendered = t.screen("saved", |s| {
        s.contains("Project configuration saved") && !s.contains("[Apply]")
    })?;
    println!("Project configuration saved\n{rendered}");
    Ok(())
}
fn cancel(t: &mut Terminal, mouse: bool) -> Result {
    open(t, mouse, "\x1b", "Cancel", "Storage: SQLite")
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

fn model_defaults_accept_any_name_and_reject_bad_structure(
    t: &mut Terminal,
    root: &Path,
    mouse: bool,
) -> Result {
    open(t, mouse, "m", "m Defaults", "Project model defaults")?;
    t.send("invalid\x13")?;
    screen(t, "Use provider/model")?;
    assert!(
        service(root)?
            .project_defaults()?
            .iter()
            .all(Option::is_none)
    );
    cancel(t, mouse)?;
    open(t, mouse, "m", "m Defaults", "Project model defaults")?;
    if mouse {
        t.click("execution provider/model")?;
    }
    t.send("codex/typo-model")?;
    apply(t, mouse)?;
    let saved = service(root)?.project_defaults()?;
    assert!(
        saved
            .iter()
            .flatten()
            .any(|m| m.provider == "codex" && m.model == "typo-model"),
        "an arbitrary model name saves: {saved:?}"
    );
    open(t, mouse, "m", "m Defaults", "Project model defaults")?;
    if mouse {
        t.click("execution provider/model")?;
        t.send("\x01codex/executor")?;
        t.click("execution effort")?;
        t.send("hight")?;
        t.click("Apply")?;
    } else {
        t.send("\x01codex/executor\thight\x13")?;
    }
    let invalid = t.screen("invalid effort", |s| {
        s.contains(if mouse {
            "command conflict: execution effort"
        } else {
            "command conflict: planning effort"
        }) && s.contains("hight")
            && s.contains("accepted values")
            && s.contains("minimal")
            && s.contains("high")
            && s.contains("ultra")
    })?;
    println!("INVALID EFFORT FORM mouse={mouse}\n{invalid}");
    assert_eq!(service(root)?.project_defaults()?, saved);
    cancel(t, mouse)?;
    Ok(())
}

#[test]
fn real_configuration_forms_mouse_keyboard_restart_and_validation() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut t = Terminal::open_real(BIN, root, 70, 22)?;
        screen(&t, "No current task")?;
        open(&mut t, mouse, "4", "4 Config", "Storage: SQLite")?;
        model_defaults_accept_any_name_and_reject_bad_structure(&mut t, root, mouse)?;
        open(&mut t, mouse, "m", "m Defaults", "Project model defaults")?;
        for (i, role) in MODEL_ROLES.iter().enumerate() {
            let label = format!("{role} provider/model");
            if mouse {
                // Mouse paging moves focus by the visible field capacity.
                let rendered = t.screen("form", |s| s.contains("Project model defaults"))?;
                if !rendered.contains(&label) {
                    t.click("Fields +")?;
                    screen(&t, &label)?;
                }
                t.click(&label)?;
            }
            t.send(&format!("\x01codex/{role}"))?;
            if mouse {
                t.click(&format!("{role} effort"))?;
            } else {
                t.send("\t")?;
            }
            t.send("high")?;
            if !mouse && i < 4 {
                t.send("\t")?;
            }
            let (model, effort) = (format!("codex/{role}"), format!("{role} effort"));
            let rendered = t.screen(&effort, |s| {
                field(s, &label).contains(&model) && field(s, &effort).contains("high")
            })?;
            println!("{effort}\n{rendered}");
        }
        apply(&mut t, mouse)?;
        let saved = service(root)?.project_defaults()?;
        assert!(
            saved
                .iter()
                .all(|m| m.as_ref().unwrap().effort.as_deref() == Some("high"))
        );
        open(&mut t, mouse, "m", "m Defaults", "Project model defaults")?;
        t.send("\x01codex/cancelled")?;
        cancel(&mut t, mouse)?;
        assert_eq!(saved, service(root)?.project_defaults()?);

        open(&mut t, mouse, "c", "c Checks", "No checks configured")?;
        open(&mut t, mouse, "\r", "Apply", "Edit project check")?;
        t.send("\x13")?;
        screen(&t, "Check needs a name")?;
        assert!(service(root)?.project_checks()?.is_empty());
        cancel(&mut t, mouse)?;
        open(&mut t, mouse, "c", "c Checks", "Project checks")?;
        open(&mut t, mouse, "\r", "Apply", "Edit project check")?;
        t.send("build")?;
        activate(&mut t, mouse, "\t", "Command")?;
        t.send("test -f Cargo.toml")?;
        activate(&mut t, mouse, "\t\t", "Timeout (ms)")?;
        t.send("\x01zero\x13")?;
        screen(&t, "Timeout must be a positive integer")?;
        t.send("\x011000")?;
        apply(&mut t, mouse)?;
        let checks = service(root)?.project_checks()?;
        assert_eq!(checks.len(), 1);
        for enabled in [false, true] {
            open(&mut t, mouse, "c", "c Checks", "build [")?;
            activate(&mut t, mouse, " ", "Check to open")?;
            open(&mut t, mouse, "\r", "Apply", "Edit project check")?;
            activate(&mut t, mouse, "\t\t\t\t ", "State")?;
            apply(&mut t, mouse)?;
            assert_eq!(service(root)?.project_checks()?[0].enabled, enabled);
        }
        open(&mut t, mouse, "c", "c Checks", "build [enabled]")?;
        activate(&mut t, mouse, " ", "Check to open")?;
        open(&mut t, mouse, "\r", "Apply", "Edit project check")?;
        activate(&mut t, mouse, "\t", "Command")?;
        t.send("\x01cancelled command")?;
        cancel(&mut t, mouse)?;
        assert_eq!(checks, service(root)?.project_checks()?);

        open(&mut t, mouse, "e", "e Preferences", "Project preferences")?;
        if mouse {
            t.click("Planner approval")?;
            t.click("< Continue queue >")?;
        } else {
            t.send("\t \t ")?;
        }
        cancel(&mut t, mouse)?;
        assert_eq!(
            service(root)?
                .preferences(&root.join("config/ktask-dev"))?
                .continuation,
            Continuation::Queue,
            "continuing the queue is the default; cancel keeps it"
        );
        open(&mut t, mouse, "e", "e Preferences", "Project preferences")?;
        if mouse {
            t.click("Planner approval")?;
            t.click("< Continue queue >")?;
        } else {
            t.send("\t \t ")?;
        }
        apply(&mut t, mouse)?;
        open(&mut t, mouse, "e", "e Preferences", "Project preferences")?;
        if mouse {
            t.click("Mouse")?;
            t.click("Apply")?;
        } else {
            t.send(" \x13")?;
        }
        screen(&t, "Mouse: Disabled")?;
        quit(&mut t)?;
        let mut t = Terminal::open_real(BIN, root, 70, 22)?;
        screen(&t, "No current task")?;
        t.send("4")?;
        let restored = screen(&t, "Mouse: Disabled")?;
        assert!(restored.contains("Run mode: Run one"));
        assert!(restored.contains("Planner approval: Review required"));
        t.send("e")?; // Keyboard re-enables mouse.
        screen(&t, "Project preferences")?;
        t.send(" \x13")?;
        screen(&t, "Mouse: Enabled")?;
        assert_eq!(saved, service(root)?.project_defaults()?);
        assert_eq!(checks, service(root)?.project_checks()?);
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn configuration_stale_writes_pinned_revisions_and_scope() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut a = service(root)?;
    let mut b = service(root)?;
    let defaults = a.project_defaults()?;
    let configured = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "codex".into(),
            model: "old".into(),
            effort: None,
        })
    });
    a.save_project_overrides(&defaults, &configured)?;
    assert!(b.save_project_overrides(&defaults, &defaults).is_err());
    let d = ktask_rs_mk3::application::Definition {
        title: "Pinned".into(),
        goal: "Goal".into(),
        criteria: vec!["Check".into()],
        kind: ktask_rs_mk3::application::Kind::Ai,
        category: Some("code".into()),
        models: a.defaults()?,
        attempt_limit: Some(3),
        timeout_ms: Some(1000),
        stage_timeout_ms: None,
    };
    let id = a.create(std::slice::from_ref(&d), None, true)?[0];
    let mut updated = configured.clone();
    updated[1].as_mut().unwrap().model = "new".into();
    b.save_project_overrides(&configured, &updated)?;
    assert_eq!(a.task(id)?.unwrap().definition().definition, d);
    let mut check = ProjectCheck {
        id: None,
        name: "build".into(),
        command: "true".into(),
        directory: ".".into(),
        timeout_ms: 1000,
        enabled: true,
    };
    a.save_check(&[], &check)?;
    assert!(b.save_check(&[], &check).is_err());
    let checks = a.project_checks()?;
    check = checks[0].clone();
    check.directory = "../".into();
    assert!(a.save_check(&checks, &check).is_err());
    check = checks[0].clone();
    let task = a.task(id)?.unwrap();
    let run = a.begin_run(
        ktask_rs_mk3::application::VersionedTask {
            id,
            version: task.version,
        },
        "test-token",
        "base",
        false,
    )?;
    check.enabled = false;
    for state in ["running", "paused", "waiting"] {
        if state != "running" {
            if state == "waiting" {
                let task = a.task(id)?.unwrap();
                a.begin_run(
                    ktask_rs_mk3::application::VersionedTask {
                        id,
                        version: task.version,
                    },
                    "resume",
                    "base",
                    false,
                )?;
            }
            a.finish_run(&run, state, None, "test state", None)?;
        }
        assert!(
            b.save_check(&checks, &check)
                .unwrap_err()
                .to_string()
                .contains("live attempt")
        );
        assert!(b.set_check("build", "false", 10).is_err());
    }
    let config = root.join("config/ktask-dev");
    let before = a.preferences(&config)?;
    let mut next = before.clone();
    next.mouse = false;
    next.planner_approval = PlannerApproval::ReviewRequired;
    next.continuation = Continuation::Queue;
    let saved = a.save_preferences(&config, &before, &next)?;
    assert!(b.save_preferences(&config, &before, &before).is_err());
    assert_eq!(b.preferences(&config)?, saved);
    assert_eq!(a.preferences(&root.join("config/ktask"))?, before);
    let other = root.join("other");
    std::fs::create_dir(&other)?;
    let other = Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        &other,
        None,
    )?;
    assert_eq!(other.preferences(&config)?, before);
    assert!(other.project_checks()?.is_empty());
    assert!(other.project_defaults()?.iter().all(Option::is_none));
    Ok(())
}

#[test]
fn real_configuration_rejects_stale_open_forms() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut t = Terminal::open_real(BIN, root, 70, 22)?;
    screen(&t, "No current task")?;
    t.send("4m")?;
    screen(&t, "Project model defaults")?;
    let mut other = service(root)?;
    other.set_default(
        "planning",
        &ModelConfig {
            provider: "codex".into(),
            model: "concurrent".into(),
            effort: None,
        },
        false,
    )?;
    t.send("codex/stale\x13")?;
    screen(&t, "Defaults changed elsewhere")?;
    cancel(&mut t, false)?;
    t.send("c\r")?;
    screen(&t, "Edit project check")?;
    other.set_check("other", "true", 1000)?;
    t.send("stale\tfalse\x13")?;
    screen(&t, "Checks changed elsewhere")?;
    cancel(&mut t, false)?;
    t.send("e")?;
    screen(&t, "Project preferences")?;
    let config = root.join("config/ktask-dev");
    let original = other.preferences(&config)?;
    let mut next = original.clone();
    next.continuation = Continuation::Queue;
    other.save_preferences(&config, &original, &next)?;
    t.send(" \x13")?;
    screen(&t, "Preferences changed elsewhere")?;
    assert!(other.preferences(&config)?.mouse);
    cancel(&mut t, false)?;
    quit(&mut t)
}

#[test]
fn a_new_project_is_set_up_from_readiness_and_later_projects_start_with_its_defaults() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        std::fs::create_dir(root.join("bin"))?;
        support::write_executable(&root.join("bin/codex"), "#!/bin/sh\nexit 0\n")?;
        let project = root.join("project");
        std::fs::create_dir(&project)?;
        support::empty_repository(&project)?;
        std::fs::write(
            project.join("Makefile"),
            "build:\n\tcc x.c\ncheck:\n\t./t\n",
        )?;
        let at = |path: &Path| -> Result<Service> {
            Ok(Service::open(
                Database::open_in(&root.join("state/ktask-dev"))?,
                path,
                None,
            )?)
        };
        let mut t = Terminal::open_project(BIN, root, &project, 110, 44)?;
        screen(&t, "Set up this project first")?;
        open(&mut t, mouse, "u", "u Readiness", "BLOCK · Project check")?;
        // The first check starts from the command the project's Makefile defines.
        open(&mut t, mouse, "c", "c Checks", "No checks configured")?;
        open(
            &mut t,
            mouse,
            "\r",
            "Apply",
            "Command suggested from Makefile",
        )?;
        apply(&mut t, mouse)?;
        assert_eq!(at(&project)?.project_checks()?[0].command, "make check");

        open(&mut t, mouse, "u", "u Readiness", "BLOCK · execution model")?;
        open(&mut t, mouse, "d", "d Defaults", "Project model defaults")?;
        for (role, model) in [
            ("execution", "codex/gpt-6-sol"),
            ("review", "codex/gpt-6-astra"),
            ("validation", "codex/gpt-6-astra"),
            ("remediation", "codex/gpt-6-astra"),
        ] {
            let label = format!("{role} provider/model");
            activate(&mut t, mouse, "\t\t", &label)?;
            t.send(model)?;
            t.screen(&label, |s| field(s, &label).contains(model))?;
        }
        // Clicking a choice's label cycles it; the keyboard moves there and presses Space.
        activate(&mut t, mouse, "\t\t ", "New projects")?;
        screen(&t, "< Start with these >")?;
        apply(&mut t, mouse)?;
        let saved = at(&project)?.project_defaults()?;
        open(&mut t, mouse, "u", "u Readiness", "OK · Project setup")?;

        // Agent network and the check before each task are project preferences.
        t.send("\x1b")?;
        t.screen("readiness closed", |s| !s.contains("Execution readiness"))?;
        open(&mut t, mouse, "4", "4 Config", "Storage: SQLite")?;
        open(&mut t, mouse, "e", "e Preferences", "Project preferences")?;
        assert!(at(&project)?.check_before_task()?, "on by default");
        if mouse {
            t.click_after("Agent network", "< Off >")?;
            t.click_after("Check before task", "< On >")?;
        } else {
            t.send("\t\t\t \t ")?;
        }
        // A field's value is the line after its own label (help text mentions both names).
        let choice = |s: &str, label: &str| -> String {
            s.lines()
                .skip_while(|line| !line.split('│').any(|cell| cell.trim() == label))
                .nth(1)
                .unwrap_or_default()
                .to_owned()
        };
        t.screen("both switches flipped", |s| {
            choice(s, "Agent network").contains("< On >")
                && choice(s, "Check before task").contains("< Off >")
        })?;
        apply(&mut t, mouse)?;
        assert!(at(&project)?.agent_network()?);
        assert!(!at(&project)?.check_before_task()?);
        quit(&mut t)?;

        // A project registered later starts with the defaults saved for new projects.
        let later = root.join("later");
        std::fs::create_dir(&later)?;
        assert_eq!(at(&later)?.project_defaults()?, saved);
        assert!(!at(&later)?.agent_network()?);
        assert!(at(&later)?.check_before_task()?);
    }
    Ok(())
}

fn claude(model: &str) -> Option<ModelConfig> {
    Some(ModelConfig {
        provider: "claude".into(),
        model: model.into(),
        effort: Some("high".into()),
    })
}

fn open_project(root: &Path, name: &str) -> Result<Service> {
    let path = root.join(name);
    std::fs::create_dir_all(&path)?;
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        &path,
        None,
    )?)
}

fn user_values(from: &UserDefaults) -> UserDefaults {
    UserDefaults {
        agent_network: true,
        check_before_task: false,
        check_timeout_ms: 90_000,
        continuation: Continuation::Single,
        planner_approval: PlannerApproval::ReviewRequired,
        delivery_mode: "pr".into(),
        ..from.clone()
    }
}

#[test]
fn new_projects_inherit_user_defaults_and_existing_ones_change_only_when_applied() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let config = root.join("config/ktask-dev");
    // Built-in defaults reproduce the behavior before user defaults existed.
    let mut early = open_project(root, "early")?;
    assert_eq!(early.user_defaults()?, UserDefaults::default());
    assert!(early.check_before_task()? && !early.agent_network()?);
    assert_eq!(early.delivery_config()?.version, 0);
    let early_prefs = early.preferences(&config)?;
    assert_eq!(
        (early_prefs.continuation, early_prefs.planner_approval),
        (Continuation::Queue, PlannerApproval::Automatic)
    );
    // One save sets every reusable setting and all five claude/high models.
    let models: [Option<ModelConfig>; 5] = std::array::from_fn(|_| claude("opus"));
    let expected = early.user_defaults()?;
    let value = user_values(&expected);
    let saved = early.save_new_project_setup(&expected, &value, &models)?;
    assert_eq!(saved.version, 1);
    assert_eq!(early.new_project_defaults()?, models);
    assert!(early.save_user_defaults(&expected, &value).is_err());
    for bad in [
        UserDefaults {
            delivery_mode: "force".into(),
            ..saved.clone()
        },
        UserDefaults {
            check_timeout_ms: 0,
            ..saved.clone()
        },
    ] {
        assert!(early.save_user_defaults(&saved, &bad).is_err());
    }
    // A project that existed before keeps every setting.
    assert!(early.check_before_task()? && !early.agent_network()?);
    assert_eq!(early.delivery_config()?.mode, "local");
    assert_eq!(early.preferences(&config)?, early_prefs);
    // Models are inherited, not copied: the project overrides nothing and now sees the
    // user defaults, which only tasks defined afterwards use.
    assert!(early.project_overrides()?.iter().all(Option::is_none));
    assert_eq!(early.project_defaults()?, models);

    let late = open_project(root, "late")?;
    assert_inherited(&late, &config, &models)?;
    // Later changes never reach a registered project, even one that saved nothing.
    let again = UserDefaults {
        agent_network: false,
        continuation: Continuation::Queue,
        ..saved.clone()
    };
    early.save_user_defaults(&saved, &again)?;
    assert!(late.agent_network()?);
    assert_eq!(
        late.preferences(&config)?.continuation,
        Continuation::Single
    );
    apply_to_own_project(late, &config, &models)
}

/// A new project inherits settings, but never a remote, branch or check command.
fn assert_inherited(late: &Service, config: &Path, models: &Defaults) -> Result {
    assert!(!late.check_before_task()? && late.agent_network()?);
    assert_eq!(&late.project_defaults()?, models);
    let delivery = late.delivery_config()?;
    assert_eq!(
        (
            delivery.mode.as_str(),
            delivery.remote.as_str(),
            delivery.base_branch.as_str()
        ),
        ("pr", "origin", "main")
    );
    let prefs = late.preferences(config)?;
    assert_eq!(
        (prefs.continuation, prefs.planner_approval),
        (Continuation::Single, PlannerApproval::ReviewRequired)
    );
    assert!(prefs.mouse && prefs.version == 0);
    assert!(late.project_checks()?.is_empty());
    Ok(())
}

fn apply_to_own_project(mut own: Service, config: &Path, models: &Defaults) -> Result {
    // The project's own settings differ from the defaults.
    own.set_agent_network(false)?;
    own.save_check(
        &[],
        &ProjectCheck {
            id: None,
            name: "check".into(),
            command: "make check".into(),
            directory: ".".into(),
            timeout_ms: 1_000,
            enabled: true,
        },
    )?;
    let current = own.delivery_config()?;
    let own_delivery = DeliveryConfig {
        remote: "upstream".into(),
        base_branch: "trunk".into(),
        ..current.clone()
    };
    own.save_delivery_config(&current, &own_delivery)?;
    // A task created before applying keeps its pinned models and limits.
    let definition = ktask_rs_mk3::application::Definition {
        title: "Pinned".into(),
        goal: "Goal".into(),
        criteria: vec!["Check".into()],
        kind: ktask_rs_mk3::application::Kind::Ai,
        category: Some("code".into()),
        models: own.defaults()?,
        attempt_limit: Some(3),
        timeout_ms: Some(1000),
        stage_timeout_ms: None,
    };
    let id = own.create(std::slice::from_ref(&definition), None, true)?[0];
    let mut own_models = own.project_overrides()?;
    own_models[1] = Some(ModelConfig {
        provider: "codex".into(),
        model: "project-own".into(),
        effort: None,
    });
    own.save_project_overrides(&own.project_overrides()?, &own_models)?;

    // Preview shows effective values before anything changes; apply then matches it.
    let preview = own.defaults_preview(config)?;
    let row = |name: &str| preview.iter().find(|r| r.setting == name).unwrap().clone();
    assert!(!row("agent network").changes);
    assert_eq!(row("check timeout").current, "1000 ms");
    assert_eq!(row("check timeout").default, "90000 ms");
    assert_eq!(
        (
            row("run mode").current.as_str(),
            row("run mode").default.as_str()
        ),
        ("single", "queue")
    );
    assert_eq!(row("delivery mode").current, "pr");
    assert!(
        row("execution model").changes
            && row("execution model")
                .default
                .contains("claude/opus / high")
    );
    assert_eq!(own.project_checks()?[0].timeout_ms, 1_000);
    let mut stale = preview.clone();
    stale[0].current.push('!');
    assert!(own.apply_user_defaults(config, &stale).is_err());
    let applied = own.apply_user_defaults(config, &preview)?;
    assert!(applied.iter().all(|r| !r.changes), "{applied:?}");
    assert_eq!(&own.project_defaults()?, models);
    let check = &own.project_checks()?[0];
    assert_eq!(
        (check.timeout_ms, check.command.as_str()),
        (90_000, "make check")
    );
    assert!(!own.agent_network()? && !own.check_before_task()?);
    let delivery = own.delivery_config()?;
    assert_eq!(
        (delivery.remote.as_str(), delivery.base_branch.as_str()),
        ("upstream", "trunk")
    );
    assert_eq!(own.preferences(config)?.continuation, Continuation::Queue);
    assert_eq!(own.task(id)?.unwrap().definition().definition, definition);
    Ok(())
}

#[test]
fn real_user_defaults_form_saves_previews_and_applies_with_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let config = root.join("config/ktask-dev");
        let mut t = Terminal::open_real(BIN, root, 100, 70)?;
        screen(&t, "No current task")?;
        open(&mut t, mouse, "4", "4 Config", "Storage: SQLite")?;
        activate(&mut t, mouse, "u", "u User defaults")?;
        let opened = screen(&t, "User defaults for new projects")?;
        assert!(opened.contains("Never inherited: the check command"));
        for label in [
            "planning provider/model",
            "remediation effort",
            "Agent network",
            "Baseline checks",
            "Timeout (ms)",
            "Run mode",
            "Planner approval",
            "Mode",
            "After saving",
        ] {
            assert!(opened.contains(label), "{label}\n{opened}");
        }
        let value = |t: &Terminal, label: &str| -> Result<String> {
            Ok(dfield(&t.current_screen(), label).to_owned())
        };
        if mouse {
            for role in [
                "planning",
                "execution",
                "review",
                "validation",
                "remediation",
            ] {
                t.click(&format!("{role} provider/model"))?;
                t.send("\x01claude/opus")?;
                t.click(&format!("{role} effort"))?;
                t.send("\x01high")?;
            }
            t.click_after("Agent network", "< Off >")?;
            t.click_after("Baseline checks", "< On >")?;
            t.click("Timeout (ms)")?;
            t.send("\x0145000")?;
            t.click_after("Run mode", "< Continue queue >")?;
            t.click_after("Planner approval", "< Automatic >")?;
            t.click_after("Mode", "< Local >")?;
            t.click_after("After saving", "< Only save >")?;
        } else {
            for _ in 0..5 {
                t.send("\x01claude/opus\t\x01high\t")?;
            }
            t.send(" \t \t\x0145000\t \t \t \t ")?; // network, baseline, timeout, run, planner, delivery
            t.send("\t ")?;
        }
        let filled = t.screen("filled", |s| {
            dfield(s, "After saving").contains("Preview applying")
        })?;
        println!("USER DEFAULTS FORM mouse={mouse}\n{filled}");
        for (label, shown) in [
            ("Agent network", "< On >"),
            ("Baseline checks", "< Off >"),
            ("Run mode", "< Run one >"),
            ("Planner approval", "< Review required >"),
            ("Mode", "< Direct push >"),
        ] {
            assert!(
                value(&t, label)?.contains(shown),
                "{label}: {}",
                value(&t, label)?
            );
        }
        activate(&mut t, mouse, "\x13", "Apply")?;
        let preview = screen(&t, "Apply user defaults to this project?")?;
        println!("USER DEFAULTS PREVIEW mouse={mouse}\n{preview}");
        for line in [
            "agent network: off -> on",
            "baseline checks: on -> off",
            "run mode: queue -> single",
            "planner approval: automatic -> review_required",
            "delivery mode: local -> direct",
            "execution model:",
        ] {
            assert!(preview.contains(line), "{line}\n{preview}");
        }
        // Saved for new projects, but this project is untouched until applied.
        let service = service(root)?;
        assert_eq!(service.user_defaults()?.check_timeout_ms, 45_000);
        assert_eq!(service.new_project_defaults()?[1], claude("opus"));
        assert!(!service.agent_network()? && service.check_before_task()?);
        if mouse {
            t.click_after("Project settings", "< Keep project settings >")?;
        } else {
            t.send(" ")?;
        }
        t.screen("choice flipped", |s| {
            dfield(s, "Project settings").contains("< Apply user defaults >")
        })?;
        activate(&mut t, mouse, "\x13", "Apply")?;
        screen(&t, "User defaults applied to this project")?;
        assert!(service.agent_network()? && !service.check_before_task()?);
        assert_eq!(service.delivery_config()?.mode, "direct");
        assert_eq!(
            service.project_defaults()?,
            std::array::from_fn(|_| claude("opus"))
        );
        assert_eq!(
            service.preferences(&config)?.continuation,
            Continuation::Single
        );
        screen(&t, "Run mode: Run one")?;
        // Reopening shows the saved values.
        open(
            &mut t,
            mouse,
            "u",
            "u User defaults",
            "User defaults for new projects",
        )?;
        assert!(dfield(&t.current_screen(), "planning provider/model").contains("claude/opus"));
        activate(&mut t, mouse, "\x1b", "Cancel")?;
        t.screen("form closed", |s| {
            !s.contains("User defaults for new projects")
        })?;
        // A project's first TUI check starts from the user's default timeout, not a constant.
        open(&mut t, mouse, "c", "c Checks", "No checks configured")?;
        open(&mut t, mouse, "\r", "Apply", "Edit project check")?;
        let form = t.current_screen();
        println!("FIRST CHECK FORM mouse={mouse}\n{form}");
        assert_eq!(
            dfield(&form, "Timeout (ms)").trim_matches(['│', ' ']),
            "45000"
        );
        if mouse {
            t.click("Name")?;
            t.send("check")?;
            t.click("Command")?;
            t.send("true")?;
        } else {
            t.send("check\ttrue")?;
        }
        apply(&mut t, mouse)?;
        assert_eq!(service.project_checks()?[0].timeout_ms, 45_000);
        quit(&mut t)?;
    }
    Ok(())
}

/// The value line under an exact dialog cell `label`, ignoring the screen behind the dialog.
fn dfield<'a>(s: &'a str, label: &str) -> &'a str {
    let mut lines = s
        .lines()
        .skip_while(|line| !line.split('│').any(|cell| cell.trim() == label))
        .skip(1);
    lines.next().unwrap_or_default()
}

#[test]
fn applying_user_defaults_is_atomic_current_and_refused_during_live_work() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let config = root.join("config/ktask-dev");
    let mut own = open_project(root, "own")?;
    let mut other = open_project(root, "own")?;
    let expected = own.user_defaults()?;
    let models: Defaults = std::array::from_fn(|_| claude("opus"));
    own.save_new_project_setup(&expected, &user_values(&expected), &models)?;
    own.save_check(
        &[],
        &ProjectCheck {
            id: None,
            name: "check".into(),
            command: "true".into(),
            directory: ".".into(),
            timeout_ms: 1_000,
            enabled: true,
        },
    )?;
    own.save_project_overrides(&own.project_overrides()?, &models)?;
    let preview = own.defaults_preview(&config)?;

    // Another handle changes the project after the preview: nothing is applied.
    other.set_check_before_task(false)?;
    other.set_agent_network(false)?;
    let before = (
        own.project_defaults()?,
        own.project_checks()?,
        own.delivery_config()?,
    );
    let error = own
        .apply_user_defaults(&config, &preview)
        .unwrap_err()
        .to_string();
    assert!(error.contains("preview them again"), "{error}");
    assert_eq!(
        (
            own.project_defaults()?,
            own.project_checks()?,
            own.delivery_config()?
        ),
        before
    );
    let preview = own.defaults_preview(&config)?;

    // A live queue session or attempt refuses without changing anything.
    own.start_execution_session(true)?;
    let error = own
        .apply_user_defaults(&config, &preview)
        .unwrap_err()
        .to_string();
    assert!(error.contains("live queue session"), "{error}");
    assert_eq!(own.project_checks()?[0].timeout_ms, 1_000);
    own.settle_execution_session("stopped", "test")?;
    let definition = ktask_rs_mk3::application::Definition {
        title: "Live".into(),
        goal: "Goal".into(),
        criteria: vec!["Check".into()],
        kind: ktask_rs_mk3::application::Kind::Ai,
        category: Some("code".into()),
        models: own.defaults()?,
        attempt_limit: None,
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    let id = own.create(std::slice::from_ref(&definition), None, true)?[0];
    let task = own.task(id)?.unwrap();
    own.begin_run(
        ktask_rs_mk3::application::VersionedTask {
            id,
            version: task.version,
        },
        "token",
        "base",
        false,
    )?;
    let error = own
        .apply_user_defaults(&config, &preview)
        .unwrap_err()
        .to_string();
    assert!(error.contains("live attempt"), "{error}");
    assert_eq!(own.project_checks()?[0].timeout_ms, 1_000);
    assert_eq!(own.delivery_config()?.mode, "local");
    Ok(())
}
