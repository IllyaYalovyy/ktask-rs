//! Named model profiles: resolution order, pinned settings and protection of selected ones.
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{
        Definition, Kind, ModelConfig, ROLES, Service, VersionedTask,
        configuration::{Defaults, MODEL_ROLES},
        profiles::Source,
    },
    storage::Database,
};
use serde_json::Value;
use std::path::Path;

const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

fn model(provider: &str, name: &str, effort: Option<&str>) -> Option<ModelConfig> {
    Some(ModelConfig {
        provider: provider.into(),
        model: name.into(),
        effort: effort.map(Into::into),
    })
}

fn all(provider: &str, name: &str, effort: Option<&str>) -> Defaults {
    std::array::from_fn(|_| model(provider, name, effort))
}

fn open(root: &Path, project: &str) -> Result<Service> {
    let path = root.join(project);
    std::fs::create_dir_all(&path)?;
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        &path,
        None,
    )?)
}

fn sources(service: &Service) -> Result<Vec<String>> {
    Ok(service
        .effective_models()?
        .iter()
        .map(|row| row.source.label())
        .collect())
}

fn task(service: &mut Service, title: &str) -> Result<i64> {
    let definition = Definition {
        title: title.into(),
        goal: "Goal".into(),
        criteria: vec!["Check".into()],
        kind: Kind::Ai,
        models: service.defaults()?,
        attempt_limit: Some(3),
        timeout_ms: None,
        stage_timeout_ms: None,
    };
    Ok(service.create(&[definition], None, true)?[0])
}

fn pinned(service: &mut Service, id: i64) -> Result<[Option<ModelConfig>; 4]> {
    Ok(service
        .task(id)?
        .unwrap()
        .definition()
        .definition
        .models
        .clone())
}

#[test]
fn layers_resolve_in_documented_order_and_clearing_an_override_inherits_again() -> Result {
    let scratch = support::sandbox()?;
    let mut service = open(scratch.path(), "one")?;
    let user = service.user_defaults()?;
    let user_models = all("codex", "user-model", Some("low"));
    service.save_new_project_setup(&user, &user, &user_models)?;
    assert_eq!(sources(&service)?, ["user defaults"; 5]);

    // The Anthropic profile captures what the project uses now, Claude with high effort.
    service.save_project_overrides(
        &service.project_overrides()?,
        &all("claude", "opus", Some("high")),
    )?;
    let anthropic = service.create_profile("Anthropic", &service.project_defaults()?)?;
    assert_eq!(anthropic.models, all("claude", "opus", Some("high")));
    service.save_project_overrides(
        &service.project_overrides()?,
        &std::array::from_fn(|_| None),
    )?;
    assert_eq!(service.project_defaults()?, user_models);

    // A partial profile sets only some roles; the rest still come from the user defaults.
    let mut partial: Defaults = std::array::from_fn(|_| None);
    partial[1] = model("codex", "profile-exec", Some("high"));
    partial[2] = model("opencode", "vendor/review", None);
    let openai = service.create_profile("OpenAI", &partial)?;
    service.select_profile(Some("openai"))?;
    let rows = service.effective_models()?;
    assert_eq!(rows[0].source, Source::UserDefaults);
    assert_eq!(rows[1].source, Source::Profile("OpenAI".into()));
    assert_eq!(rows[1].model, partial[1]);
    assert_eq!(rows[3].model, user_models[3]);

    // A project override wins over the profile; clearing it restores the profile value.
    let mut own: Defaults = std::array::from_fn(|_| None);
    own[2] = model("claude", "sonnet", None);
    service.save_project_overrides(&service.project_overrides()?, &own)?;
    assert_eq!(service.effective_models()?[2].source, Source::Project);
    assert_eq!(service.project_defaults()?[2], own[2]);
    service.save_project_overrides(&own, &std::array::from_fn(|_| None))?;
    assert_eq!(
        service.effective_models()?[2].source,
        Source::Profile("OpenAI".into())
    );
    assert_eq!(service.project_defaults()?[2], partial[2]);

    // Another selection and no selection change the effective values immediately.
    service.select_profile(Some("Anthropic"))?;
    assert_eq!(service.project_defaults()?, anthropic.models);
    assert_eq!(sources(&service)?, ["profile Anthropic"; 5]);
    service.select_profile(None)?;
    assert_eq!(service.project_defaults()?, user_models);
    assert_eq!(openai.version, 1);
    Ok(())
}

#[test]
fn a_new_project_selects_the_default_profile_and_inherits_without_copying() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut first = open(root, "first")?;
    first.create_profile("Qwen", &all("opencode", "vendor/qwen", None))?;
    first.set_default_profile(Some("qwen"))?;
    assert_eq!(
        first.user_defaults()?.default_profile.as_deref(),
        Some("Qwen")
    );
    assert_eq!(
        first.selected_profile()?,
        None,
        "existing projects keep their selection"
    );

    let mut second = open(root, "second")?;
    assert_eq!(second.selected_profile()?.unwrap().name, "Qwen");
    assert!(second.project_overrides()?.iter().all(Option::is_none));
    assert_eq!(
        second.project_defaults()?,
        all("opencode", "vendor/qwen", None)
    );
    assert_eq!(sources(&second)?, ["profile Qwen"; 5]);
    // Editing the profile reaches the inheriting project without any per-project repetition.
    let qwen = second.profile("Qwen")?.unwrap();
    second.save_profile(&qwen, "Qwen", &all("opencode", "vendor/qwen-2", None))?;
    assert_eq!(
        second.project_defaults()?,
        all("opencode", "vendor/qwen-2", None)
    );
    assert_eq!(
        first.new_project_effective()?[1].model,
        model("opencode", "vendor/qwen-2", None)
    );
    assert_eq!(first.project_defaults()?, [None, None, None, None, None]);

    // Applying the user defaults makes an existing project select the default profile and
    // keep every model that no new project value supplies.
    first.save_project_overrides(&first.project_overrides()?, &{
        let mut own: Defaults = std::array::from_fn(|_| None);
        own[0] = model("claude", "opus", Some("high"));
        own
    })?;
    let config = root.join("config/ktask-dev");
    let preview = first.defaults_preview(&config)?;
    let profile = preview.iter().find(|row| row.setting == "profile").unwrap();
    assert_eq!(
        (profile.current.as_str(), profile.default.as_str()),
        ("none", "Qwen")
    );
    let applied = first.apply_user_defaults(&config, &preview)?;
    assert!(applied.iter().all(|row| !row.changes), "{applied:?}");
    assert_eq!(first.selected_profile()?.unwrap().name, "Qwen");
    assert_eq!(
        first.project_defaults()?,
        all("opencode", "vendor/qwen-2", None)
    );
    Ok(())
}

#[test]
fn profile_changes_reach_only_future_tasks_and_pending_tasks_change_explicitly() -> Result {
    let scratch = support::sandbox()?;
    let mut service = open(scratch.path(), "pinned")?;
    service.create_profile("Old", &all("codex", "old-model", Some("high")))?;
    service.create_profile("New", &all("claude", "new-model", Some("max")))?;
    service.select_profile(Some("Old"))?;
    let id = task(&mut service, "Defined under Old")?;
    let old = all("codex", "old-model", Some("high"));
    let old_roles: [Option<ModelConfig>; 4] = std::array::from_fn(|i| old[i + 1].clone());
    assert_eq!(pinned(&mut service, id)?, old_roles);

    // Selecting another profile, editing it, and changing the user defaults never touch the task.
    service.select_profile(Some("New"))?;
    let new = service.profile("New")?.unwrap();
    service.save_profile(&new, "New", &all("claude", "newer-model", Some("max")))?;
    let user = service.user_defaults()?;
    service.save_new_project_setup(&user, &user, &all("codex", "user-model", None))?;
    assert_eq!(pinned(&mut service, id)?, old_roles);
    let fresh = task(&mut service, "Defined under New")?;
    let newer = all("claude", "newer-model", Some("max"));
    assert_eq!(
        pinned(&mut service, fresh)?,
        std::array::from_fn(|i| newer[i + 1].clone())
    );
    // The effective-versus-pinned view names what differs without changing anything.
    let rows = ktask_rs_mk3::application::profiles::pinned_rows(
        &pinned(&mut service, id)?,
        &service.effective_models()?,
    );
    assert!(rows.iter().all(|row| row.differs));
    assert_eq!(rows[0].current_source, Source::Profile("New".into()));

    // Applying to the pending task is its own explicit revision; the old one stays recorded.
    let task_now = service.task(id)?.unwrap();
    let mut definition = task_now.definition().definition.clone();
    definition.models = service.defaults()?;
    service.revise(
        VersionedTask {
            id,
            version: task_now.version,
        },
        &definition,
        true,
    )?;
    let revised = service.task(id)?.unwrap();
    assert_eq!(revised.revisions.len(), 2);
    assert_eq!(revised.revisions[0].definition.models, old_roles);
    assert_eq!(
        revised.definition().definition.models,
        std::array::from_fn(|i| newer[i + 1].clone())
    );
    assert_eq!(ROLES.len(), 4);
    Ok(())
}

#[test]
fn selected_profiles_cannot_be_deleted_broken_or_shadowed_by_bad_input() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut service = open(root, "guarded")?;
    let profile = service.create_profile("Guarded", &all("claude", "opus", Some("high")))?;
    for bad in ["", "   ", "none", "NONE", &"x".repeat(65), "bad\nname"] {
        assert!(
            service
                .create_profile(bad, &all("claude", "opus", None))
                .is_err(),
            "{bad:?}"
        );
    }
    assert!(
        service
            .create_profile("guarded", &all("claude", "opus", None))
            .is_err()
    );
    assert!(
        service
            .create_profile("Empty", &std::array::from_fn(|_| None))
            .is_err()
    );
    let invalid = service
        .create_profile("Typo", &all("claude", "opus", Some("hight")))
        .unwrap_err()
        .to_string();
    assert!(invalid.contains("accepted values"), "{invalid}");
    assert!(
        service
            .create_profile("Alien", &all("gemini", "pro", None))
            .is_err()
    );
    assert_eq!(service.profiles()?.len(), 1);

    service.select_profile(Some("Guarded"))?;
    let mut other = open(root, "other")?;
    other.set_default_profile(Some("Guarded"))?;
    let error = service.delete_profile(&profile).unwrap_err().to_string();
    assert!(
        error.contains("project guarded") && error.contains("the default for new projects"),
        "{error}"
    );
    // A role cannot be dropped while selected, but its model can change.
    let mut dropped = profile.models.clone();
    dropped[3] = None;
    let error = service
        .save_profile(&profile, "Guarded", &dropped)
        .unwrap_err()
        .to_string();
    assert!(error.contains("validation"), "{error}");
    let renamed =
        service.save_profile(&profile, "Renamed", &all("codex", "gpt-next", Some("high")))?;
    assert_eq!((renamed.version, renamed.name.as_str()), (2, "Renamed"));
    assert_eq!(service.selected_profile()?.unwrap().name, "Renamed");
    assert_eq!(
        other.user_defaults()?.default_profile.as_deref(),
        Some("Renamed")
    );
    // A stale copy cannot overwrite or delete.
    assert!(
        service
            .save_profile(&profile, "Stale", &profile.models)
            .is_err()
    );
    assert!(service.delete_profile(&profile).is_err());

    service.select_profile(None)?;
    assert!(
        service.delete_profile(&renamed).is_err(),
        "still the new-project default"
    );
    other.set_default_profile(None)?;
    service.delete_profile(&renamed)?;
    assert!(service.profiles()?.is_empty());
    assert!(service.select_profile(Some("Renamed")).is_err());
    assert!(MODEL_ROLES.contains(&"planning"));
    Ok(())
}

fn json(root: &Path, project: &Path, args: &[&str], exit: i32) -> Result<Value> {
    let output = std::process::Command::new(BIN)
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .current_dir(root)
        .args(["--json", "--project"])
        .arg(project)
        .args(args)
        .output()?;
    assert_eq!(
        output.status.code(),
        Some(exit),
        "{args:?}\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = if exit == 0 {
        output.stdout
    } else {
        output.stderr
    };
    Ok(serde_json::from_slice(&bytes)?)
}

#[test]
fn cli_manages_profiles_and_reports_effective_values_with_sources() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = root.join("project");
    std::fs::create_dir(&project)?;
    let run = |args: &[&str], exit: i32| json(root, &project, args, exit);
    for role in MODEL_ROLES {
        run(
            &[
                "model-default",
                role,
                "--provider",
                "claude",
                "--model",
                "opus",
                "--effort",
                "high",
            ],
            0,
        )?;
    }
    let created = run(&["profile", "create", "Anthropic", "--from-project"], 0)?;
    assert_eq!(
        created["data"]["profile"]["models"]["execution"]["effort"],
        "high"
    );
    assert_eq!(created["data"]["profile"]["version"], 1);
    run(
        &[
            "profile",
            "create",
            "Qwen",
            "--model",
            "execution=opencode/vendor/qwen",
            "--model",
            "review=opencode/vendor/qwen",
            "--effort",
            "review=high",
        ],
        0,
    )?;
    let listed = run(&["profile", "list"], 0)?;
    assert_eq!(listed["data"]["profiles"].as_array().unwrap().len(), 2);
    let shown = run(&["profile", "show", "qwen"], 0)?;
    assert_eq!(
        shown["data"]["profile"]["models"]["review"]["model"],
        "vendor/qwen"
    );
    assert_eq!(shown["data"]["profile"]["models"]["planning"], Value::Null);
    run(&["profile", "show", "missing"], 3)?;
    run(
        &[
            "profile",
            "create",
            "qwen",
            "--model",
            "execution=claude/opus",
        ],
        4,
    )?;
    run(
        &[
            "profile",
            "create",
            "Bad",
            "--model",
            "execution=claude/opus",
            "--effort",
            "execution=hight",
        ],
        4,
    )?;
    run(
        &["profile", "create", "Bad", "--effort", "execution=high"],
        2,
    )?;

    // The project overrides everything now, so the profile is shadowed until it inherits.
    let effective = run(&["profile", "effective"], 0)?;
    assert_eq!(effective["data"]["roles"][1]["source"]["layer"], "project");
    run(&["profile", "select", "Qwen"], 0)?;
    run(&["profile", "inherit", "execution", "review"], 0)?;
    let effective = run(&["profile", "effective"], 0)?;
    let roles = effective["data"]["roles"].as_array().unwrap();
    assert_eq!(effective["data"]["profile"], "Qwen");
    assert_eq!(
        roles[1]["source"],
        serde_json::json!({"layer":"profile","name":"Qwen"})
    );
    assert_eq!(roles[1]["model"]["provider"], "opencode");
    assert_eq!(roles[0]["source"]["layer"], "project");
    run(&["profile", "inherit", "--all"], 0)?;
    let effective = run(&["profile", "effective"], 0)?;
    assert_eq!(effective["data"]["roles"][0]["source"]["layer"], "unset");
    assert_eq!(effective["data"]["roles"][3]["source"]["layer"], "unset");
    run(&["profile", "inherit"], 2)?;

    // Selected and default profiles are protected; stale versions are rejected.
    run(&["profile", "delete", "Qwen"], 4)?;
    run(&["profile", "default", "Anthropic"], 0)?;
    run(&["profile", "delete", "Anthropic"], 4)?;
    run(&["profile", "edit", "Qwen", "--clear-model", "review"], 4)?;
    run(
        &[
            "profile",
            "edit",
            "Qwen",
            "--model",
            "review=opencode/vendor/qwen-2",
            "--if-version",
            "9",
        ],
        4,
    )?;
    let edited = run(
        &[
            "profile",
            "edit",
            "Qwen",
            "--model",
            "review=opencode/vendor/qwen-2",
            "--if-version",
            "1",
        ],
        0,
    )?;
    assert_eq!(edited["data"]["profile"]["version"], 2);
    assert_eq!(
        edited["data"]["profile"]["models"]["review"]["effort"],
        "high"
    );

    // A task keeps the models it was defined with while the profile moves on.
    for role in ["planning", "validation", "remediation"] {
        run(
            &[
                "profile",
                "edit",
                "Qwen",
                "--model",
                &format!("{role}=opencode/vendor/qwen"),
            ],
            0,
        )?;
    }
    let added = run(
        &["add", "--title", "T", "--goal", "G", "--criterion", "C"],
        0,
    )?;
    let id = added["data"]["tasks"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();
    run(
        &[
            "profile",
            "edit",
            "Qwen",
            "--model",
            "execution=opencode/vendor/qwen-3",
        ],
        0,
    )?;
    let pinned = run(&["profile", "effective", "--task", &id], 0)?;
    let exec = &pinned["data"]["roles"][0];
    assert_eq!(exec["pinned"]["model"], "vendor/qwen");
    assert_eq!(exec["current"]["model"], "vendor/qwen-3");
    assert_eq!(exec["differs"], true);
    assert_eq!(pinned["data"]["roles"][1]["differs"], false);
    let task = run(&["show", &id], 0)?;
    assert_eq!(
        task["data"]["definition"]["models"]["execution"]["model"],
        "vendor/qwen"
    );
    // Applying is explicit, and the new-project view shows the Anthropic default.
    run(&["edit", &id, "--apply-project-defaults"], 0)?;
    let task = run(&["show", &id], 0)?;
    assert_eq!(
        task["data"]["definition"]["models"]["execution"]["model"],
        "vendor/qwen-3"
    );
    let fresh = run(&["profile", "effective", "--new-project"], 0)?;
    assert_eq!(fresh["data"]["profile"], "Anthropic");

    run(&["profile", "select", "--none"], 0)?;
    run(&["profile", "default", "--none"], 0)?;
    run(&["profile", "delete", "Qwen", "--if-version", "999"], 4)?;
    run(&["profile", "delete", "Qwen"], 0)?;
    run(&["profile", "delete", "Anthropic"], 0)?;
    assert_eq!(
        run(&["profile", "list"], 0)?["data"]["profiles"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let _ = Terminal::open_real; // The real-terminal flows are in the next tests.
    Ok(())
}

fn screen(t: &Terminal, label: &str) -> Result<String> {
    let rendered = t.screen(label, |s| s.contains(label))?;
    println!("{label}\n{rendered}");
    Ok(rendered)
}

fn press(t: &mut Terminal, mouse: bool, key: &str, label: &str) -> Result {
    if mouse { t.click(label) } else { t.send(key) }
}

/// Advances the choice under `anchor` through `labels` (starting at `from`) `times` steps.
fn cycle(
    t: &mut Terminal,
    mouse: bool,
    anchor: &str,
    labels: &[&str],
    from: usize,
    times: usize,
) -> Result {
    for step in 0..times {
        if mouse {
            t.click_after(
                anchor,
                &format!("< {} >", labels[(from + step) % labels.len()]),
            )?;
        } else {
            t.send(" ")?;
        }
        let next = format!("< {} >", labels[(from + step + 1) % labels.len()]);
        t.screen("choice advanced", |s| s.contains(&next))?;
    }
    Ok(())
}

fn seeded(root: &Path) -> Result<Service> {
    let mut service = Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        root,
        None,
    )?;
    for role in MODEL_ROLES {
        service.set_default(role, &model("claude", "opus", Some("high")).unwrap(), false)?;
    }
    Ok(service)
}

const FIXED: [&str; 4] = [
    "New profile",
    "New from this project's models",
    "No profile for this project",
    "No default for new projects",
];
const ACTIONS: [&str; 4] = [
    "Save",
    "Save and select for this project",
    "Save and make the default for new projects",
    "Delete profile",
];

/// A real terminal driven by keyboard or mouse beside a service reading the same database.
struct Flow {
    t: Terminal,
    mouse: bool,
    service: Service,
}

impl Flow {
    fn press(&mut self, key: &str, label: &str) -> Result {
        press(&mut self.t, self.mouse, key, label)
    }

    fn apply(&mut self) -> Result {
        self.press("\x13", "Apply")
    }

    /// Opens the profile list on `waits_for`, a line of its body.
    fn open_list(&mut self, waits_for: &str) -> Result<String> {
        self.press("f", "f Profiles")?;
        screen(&self.t, waits_for)
    }

    /// Moves the list's choice from entry `from` forward `times` entries and applies it.
    fn choose(&mut self, entries: &[&str], from: usize, times: usize) -> Result {
        cycle(&mut self.t, self.mouse, "Profile", entries, from, times)?;
        self.apply()
    }

    fn action(&mut self, labels: &[&str], times: usize) -> Result {
        cycle(&mut self.t, self.mouse, "Action", labels, 0, times)?;
        let chosen = format!("< {} >", labels[times % labels.len()]);
        self.t.screen("action chosen", |s| s.contains(&chosen))?;
        Ok(())
    }

    fn create_anthropic_from_project(&mut self) -> Result {
        let listing = self.open_list("Model profiles")?;
        for line in [
            "Precedence: user defaults, then the project's profile, then",
            "execution: claude/opus / high [project override]",
            "No profiles yet.",
        ] {
            assert!(listing.contains(line), "{line}\n{listing}");
        }
        cycle(&mut self.t, self.mouse, "Profile", &FIXED, 0, 1)?;
        self.apply()?;
        let form = screen(&self.t, "Profile name")?;
        assert!(form.contains("claude/opus"), "{form}");
        if self.mouse {
            self.t.click("Profile name")?;
        }
        self.t.send("Anthropic")?;
        if !self.mouse {
            self.t.send("\t")?;
        }
        self.action(&ACTIONS[..2], 1)?;
        self.apply()?;
        screen(&self.t, "Saved profile Anthropic; this project uses it")?;
        let anthropic = self.service.profile("Anthropic")?.unwrap();
        assert_eq!(anthropic.models, all("claude", "opus", Some("high")));
        assert_eq!(self.service.selected_profile()?.unwrap().name, "Anthropic");
        // The project's own overrides still shadow the profile's identical values.
        assert_eq!(self.service.effective_models()?[1].source, Source::Project);
        Ok(())
    }

    fn clear_project_overrides(&mut self) -> Result {
        self.press("m", "m Defaults")?;
        let defaults = screen(&self.t, "Project model defaults")?;
        assert!(defaults.contains("blank inherits the"), "{defaults}");
        for role in MODEL_ROLES {
            if self.mouse {
                self.t.click(&format!("{role} provider/model"))?;
                self.t.send("\x01")?;
                self.t.click(&format!("{role} effort"))?;
                self.t.send("\x01")?;
            } else {
                self.t.send("\x01\t\x01\t")?;
            }
        }
        self.apply()?;
        self.t.screen("overrides cleared", |s| {
            s.contains("Project configuration saved") && !s.contains("[Apply]")
        })?;
        assert!(
            self.service
                .project_overrides()?
                .iter()
                .all(Option::is_none)
        );
        assert_eq!(sources(&self.service)?, ["profile Anthropic"; 5]);
        Ok(())
    }

    fn create_qwen(&mut self) -> Result {
        let listing = self.open_list("Anthropic v1")?;
        assert!(
            listing.contains("execution: claude/opus / high [profile Anthropic]"),
            "{listing}"
        );
        // The list opens on the selected profile; wrap around to "New profile".
        let entries = [
            FIXED[0],
            FIXED[1],
            FIXED[2],
            FIXED[3],
            "1. Anthropic (this project)",
        ];
        assert!(listing.contains(entries[4]), "{listing}");
        self.choose(&entries, 4, 1)?;
        screen(&self.t, "Profile name")?;
        if self.mouse {
            self.t.click("Profile name")?;
            self.t.send("Qwen")?;
            self.t.click("execution provider/model")?;
            self.t.send("opencode/vendor/qwen")?;
        } else {
            self.t.send("Qwen\t\t\t\topencode/vendor/qwen")?;
        }
        self.apply()?;
        screen(&self.t, "Saved profile Qwen")?;
        let qwen = self.service.profile("Qwen")?.unwrap();
        assert_eq!(qwen.models[1], model("opencode", "vendor/qwen", None));
        assert!(
            qwen.models
                .iter()
                .enumerate()
                .all(|(i, m)| (i == 1) == m.is_some())
        );
        Ok(())
    }

    fn deleting_a_selected_profile_is_refused(&mut self) -> Result {
        self.open_list("Qwen v1")?;
        self.apply()?;
        let form = screen(&self.t, "Profile Anthropic")?;
        assert!(form.contains("used by project"), "{form}");
        if !self.mouse {
            self.t.send("\t")?;
        }
        self.action(&ACTIONS, 3)?;
        self.apply()?;
        let refused = self
            .t
            .screen("delete refused", |s| s.contains("another profile first"))?;
        println!("DELETE REFUSED mouse={}\n{refused}", self.mouse);
        assert!(self.service.profile("Anthropic")?.is_some());
        self.press("\x1b", "Cancel")?;
        self.t
            .screen("closed", |s| !s.contains("Profile Anthropic"))?;
        Ok(())
    }

    fn unselect_then_delete_anthropic(&mut self) -> Result {
        self.open_list("Qwen v1")?;
        let entries = [
            FIXED[0],
            FIXED[1],
            FIXED[2],
            FIXED[3],
            "1. Anthropic (this project)",
            "2. Qwen",
        ];
        self.choose(&entries, 4, 4)?;
        screen(&self.t, "Project uses the user defaults without a profile")?;
        assert_eq!(self.service.selected_profile()?, None);
        assert_eq!(
            self.service.project_defaults()?,
            [None, None, None, None, None]
        );

        self.open_list("Anthropic v1")?;
        let entries = [
            FIXED[0],
            FIXED[1],
            FIXED[2],
            FIXED[3],
            "1. Anthropic",
            "2. Qwen",
        ];
        self.choose(&entries, 0, 4)?;
        screen(&self.t, "Profile Anthropic")?;
        if !self.mouse {
            self.t.send("\t")?;
        }
        self.action(&ACTIONS, 3)?;
        self.apply()?;
        screen(&self.t, "Deleted profile Anthropic")?;
        assert!(self.service.profile("Anthropic")?.is_none());
        Ok(())
    }

    fn make_qwen_the_default(&mut self) -> Result {
        self.open_list("Qwen v1")?;
        let entries = [FIXED[0], FIXED[1], FIXED[2], FIXED[3], "1. Qwen"];
        self.choose(&entries, 0, 4)?;
        screen(&self.t, "Profile Qwen")?;
        if !self.mouse {
            self.t.send("\t")?;
        }
        self.action(&ACTIONS[..3], 2)?;
        self.apply()?;
        screen(&self.t, "new projects start with it")?;
        assert_eq!(
            self.service.user_defaults()?.default_profile.as_deref(),
            Some("Qwen")
        );
        Ok(())
    }
}

#[test]
fn real_profile_flows_create_select_clear_override_protect_and_delete_with_keyboard_and_mouse()
-> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let service = seeded(root)?;
        let mut t = Terminal::open_real(BIN, root, 100, 70)?;
        screen(&t, "No current task")?;
        press(&mut t, mouse, "4", "4 Config")?;
        let config = screen(&t, "Storage: SQLite")?;
        assert!(config.contains("f Profiles"), "{config}");
        let mut flow = Flow { t, mouse, service };
        flow.create_anthropic_from_project()?;
        flow.clear_project_overrides()?;
        flow.create_qwen()?;
        flow.deleting_a_selected_profile_is_refused()?;
        flow.unselect_then_delete_anthropic()?;
        flow.make_qwen_the_default()?;
        flow.t.send("\x03")?;
        flow.t.finish()?;
    }
    Ok(())
}
