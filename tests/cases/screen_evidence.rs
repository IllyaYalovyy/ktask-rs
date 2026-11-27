//! What a project with a golden screen set hands its checks, judges and owner. The provider
//! is a fixture; the check, the packet, the CLI and the TUI are real.
use super::*;

const REPORT: &str = "Changed screens report.\n=== NEW ui · opened · 80x24 · keyboard ===\n+++ after\nhello from the new screen\n";

/// A project with a committed golden set whose check keeps `command`'s evidence.
fn golden_project(command: &str) -> Result<(Fixture, String)> {
    let f = Fixture::new()?;
    fs::create_dir_all(f.root.join("tests/golden"))?;
    fs::write(
        f.root.join("tests/golden/ui.screens"),
        "@@ opened · 80x24 · keyboard\nhello\n",
    )?;
    f.git(&["add", "tests/golden"])?;
    f.git(&["commit", "-qm", "approved screens"])?;
    f.ok(&["check-set", "build", "--command", command])?;
    f.add("Change a screen")?;
    let head = f.git(&["rev-parse", "HEAD"])?.trim().to_owned();
    Ok((f, head))
}

fn prompt(f: &Fixture, role: &str) -> Result<String> {
    Ok(fs::read_to_string(
        f.scratch.path().join(format!("input-{role}")),
    )?)
}

fn plain(f: &Fixture, args: &[&str]) -> Result<String> {
    let output = f.process().args(args).output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
fn judges_get_the_changed_screens_report_and_attempt_details_show_it() -> Result {
    let command = format!(
        "printf %s \"$KTASK_BASE_COMMIT\" > \"$HOME/check-base\"; printf '%s' '{REPORT}' > \"$KTASK_ARTIFACT_DIR/changed-screens.txt\"; test -f result.txt"
    );
    let (f, head) = golden_project(&command)?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    // The check compares against the commit the work started from, also after delivery.
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("check-base"))?,
        head
    );
    let mut artifact = 0;
    for role in ["review", "validation"] {
        let prompt = prompt(&f, role)?;
        let report = f.packet_artifact(role, "changed-screens.txt")?;
        artifact = report["id"].as_i64().ok_or("report ID")?;
        let path = report["path"].as_str().ok_or("report path")?;
        for part in [
            "Screen evidence: this project keeps an approved set of terminal screens in tests/golden",
            "Screens that match the approved set were verified in code against it",
            &format!(
                "The changed-screens report, artifact #{artifact} at {path}, is the only screen evidence"
            ),
            "judge those new and changed screens against the criteria",
        ] {
            assert!(prompt.contains(part), "{role} lacks {part:?}:\n{prompt}");
        }
        let page = plain(&f, &["evidence", path, "--offset", "0", "--limit", "4096"])?;
        assert!(page.contains("hello from the new screen"), "{page}");
    }
    assert!(!prompt(&f, "execution")?.contains("Screen evidence:"));
    let shown = plain(&f, &["show", "1"])?;
    assert!(
        shown.contains(&format!(
            "Changed-screens report: artifact {artifact} (read it with: artifact {artifact})"
        )),
        "{shown}"
    );
    let text = plain(&f, &["artifact", &artifact.to_string()])?;
    assert_eq!(text.trim_end(), REPORT.trim_end());

    for (cols, rows) in [(120, 36), (80, 24)] {
        for mouse in [false, true] {
            let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, cols, rows)?;
            t.screen("queue complete", |s| s.contains("Queue complete"))?;
            press(&mut t, mouse, "3 History", "3")?;
            t.screen("history", |s| s.contains("History · 1 shown"))?;
            press(&mut t, mouse, "Enter Details", "\r")?;
            t.screen("details", |s| s.contains("Task details / attempts"))?;
            press(&mut t, mouse, "Summary", "s")?;
            let summary = t.screen("summary names the report", |s| {
                s.contains(&format!("Changed-screens report: artifact #{artifact}"))
            })?;
            crate::support::record("changed screens report in attempt summary", mouse, &summary);
            press(&mut t, mouse, "Transcripts", "t")?;
            t.screen("transcripts", |s| s.contains("Artifact 1 / "))?;
            // The report is the first file the check's worker left.
            let report = t.screen("report text", |s| {
                s.contains("changed-screens.txt") && s.contains("hello from the new screen")
            });
            let report = match report {
                Ok(report) => report,
                Err(_) => {
                    press(&mut t, mouse, "Check +", ".")?;
                    t.screen("report text", |s| s.contains("hello from the new screen"))?
                }
            };
            crate::support::record("changed screens report text", mouse, &report);
            press(&mut t, mouse, "Close", "\x1b")?;
            t.screen("details closed", |s| !s.contains("Task details / attempts"))?;
            t.send("q")?;
            t.finish()?;
        }
    }
    Ok(())
}

#[test]
fn judges_are_told_when_no_screen_changed_and_nothing_without_a_golden_set() -> Result {
    let (f, _) = golden_project("test -f result.txt")?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    for role in ["review", "validation"] {
        let prompt = prompt(&f, role)?;
        assert!(
            prompt.contains("verified in code against it")
                && prompt.contains("No changed-screens report is archived")
                && prompt.contains("there is no screen to review"),
            "{prompt}"
        );
    }
    let shown = plain(&f, &["show", "1"])?;
    assert!(!shown.contains("Changed-screens report"), "{shown}");

    let f = Fixture::new()?;
    f.add("No golden set")?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    for role in ["review", "validation"] {
        assert!(!prompt(&f, role)?.contains("Screen evidence:"));
    }
    Ok(())
}

/// What judges are told when the report cannot stand for the screens.
const BLOCKS: &str = "this missing evidence blocks acceptance";

/// The screen statement a judge's packet for the checked source would hold now.
fn screen_statement(f: &Fixture) -> Result<String> {
    let (attempt, tree): (i64, String) = f.db()?.connection().query_row(
        "SELECT attempt_id,source_tree FROM attempt_steps WHERE role='check' ORDER BY id DESC",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let packet = Service::open(f.db()?, &f.root, None)?.review_bundle(
        attempt,
        &f.scratch.path().join("state/ktask-dev"),
        &f.scratch.path().join("packet"),
        &tree,
        "",
        true,
    )?;
    let statement = packet
        .lines()
        .find(|line| line.starts_with("Screen evidence:"));
    Ok(statement.ok_or("no screen statement")?.to_owned())
}

fn assert_unavailable(statement: &str) {
    assert!(
        statement.contains("The changed-screens report of the current source tree is unavailable")
            && statement.contains(BLOCKS)
            && !statement.contains("no screen to review"),
        "{statement}"
    );
}

#[test]
fn a_report_over_the_artifact_limit_is_unavailable_not_a_sign_of_no_change() -> Result {
    // The report is over the file limit; the check's empty transcript is not.
    let command = "head -c 2000 /dev/zero | tr '\\000' x > \"$KTASK_ARTIFACT_DIR/changed-screens.txt\"; test -f result.txt";
    let (f, _) = golden_project(command)?;
    // A ceiling at the limit, so that the limit does not grow to take the report.
    f.ok(&[
        "limits",
        "artifact_file_bytes",
        "--value",
        "1024",
        "--ceiling",
        "1024",
    ])?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    for role in ["review", "validation"] {
        assert_unavailable(&prompt(&f, role)?);
    }
    Ok(())
}

#[test]
fn a_missing_or_pruned_report_blocks_acceptance() -> Result {
    let command = format!(
        "printf %s '{REPORT}' > \"$KTASK_ARTIFACT_DIR/changed-screens.txt\"; test -f result.txt"
    );
    let (f, _) = golden_project(&command)?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    assert!(screen_statement(&f)?.contains("is the only screen evidence"));
    let db = f.db()?;
    let relative: String = db.connection().query_row(
        "SELECT relative_path FROM artifacts WHERE name='changed-screens.txt'",
        [],
        |r| r.get(0),
    )?;
    let report = f.scratch.path().join("state/ktask-dev").join(relative);
    let kept = f.scratch.path().join("kept-report");
    fs::rename(&report, &kept)?;
    assert_unavailable(&screen_statement(&f)?);
    fs::rename(&kept, &report)?;
    db.connection().execute(
        "UPDATE artifacts SET pruned_at_ms=created_at_ms WHERE name='changed-screens.txt'",
        [],
    )?;
    assert_unavailable(&screen_statement(&f)?);
    Ok(())
}

#[test]
fn no_change_is_said_only_when_the_checks_evidence_was_collected() -> Result {
    let (f, _) = golden_project("test -f result.txt")?;
    f.ok(&["run", "--one"])?;
    f.state("succeeded")?;
    assert!(screen_statement(&f)?.contains("there is no screen to review"));
    let db = f.db()?;
    for state in ["truncated", "collecting"] {
        db.connection()
            .execute("UPDATE evidence_collections SET state=?1", [state])?;
        assert_unavailable(&screen_statement(&f)?);
    }
    db.connection()
        .execute("DELETE FROM evidence_collections", [])?;
    assert_unavailable(&screen_statement(&f)?);
    Ok(())
}
