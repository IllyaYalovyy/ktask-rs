use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{
        ModelConfig, Service,
        configuration::{MODEL_ROLES, ProjectCheck},
        preferences::{Continuation, PlannerApproval},
    },
    storage::Database,
};
use std::path::Path;
const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

#[test]
fn model_metadata_rejects_each_save_atomically_and_accepts_explicit_aliases() -> Result {
    use ktask_rs_mk3::application::{Definition, Kind, ROLES, VersionedTask};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut service = service(root)?;
    let defaults = service.project_defaults()?;
    for (index, role) in MODEL_ROLES.iter().enumerate() {
        let invalid = ModelConfig {
            provider: "codex".into(),
            model: "typo-model".into(),
            effort: Some("high".into()),
        };
        let mut values = defaults.clone();
        values[index] = Some(invalid.clone());
        for error in [
            service.save_defaults(&defaults, &values).unwrap_err(),
            service.save_new_project_defaults(&values).unwrap_err(),
            service.set_default(role, &invalid, true).unwrap_err(),
        ] {
            let message = error.to_string();
            assert!(
                message.contains(role)
                    && message.contains("codex/typo-model")
                    && message.contains("model-catalog.json"),
                "{message}"
            );
        }
        assert_eq!(service.project_defaults()?, defaults);
        assert!(service.new_project_defaults()?.iter().all(Option::is_none));
    }
    let mut definition = Definition {
        title: "Atomic models".into(),
        goal: "Validate every model".into(),
        kind: Kind::Ai,
        criteria: vec!["No partial writes".into()],
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(2),
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    let id = service.create(&[definition.clone()], None, true)?[0];
    for (index, role) in ROLES.iter().enumerate() {
        definition.models[index] = Some(ModelConfig {
            provider: "codex".into(),
            model: "typo-model".into(),
            effort: None,
        });
        let before = service.task(id)?.unwrap();
        let error = service
            .revise(
                VersionedTask {
                    id,
                    version: before.version,
                },
                &definition,
                true,
            )
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(role) && error.contains("typo-model"),
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
        definition.models[index] = None;
    }
    let catalog = root.join("state/ktask-dev/model-catalog.json");
    std::fs::write(
        &catalog,
        r#"[{"provider":"codex","model":"custom-deployment","aliases":["custom-alias"],"efforts":["low"]}]"#,
    )?;
    let alias = ModelConfig {
        provider: "codex".into(),
        model: "custom-alias".into(),
        effort: Some("low".into()),
    };
    service.set_default("planning", &alias, true)?;
    assert_eq!(service.project_defaults()?[0], Some(alias.clone()));
    let mut bad_effort = alias;
    bad_effort.effort = Some("high".into());
    let error = service
        .set_default("planning", &bad_effort, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("custom-alias")
            && error.contains("accepted values")
            && error.contains("low"),
        "{error}"
    );
    std::fs::write(&catalog, "malformed metadata")?;
    let error = service
        .set_default("planning", &bad_effort, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("planning")
            && error.contains("codex/custom-alias")
            && error.contains("invalid metadata"),
        "{error}"
    );
    std::fs::write(
        &catalog,
        r#"[{"provider":"opencode","model":"custom/vendor","efforts":["deep"]}]"#,
    )?;
    let custom = ModelConfig {
        provider: "opencode".into(),
        model: "custom/vendor".into(),
        effort: Some("deep".into()),
    };
    service.set_default("planning", &custom, false)?;
    let mut invalid = custom.clone();
    invalid.effort = Some("hight".into());
    assert!(
        service
            .set_default("planning", &invalid, false)
            .unwrap_err()
            .to_string()
            .contains("accepted values")
    );
    std::fs::write(
        &catalog,
        r#"[{"provider":"opencode","model":"custom/vendor"}]"#,
    )?;
    assert!(
        service
            .set_default("planning", &custom, false)
            .unwrap_err()
            .to_string()
            .contains("accepted values")
    );
    std::fs::write(
        &catalog,
        r#"[{"provider":"codex","model":"first","aliases":["collision"]},{"provider":"codex","model":"second","aliases":["collision"]}]"#,
    )?;
    let collision = ModelConfig {
        provider: "codex".into(),
        model: "collision".into(),
        effort: None,
    };
    let error = service
        .set_default("planning", &collision, false)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("planning")
            && error.contains("collision")
            && error.contains("model-catalog.json"),
        "{error}"
    );
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
    support::model_catalog(root)?;
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

fn reject_invalid_model_defaults(t: &mut Terminal, root: &Path, mouse: bool) -> Result {
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
    if mouse {
        t.click("Apply")?;
    } else {
        t.send("\x13")?;
    }
    let rejected = t.screen("invalid model save", |s| {
        s.contains(if mouse {
            "execution model"
        } else {
            "planning model"
        }) && s.contains("codex/typo-model")
    })?;
    println!("INVALID MODEL SAVE mouse={mouse}\n{rejected}");
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
        t.send("codex/executor")?;
        t.click("execution effort")?;
        t.send("hight")?;
        t.click("Apply")?;
    } else {
        t.send("codex/executor\thight\x13")?;
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
    assert!(
        service(root)?
            .project_defaults()?
            .iter()
            .all(Option::is_none)
    );
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
        reject_invalid_model_defaults(&mut t, root, mouse)?;
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
    a.save_defaults(&defaults, &configured)?;
    assert!(b.save_defaults(&defaults, &defaults).is_err());
    let d = ktask_rs_mk3::application::Definition {
        title: "Pinned".into(),
        goal: "Goal".into(),
        criteria: vec!["Check".into()],
        kind: ktask_rs_mk3::application::Kind::Ai,
        models: a.defaults()?,
        attempt_limit: Some(3),
        timeout_ms: Some(1000),
        stage_timeout_ms: None,
    };
    let id = a.create(std::slice::from_ref(&d), None, true)?[0];
    let mut updated = configured.clone();
    updated[1].as_mut().unwrap().model = "new".into();
    b.save_defaults(&configured, &updated)?;
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
        support::model_catalog(root)?;
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
