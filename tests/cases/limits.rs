//! Resource limits: they stop a runaway producer and nothing else, they are stored settings,
//! and a pause they cause is not repeated unchanged. Every limit here is small and explicit.
use super::*;

const ESCALATE: &str = "The use is not something a worker repair would change";

fn escalate(f: &Fixture) -> Result {
    f.breach_judgment(serde_json::json!({"action": "escalate", "reason": ESCALATE}))
}

/// How often the project check ran.
fn check_runs(f: &Fixture) -> String {
    fs::read_to_string(f.scratch.path().join("check-runs")).unwrap_or_default()
}

fn check(f: &Fixture, command: &str) -> Result {
    f.ok(&["check-set", "build", "--command", command])?;
    Ok(())
}

fn setting(f: &Fixture, limit: &str) -> Result<Value> {
    Ok(f.ok(&["limits", limit])?["limits"][0].clone())
}

/// A passing check whose transcript is larger than twice a 1 KiB per-file limit.
fn paused_transcript_fixture(title: &str) -> Result<Fixture> {
    let f = Fixture::new()?;
    escalate(&f)?;
    check(
        &f,
        "printf x >> \"$HOME/check-runs\"; head -c 3000 /dev/zero | tr '\\000' y; test -f result.txt",
    )?;
    f.ok(&["limits", "artifact_file_bytes", "--value", "1024"])?;
    f.add(title)?;
    Ok(f)
}

fn save_screen(screen: &str, name: &str) -> Result {
    println!("LIMITS PTY {name}\n{screen}");
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            screen,
            &PathBuf::from(directory).join(format!("{name}.png")),
        )?;
    }
    Ok(())
}

const SIZES: [(u16, u16); 2] = [(120, 36), (80, 24)];

#[test]
fn a_passing_check_with_too_much_evidence_completes_with_a_full_manifest() -> Result {
    let f = Fixture::new()?;
    f.ok(&[
        "limits",
        "artifact_attempt_files",
        "--value",
        "3",
        "--ceiling",
        "3",
    ])?;
    // 305 files: more than the 256 entries a separate scan limit once allowed.
    check(
        &f,
        "printf x >> \"$HOME/check-runs\"; d=\"$KTASK_ARTIFACT_DIR\"; printf '\\211PNG\\r\\n\\032\\n' > \"$d/a-screen.png\"; head -c 64 /dev/zero > \"$d/b.bin\"; printf 'notes\\n' > \"$d/notes.txt\"; printf 'other\\n' > \"$d/other.txt\"; printf 'ran\\n' > \"$d/z-check.log\"; i=0; while [ $i -lt 300 ]; do : > \"$d/zz-$i\"; i=$((i+1)); done; test -f result.txt",
    )?;
    f.add("Keep more check evidence than allowed")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    let task = f.ok(&["show", "1", "--manifest"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(check_runs(&f), "x");
    let truncated = &task["attempts"][0]["truncated_evidence"];
    assert_eq!(truncated["total_files"], 305, "{truncated}");
    assert_eq!(truncated["archived_files"], 3, "{truncated}");
    let files = task["evidence_manifest"][0]["files"].as_array().unwrap();
    assert_eq!(files.len(), 305);
    assert!(
        files.iter().all(|file| file["size_bytes"].is_u64()
            && file["checksum_sha256"]
                .as_str()
                .is_some_and(|sum| sum.len() == 64)),
        "every file has a size and checksum, archived or not"
    );
    let archived: Vec<_> = files
        .iter()
        .filter(|file| file["omitted"].is_null())
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(archived, ["z-check.log", "notes.txt", "other.txt"]);
    // Text before images before anything else.
    let order: Vec<_> = files
        .iter()
        .map(|file| file["name"].as_str().unwrap())
        .collect();
    assert_eq!(order[303..], ["a-screen.png", "b.bin"]);
    let shown = f.process().args(["show", "1"]).output()?;
    let shown = String::from_utf8(shown.stdout)?;
    assert!(
        shown.contains("Evidence truncated: 3 of 305 files archived"),
        "{shown}"
    );
    for role in ["review", "validation"] {
        let prompt = fs::read_to_string(f.scratch.path().join(format!("prompt-{role}")))?;
        assert!(
            prompt.contains("Supervisor fact: Evidence truncated: 3 of 305 files archived"),
            "{role}: {prompt}"
        );
        assert_eq!(f.packet_text(role, "manifest.jsonl")?.lines().count(), 305);
    }
    assert!(breaches(&f)?.is_empty());
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn an_unchanged_deterministic_breach_is_refused_until_the_limit_is_raised() -> Result {
    let f = Fixture::new()?;
    escalate(&f)?;
    check(
        &f,
        "printf x >> \"$HOME/check-runs\"; head -c 3000000 /dev/zero > \"$TMPDIR/flood.bin\"; sleep 1; test -f result.txt",
    )?;
    f.ok(&["limits", "scratch_bytes", "--value", "1048576"])?;
    f.add("Repeat no deterministic breach")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    let task = f.ok(&["show", "1"])?;
    let pause = &task["attempts"][0]["resource_pause"];
    assert_eq!(pause["limit"], "scratch_bytes", "{task}");
    assert_eq!(pause["value"], 1_048_576);
    assert!(pause["measured"].as_u64().unwrap() >= 3_000_000, "{pause}");
    assert_eq!(pause["deterministic"], true);
    assert_eq!(pause["proposal"], 4_194_304, "the ceiling covers the need");
    assert_eq!(check_runs(&f), "x");

    let refused = f.process().args(["run", "--one"]).output()?;
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let error = String::from_utf8(refused.stderr)?;
    assert!(
        error.contains("Resume refused: scratch_bytes is 1048576 bytes, needed")
            && error.contains("ktask run --raise"),
        "{error}"
    );
    assert_eq!(check_runs(&f), "x", "the check is not re-run");
    for (value, expected) in [("100", "must be above"), ("999999999", "owner ceiling")] {
        let rejected = f
            .process()
            .args(["run", "--one", "--raise", value])
            .output()?;
        assert_eq!(rejected.status.code(), Some(4), "{rejected:?}");
        assert!(
            String::from_utf8(rejected.stderr)?.contains(expected),
            "{value}"
        );
    }

    let raised = f.process().args(["run", "--one", "--raise"]).output()?;
    assert!(raised.status.success(), "{raised:?}");
    assert!(String::from_utf8(raised.stdout)?.contains("Raised scratch_bytes from 1048576"),);
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(check_runs(&f), "xx");
    let limit = setting(&f, "scratch_bytes")?;
    assert_eq!(
        (limit["value"].as_u64(), limit["source"].as_str()),
        (Some(4_194_304), Some("project"))
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn a_passed_check_is_not_repeated_after_its_transcript_pauses_the_attempt() -> Result {
    let f = paused_transcript_fixture("Archive the recorded check later")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("transcript evidence incomplete"),
        "{run:?}"
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "paused", "{task}");
    assert_eq!(
        task["attempts"][0]["resource_pause"]["limit"],
        "artifact_file_bytes"
    );
    assert_eq!(task["attempts"][0]["resource_pause"]["measured"], 3000);
    // A changed stored limit applies to the next step of the paused attempt.
    f.ok(&["limits", "artifact_file_bytes", "--value", "8192"])?;
    let resumed = f.process().args(["run", "--one"]).output()?;
    assert!(resumed.status.success(), "{resumed:?}");
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(check_runs(&f), "x", "the passed check is not run again");
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE role='check'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    assert!(
        task["attempts"][0]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| artifact["name"] == "build" && artifact["size_bytes"] == 3000),
        "{task}"
    );
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn a_limit_raised_from_the_cli_is_honoured_by_a_tui_started_resume() -> Result {
    let f = paused_transcript_fixture("Resume from the terminal")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    f.state("paused")?;
    f.ok(&["limits", "artifact_file_bytes", "--value", "8192"])?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
    t.screen("paused task", |s| s.contains("[r Resume]"))?;
    t.send("r")?;
    f.state("succeeded")?;
    assert_eq!(check_runs(&f), "x");
    f.supervisor_stopped()?;
    t.send("q")?;
    t.finish()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn resource_pause_and_raise_and_resume_by_keyboard_and_mouse_at_both_sizes() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            let name = format!("{cols}x{rows}-{}", if mouse { "mouse" } else { "keys" });
            let f = paused_transcript_fixture("Raise and resume the pause")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            t.screen("task ready", |s| s.contains("[r Run queue]"))?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            let paused = t.screen("pause with its limit", |s| {
                s.contains("Limit artifact_file_bytes")
                    && s.contains("Now 1024 · needed 3000")
                    && s.contains("+ Raise to 4096")
                    && s.contains("[+ Raise and resume]")
            })?;
            save_screen(&paused, &format!("resource-pause-{name}"))?;
            f.supervisor_stopped()?;
            t.still("paused", |s| s.contains("[r Resume]"))?;
            if mouse {
                t.click("r Resume")?;
            } else {
                t.send("r")?;
            }
            let refused = t.screen("plain resume refused", |s| {
                s.contains("Resume refused") && s.contains("New value")
            })?;
            save_screen(&refused, &format!("resource-refused-{name}"))?;
            if mouse {
                t.click("Cancel")?;
            } else {
                t.send("\x1b")?;
            }
            t.screen("refusal closed", |s| !s.contains("New value"))?;
            if mouse {
                t.click("+ Raise and resume")?;
            } else {
                t.send("+")?;
            }
            let dialog = t.screen("raise dialog", |s| {
                s.contains("New value") && s.contains("4096") && s.contains("[Raise and resume]")
            })?;
            save_screen(&dialog, &format!("resource-raise-{name}"))?;
            assert_eq!(check_runs(&f), "x");
            if mouse {
                t.click("Raise and resume")?;
            } else {
                t.send("\r")?;
            }
            f.state("succeeded")?;
            assert_eq!(check_runs(&f), "x", "the recorded check is not repeated");
            assert_eq!(setting(&f, "artifact_file_bytes")?["value"], 4096);
            f.supervisor_stopped()?;
            let done = t.screen("queue complete", |s| s.contains("Queue complete"))?;
            save_screen(&done, &format!("resource-resumed-{name}"))?;
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}

#[test]
fn configuration_edits_limits_and_warns_by_keyboard_and_mouse_at_both_sizes() -> Result {
    let f = Fixture::new()?;
    check(
        &f,
        "for n in 1 2 3; do printf p > \"$KTASK_ARTIFACT_DIR/proof-$n.txt\"; done; test -f result.txt",
    )?;
    f.add("Measure the evidence a task needs")?;
    f.ok(&["run", "--one"])?;
    let mut expected = 6;
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            let name = format!("{cols}x{rows}-{}", if mouse { "mouse" } else { "keys" });
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            t.screen("started", |s| s.contains("Queue complete"))?;
            if mouse {
                t.click("4 Config")?;
            } else {
                t.send("4")?;
            }
            t.screen("configuration", |s| s.contains("[R Limits]"))?;
            if mouse {
                t.click("R Limits")?;
            } else {
                t.send("R")?;
            }
            let overview = t.screen("limits overview", |s| {
                s.contains("Limit to edit") && s.contains("scratch_bytes: ")
            })?;
            save_screen(&overview, &format!("limits-overview-{name}"))?;
            let names = [
                "scratch_bytes",
                "scratch_entries",
                "artifact_file_bytes",
                "artifact_attempt_bytes",
                "artifact_attempt_files",
            ];
            for pair in names.windows(2) {
                if mouse {
                    t.click_after("Limit to edit", pair[0])?;
                } else {
                    t.send(" ")?;
                }
                t.screen("next limit chosen", |s| {
                    s.split_once("Limit to edit")
                        .is_some_and(|(_, field)| field.contains(pair[1]))
                })?;
            }
            if mouse {
                t.click("Apply")?;
            } else {
                t.send("\r")?;
            }
            let form = t.screen("limit form", |s| {
                s.contains("Resource limit · artifact_attempt_files") && s.contains("Project value")
            })?;
            save_screen(&form, &format!("limits-form-{name}"))?;
            if mouse {
                t.click("Project value")?;
            }
            // Ctrl-A clears the field before the new value.
            t.send(&format!("\x01{expected}"))?;
            t.screen("value typed", |s| {
                s.split_once("Project value")
                    .and_then(|(_, rest)| rest.split_once("Project ceiling"))
                    .is_some_and(|(field, _)| field.contains(&expected.to_string()))
            })?;
            if mouse {
                t.click("Apply")?;
            } else {
                t.send("\r")?;
            }
            // The task needed four files (three proofs and its transcript).
            let saved = t.screen("saved with a warning", |s| {
                s.contains(&format!("Saved artifact_attempt_files: {expected}"))
                    && s.contains("Limit warning: artifact_attempt_files")
            })?;
            save_screen(&saved, &format!("limits-saved-{name}"))?;
            assert_eq!(setting(&f, "artifact_attempt_files")?["value"], expected);
            t.send("q")?;
            t.finish()?;
            // Still under twice the four files used, so the warning stays.
            expected = if expected == 7 { 4 } else { expected + 1 };
        }
    }
    Ok(())
}

#[test]
fn limits_command_stores_project_and_instance_values_and_warns_at_task_start() -> Result {
    let f = Fixture::new()?;
    let other = f.second_project("other")?;
    let all = f.ok(&["limits"])?;
    let names: Vec<_> = all["limits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|limit| limit["limit"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "scratch_bytes",
            "scratch_entries",
            "artifact_file_bytes",
            "artifact_attempt_bytes",
            "artifact_attempt_files",
            "output_bytes",
            "output_rows",
            "runner_log_bytes"
        ]
    );
    f.ok(&["limits", "output_rows", "--value", "500", "--instance"])?;
    let shared = f.ok_in(&other, &["limits", "output_rows"])?["limits"][0].clone();
    assert_eq!(
        (shared["value"].as_u64(), shared["source"].as_str()),
        (Some(500), Some("instance"))
    );
    f.ok(&[
        "limits",
        "output_rows",
        "--value",
        "900",
        "--ceiling",
        "950",
    ])?;
    let own = setting(&f, "output_rows")?;
    assert_eq!(
        (
            own["value"].as_u64(),
            own["ceiling"].as_u64(),
            own["source"].as_str()
        ),
        (Some(900), Some(950), Some("project"))
    );
    assert_eq!(
        f.ok_in(&other, &["limits", "output_rows"])?["limits"][0]["value"],
        500
    );
    let below = f
        .process()
        .args(["limits", "output_rows", "--value", "960"])
        .output()?;
    assert_eq!(below.status.code(), Some(4), "{below:?}");
    f.ok(&["limits", "output_rows", "--reset"])?;
    assert_eq!(setting(&f, "output_rows")?["source"], "instance");
    let overridden = f
        .process()
        .env("KTASK_OUTPUT_ROWS", "77")
        .args(["--json", "limits", "output_rows"])
        .output()?;
    let overridden: Value = serde_json::from_slice(&overridden.stdout)?;
    assert_eq!(overridden["data"]["limits"][0]["source"], "env");
    assert_eq!(overridden["data"]["limits"][0]["value"], 77);
    let text = f.process().args(["limits"]).output()?;
    assert!(String::from_utf8(text.stdout)?.contains("output_rows\t500\t2000\tinstance\t-"));

    check(
        &f,
        "for n in 1 2 3; do printf p > \"$KTASK_ARTIFACT_DIR/proof-$n.txt\"; done; test -f result.txt",
    )?;
    f.add("Record what a task needs")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(setting(&f, "artifact_attempt_files")?["high_water"], 4);
    f.ok(&["limits", "artifact_attempt_files", "--value", "5"])?;
    let warned = setting(&f, "artifact_attempt_files")?;
    assert!(
        warned["warning"]
            .as_str()
            .unwrap()
            .contains("less than twice"),
        "{warned}"
    );
    f.add("Warn before the next task")?;
    f.ok(&["run", "--one"])?;
    assert!(f.db()?.connection().query_row(
        "SELECT EXISTS(SELECT 1 FROM attempt_output WHERE attempt_id=2 AND content LIKE 'Limit warning: artifact_attempt_files%')",
        [],
        |r| r.get::<_, bool>(0),
    )?);
    Ok(())
}

#[test]
fn a_stored_scratch_limit_still_stops_a_runaway_producer() -> Result {
    let f = Fixture::new()?;
    escalate(&f)?;
    f.provider("scratch-continuing-bytes")?;
    f.ok(&["limits", "scratch_bytes", "--value", "65536"])?;
    f.add("Stop the runaway producer")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    assert!(String::from_utf8_lossy(&run.stdout).contains("managed scratch exceeded"));
    assert!(!f.scratch.path().join("scratch-natural-completion").exists());
    assert_eq!(breaches(&f)?[0].0, "scratch_bytes");
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn kept_evidence_is_archived_or_recorded_as_truncated_when_the_runner_continues() -> Result {
    let f = Fixture::new()?;
    f.add("Leave evidence behind")?;
    f.ok(&["run", "--one"])?;
    let kept = f.scratch.path().join("kept-evidence");
    fs::create_dir(&kept)?;
    fs::write(kept.join("late.txt"), "saved after an interruption\n")?;
    let gone = f.scratch.path().join("gone-evidence");
    for (step, source) in [(900, &kept), (901, &gone)] {
        f.db()?.connection().execute(
            "INSERT INTO evidence_collections(attempt_id,relative_prefix,source_path,source_tree,state) VALUES (1,?1,?2,?3,'collecting')",
            rusqlite::params![
                format!("evidence/attempt-1/step-{step}"),
                source.as_os_str().as_encoded_bytes(),
                "a".repeat(40)
            ],
        )?;
    }
    f.add("Continue the runner")?;
    f.ok(&["run", "--one"])?;
    assert!(!kept.exists());
    let task = f.ok(&["show", "1"])?;
    assert!(
        task["attempts"][0]["artifacts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|artifact| artifact["name"] == "late.txt"),
        "{task}"
    );
    assert!(
        task["attempts"][0]["truncated_evidence"]["note"]
            .as_str()
            .unwrap()
            .contains("the retained files are gone"),
        "{task}"
    );
    Ok(())
}

#[test]
fn truncated_evidence_is_shown_in_current_history_and_details() -> Result {
    for (cols, rows) in SIZES {
        let name = format!("{cols}x{rows}");
        let f = paused_transcript_fixture("Show what was left out")?;
        f.ok(&[
            "limits",
            "artifact_attempt_files",
            "--value",
            "2",
            "--ceiling",
            "2",
        ])?;
        check(
            &f,
            "for n in 1 2 3 4 5; do printf p > \"$KTASK_ARTIFACT_DIR/proof-$n.txt\"; done; head -c 3000 /dev/zero | tr '\\000' y; test -f result.txt",
        )?;
        f.ok(&["run", "--one"])?;
        f.state("paused")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
        let current = t.screen("current evidence", |s| s.contains("Truncated evidence 2/5"))?;
        save_screen(&current, &format!("evidence-current-{name}"))?;
        t.send("q")?;
        t.finish()?;
        f.ok(&["limits", "artifact_file_bytes", "--value", "8192"])?;
        f.ok(&["run", "--one"])?;
        f.state("succeeded")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
        t.screen("started", |s| s.contains("Queue complete"))?;
        t.send("3")?;
        t.screen("history", |s| s.contains("History · 1 shown"))?;
        t.send("\r")?;
        let details = t.screen("details evidence", |s| {
            s.contains("Task details") && s.contains("Evidence truncated: 2 of 5 files archived")
        })?;
        save_screen(&details, &format!("evidence-history-{name}"))?;
        t.send("\x1b")?;
        t.screen("details closed", |s| !s.contains("Task details"))?;
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}

/// What a refused or repeated run must not add to.
fn recorded_work(f: &Fixture) -> Result<(i64, i64, i64)> {
    Ok(f.db()?.connection().query_row(
        "SELECT (SELECT count(*) FROM attempts),(SELECT count(*) FROM attempt_steps),(SELECT count(*) FROM attempt_output)",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?)
}

fn storage_budget(f: &Fixture) -> Result<u64> {
    Ok(f.ok(&["storage"])?["preview"]["policy"]["max_bytes"]
        .as_u64()
        .ok_or("storage budget")?)
}

/// The pause names the storage budget, its value, the need and a covering proposal.
fn storage_pause(f: &Fixture) -> Result<Value> {
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "paused", "{task}");
    let pause = task["attempts"][0]["resource_pause"].clone();
    assert_eq!(pause["limit"], "storage_max_bytes", "{task}");
    assert_eq!(pause["deterministic"], true, "{pause}");
    let number = |name: &str| {
        pause[name]
            .as_u64()
            .ok_or_else(|| format!("{name}: {pause}"))
    };
    assert_eq!(number("value")?, number("current")?);
    assert!(number("measured")? > number("value")?, "{pause}");
    assert!(number("proposal")? >= number("measured")?, "{pause}");
    Ok(pause)
}

fn assert_refused_without_work(f: &Fixture) -> Result {
    let before = recorded_work(f)?;
    let refused = f.process().args(["run", "--one"]).output()?;
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let error = String::from_utf8(refused.stderr)?;
    assert!(
        error.contains("Resume refused: storage_max_bytes is")
            && error.contains("Raise and resume sets storage_max_bytes"),
        "{error}"
    );
    assert_eq!(recorded_work(f)?, before, "a refused resume does no work");
    Ok(())
}

fn raise_and_succeed(f: &Fixture) -> Result {
    // The proposal follows the current allocation, so read it just before raising.
    let pause = storage_pause(f)?;
    let raised = f.process().args(["run", "--one", "--raise"]).output()?;
    assert!(raised.status.success(), "{raised:?}");
    assert!(String::from_utf8(raised.stdout)?.contains("Raised storage_max_bytes from"));
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded", "{task}");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    assert_eq!(Some(storage_budget(f)?), pause["proposal"].as_u64());
    f.guardian_finished()?;
    f.no_scratch()?;
    Ok(())
}

#[test]
fn output_refused_by_the_storage_budget_offers_raise_and_resume() -> Result {
    let f = Fixture::new()?;
    // The first run shrinks the budget to its minimum and then prints: that output is refused.
    check(
        &f,
        &format!(
            "if [ ! -f \"$HOME/flooded\" ]; then touch \"$HOME/flooded\"; env -u KTASK_WORKER_CALLBACK '{BIN}' retention 30 --max-bytes 1048576 >/dev/null 2>&1; head -c 100000 /dev/zero | tr '\\000' y; fi; test -f result.txt"
        ),
    )?;
    f.add("Pause on the storage budget")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("shared storage capacity reserve"),
        "{run:?}"
    );
    assert_eq!(storage_pause(&f)?["value"], 1_048_576);
    let shown = String::from_utf8(f.process().args(["show", "1"]).output()?.stdout)?;
    assert!(
        shown.contains("Resource pause: storage_max_bytes is 1048576 bytes, needed"),
        "{shown}"
    );
    assert_refused_without_work(&f)?;
    raise_and_succeed(&f)
}

#[test]
fn a_launch_refused_by_the_storage_budget_records_the_same_pause() -> Result {
    let f = Fixture::new()?;
    let allocated = f.ok(&["storage"])?["preview"]["allocated_bytes"]
        .as_u64()
        .unwrap();
    let budget = (allocated + 8 * 1024 * 1024).to_string();
    f.ok(&["retention", "30", "--max-bytes", &budget])?;
    // A runner log segment the check grows: only the next launch's probe charges it.
    check(
        &f,
        "printf x >> \"$HOME/check-runs\"; head -c 8388608 /dev/zero >> \"$XDG_STATE_HOME/ktask-dev/runner-1.log\"; test -f result.txt",
    )?;
    f.add("Refuse the next launch")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    assert!(
        String::from_utf8_lossy(&run.stdout)
            .contains("Resource limit: storage capacity reserve reached"),
        "{run:?}"
    );
    storage_pause(&f)?;
    assert_refused_without_work(&f)?;
    raise_and_succeed(&f)?;
    assert_eq!(check_runs(&f), "x", "the passed check is not repeated");
    Ok(())
}

/// A silent passing check that first shrinks the storage budget to its minimum, so only
/// its own transcript is refused.
fn paused_by_storage_fixture(title: &str) -> Result<Fixture> {
    let f = Fixture::new()?;
    check(
        &f,
        &format!(
            "printf x >> \"$HOME/check-runs\"; env -u KTASK_WORKER_CALLBACK '{BIN}' retention 30 --max-bytes 1048576 >/dev/null 2>&1; test -f result.txt"
        ),
    )?;
    f.add(title)?;
    Ok(f)
}

#[test]
fn a_transcript_refused_by_storage_is_archived_after_the_raise_without_a_rerun() -> Result {
    let f = paused_by_storage_fixture("Keep the recorded check")?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    assert!(
        String::from_utf8_lossy(&run.stdout).contains("transcript evidence incomplete"),
        "{run:?}"
    );
    storage_pause(&f)?;
    assert_refused_without_work(&f)?;
    raise_and_succeed(&f)?;
    assert_eq!(check_runs(&f), "x");
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT count(*) FROM artifacts WHERE attempt_id=1 AND name='build'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        1
    );
    Ok(())
}

#[test]
fn a_storage_pause_another_projects_budget_governs_cannot_be_raised_here() -> Result {
    let f = Fixture::new()?;
    let other = f.second_project("other")?;
    check(
        &f,
        &format!(
            "printf x >> \"$HOME/check-runs\"; env -u KTASK_WORKER_CALLBACK '{BIN}' --project '{}' retention 30 --max-bytes 1048576 >/dev/null 2>&1; test -f result.txt",
            other.display()
        ),
    )?;
    f.add("Name the governing budget")?;
    let own = storage_budget(&f)?;
    let run = f.process().args(["run", "--one"]).output()?;
    assert!(run.status.success(), "{run:?}");
    let pause = f.ok(&["show", "1"])?["attempts"][0]["resource_pause"].clone();
    assert!(pause["proposal"].is_null(), "{pause}");
    assert!(
        pause["blocked"]
            .as_str()
            .unwrap()
            .contains("Another project's smaller storage_max_bytes"),
        "{pause}"
    );
    for args in [
        &["run", "--one", "--raise"][..],
        &["run", "--one", "--raise=9000000"],
    ] {
        let rejected = f.process().args(args).output()?;
        assert_eq!(rejected.status.code(), Some(4), "{rejected:?}");
        assert!(String::from_utf8(rejected.stderr)?.contains("Another project's"));
    }
    assert_eq!(storage_budget(&f)?, own);
    // Raising the governing budget resumes without a raise here.
    f.ok_in(&other, &["retention", "30", "--max-bytes", "1073741824"])?;
    let resumed = f.process().args(["run", "--one"]).output()?;
    assert!(resumed.status.success(), "{resumed:?}");
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(check_runs(&f), "x");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn storage_pause_and_raise_and_resume_by_keyboard_and_mouse_at_both_sizes() -> Result {
    for (cols, rows) in SIZES {
        for mouse in [false, true] {
            let name = format!("{cols}x{rows}-{}", if mouse { "mouse" } else { "keys" });
            let f = paused_by_storage_fixture("Raise the storage budget")?;
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            t.screen("task ready", |s| s.contains("[r Run queue]"))?;
            if mouse {
                t.click("r Run queue")?;
            } else {
                t.send("r")?;
            }
            let paused = t.screen("storage pause", |s| {
                s.contains("Limit storage_max_bytes")
                    && s.contains("Now 1048576")
                    && s.contains("needed ")
                    && s.contains("+ Raise to ")
                    && s.contains("[+ Raise and resume]")
            })?;
            save_screen(&paused, &format!("storage-pause-{name}"))?;
            f.supervisor_stopped()?;
            let proposal = storage_pause(&f)?["proposal"].as_u64().unwrap();
            assert!(
                paused.contains(&format!("+ Raise to {proposal}")),
                "{paused}"
            );
            t.still("paused", |s| s.contains("[r Resume]"))?;
            if mouse {
                t.click("r Resume")?;
            } else {
                t.send("r")?;
            }
            let refused = t.screen("plain resume refused", |s| {
                s.contains("Resume refused") && s.contains("New value")
            })?;
            save_screen(&refused, &format!("storage-refused-{name}"))?;
            f.supervisor_stopped()?;
            if mouse {
                t.click("Cancel")?;
            } else {
                t.send("\x1b")?;
            }
            t.screen("refusal closed", |s| !s.contains("New value"))?;
            if mouse {
                t.click("+ Raise and resume")?;
            } else {
                t.send("+")?;
            }
            let dialog = t.screen("raise dialog", |s| {
                s.contains("New value") && s.contains("[Raise and resume]")
            })?;
            save_screen(&dialog, &format!("storage-raise-{name}"))?;
            if mouse {
                t.click("Raise and resume")?;
            } else {
                t.send("\r")?;
            }
            f.state("succeeded")?;
            assert_eq!(check_runs(&f), "x");
            assert!(storage_budget(&f)? > 1_048_576);
            f.supervisor_stopped()?;
            let done = t.screen("queue complete", |s| s.contains("Queue complete"))?;
            save_screen(&done, &format!("storage-resumed-{name}"))?;
            t.send("q")?;
            t.finish()?;
            f.no_scratch()?;
        }
    }
    Ok(())
}
