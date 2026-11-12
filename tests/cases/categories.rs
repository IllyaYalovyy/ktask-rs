//! Work categories: scoped templates, selection on tasks, pinned revisions, checks and previews.
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{
        Definition, Kind, ROLES, Service,
        categories::{Category, Scope, Templates, builtin},
        configuration::MODEL_ROLES,
    },
    storage::Database,
};
use serde_json::Value;
use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

fn cli(root: &Path, project: &Path, args: &[&str], exit: i32) -> Result<Value> {
    let output = Command::new(BIN)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .current_dir(project)
        .arg("--json")
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

fn setup(scratch: &Path) -> Result<std::path::PathBuf> {
    let project = scratch.join("project");
    std::fs::create_dir_all(&project)?;
    Ok(project)
}

fn open(root: &Path, project: &Path) -> Result<Service> {
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        project,
        None,
    )?)
}

fn custom(name: &str, execution: &str, checks: bool) -> Category {
    Category {
        name: name.into(),
        description: "Test category".into(),
        checks,
        templates: Templates {
            execution: execution.into(),
            review: "Review {{goal}}".into(),
            validation: "Validate {{criteria}}".into(),
            remediation: "Remediate {{title}}".into(),
        },
        scope: Scope::User,
    }
}

fn definition(title: &str, kind: Kind, category: Option<&str>) -> Definition {
    Definition {
        title: title.into(),
        goal: "Goal with {{context}} kept literally".into(),
        kind,
        category: category.map(Into::into),
        criteria: vec!["First".into(), "Second".into()],
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: None,
        stage_timeout_ms: None,
    }
}

fn seeded(root: &Path, project: &Path) -> Result<Service> {
    let mut service = open(root, project)?;
    for role in MODEL_ROLES {
        service.set_default(
            role,
            &ktask_rs_mk3::application::ModelConfig {
                provider: "claude".into(),
                model: "opus".into(),
                effort: Some("high".into()),
            },
            false,
        )?;
    }
    Ok(service)
}

#[test]
fn categories_resolve_project_over_user_over_built_in_and_tasks_default_by_kind() -> Result {
    let scratch = support::sandbox()?;
    let project = setup(scratch.path())?;
    let mut service = seeded(scratch.path(), &project)?;

    let names: Vec<_> = service
        .categories()?
        .into_iter()
        .map(|e| e.category.name)
        .collect();
    assert_eq!(names, ["code", "documents", "art"]);
    let checks: Vec<_> = service
        .categories()?
        .iter()
        .map(|e| e.category.checks)
        .collect();
    assert_eq!(
        checks,
        [true, false, false],
        "code gates on checks; documents and art do not"
    );

    // A user category overrides the built-in of the same name; a project row beats both.
    let mut user_code = builtin::named("code").unwrap();
    user_code.templates.execution = "USER code {{goal}}".into();
    service.save_category(Scope::User, None, &user_code)?;
    assert_eq!(
        service.category("CODE")?.unwrap().category.scope,
        Scope::User
    );
    let mut project_code = user_code.clone();
    project_code.templates.execution = "PROJECT code {{goal}}".into();
    service.save_category(Scope::Project, None, &project_code)?;
    let effective = service.category("code")?.unwrap();
    assert_eq!(effective.category.scope, Scope::Project);
    assert!(
        effective
            .category
            .templates
            .execution
            .starts_with("PROJECT")
    );
    assert!(
        service
            .category_in(Scope::User, "code")?
            .unwrap()
            .category
            .templates
            .execution
            .starts_with("USER")
    );
    assert!(
        service
            .category_in(Scope::BuiltIn, "code")?
            .unwrap()
            .category
            .templates
            .execution
            .starts_with("Own this")
    );

    // A task with no explicit category follows its kind; Human work has none.
    let ids = service.create(
        &[
            definition("Code", Kind::Ai, None),
            definition("Docs", Kind::Docs, None),
            definition("Art", Kind::Ai, Some("art")),
            definition("Owner", Kind::Human, None),
        ],
        None,
        true,
    )?;
    let pinned: Vec<_> = ids
        .iter()
        .map(|id| {
            let task = service.task(*id).unwrap().unwrap();
            let revision = task.definition();
            (
                revision.definition.category.clone(),
                revision.category.as_ref().map(|c| (c.scope, c.checks)),
                revision.runs_checks(),
            )
        })
        .collect();
    assert_eq!(
        pinned,
        [
            (Some("code".into()), Some((Scope::Project, true)), true),
            (
                Some("documents".into()),
                Some((Scope::BuiltIn, false)),
                false
            ),
            (Some("art".into()), Some((Scope::BuiltIn, false)), false),
            (None, None, false),
        ]
    );
    // Human tasks are done by the owner: a category is refused, an unknown name too.
    let error = service
        .create(&[definition("Owner", Kind::Human, Some("art"))], None, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("no work category"), "{error}");
    let error = service
        .create(&[definition("X", Kind::Ai, Some("nosuch"))], None, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Unknown work category nosuch"), "{error}");
    Ok(())
}

#[test]
fn saving_reports_invalid_placeholders_names_and_stale_versions() -> Result {
    let scratch = support::sandbox()?;
    let project = setup(scratch.path())?;
    let mut service = seeded(scratch.path(), &project)?;
    for (template, expected) in [
        ("Do {{titel}}", "Unknown placeholder {{titel}}"),
        ("Do {{title", "Unterminated placeholder"),
        ("   ", "execution template is required"),
    ] {
        let error = service
            .save_category(Scope::User, None, &custom("Bad", template, false))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(expected) && error.contains("execution"),
            "{template}: {error}"
        );
    }
    let error = service
        .save_category(Scope::User, None, &custom("Bad", "{{title}}", false))
        .and_then(|_| service.save_category(Scope::User, None, &custom("default", "x", false)))
        .unwrap_err()
        .to_string();
    assert!(error.contains("reserved"), "{error}");
    assert!(
        service
            .save_category(Scope::BuiltIn, None, &custom("Z", "x", false))
            .is_err()
    );

    let first =
        service.save_category(Scope::User, None, &custom("Poster", "One {{goal}}", false))?;
    assert_eq!(first.version, 1);
    let second = service.save_category(
        Scope::User,
        Some(1),
        &custom("poster", "Two {{goal}}", false),
    )?;
    assert_eq!(
        (second.version, second.category.name.as_str()),
        (2, "Poster")
    );
    let stale = service
        .save_category(Scope::User, Some(1), &custom("Poster", "Three", false))
        .unwrap_err()
        .to_string();
    assert!(stale.contains("is version 2, not 1"), "{stale}");
    assert!(
        service
            .delete_category(Scope::BuiltIn, "code", None)
            .is_err()
    );
    service.delete_category(Scope::User, "poster", Some(2))?;
    assert!(service.category("Poster")?.is_none());
    Ok(())
}

#[test]
fn revisions_pin_templates_so_later_edits_and_deletes_cannot_change_them() -> Result {
    let scratch = support::sandbox()?;
    let project = setup(scratch.path())?;
    let mut service = seeded(scratch.path(), &project)?;
    service.save_category(
        Scope::User,
        None,
        &custom("Poster", "ORIGINAL {{goal}}", false),
    )?;
    let id = service.create(
        &[definition("Pinned", Kind::Ai, Some("Poster"))],
        None,
        true,
    )?[0];
    let pinned_text = |service: &mut Service, id: i64| -> String {
        service
            .task(id)
            .unwrap()
            .unwrap()
            .definition()
            .category
            .clone()
            .unwrap()
            .templates
            .execution
    };
    assert_eq!(pinned_text(&mut service, id), "ORIGINAL {{goal}}");

    // Editing, overriding and deleting the category leave the stored revision untouched.
    service.save_category(
        Scope::User,
        Some(1),
        &custom("Poster", "EDITED {{goal}}", true),
    )?;
    service.save_category(
        Scope::Project,
        None,
        &custom("Poster", "PROJECT {{goal}}", true),
    )?;
    assert_eq!(pinned_text(&mut service, id), "ORIGINAL {{goal}}");
    assert!(!service.task(id)?.unwrap().definition().runs_checks());
    service.delete_category(Scope::Project, "Poster", None)?;
    service.delete_category(Scope::User, "Poster", None)?;
    assert_eq!(pinned_text(&mut service, id), "ORIGINAL {{goal}}");

    // A revision made after the category vanished keeps the pinned copy of this task.
    let task = service.task(id)?.unwrap();
    let target = ktask_rs_mk3::application::VersionedTask {
        id,
        version: task.version,
    };
    let mut revised = task.definition().definition.clone();
    revised.title = "Retitled".into();
    service.revise(target, &revised, true)?;
    let after = service.task(id)?.unwrap();
    assert_eq!(after.definition().number, 2);
    assert_eq!(
        after.revisions[1]
            .category
            .as_ref()
            .unwrap()
            .templates
            .execution,
        "ORIGINAL {{goal}}"
    );

    // A new task naming the vanished category is refused.
    assert!(
        service
            .create(&[definition("New", Kind::Ai, Some("Poster"))], None, true)
            .is_err()
    );

    // Re-creating the category and revising picks up the new text: only new revisions change.
    service.save_category(
        Scope::User,
        None,
        &custom("Poster", "REBORN {{goal}}", false),
    )?;
    let task = service.task(id)?.unwrap();
    let target = ktask_rs_mk3::application::VersionedTask {
        id,
        version: task.version,
    };
    service.revise(target, &task.definition().definition.clone(), true)?;
    let latest = service.task(id)?.unwrap();
    assert_eq!(
        latest.revisions[2]
            .category
            .as_ref()
            .unwrap()
            .templates
            .execution,
        "REBORN {{goal}}"
    );
    assert_eq!(
        latest.revisions[1]
            .category
            .as_ref()
            .unwrap()
            .templates
            .execution,
        "ORIGINAL {{goal}}"
    );
    assert_eq!(ROLES.len(), 4);
    Ok(())
}

#[test]
fn a_revision_defined_before_categories_existed_runs_its_kinds_built_in_category() -> Result {
    let scratch = support::sandbox()?;
    let project = setup(scratch.path())?;
    let mut service = seeded(scratch.path(), &project)?;
    // The owner customizes code at user scope after the old task was defined.
    let ids = service.create(
        &[
            definition("Old", Kind::Ai, None),
            definition("Old docs", Kind::Docs, None),
        ],
        None,
        true,
    )?;
    drop(service);
    let db = Database::open_in(&scratch.path().join("state/ktask-dev"))?;
    db.connection()
        .execute("DELETE FROM revision_categories", [])?;
    let mut service = open(scratch.path(), &project)?;
    let mut user_code = builtin::named("code").unwrap();
    user_code.templates.execution = "CUSTOM".into();
    service.save_category(Scope::User, None, &user_code)?;
    let old = service.task(ids[0])?.unwrap();
    let revision = old.definition();
    assert_eq!(revision.category.as_ref().unwrap().scope, Scope::BuiltIn);
    assert!(
        revision
            .category
            .as_ref()
            .unwrap()
            .templates
            .execution
            .starts_with("Own this high-level task")
    );
    assert!(revision.runs_checks());
    let docs = service.task(ids[1])?.unwrap();
    assert!(!docs.definition().runs_checks());
    assert_eq!(
        docs.definition().definition.category.as_deref(),
        Some("documents")
    );
    Ok(())
}

#[test]
fn cli_creates_edits_overrides_selects_previews_and_deletes_categories() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = setup(root)?;
    let run = |args: &[&str], exit: i32| cli(root, &project, args, exit);

    let listed = run(&["category", "list"], 0)?;
    let rows = listed["data"]["categories"].as_array().unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["code", "documents", "art"]
    );
    assert_eq!(rows[0]["checks"], true);
    assert_eq!(rows[2]["checks"], false);
    let placeholders = run(&["category", "placeholders"], 0)?;
    let documented: Vec<_> = placeholders["data"]["placeholders"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap().to_owned(),
                p["runtime_only"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(documented.last(), Some(&("context".to_owned(), true)));
    assert!(documented.len() == 5 && documented[..4].iter().all(|(_, runtime)| !runtime));

    // A brand-new name needs everything; an invalid placeholder is reported and nothing saved.
    run(&["category", "save", "Poster", "--no-checks"], 2)?;
    let template_file = root.join("review.txt");
    std::fs::write(
        &template_file,
        "Review the poster against {{criteria}}.\nSecond line.\n",
    )?;
    let bad = run(
        &[
            "category",
            "save",
            "Poster",
            "--no-checks",
            "--execution",
            "Paint {{tilte}}",
            "--review",
            "r",
            "--validation",
            "v",
            "--remediation",
            "m",
        ],
        4,
    )?;
    assert!(
        bad["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Unknown placeholder {{tilte}}"),
        "{bad}"
    );
    run(&["category", "show", "Poster"], 3)?;
    let created = run(
        &[
            "category",
            "save",
            "Poster",
            "--no-checks",
            "--description",
            "Posters",
            "--execution",
            "Paint the poster: {{title}} / {{goal}} / {{category}}",
            "--review",
            &format!("@{}", template_file.display()),
            "--validation",
            "Validate {{criteria}} against the delivered file",
            "--remediation",
            "Repair the poster",
        ],
        0,
    )?;
    assert_eq!(created["data"]["category"]["version"], 1);
    assert_eq!(
        created["data"]["category"]["templates"]["review"],
        "Review the poster against {{criteria}}.\nSecond line."
    );

    // A project override starts from the effective category and changes only what is given.
    let overridden = run(
        &[
            "category",
            "save",
            "poster",
            "--scope",
            "project",
            "--review",
            "Project review {{goal}}",
            "--checks",
        ],
        0,
    )?;
    assert_eq!(overridden["data"]["category"]["scope"], "project");
    assert_eq!(overridden["data"]["category"]["checks"], true);
    assert!(
        overridden["data"]["category"]["templates"]["execution"]
            .as_str()
            .unwrap()
            .starts_with("Paint the poster")
    );
    let shown_user = run(&["category", "show", "Poster", "--scope", "user"], 0)?;
    assert_eq!(shown_user["data"]["category"]["checks"], false);
    assert_eq!(
        run(&["category", "show", "Poster"], 0)?["data"]["category"]["scope"],
        "project"
    );
    run(
        &[
            "category",
            "save",
            "Poster",
            "--scope",
            "project",
            "--if-version",
            "9",
            "--checks",
        ],
        4,
    )?;
    run(
        &[
            "category", "save", "Poster", "--scope", "built-in", "--checks",
        ],
        2,
    )?;

    Ok(())
}

#[test]
fn cli_selects_categories_on_tasks_previews_them_and_delete_uncovers_the_next_scope() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = setup(root)?;
    let run = |args: &[&str], exit: i32| cli(root, &project, args, exit);
    run(
        &[
            "category",
            "save",
            "Poster",
            "--no-checks",
            "--execution",
            "Paint the poster: {{title}} / {{goal}} / {{category}}",
            "--review",
            "Review {{goal}}",
            "--validation",
            "Validate {{criteria}}",
            "--remediation",
            "Repair the poster",
        ],
        0,
    )?;
    run(
        &[
            "category", "save", "poster", "--scope", "project", "--checks",
        ],
        0,
    )?;

    // Select on a task; kind defaults stay as they were.
    let added = run(
        &[
            "add",
            "--title",
            "Poster",
            "--goal",
            "Make a poster",
            "--criterion",
            "A poster exists",
            "--category",
            "POSTER",
        ],
        0,
    )?;
    let id = added["data"]["tasks"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();
    let shown = run(&["show", &id], 0)?;
    assert_eq!(shown["data"]["definition"]["category"], "Poster");
    run(
        &[
            "add",
            "--title",
            "Owner",
            "--goal",
            "G",
            "--criterion",
            "C",
            "--kind",
            "human",
            "--category",
            "art",
        ],
        4,
    )?;
    let plain = run(
        &["add", "--title", "Plain", "--goal", "G", "--criterion", "C"],
        0,
    )?;
    let plain_id = plain["data"]["tasks"][0]["id"]
        .as_i64()
        .unwrap()
        .to_string();
    assert_eq!(
        run(&["show", &plain_id], 0)?["data"]["definition"]["category"],
        "code"
    );
    let docs = run(
        &[
            "add",
            "--title",
            "Docs",
            "--goal",
            "G",
            "--criterion",
            "C",
            "--kind",
            "docs",
        ],
        0,
    )?;
    let docs_id = docs["data"]["tasks"][0]["id"].as_i64().unwrap().to_string();
    assert_eq!(
        run(&["show", &docs_id], 0)?["data"]["definition"]["category"],
        "documents"
    );
    // Changing only the kind returns to that kind's default; naming a category keeps it.
    run(&["edit", &plain_id, "--kind", "docs"], 0)?;
    assert_eq!(
        run(&["show", &plain_id], 0)?["data"]["definition"]["category"],
        "documents"
    );
    run(&["edit", &plain_id, "--category", "art"], 0)?;
    assert_eq!(
        run(&["show", &plain_id], 0)?["data"]["definition"]["category"],
        "art"
    );
    let text = Command::new(BIN)
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .current_dir(&project)
        .args(["show", &id])
        .output()?;
    assert!(
        String::from_utf8_lossy(&text.stdout).contains("Category: Poster (project, checks apply)")
    );

    // Previews label the supervisor's fixed rules and the runtime-only parts.
    let preview = run(
        &["category", "preview", "--task", &id, "--role", "execution"],
        0,
    )?;
    let text = preview["data"]["previews"][0]["prompt"].as_str().unwrap();
    assert!(text.contains("== Supervisor rules: fixed, not editable by the category =="));
    assert!(
        text.contains("Paint the poster: Poster / Make a poster / Poster"),
        "{text}"
    );
    assert!(
        text.contains("[runtime-only context:")
            && text.contains("[runtime-only: for execution and repair turns")
    );
    assert_eq!(preview["data"]["example_task"], false);
    let example = run(&["category", "preview", "art"], 0)?;
    assert_eq!(example["data"]["previews"].as_array().unwrap().len(), 4);
    assert_eq!(example["data"]["example_task"], true);
    assert!(
        example["data"]["previews"][1]["prompt"]
            .as_str()
            .unwrap()
            .contains("Example task")
    );

    // Deleting a scope uncovers the next one; the pinned task keeps its project copy.
    run(
        &[
            "category",
            "delete",
            "Poster",
            "--scope",
            "project",
            "--if-version",
            "1",
        ],
        0,
    )?;
    assert_eq!(
        run(&["category", "show", "Poster"], 0)?["data"]["category"]["scope"],
        "user"
    );
    assert_eq!(
        run(&["show", &id], 0)?["data"]["revisions"][0]["definition"]["category"],
        "Poster"
    );
    run(&["category", "delete", "Poster", "--scope", "user"], 0)?;
    run(&["category", "show", "Poster"], 3)?;
    run(&["category", "delete", "code", "--scope", "user"], 4)?;
    Ok(())
}

#[test]
fn tsv_export_keeps_the_category_and_older_exports_still_import() -> Result {
    use ktask_rs_mk3::application::transfer;
    let definitions = vec![
        definition("Art", Kind::Ai, Some("art")),
        definition("Owner", Kind::Human, None),
    ];
    let text = transfer::encode(&definitions)?;
    assert!(text.starts_with("ktask-tasks-v3\n"));
    assert_eq!(transfer::decode(&text)?, definitions);
    let mut row = vec!["ai", "Title", "Goal", "3", "", ""];
    row.extend(std::iter::repeat_n("", 12));
    row.push("Only criterion");
    let v2 = format!("ktask-tasks-v2\n{}\n", row.join("\t"));
    let old = transfer::decode(&v2)?;
    assert_eq!(
        (old[0].category.clone(), old[0].criteria.clone()),
        (None, vec!["Only criterion".to_owned()])
    );
    Ok(())
}

fn screen(t: &Terminal, label: &str) -> Result<String> {
    let rendered = t.screen(label, |s| s.contains(label))?;
    println!("{label}\n{rendered}");
    Ok(rendered)
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

const ACTIONS: [&str; 6] = [
    "Save",
    "Preview execution prompt",
    "Preview review prompt",
    "Preview validation prompt",
    "Preview remediation prompt",
    "Delete (inherit again)",
];

/// A real terminal driven by keyboard or mouse beside a service reading the same database.
struct Flow {
    t: Terminal,
    mouse: bool,
    service: Service,
}

impl Flow {
    fn press(&mut self, key: &str, label: &str) -> Result {
        if self.mouse {
            self.t.click(label)
        } else {
            self.t.send(key)
        }
    }

    fn apply(&mut self) -> Result {
        self.press("\x13", "Apply")
    }

    /// Moves focus to `label` and, from the keyboard, the given number of Tab presses away.
    fn focus(&mut self, label: &str, tabs: &str) -> Result {
        if self.mouse {
            self.t.click(label)
        } else {
            self.t.send(tabs)
        }
    }

    /// Keyboard only: a choice takes focus from the click that cycles it, and clicking its
    /// label would land on body text of the same words.
    fn tab(&mut self, tabs: &str) -> Result {
        if self.mouse {
            Ok(())
        } else {
            self.t.send(tabs)
        }
    }

    fn replace(&mut self, label: &str, tabs: &str, text: &str) -> Result {
        self.focus(label, tabs)?;
        self.t.send(&format!("\x01{text}"))?;
        self.t.screen("replaced", |s| s.contains(text))?;
        Ok(())
    }

    /// Scrolls the dialog body, by key or by its Down button, until `needle` is on screen.
    fn scroll_to(&mut self, needle: &str) -> Result<String> {
        let mouse = self.mouse;
        let mut screen = self.t.current_screen();
        for _ in 0..24 {
            if screen.contains(needle) {
                return Ok(screen);
            }
            let previous = screen;
            if mouse {
                self.t.click("Down")?;
            } else {
                self.t.send("\x1b[6~")?;
            }
            screen = self.t.screen("preview scrolled", |s| s != previous)?;
        }
        Err(format!("{needle:?} not reached:\n{screen}").into())
    }

    fn open_list(&mut self) -> Result<String> {
        self.press("w", "w Work categories")?;
        screen(&self.t, "A work category tells AI turns")
    }

    fn choose(&mut self, entries: &[&str], from: usize, times: usize) -> Result {
        cycle(&mut self.t, self.mouse, "Category", entries, from, times)?;
        self.apply()
    }

    fn create_poster_art(&mut self) -> Result {
        let list = self.open_list()?;
        for line in [
            "code · built-in scope · project checks apply",
            "documents · built-in scope · project checks do not apply",
            "art · built-in scope · project checks do not apply",
            "Placeholders: {{title}}, {{goal}}, {{criteria}}, {{category}}, {{context}}.",
            "{{context}} is runtime-only",
        ] {
            assert!(list.contains(line), "{line}\n{list}");
        }
        self.apply()?;
        let form = screen(&self.t, "A new category starts as a copy")?;
        assert!(
            form.contains("task #1") && form.contains("The supervisor's rules"),
            "{form}"
        );
        self.focus("Category name", "")?;
        self.t.send("Poster art")?;
        self.replace("Description", "\t\t\t\t", "Print posters")?;
        // The first template has an unknown placeholder: saving reports it and saves nothing.
        self.replace(
            "Execution template",
            "\t",
            "Paint {{title}} \\n then {{nope}}",
        )?;
        self.apply()?;
        let error = self.t.screen("invalid placeholder reported", |s| {
            s.contains("Unknown placeholder {{nope}}")
        })?;
        println!("INVALID PLACEHOLDER mouse={}\n{error}", self.mouse);
        assert!(error.contains("The execution template"), "{error}");
        assert!(
            self.service
                .category_in(Scope::User, "Poster art")?
                .is_none()
        );
        self.replace(
            "Execution template",
            "",
            "Paint {{title}}.\\nGoal: {{goal}}",
        )?;
        // Documents and art work needs no project checks.
        self.tab("\x1b[Z\x1b[Z")?;
        cycle(
            &mut self.t,
            self.mouse,
            "Project checks",
            &["Apply", "Do not apply"],
            0,
            1,
        )?;
        // Preview the review prompt before saving: runtime-only context is labelled.
        self.tab("\x1b[Z\x1b[Z")?;
        cycle(&mut self.t, self.mouse, "Action", &ACTIONS[..5], 0, 2)?;
        self.apply()?;
        let preview = screen(
            &self.t,
            "Preview of the review prompt for task #1. Nothing is saved.",
        )?;
        // The owner's template comes first. Its heading is on the first screen; scrolling
        // reaches the filled task content, the runtime-only label and then the fixed rules.
        assert!(
            preview.contains("== Work category Poster art (user scope), review template"),
            "{preview}"
        );
        for line in [
            "Goal: Goal with {{context}} kept literally",
            "[runtime-only context:",
        ] {
            let shown = self.scroll_to(line)?;
            if line.starts_with("Goal") {
                assert!(
                    shown.contains("1. First") && shown.contains("2. Second"),
                    "{shown}"
                );
            }
            println!("PREVIEW {line} mouse={}\n{shown}", self.mouse);
        }
        let rules =
            self.scroll_to("== Supervisor rules: fixed, not editable by the category ==")?;
        println!("PREVIEW RULES mouse={}\n{rules}", self.mouse);
        assert!(
            self.service
                .category_in(Scope::User, "Poster art")?
                .is_none()
        );
        // Back to Save through the remaining choices, then save.
        cycle(&mut self.t, self.mouse, "Action", &ACTIONS[..5], 2, 3)?;
        self.apply()?;
        screen(&self.t, "Saved user category Poster art")?;
        let saved = self
            .service
            .category_in(Scope::User, "Poster art")?
            .unwrap();
        assert!(!saved.category.checks);
        assert_eq!(saved.category.description, "Print posters");
        assert_eq!(
            saved.category.templates.execution,
            "Paint {{title}}.\nGoal: {{goal}}"
        );
        assert!(
            saved
                .category
                .templates
                .review
                .starts_with("Independently review")
        );
        Ok(())
    }

    /// Renaming a category onto another saved one is refused and changes nothing.
    fn refuse_rename_onto_existing(&mut self) -> Result {
        let banner = custom("Banner", "Banner {{goal}}", false);
        let before = self.service.save_category(Scope::User, None, &banner)?;
        self.open_list()?;
        let entries = [
            "New category",
            "1. code (built-in, checks apply)",
            "2. documents (built-in, no checks)",
            "3. art (built-in, no checks)",
            "4. Banner (user, no checks)",
            "5. Poster art (user, no checks)",
        ];
        self.choose(&entries, 0, 5)?;
        screen(&self.t, "Category Poster art (user)")?;
        self.replace("Category name", "", "Banner")?;
        self.apply()?;
        let refused = self.t.screen("rename refused", |s| {
            s.contains("Category Banner already exists in the user scope")
        })?;
        println!("RENAME REFUSED mouse={}\n{refused}", self.mouse);
        let after = self.service.category_in(Scope::User, "Banner")?.unwrap();
        assert_eq!(after, before, "the other category must not change");
        assert!(
            self.service
                .category_in(Scope::User, "Poster art")?
                .is_some()
        );
        self.press("\x1b", "Cancel")?;
        self.t
            .screen("closed", |s| !s.contains("Category Poster art (user)"))?;
        self.service.delete_category(Scope::User, "Banner", None)?;
        Ok(())
    }

    /// Moving a project override to the user scope onto an existing user category is refused.
    fn refuse_scope_switch_onto_existing(&mut self) -> Result {
        let user = self
            .service
            .category_in(Scope::User, "Poster art")?
            .unwrap();
        self.open_list()?;
        let entries = [
            "New category",
            "1. code (built-in, checks apply)",
            "2. documents (built-in, no checks)",
            "3. art (built-in, no checks)",
            "4. Poster art (project, checks apply)",
        ];
        self.choose(&entries, 0, 4)?;
        screen(&self.t, "Category Poster art (project)")?;
        self.tab("\t\t")?;
        cycle(
            &mut self.t,
            self.mouse,
            "Save in",
            &["Project scope", "User scope"],
            0,
            1,
        )?;
        self.apply()?;
        let refused = self.t.screen("scope switch refused", |s| {
            s.contains("Category Poster art already exists in the user scope")
        })?;
        println!("SCOPE SWITCH REFUSED mouse={}\n{refused}", self.mouse);
        assert_eq!(
            self.service
                .category_in(Scope::User, "Poster art")?
                .unwrap(),
            user
        );
        assert!(
            self.service
                .category_in(Scope::Project, "Poster art")?
                .is_some()
        );
        self.press("\x1b", "Cancel")?;
        self.t
            .screen("closed", |s| !s.contains("Category Poster art (project)"))?;
        Ok(())
    }

    fn override_for_this_project(&mut self) -> Result {
        self.open_list()?;
        let entries = [
            "New category",
            "1. code (built-in, checks apply)",
            "2. documents (built-in, no checks)",
            "3. art (built-in, no checks)",
            "4. Poster art (user, no checks)",
        ];
        self.choose(&entries, 0, 4)?;
        let form = screen(&self.t, "Category Poster art (user)")?;
        assert!(
            form.contains("< Delete (inherit again) >") || form.contains("Action"),
            "{form}"
        );
        self.tab("\t\t")?;
        cycle(
            &mut self.t,
            self.mouse,
            "Save in",
            &["User scope", "Project scope"],
            0,
            1,
        )?;
        self.tab("\t")?;
        cycle(
            &mut self.t,
            self.mouse,
            "Project checks",
            &["Apply", "Do not apply"],
            1,
            1,
        )?;
        self.apply()?;
        screen(&self.t, "Saved project category Poster art")?;
        let project = self
            .service
            .category_in(Scope::Project, "Poster art")?
            .unwrap();
        assert!(project.category.checks);
        assert!(
            !self
                .service
                .category_in(Scope::User, "Poster art")?
                .unwrap()
                .category
                .checks
        );
        assert_eq!(
            self.service.category("poster art")?.unwrap().category.scope,
            Scope::Project
        );
        Ok(())
    }

    fn select_on_a_task(&mut self) -> Result {
        self.press("2", "2 Queue")?;
        self.t.screen("queue", |s| {
            s.contains("Existing") && !s.contains("A work category")
        })?;
        self.press("a", "a Add")?;
        screen(&self.t, "Work category")?;
        for (label, text) in [
            ("Title", "Poster task"),
            ("Goal", "Print it"),
            ("Acceptance check", "A poster exists"),
        ] {
            // The queue behind the form has its own Title and Goal; click the form's.
            if self.mouse {
                self.t.click_after("Add task", label)?;
            } else if label != "Title" {
                self.t.send("\t")?;
            }
            self.t.send(text)?;
        }
        if !self.mouse {
            self.t.send("\t\t\t\t\t")?;
        }
        let names = ["Default", "code", "documents", "art", "Poster art"];
        cycle(&mut self.t, self.mouse, "Work category", &names, 0, 4)?;
        self.apply()?;
        self.t.screen("task saved", |s| {
            s.contains("Poster task") && !s.contains("Work category")
        })?;
        let id = self.service.snapshot_brief()?.queue.last().unwrap().id;
        let task = self.service.task(id)?.unwrap();
        let revision = task.definition();
        assert_eq!(revision.definition.title, "Poster task");
        assert_eq!(revision.definition.category.as_deref(), Some("Poster art"));
        let pinned = revision.category.as_ref().unwrap();
        assert_eq!((pinned.scope, pinned.checks), (Scope::Project, true));
        Ok(())
    }

    fn delete_the_override(&mut self) -> Result {
        self.press("4", "4 Config")?;
        screen(&self.t, "Storage: SQLite")?;
        self.open_list()?;
        let entries = [
            "New category",
            "1. code (built-in, checks apply)",
            "2. documents (built-in, no checks)",
            "3. art (built-in, no checks)",
            "4. Poster art (project, checks apply)",
        ];
        self.choose(&entries, 0, 4)?;
        screen(&self.t, "Category Poster art (project)")?;
        self.tab("\t")?;
        cycle(&mut self.t, self.mouse, "Action", &ACTIONS, 0, 5)?;
        self.apply()?;
        screen(&self.t, "Deleted project category Poster art")?;
        assert!(
            self.service
                .category_in(Scope::Project, "Poster art")?
                .is_none()
        );
        assert_eq!(
            self.service.category("Poster art")?.unwrap().category.scope,
            Scope::User
        );
        // The task defined while the override existed keeps its pinned copy.
        let id = self.service.snapshot_brief()?.queue.last().unwrap().id;
        let pinned = self
            .service
            .task(id)?
            .unwrap()
            .definition()
            .category
            .clone()
            .unwrap();
        assert_eq!((pinned.scope, pinned.checks), (Scope::Project, true));
        Ok(())
    }
}

#[test]
fn real_category_flows_create_preview_override_select_and_delete_with_keyboard_and_mouse() -> Result
{
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let mut service = seeded(root, root)?;
        service.create(&[definition("Existing", Kind::Ai, None)], None, true)?;
        let mut t = Terminal::open_real(BIN, root, 100, 70)?;
        screen(&t, "Existing")?;
        if mouse {
            t.click("4 Config")?;
        } else {
            t.send("4")?;
        }
        let config = screen(&t, "Storage: SQLite")?;
        assert!(config.contains("w Work categories"), "{config}");
        let mut flow = Flow { t, mouse, service };
        flow.create_poster_art()?;
        flow.refuse_rename_onto_existing()?;
        flow.override_for_this_project()?;
        flow.refuse_scope_switch_onto_existing()?;
        flow.select_on_a_task()?;
        flow.delete_the_override()?;
        flow.t.send("\x03")?;
        flow.t.finish()?;
    }
    Ok(())
}
