//! Review lenses: definitions and selection, pinned instructions, independent read-only
//! reviews, aggregation into one repair, the shared ten-round budget, and pause/resume,
//! with the deterministic provider fixture and real terminals.
use super::*;

fn query<T: rusqlite::types::FromSql>(f: &Fixture, sql: &str) -> Result<T> {
    Ok(f.db()?.connection().query_row(sql, [], |r| r.get(0))?)
}

fn lens_log(f: &Fixture) -> Result<Vec<String>> {
    Ok(fs::read_to_string(f.scratch.path().join("lens-log"))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect())
}

fn lens_count(f: &Fixture, lens: &str) -> usize {
    fs::read_to_string(f.scratch.path().join(format!("lens-count-{lens}")))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

fn set(f: &Fixture, name: &str, content: &str) -> Result {
    fs::write(f.scratch.path().join(name), content)?;
    Ok(())
}

fn exit(f: &Fixture, args: &[&str]) -> Result<(Option<i32>, String)> {
    let output = f.process().args(args).output()?;
    Ok((
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    ))
}

fn lens_names(task: &Value) -> Vec<String> {
    task["definition"]["lenses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn built_in_lenses_are_opt_in_and_ask_for_reachable_defects() -> Result {
    let f = Fixture::new()?;
    let list = f.ok(&["lens", "list"])?;
    let names: Vec<_> = list["lenses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["concurrency", "security", "simplicity"]);
    for lens in list["lenses"].as_array().unwrap() {
        let shown = f.ok(&["lens", "show", lens["name"].as_str().unwrap()])?["lens"].clone();
        let text = shown["instructions"].as_str().unwrap();
        assert!(
            text.contains("Report only a defect you can reach")
                && text.contains("smallest proportionate correction"),
            "{shown}"
        );
    }
    // Specialist review is opt-in: nothing is selected until a task or the project says so.
    f.add("Ordinary")?;
    assert_eq!(lens_names(&f.ok(&["show", "1"])?), Vec::<String>::new());
    f.no_scratch()?;
    Ok(())
}

#[test]
fn user_and_project_lenses_and_the_project_defaults_are_guarded() -> Result {
    let f = Fixture::new()?;
    // A user lens, overridden by the project, and a project default selection in order.
    f.ok(&[
        "lens",
        "save",
        "Data races",
        "--description",
        "Shared state",
        "--instructions",
        "User view of {{title}}: {{criteria}} then {{context}}",
    ])?;
    f.ok(&[
        "lens",
        "save",
        "Data races",
        "--scope",
        "project",
        "--instructions",
        "Project view of {{goal}}",
    ])?;
    let effective = f.ok(&["lens", "show", "data races"])?;
    assert_eq!(effective["lens"]["scope"], "project");
    assert_eq!(effective["lens"]["description"], "Shared state");
    let user = f.ok(&["lens", "show", "Data races", "--scope", "user"])?;
    assert!(
        user["lens"]["instructions"]
            .as_str()
            .unwrap()
            .starts_with("User view")
    );
    // Edits and deletes can be guarded by the version a read returned; built-ins are fixed.
    let (code, error) = exit(
        &f,
        &[
            "lens",
            "save",
            "Data races",
            "--scope",
            "project",
            "--description",
            "x",
            "--if-version",
            "99",
        ],
    )?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("is version 1, not 99"), "{error}");
    let (code, error) = exit(
        &f,
        &[
            "lens",
            "delete",
            "Data races",
            "--scope",
            "project",
            "--if-version",
            "99",
        ],
    )?;
    assert_eq!(code, Some(4), "{error}");
    let (code, error) = exit(
        &f,
        &[
            "lens",
            "save",
            "security",
            "--scope",
            "built-in",
            "--instructions",
            "x",
        ],
    )?;
    assert_eq!(code, Some(2), "{error}");
    let (code, error) = exit(&f, &["lens", "save", "Brand new"])?;
    assert_eq!(code, Some(2), "a new lens needs instructions: {error}");
    let (code, error) = exit(
        &f,
        &["lens", "save", "Bad", "--instructions", "Look at {{nope}}"],
    )?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("Unknown placeholder {{nope}}"), "{error}");
    let set_defaults = f.ok(&[
        "lens",
        "defaults",
        "--set",
        "security",
        "Data races",
        "--if-current",
        "",
    ])?;
    assert_eq!(
        set_defaults["defaults"],
        json_names(&["security", "Data races"])
    );
    let (code, error) = exit(
        &f,
        &[
            "lens",
            "defaults",
            "--set",
            "simplicity",
            "--if-current",
            "nope",
        ],
    )?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("changed elsewhere"), "{error}");
    let (code, error) = exit(&f, &["lens", "defaults", "--set", "security", "SECURITY"])?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("selected twice"), "{error}");
    let (code, error) = exit(&f, &["lens", "defaults", "--set", "unknown"])?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("Unknown review lens unknown"), "{error}");

    f.no_scratch()?;
    Ok(())
}

/// A user lens overridden by the project, and the project's default selection.
fn define_data_races(f: &Fixture) -> Result {
    f.ok(&[
        "lens",
        "save",
        "Data races",
        "--description",
        "Shared state",
        "--instructions",
        "User view of {{title}}: {{criteria}} then {{context}}",
    ])?;
    f.ok(&[
        "lens",
        "save",
        "Data races",
        "--scope",
        "project",
        "--instructions",
        "Project view of {{goal}}",
    ])?;
    f.ok(&["lens", "defaults", "--set", "security", "Data races"])?;
    Ok(())
}

#[test]
fn tasks_select_lenses_and_revisions_pin_their_instructions() -> Result {
    let f = Fixture::new()?;
    f.add("Ordinary")?;
    define_data_races(&f)?;
    // A new task takes the defaults; the override and the opt-out are per task.
    f.add("Defaulted")?;
    assert_eq!(
        lens_names(&f.ok(&["show", "2"])?),
        ["security", "Data races"]
    );
    f.ok(&[
        "add",
        "--title",
        "Chosen",
        "--goal",
        "G",
        "--criterion",
        "C",
        "--lens",
        "simplicity",
        "--lens",
        "concurrency",
    ])?;
    assert_eq!(
        lens_names(&f.ok(&["show", "3"])?),
        ["simplicity", "concurrency"]
    );
    f.ok(&[
        "add",
        "--title",
        "Plain",
        "--goal",
        "G",
        "--criterion",
        "C",
        "--no-lenses",
    ])?;
    assert_eq!(lens_names(&f.ok(&["show", "4"])?), Vec::<String>::new());
    let (code, error) = exit(
        &f,
        &[
            "add",
            "--title",
            "T",
            "--goal",
            "G",
            "--criterion",
            "C",
            "--lens",
            "nope",
        ],
    )?;
    assert_eq!(code, Some(4), "{error}");
    let (code, error) = exit(
        &f,
        &[
            "add",
            "--title",
            "T",
            "--goal",
            "G",
            "--criterion",
            "C",
            "--lens",
            "a",
            "--no-lenses",
        ],
    )?;
    assert_eq!(code, Some(2), "{error}");
    let (code, error) = exit(
        &f,
        &[
            "add",
            "--title",
            "T",
            "--goal",
            "G",
            "--criterion",
            "C",
            "--kind",
            "human",
            "--lens",
            "security",
        ],
    )?;
    assert_eq!(code, Some(4), "{error}");
    assert!(error.contains("Human tasks"), "{error}");
    let text = String::from_utf8(f.process().args(["show", "3"]).output()?.stdout)?;
    assert!(
        text.contains("Review lenses: simplicity, concurrency"),
        "{text}"
    );

    // The revision pins the instructions; editing the lens or the defaults changes nothing.
    let pinned = |f: &Fixture, task: i64, revision: i64| -> Result<Vec<(String, String, String)>> {
        let db = f.db()?;
        let mut statement = db.connection().prepare(
            "SELECT name, scope, instructions FROM revision_lenses WHERE task_id=?1 AND revision=?2 ORDER BY ordinal",
        )?;
        Ok(statement
            .query_map([task, revision], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?)
    };
    let before = pinned(&f, 2, 1)?;
    assert_eq!(before[0].0, "security");
    assert_eq!(before[0].1, "built_in");
    assert_eq!(
        before[1],
        (
            "Data races".into(),
            "project".into(),
            "Project view of {{goal}}".into()
        )
    );
    f.ok(&[
        "lens",
        "save",
        "Data races",
        "--scope",
        "project",
        "--instructions",
        "Changed {{goal}}",
    ])?;
    f.ok(&["lens", "defaults", "--clear"])?;
    f.ok(&["lens", "delete", "Data races", "--scope", "project"])?;
    assert_eq!(pinned(&f, 2, 1)?, before);
    // The next revision of the task keeps its selection and pins what resolves now: the
    // deleted project override falls back to the user lens.
    f.ok(&["edit", "2", "--title", "Defaulted again"])?;
    let after = pinned(&f, 2, 2)?;
    assert_eq!(after[0], before[0]);
    assert_eq!(after[1].1, "user");
    assert!(after[1].2.starts_with("User view"));
    f.ok(&["edit", "2", "--no-lenses"])?;
    assert_eq!(lens_names(&f.ok(&["show", "2"])?), Vec::<String>::new());
    f.ok(&["edit", "2", "--lens", "concurrency"])?;
    assert_eq!(lens_names(&f.ok(&["show", "2"])?), ["concurrency"]);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn defaults_naming_a_deleted_lens_are_reported_and_prompts_preview() -> Result {
    let f = Fixture::new()?;
    f.add("Ordinary")?;
    // Defaults naming a lens that no longer exists are reported when a task needs them.
    f.ok(&[
        "lens",
        "save",
        "Fleeting",
        "--instructions",
        "Look at {{goal}}",
    ])?;
    f.ok(&["lens", "defaults", "--set", "Fleeting"])?;
    f.ok(&["lens", "delete", "Fleeting"])?;
    let (code, error) = exit(
        &f,
        &["add", "--title", "T", "--goal", "G", "--criterion", "C"],
    )?;
    assert_eq!(code, Some(4), "{error}");
    assert!(
        error.contains("default review lens Fleeting no longer exists"),
        "{error}"
    );
    f.ok(&["lens", "defaults", "--clear"])?;

    let preview = f.ok(&["lens", "preview", "security", "--task", "1"])?;
    let text = preview["prompt"].as_str().unwrap();
    assert!(
        text.contains("Specialist review through the security lens")
            && text.contains("Goal: Create result.txt containing verified")
            && text.contains("1. result.txt contains verified")
            && text.contains("[runtime-only context:"),
        "{text}"
    );
    f.no_scratch()?;
    f.no_scratch()?;
    Ok(())
}

fn json_names(names: &[&str]) -> Value {
    Value::Array(names.iter().map(|n| Value::String((*n).into())).collect())
}

fn assert_every_lens_ran_in_a_session_of_its_own(f: &Fixture) -> Result {
    // Every lens ran in every round, one at a time and in the selected order, each in a
    // session of its own.
    let log = lens_log(f)?;
    let lenses: Vec<&str> = log.iter().map(|l| l.split(' ').next().unwrap()).collect();
    assert_eq!(
        lenses,
        [
            "concurrency",
            "security",
            "simplicity",
            "concurrency",
            "security",
            "simplicity"
        ]
    );
    let sessions: std::collections::BTreeSet<_> =
        log.iter().map(|l| l.split(' ').nth(1).unwrap()).collect();
    assert_eq!(sessions.len(), 6, "{log:?}");
    assert!(log.iter().all(|l| l.ends_with("resumed=")), "{log:?}");
    let invocations = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    let turns = |role: &str| invocations.lines().filter(|l| *l == role).count();
    assert_eq!(
        turns("review"),
        6,
        "the configured review model ran each lens"
    );
    assert_eq!(
        turns("remediation"),
        1,
        "the failure judge planned one repair"
    );
    assert_eq!(turns("execution"), 2, "one implementation and one repair");
    assert_eq!(turns("validation"), 1);
    Ok(())
}

fn assert_each_lens_prompt_holds_only_its_own_instructions(f: &Fixture) -> Result {
    // Each lens got its own instructions, the task fields and the evidence, and nothing of
    // the others.
    let first = fs::read_to_string(f.scratch.path().join("lens-prompt-security-1"))?;
    for needed in [
        "Specialist review through the security lens",
        "Judge it only through the security lens",
        "read-only by design",
        "Goal: Create result.txt containing verified",
        "1. result.txt contains verified",
        "\"exit_code\":0",
        "Read-only index directory:",
    ] {
        assert!(first.contains(needed), "{needed}\n{first}");
    }
    for other in ["concurrency", "simplicity"] {
        assert!(
            !first.contains(&format!("Specialist review through the {other} lens")),
            "{other} leaked into the security review"
        );
    }
    Ok(())
}

fn assert_one_repair_covered_every_lens(f: &Fixture) -> Result {
    // One repair round, whatever the number of lenses that asked for it.
    let round: i64 = query(
        f,
        "SELECT repair_round FROM attempt_progress WHERE attempt_id=1",
    )?;
    assert_eq!(round, 1);
    let repair_context = fs::read_to_string(f.scratch.path().join("prompt-judge"))?;
    for needed in [
        "[concurrency lens] result.txt:1: the concurrency defect is reachable",
        "[security lens] result.txt:1: the security defect is reachable",
    ] {
        assert!(
            repair_context.contains(needed),
            "{needed}\n{repair_context}"
        );
    }
    assert!(
        !repair_context.contains("simplicity lens] result.txt:1: the simplicity"),
        "only critical findings reach the judge's plan"
    );
    Ok(())
}

fn assert_results_name_each_lens_and_its_findings(f: &Fixture) -> Result {
    // Results name each lens and its findings.
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    let attempt = &task["attempts"][0];
    let reviews = attempt["lens_reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 6);
    let summary: Vec<_> = reviews
        .iter()
        .map(|r| {
            (
                r["round"].as_i64().unwrap(),
                r["lens"].as_str().unwrap(),
                r["critical"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (0, "concurrency", true),
            (0, "security", true),
            (0, "simplicity", false),
            (1, "concurrency", false),
            (1, "security", false),
            (1, "simplicity", false),
        ]
    );
    assert_eq!(
        reviews[1]["findings"][0]["description"],
        "result.txt:1: the security defect is reachable; apply the smallest correction"
    );
    assert_eq!(reviews[2]["findings"][0]["critical"], false);
    let review_steps: Vec<_> = attempt["steps"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["role"] == "review")
        .collect();
    assert_eq!(review_steps.len(), 6);
    assert_eq!(review_steps[0]["lens"], "concurrency");
    assert_eq!(review_steps[0]["state"], "changes_requested");
    assert!(
        review_steps[1]["report"]
            .as_str()
            .unwrap()
            .starts_with("security lens: security defect found\nCritical: ")
    );
    let text = String::from_utf8(f.process().args(["show", "1"]).output()?.stdout)?;
    assert!(
        text.contains("review (security lens) verdict")
            && text.contains("security lens: security defect found"),
        "{text}"
    );
    Ok(())
}

#[test]
fn every_lens_reviews_alone_and_one_repair_covers_all_of_them() -> Result {
    let f = Fixture::new()?;
    f.provider("lens-flow")?;
    f.ok(&[
        "lens",
        "defaults",
        "--set",
        "concurrency",
        "security",
        "simplicity",
    ])?;
    // Two lenses find a critical problem in the first round, the third only a polish note.
    set(&f, "lens-critical-concurrency", "1")?;
    set(&f, "lens-critical-security", "1")?;
    set(&f, "lens-minor-simplicity", "")?;
    f.add("Reviewed through lenses")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    f.guardian_finished()?;

    assert_every_lens_ran_in_a_session_of_its_own(&f)?;
    assert_each_lens_prompt_holds_only_its_own_instructions(&f)?;
    assert_one_repair_covered_every_lens(&f)?;
    assert_results_name_each_lens_and_its_findings(&f)?;
    // The minor note travels with the accepted result to validation.
    let validation = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
    assert!(
        validation.contains("optional simplicity polish"),
        "{validation}"
    );
    // No question or approval gate appeared for any lens.
    assert_eq!(query::<i64>(&f, "SELECT count(*) FROM questions")?, 0);
    f.no_scratch()?;
    Ok(())
}

#[test]
fn minor_only_lens_findings_are_recorded_and_never_request_a_repair() -> Result {
    let f = Fixture::new()?;
    f.provider("lens-flow")?;
    set(&f, "lens-minor-security", "")?;
    set(&f, "lens-minor-simplicity", "")?;
    f.ok(&[
        "add",
        "--title",
        "Minor lenses",
        "--goal",
        "Create result.txt containing verified",
        "--criterion",
        "result.txt contains verified",
        "--lens",
        "security",
        "--lens",
        "simplicity",
    ])?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    f.guardian_finished()?;
    assert_eq!(lens_log(&f)?.len(), 2);
    assert_eq!(
        query::<i64>(&f, "SELECT repair_round FROM attempt_progress")?,
        0
    );
    assert_eq!(
        query::<i64>(
            &f,
            "SELECT count(*) FROM attempt_steps WHERE role='remediation'"
        )?,
        0
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    let reviews = task["attempts"][0]["lens_reviews"].as_array().unwrap();
    assert_eq!(reviews.len(), 2);
    for (review, lens) in reviews.iter().zip(["security", "simplicity"]) {
        assert_eq!(review["lens"], lens);
        assert_eq!(review["critical"], false);
        assert_eq!(review["findings"][0]["critical"], false);
        assert_eq!(
            review["findings"][0]["description"],
            format!("result.txt:1: optional {lens} polish")
        );
    }
    let validation = fs::read_to_string(f.scratch.path().join("prompt-validation"))?;
    assert!(
        validation.contains("[security lens] result.txt:1: optional security polish")
            && validation.contains("[simplicity lens] result.txt:1: optional simplicity polish"),
        "{validation}"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn several_critical_lenses_share_the_ten_repair_rounds_without_asking_the_owner() -> Result {
    let f = Fixture::new()?;
    f.provider("lens-flow")?;
    set(&f, "lens-critical-concurrency", "always")?;
    set(&f, "lens-critical-security", "always")?;
    set(&f, "lens-final-critical", "")?;
    f.ok(&[
        "add",
        "--title",
        "Never satisfied",
        "--goal",
        "Create result.txt containing verified",
        "--criterion",
        "result.txt contains verified",
        "--lens",
        "concurrency",
        "--lens",
        "security",
    ])?;
    f.ok(&["edit", "1", "--attempt-limit", "1"])?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert_eq!(
        run.status.code(),
        Some(4),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    f.guardian_finished()?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed", "{task}");
    assert!(
        task["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("Critical findings remain after 10 repair rounds"),
        "{task}"
    );
    // Two lenses asked for a repair eleven times (rounds 0 to 10) and got ten: the count
    // is the attempt's, not the lens's, and every review of every round still ran.
    assert_eq!(
        query::<i64>(&f, "SELECT repair_round FROM attempt_progress")?,
        10
    );
    assert_eq!(
        query::<i64>(
            &f,
            "SELECT count(*) FROM attempt_steps WHERE role='remediation'"
        )?,
        10
    );
    assert_eq!(query::<i64>(&f, "SELECT count(*) FROM lens_reviews")?, 22);
    assert_eq!(
        query::<i64>(
            &f,
            "SELECT count(*) FROM lens_reviews WHERE repair_round=10"
        )?,
        2
    );
    assert_eq!(query::<i64>(&f, "SELECT count(*) FROM questions")?, 0);
    assert_eq!(query::<i64>(&f, "SELECT count(*) FROM attempts")?, 1);
    // The owner can read which lenses still object: the final round's reviews name each.
    let last: Vec<_> = task["attempts"][0]["lens_reviews"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["round"] == 10)
        .map(|r| {
            (
                r["lens"].as_str().unwrap(),
                r["critical"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(last, [("concurrency", true), ("security", true)], "{task}");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn paused_lens_review_resumes_without_repeating_finished_lenses_in_real_terminal() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.provider("lens-flow")?;
        // The first lens finds a polish note, the second is slow, so Pause lands between them.
        set(&f, "lens-minor-concurrency", "")?;
        set(&f, "lens-sleep-security", "")?;
        f.ok(&[
            "add",
            "--title",
            "Pause between lenses",
            "--goal",
            "Create result.txt containing verified",
            "--criterion",
            "result.txt contains verified",
            "--lens",
            "concurrency",
            "--lens",
            "security",
        ])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        t.screen("ready", |s| s.contains("r Run queue"))?;
        t.send("r")?;
        support::eventually("security lens started", || {
            Ok(f.scratch
                .path()
                .join("lens-started-security")
                .exists()
                .then_some(()))
        })?;
        println!(
            "LENS REVIEW ACTIVE mouse={mouse}\n{}",
            t.screen("review phase", |s| s.contains("Workflow · Review"))?
        );
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "LENS REVIEW PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| s.contains("Paused · runner stopped")
                && s.contains("r Resume"))?
        );
        assert_eq!(lens_count(&f, "concurrency"), 1);
        assert_eq!(lens_count(&f, "security"), 1);
        assert_eq!(query::<i64>(&f, "SELECT count(*) FROM lens_reviews")?, 1);
        fs::remove_file(f.scratch.path().join("lens-sleep-security"))?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        f.state("succeeded")?;
        // The finished concurrency review of the unchanged source state was not repeated; the
        // interrupted security review ran again, in a new session.
        assert_eq!(lens_count(&f, "concurrency"), 1, "mouse={mouse}");
        assert_eq!(lens_count(&f, "security"), 2, "mouse={mouse}");
        assert_eq!(query::<i64>(&f, "SELECT count(*) FROM lens_reviews")?, 2);
        assert_eq!(query::<i64>(&f, "SELECT count(*) FROM attempts")?, 1);
        let log = lens_log(&f)?;
        assert!(log.iter().all(|l| l.ends_with("resumed=")), "{log:?}");

        // Task details list each lens verdict and its findings.
        t.send("q")?;
        t.finish()?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 34)?;
        t.send("3")?;
        t.screen("History", |s| s.contains("Pause between lenses"))?;
        if mouse {
            t.click("Enter Details")?;
        } else {
            t.send("\r")?;
        }
        t.screen("details", |s| s.contains("Task details / attempts"))?;
        if mouse {
            t.click("Results")?;
        } else {
            t.send("g")?;
        }
        let results = t.screen("lens results", |s| {
            s.contains("Role steps, findings") && s.contains("(concurrency lens)")
        })?;
        println!("LENS RESULTS mouse={mouse}\n{results}");
        assert!(
            results.contains("concurrency lens: concurrency has only a polish note"),
            "{results}"
        );
        t.send("s")?;
        let summary = t.screen("summary labels lenses", |s| {
            s.contains("Recorded steps (select Reviews")
        })?;
        assert!(
            summary.contains("review (concurrency lens)")
                && summary.contains("review (security lens)"),
            "{summary}"
        );
        println!("LENS SUMMARY mouse={mouse}\n{summary}");
        t.send("\x1b")?;
        t.screen("details closed", |s| !s.contains("Task details"))?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

/// Advances the choice under `anchor` through `labels` (starting at `from`) `times` steps,
/// waiting for the screen to change after each, and checks where it ended.
fn choice_line<'a>(screen: &'a str, anchor: &str) -> Option<&'a str> {
    screen
        .lines()
        .zip(screen.lines().skip(1))
        .find_map(|(label, choice)| {
            (label.contains(anchor) && choice.contains("< ")).then_some(choice)
        })
}

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
        t.screen("choice advanced", |s| {
            choice_line(s, anchor).is_some_and(|line| line.contains(&next))
        })?;
    }
    let end = format!("< {} >", labels[(from + times) % labels.len()]);
    let screen = t.current_screen();
    let line = choice_line(&screen, anchor).unwrap_or_default();
    assert!(
        line.contains(&end),
        "{anchor}: expected {end} in {line:?}\n{screen}"
    );
    Ok(())
}

fn apply(t: &mut Terminal, mouse: bool) -> Result {
    if mouse {
        t.click("Apply")
    } else {
        t.send("\x13")
    }
}

const NAMES: [&str; 3] = ["concurrency", "security", "simplicity"];

#[test]
fn lens_screens_set_defaults_and_task_selection_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 44)?;
        t.screen("ready", |s| s.contains("a Add"))?;
        t.send("4")?;
        let config = t.screen("configuration", |s| s.contains("L Review lenses"))?;
        println!("LENS CONFIG mouse={mouse}\n{config}");
        if mouse {
            t.click("L Review lenses")?;
        } else {
            t.send("L")?;
        }
        let list = t.screen("lens list", |s| {
            s.contains("A review lens is a specialist review")
        })?;
        println!("LENS LIST mouse={mouse}\n{list}");
        for line in [
            "concurrency · built-in scope",
            "security · built-in scope",
            "simplicity · built-in scope",
            "Default lenses of this project: none (ordinary single review)",
            "{{context}} is runtime-only",
        ] {
            assert!(list.contains(line), "{line}\n{list}");
        }
        // The project defaults form: two lenses in order.
        cycle(
            &mut t,
            mouse,
            "Lens",
            &["New lens", "Project default lenses"],
            0,
            1,
        )?;
        apply(&mut t, mouse)?;
        let defaults = t.screen("defaults form", |s| {
            s.contains("The lenses new tasks of this project select")
        })?;
        println!("LENS DEFAULTS FORM mouse={mouse}\n{defaults}");
        let first: Vec<&str> = ["None (ordinary review)"]
            .into_iter()
            .chain(NAMES)
            .collect();
        cycle(&mut t, mouse, "Review lens 1", &first, 0, 2)?;
        if !mouse {
            t.send("\t")?;
        }
        let later: Vec<&str> = ["-"].into_iter().chain(NAMES).collect();
        cycle(&mut t, mouse, "Review lens 2", &later, 0, 3)?;
        apply(&mut t, mouse)?;
        let saved = t.screen("defaults saved", |s| {
            s.contains("Default review lenses: security, simplicity")
        })?;
        println!("LENS DEFAULTS SAVED mouse={mouse}\n{saved}");
        assert_eq!(
            f.ok(&["lens", "defaults"])?["defaults"],
            json_names(&["security", "simplicity"])
        );

        // A new task starts on the project default; choosing lenses on the form overrides it.
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        t.screen("queue", |s| s.contains("a Add"))?;
        if mouse {
            t.click("a Add")?;
        } else {
            t.send("a")?;
        }
        let form = t.screen("add form", |s| s.contains("Review lens 1"))?;
        println!("LENS TASK FORM mouse={mouse}\n{form}");
        assert!(form.contains("< Project default >"), "{form}");
        t.send(&format!(
            "Chosen lenses\t{}\tresult.txt contains verified",
            "Create result.txt containing verified"
        ))?;
        // Keyboard: Kind, model, effort, approval and category come before the lens fields;
        // by mouse the first click on a lens choice takes the focus.
        if !mouse {
            t.send("\t\t\t\t\t\t")?;
        }
        let first: Vec<&str> = ["Project default", "None (ordinary review)"]
            .into_iter()
            .chain(NAMES)
            .collect();
        cycle(&mut t, mouse, "Review lens 1", &first, 0, 2)?;
        if !mouse {
            t.send("\t")?;
        }
        cycle(&mut t, mouse, "Review lens 2", &later, 0, 2)?;
        let filled = t.current_screen();
        println!("LENS TASK FORM FILLED mouse={mouse}\n{filled}");
        apply(&mut t, mouse)?;
        t.screen("task saved", |s| !s.contains("Add task"))?;
        let created = f.ok(&["show", "1"])?;
        assert_eq!(
            lens_names(&created),
            ["concurrency", "security"],
            "{created}"
        );
        // The task's own details name the lenses it pinned.
        t.send("\r")?;
        let details = t.screen("details", |s| {
            s.contains("Review lenses: concurrency, security")
        })?;
        println!("LENS TASK DETAILS mouse={mouse}\n{details}");
        t.send("\x1b")?;
        t.screen("details closed", |s| !s.contains("Task details"))?;
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}

/// Focus the field labelled `label`: by clicking it, or by the keys that reach it.
fn focus(t: &mut Terminal, mouse: bool, label: &str, keys: &str) -> Result {
    if mouse { t.click(label) } else { t.send(keys) }
}

fn replace(t: &mut Terminal, mouse: bool, label: &str, keys: &str, text: &str) -> Result {
    focus(t, mouse, label, keys)?;
    t.send(&format!("\x01{text}"))?;
    t.screen("replaced", |s| s.contains(text))?;
    Ok(())
}

fn open_lenses(t: &mut Terminal, mouse: bool) -> Result {
    if mouse {
        t.click("L Review lenses")?;
    } else {
        t.send("L")?;
    }
    t.screen("lens list", |s| {
        s.contains("A review lens is a specialist review")
    })?;
    Ok(())
}

#[test]
fn a_lens_is_created_previewed_and_deleted_by_keyboard_and_mouse() -> Result {
    const ACTIONS: [&str; 3] = ["Save", "Preview prompt", "Delete (inherit again)"];
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 44)?;
        t.screen("ready", |s| s.contains("a Add"))?;
        t.send("4")?;
        t.screen("configuration", |s| s.contains("L Review lenses"))?;
        open_lenses(&mut t, mouse)?;
        apply(&mut t, mouse)?;
        let form = t.screen("new lens form", |s| {
            s.contains("A new lens starts as a copy of the security lens")
        })?;
        println!("LENS NEW FORM mouse={mouse}\n{form}");
        focus(&mut t, mouse, "Lens name", "")?;
        t.send("Lock misuse")?;
        // An unknown placeholder is reported and nothing is saved.
        replace(&mut t, mouse, "Instructions", "\t\t\t\t", "Check {{nope}}")?;
        apply(&mut t, mouse)?;
        let error = t.screen("invalid placeholder reported", |s| {
            s.contains("Unknown placeholder {{nope}}")
        })?;
        println!("LENS INVALID PLACEHOLDER mouse={mouse}\n{error}");
        assert_eq!(exit(&f, &["lens", "show", "Lock misuse"])?.0, Some(3));
        replace(
            &mut t,
            mouse,
            "Instructions",
            "",
            "Look for lock misuse in {{title}}.\\nGoal: {{goal}}",
        )?;
        // Preview the prompt before saving.
        if !mouse {
            t.send("\x1b[Z\x1b[Z\x1b[Z")?;
        }
        cycle(&mut t, mouse, "Action", &ACTIONS[..2], 0, 1)?;
        apply(&mut t, mouse)?;
        let preview = t.screen("preview shown", |s| {
            s.contains("Preview of the Lock misuse lens prompt for an example task")
        })?;
        println!("LENS PREVIEW mouse={mouse}\n{preview}");
        assert!(
            preview.contains("== Review lens Lock misuse (user scope) for work category code"),
            "{preview}"
        );
        assert_eq!(exit(&f, &["lens", "show", "Lock misuse"])?.0, Some(3));
        cycle(&mut t, mouse, "Action", &ACTIONS[..2], 1, 1)?;
        apply(&mut t, mouse)?;
        t.screen("saved", |s| s.contains("Saved user lens Lock misuse"))?;
        let saved = f.ok(&["lens", "show", "Lock misuse"])?;
        assert_eq!(
            saved["lens"]["instructions"],
            "Look for lock misuse in {{title}}.\nGoal: {{goal}}"
        );

        // Reopen it from the list and delete it again.
        open_lenses(&mut t, mouse)?;
        let entries = [
            "New lens",
            "Project default lenses",
            "1. concurrency (built-in)",
            "2. security (built-in)",
            "3. simplicity (built-in)",
            "4. Lock misuse (user)",
        ];
        cycle(&mut t, mouse, "Lens", &entries, 0, 5)?;
        apply(&mut t, mouse)?;
        t.screen("edit form", |s| s.contains("Lens Lock misuse (user)"))?;
        if !mouse {
            t.send("\t")?;
        }
        cycle(&mut t, mouse, "Action", &ACTIONS, 0, 2)?;
        apply(&mut t, mouse)?;
        let deleted = t.screen("deleted", |s| s.contains("Deleted user lens Lock misuse"))?;
        println!("LENS DELETED mouse={mouse}\n{deleted}");
        assert_eq!(exit(&f, &["lens", "show", "Lock misuse"])?.0, Some(3));
        t.send("q")?;
        t.finish()?;
    }
    Ok(())
}
