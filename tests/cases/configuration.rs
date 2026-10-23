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

#[test]
fn real_configuration_forms_mouse_keyboard_restart_and_validation() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut t = Terminal::open_real(BIN, root, 70, 22)?;
        screen(&t, "No current task")?;
        open(&mut t, mouse, "4", "4 Config", "Storage: SQLite")?;
        open(&mut t, mouse, "m", "m Defaults", "Project model defaults")?;
        t.send("invalid\x13")?;
        screen(&t, "Use provider/model")?;
        assert!(
            service(root)?
                .project_defaults()?
                .iter()
                .all(Option::is_none)
        );
        cancel(&mut t, mouse)?;
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
