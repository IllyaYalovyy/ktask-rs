use super::*;
use ktask_rs_mk3::application::evidence_pin::GUARDIAN_RESERVED;
use nix::{
    sys::signal::{Signal, kill},
    unistd::Pid,
};
use std::io::Read;
use std::{
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");
const TRANSCRIPT: &str = "retention must preserve this judge transcript";

fn command(binary: &str, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .current_dir(project(root))
        .arg("--json");
    command
}

struct Running {
    child: Option<Child>,
    root: PathBuf,
}
impl Running {
    fn start(root: &Path, id: i64) -> Result<Self> {
        Ok(Self {
            child: Some(
                command(DEV, root)
                    .args(["remediate", &id.to_string()])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()?,
            ),
            root: root.to_owned(),
        })
    }

    fn finish(&mut self) -> Result<Output> {
        let child = self.child.as_mut().unwrap();
        support::eventually("CLI judge finishes", || Ok(child.try_wait()?.map(drop)))?;
        Ok(self.child.take().unwrap().wait_with_output()?)
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let children = format!("/proc/{0}/task/{0}/children", child.id());
            let guardians = fs::read_to_string(children).unwrap_or_default();
            let _ = child.kill();
            let _ = child.wait();
            let deadline = Instant::now() + support::PATIENCE;
            for pid in guardians.split_whitespace() {
                while process_running(pid) && Instant::now() < deadline {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            // The lifeline closes asynchronously. Wait for the owned provider tree
            // before the surrounding managed sandbox can remove its scratch.
            if let Ok(calls) = fs::read_dir(self.root.join("provider-calls")) {
                for call in calls.flatten() {
                    let _ = reaped(&call.path());
                }
            }
        }
    }
}

fn transcript(root: &Path, instance: &str, attempt: i64) -> Result<PathBuf> {
    let state = root.join("state").join(instance);
    let relative = format!("evidence/attempt-{attempt}/step-987.log");
    let path = state.join(&relative);
    fs::create_dir_all(path.parent().unwrap())?;
    fs::write(&path, TRANSCRIPT)?;
    let db = Database::open_in(&state)?;
    db.connection().execute(
        "INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'retention judge transcript','text/plain',?2,?3,1)",
        rusqlite::params![attempt, relative, TRANSCRIPT.len() as i64],
    )?;
    Ok(path)
}

fn closed_attempt(writer: &mut Service, token: &str) -> Result<(i64, i64)> {
    let mut definition = definition("Previously closed task");
    for model in &mut definition.models {
        *model = Some(ModelConfig {
            provider: "codex".into(),
            model: "fixture-model".into(),
            effort: None,
        });
    }
    let id = writer.create(&[definition], None, true)?[0];
    let target = target(writer, id)?;
    let run = writer.begin_run(target, token, "fixture-base")?;
    writer.output(&run, "stderr", "previously closed output")?;
    writer.finish_run(&run, "failed", Some("check"), "Fixture failure", None)?;
    let target = super::target(writer, id)?;
    writer.resolve(target, Resolution::FailedSkipped, "Reviewed fixture")?;
    Ok((id, run.attempt))
}

struct Evidence {
    failed: i64,
    attempts: [i64; 2],
    transcripts: [PathBuf; 2],
}

impl Evidence {
    fn new(root: &Path) -> Result<Self> {
        support::empty_repository(&project(root))?;
        let mut writer = service(root)?;
        let (_, closed) = closed_attempt(&mut writer, "retention-closed-token")?;
        let policy = writer.retention_policy()?;
        writer.set_retention(policy, 0)?;
        drop(writer);
        let (failed, attempt) = failed_task(root)?;
        Ok(Self {
            failed,
            attempts: [closed, attempt],
            transcripts: [
                transcript(root, "ktask-dev", closed)?,
                transcript(root, "ktask-dev", attempt)?,
            ],
        })
    }

    fn resolve(&self, writer: &mut Service) -> Result {
        let target = target(writer, self.failed)?;
        writer.resolve(
            target,
            Resolution::FailedSkipped,
            "Resolved while judges read",
        )?;
        Ok(())
    }

    fn preserved(&self, root: &Path) -> Result {
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        for (attempt, path) in self.attempts.iter().zip(&self.transcripts) {
            assert_eq!(fs::read_to_string(path)?, TRANSCRIPT);
            let rows: i64 = db.connection().query_row(
                "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
                [attempt],
                |row| row.get(0),
            )?;
            assert!(rows > 0, "judge output for attempt {attempt} was pruned");
            let pruned: i64 = db.connection().query_row(
                "SELECT count(*) FROM artifacts WHERE attempt_id=?1 AND (pruned_at_ms IS NOT NULL OR pruning_requested_at_ms IS NOT NULL)",
                [attempt],
                |row| row.get(0),
            )?;
            assert_eq!(pruned, 0, "active judge evidence acquired a pruning intent");
        }
        Ok(())
    }

    fn cleanup(&self, root: &Path) -> Result {
        let mut writer = service(root)?;
        let preview = writer.storage_preview()?;
        assert!(!preview.deferred_for_live_work);
        assert_eq!(preview.candidates.len(), 2);
        no_pins(root)?;
        writer.cleanup_storage(&preview)?;
        for path in &self.transcripts {
            assert!(!path.exists(), "finished judges left evidence pinned");
        }
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        for attempt in self.attempts {
            let rows: i64 = db.connection().query_row(
                "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
                [attempt],
                |row| row.get(0),
            )?;
            assert_eq!(rows, 0);
        }
        Ok(())
    }
}

fn held_provider(root: &Path) -> Result {
    provider_fixture(
        root,
        serde_json::json!({"action":"repair","plan":"Repair after inspecting retained evidence","reason":"The original goal remains correct"}),
        false,
    )?;
    fs::create_dir(root.join("provider-calls"))?;
    let quoted = format!("'{}'", root.to_string_lossy().replace('\'', "'\\''"));
    fs::write(
        root.join("bin/codex"),
        format!(
            "#!/bin/sh\nset -eu\nfixture={quoted}\ncall=\"$fixture/provider-calls/$$\"\nmkdir \"$call\"\nprintf '%s\\n' \"$$\" > \"$call/provider-pid\"\ncat > \"$call/prompt\"\nsleep 60 &\nprintf '%s\\n' \"$!\" > \"$call/descendant-pid\"\nwhile [ ! -f \"$call/release\" ]; do sleep 0.02; done\nif [ -f \"$fixture/provider-fail\" ]; then echo held-provider-failed >&2; exit 9; fi\ncat \"$fixture/provider-events.jsonl\"\n"
        ),
    )?;
    Ok(())
}

fn calls(root: &Path, count: usize) -> Result<Vec<PathBuf>> {
    support::eventually(&format!("{count} held providers"), || {
        let mut calls = fs::read_dir(root.join("provider-calls"))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<Vec<_>>>()?;
        calls.retain(|path| path.join("descendant-pid").exists());
        calls.sort();
        Ok((calls.len() == count).then_some(calls))
    })
}

fn reaped(call: &Path) -> Result {
    for file in ["provider-pid", "descendant-pid"] {
        let pid = fs::read_to_string(call.join(file))?;
        let process = Path::new("/proc").join(pid.trim());
        support::eventually(&format!("judge reaps {file} {pid}"), || {
            Ok((!process.exists()).then_some(()))
        })?;
    }
    Ok(())
}

fn process_running(pid: &str) -> bool {
    fs::read_to_string(format!("/proc/{pid}/status")).is_ok_and(|status| {
        !status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
    })
}

fn pin_count(root: &Path) -> Result<i64> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    Ok(db
        .connection()
        .query_row("SELECT count(*) FROM evidence_pins", [], |row| row.get(0))?)
}

fn no_pins(root: &Path) -> Result {
    assert_eq!(pin_count(root)?, 0, "judge left a durable pin behind");
    let directory = root.join("state/ktask-dev/evidence-pins");
    if directory.exists() {
        assert_eq!(
            fs::read_dir(directory)?.count(),
            0,
            "judge left lock files behind"
        );
    }
    Ok(())
}

fn blocked(root: &Path, evidence: &Evidence) -> Result {
    let mut writer = service(root)?;
    let preview = writer.storage_preview()?;
    assert!(preview.deferred_for_live_work);
    assert!(preview.candidates.is_empty());
    assert!(writer.cleanup_storage(&preview).is_err());
    let output = command(DEV, root).args(["storage", "--apply"]).output()?;
    assert_eq!(output.status.code(), Some(4), "{output:?}");
    let error: serde_json::Value = serde_json::from_slice(&output.stderr)?;
    assert_eq!(error["error"]["code"], "conflict");
    evidence.preserved(root)
}

#[test]
fn retention_waits_for_overlapping_cli_and_tui_judges_and_is_instance_scoped() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        assert_eq!(stale.candidates.len(), 1);
        held_provider(root)?;
        let mut cli = Running::start(root, evidence.failed)?;
        let first = calls(root, 1)?.remove(0);
        let mut t = Terminal::open_project(DEV, root, &project(root), 110, 42)?;
        t.screen("failed current before held judge", |s| {
            s.contains("Failed current")
        })?;
        if mouse {
            t.click("d Remediate")?;
        } else {
            t.send("d")?;
        }
        println!(
            "RETENTION HELD TUI mouse={mouse}\n{}",
            t.screen("held TUI judge", |s| s
                .contains("Requesting failure judgment"))?
        );
        let second = calls(root, 2)?
            .into_iter()
            .find(|path| path != &first)
            .unwrap();
        assert_eq!(pin_count(root)?, 2);
        for call in [&first, &second] {
            assert!(fs::read_to_string(call.join("prompt"))?.contains(TRANSCRIPT));
        }
        // Reject a preview captured before the pin even though its closed task did not change.
        assert!(writer.cleanup_storage(&stale).is_err());
        evidence.resolve(&mut writer)?;
        blocked(root, &evidence)?;

        // Development's judges must not defer retention in the production instance.
        let mut production = Service::open(
            Database::open_in(&root.join("state/ktask"))?,
            &project(root),
            None,
        )?;
        let (_, attempt) = closed_attempt(&mut production, "production-retention-token")?;
        let production_transcript = transcript(root, "ktask", attempt)?;
        let policy = production.retention_policy()?;
        production.set_retention(policy, 0)?;
        drop(production);
        let output = command(PROD, root).args(["storage", "--apply"]).output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(!production_transcript.exists());
        blocked(root, &evidence)?;

        fs::write(first.join("release"), "")?;
        let output = cli.finish()?;
        assert!(output.status.success(), "{output:?}");
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(reply["data"]["judgment"]["action"], "repair");
        reaped(&first)?;
        assert_eq!(pin_count(root)?, 1);
        blocked(root, &evidence)?;
        if mouse {
            t.click("Cancel")?;
        } else {
            t.send("\x1b")?;
        }
        println!(
            "RETENTION LAST JUDGE CANCELLED mouse={mouse}\n{}",
            t.screen("held judge cancelled", |s| !s
                .contains("Requesting failure judgment"))?
        );
        reaped(&second)?;
        quit(&mut t)?;
        evidence.preserved(root)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}

#[test]
fn retention_releases_cli_and_tui_judge_pins_after_provider_errors_and_cancellation() -> Result {
    for (input, mouse) in [
        ("cli", false),
        ("cancel", false),
        ("tui", false),
        ("tui", true),
    ] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        held_provider(root)?;
        fs::write(root.join("provider-fail"), "")?;
        let mut cli = None;
        let mut terminal = None;
        if input != "tui" {
            cli = Some(Running::start(root, evidence.failed)?);
        } else {
            let mut t = Terminal::open_project(DEV, root, &project(root), 110, 42)?;
            t.screen("failed task before provider error", |s| {
                s.contains("Failed current")
            })?;
            if mouse {
                t.click("d Remediate")?;
            } else {
                t.send("d")?;
            }
            println!(
                "RETENTION ERROR PENDING mouse={mouse}\n{}",
                t.screen("provider error pending", |s| s
                    .contains("Requesting failure judgment"))?
            );
            terminal = Some(t);
        }
        let call = calls(root, 1)?.remove(0);
        evidence.resolve(&mut service(root)?)?;
        blocked(root, &evidence)?;
        if input == "cancel" {
            kill(
                Pid::from_raw(cli.as_ref().unwrap().child.as_ref().unwrap().id() as i32),
                Signal::SIGTERM,
            )?;
        } else {
            fs::write(call.join("release"), "")?;
        }
        if let Some(cli) = &mut cli {
            let output = cli.finish()?;
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let expected = if input == "cancel" {
                "cancelled"
            } else {
                "held-provider-failed"
            };
            assert!(
                String::from_utf8_lossy(&output.stderr).contains(expected),
                "{output:?}"
            );
        }
        if let Some(t) = &mut terminal {
            println!(
                "RETENTION PROVIDER ERROR mouse={mouse}\n{}",
                t.screen("provider failure rendered", |s| s
                    .contains("held-provider-failed"))?
            );
            quit(t)?;
        }
        reaped(&call)?;
        evidence.preserved(root)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}

struct PausedGuardian(Pid);
impl Drop for PausedGuardian {
    fn drop(&mut self) {
        let _ = kill(self.0, Signal::SIGCONT);
    }
}

#[test]
fn retention_recovers_killed_cli_owner_only_after_its_guardian_reaps_judge() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let evidence = Evidence::new(root)?;
    held_provider(root)?;
    let mut cli = Running::start(root, evidence.failed)?;
    let call = calls(root, 1)?.remove(0);
    let provider = fs::read_to_string(call.join("provider-pid"))?;
    // The provider runs inside the read-only sandbox, so its guardian is an ancestor.
    let mut guardian = provider.trim().parse::<i32>()?;
    while !fs::read_to_string(format!("/proc/{guardian}/cmdline"))?.contains("__worker") {
        guardian = fs::read_to_string(format!("/proc/{guardian}/status"))?
            .lines()
            .find_map(|line| line.strip_prefix("PPid:"))
            .unwrap()
            .trim()
            .parse()?;
        assert!(guardian > 1, "provider has no guardian ancestor");
    }
    let guardian = PausedGuardian(Pid::from_raw(guardian));
    kill(guardian.0, Signal::SIGSTOP)?;
    support::eventually("guardian stopped", || {
        Ok(fs::read_to_string(format!("/proc/{}/status", guardian.0))?
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('T'))
            .then_some(()))
    })?;
    evidence.resolve(&mut service(root)?)?;
    cli.child.as_mut().unwrap().kill()?;
    let output = cli.finish()?;
    assert!(!output.status.success());
    assert!(Path::new("/proc").join(provider.trim()).exists());
    blocked(root, &evidence)?;
    drop(guardian);
    reaped(&call)?;
    evidence.preserved(root)?;
    // Opening the service/preview reconciles the abandoned pin only after cleanup proof.
    support::eventually("guardian releases the cleanup marker", || {
        Ok((!service(root)?.storage_preview()?.deferred_for_live_work).then_some(()))
    })?;
    evidence.cleanup(root)?;
    Ok(())
}

#[test]
fn retention_revokes_reservation_left_by_owner_that_died_before_guardian_claim() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let evidence = Evidence::new(root)?;
    let mut writer = service(root)?;
    let target = target(&mut writer, evidence.failed)?;
    let pin = writer.begin_remediation_evidence(target)?;
    let id: String = Database::open_in(&root.join("state/ktask-dev"))?
        .connection()
        .query_row("SELECT id FROM evidence_pins", [], |row| row.get(0))?;
    let path = root.join(format!("state/ktask-dev/evidence-pins/{id}.guardian"));
    // Persist the reservation exactly as the owner does before spawning a guardian.
    fs::write(pin.guardian_lock(), GUARDIAN_RESERVED)?;
    let delayed = fs::OpenOptions::new().read(true).write(true).open(&path)?;
    blocked(root, &evidence)?;
    evidence.resolve(&mut service(root)?)?;
    // A live guardian that already claimed keeps the pin even after its owner is gone.
    delayed.lock()?;
    drop(pin);
    blocked(root, &evidence)?;
    assert_eq!(pin_count(root)?, 1);
    delayed.unlock()?;
    fs::write(&path, GUARDIAN_RESERVED)?;
    // The owner died before any guardian claimed: recovery revokes and retires the pin.
    evidence.cleanup(root)?;
    let mut marker = Vec::new();
    (&delayed).read_to_end(&mut marker)?;
    assert!(
        marker.is_empty(),
        "a delayed guardian could still claim the revoked reservation"
    );
    Ok(())
}

#[test]
fn retention_releases_cli_judge_pins_for_missing_launch_and_malformed_responses() -> Result {
    for failure in ["missing", "launch", "malformed"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        provider_fixture(root, serde_json::Value::Null, true)?;
        match failure {
            "missing" => fs::remove_file(root.join("bin/codex"))?,
            "launch" => fs::write(root.join("bin/codex"), "invalid executable format\n")?,
            _ => (),
        }
        let mut cli = command(DEV, root);
        if failure == "missing" {
            cli.env("PATH", root.join("bin"));
        }
        let output = cli
            .args(["remediate", &evidence.failed.to_string()])
            .output()?;
        assert_eq!(output.status.code(), Some(4), "{failure}: {output:?}");
        if failure == "malformed" {
            assert!(String::from_utf8_lossy(&output.stderr).contains("Invalid Codex event"));
        }
        no_pins(root)?;
        evidence.preserved(root)?;
        evidence.resolve(&mut service(root)?)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}

#[test]
fn remediation_evidence_pin_transaction_rejects_stale_version_and_resolved_current() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let evidence = Evidence::new(root)?;
    let mut writer = service(root)?;
    let original = target(&mut writer, evidence.failed)?;
    let stale = VersionedTask {
        id: original.id,
        version: original.version - 1,
    };
    assert!(writer.begin_remediation_evidence(stale).is_err());
    no_pins(root)?;
    let pin = writer.begin_remediation_evidence(original)?;
    assert_eq!(pin_count(root)?, 1);
    blocked(root, &evidence)?;
    drop(pin);
    no_pins(root)?;
    evidence.resolve(&mut service(root)?)?;
    assert!(writer.begin_remediation_evidence(original).is_err());
    no_pins(root)?;
    evidence.cleanup(root)?;
    Ok(())
}

/// Restore a deliberately replaced test path before the worker guard is dropped,
/// including assertion failures. Every path involved belongs to this sandbox.
struct ReplacedPath {
    original: PathBuf,
    parked: PathBuf,
    active: bool,
}

impl ReplacedPath {
    fn park(original: &Path, parked: &Path) -> Result<Self> {
        fs::rename(original, parked)?;
        Ok(Self {
            original: original.to_owned(),
            parked: parked.to_owned(),
            active: true,
        })
    }

    fn restore(&mut self) -> std::io::Result<()> {
        if !self.active {
            return Ok(());
        }
        match fs::symlink_metadata(&self.original) {
            Ok(metadata) if metadata.is_dir() => fs::remove_dir_all(&self.original)?,
            Ok(_) => fs::remove_file(&self.original)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error),
        }
        fs::rename(&self.parked, &self.original)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for ReplacedPath {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

fn fail_closed(
    writer: &mut Service,
    stale: &ktask_rs_mk3::application::retention::Preview,
) -> Result {
    if let Ok(preview) = writer.storage_preview() {
        assert!(
            preview.deferred_for_live_work,
            "path substitution retired a live pin"
        );
        assert!(preview.candidates.is_empty());
    }
    assert!(writer.cleanup_storage(stale).is_err());
    Ok(())
}

fn original_evidence_is_protected(evidence: &Evidence, db: &Database, state: &Path) -> Result {
    let live: i64 = db
        .connection()
        .query_row("SELECT count(*) FROM evidence_pins", [], |row| row.get(0))?;
    assert_eq!(live, 1, "path substitution deleted the original live pin");
    for attempt in evidence.attempts {
        assert_eq!(
            fs::read_to_string(state.join(format!("evidence/attempt-{attempt}/step-987.log")))?,
            TRANSCRIPT
        );
        let output: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_output WHERE attempt_id=?1",
            [attempt],
            |row| row.get(0),
        )?;
        assert!(output > 0, "path substitution pruned original output");
        let marked: i64 = db.connection().query_row("SELECT count(*) FROM artifacts WHERE attempt_id=?1 AND (pruned_at_ms IS NOT NULL OR pruning_requested_at_ms IS NOT NULL)", [attempt], |row| row.get(0))?;
        assert_eq!(
            marked, 0,
            "path substitution marked original artifacts for pruning"
        );
    }
    Ok(())
}

fn tree_contents(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    fn visit(root: &Path, directory: &Path, entries: &mut Vec<(PathBuf, Vec<u8>)>) -> Result {
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path.is_dir() {
                visit(root, &path, entries)?;
            } else {
                entries.push((path.strip_prefix(root)?.to_owned(), fs::read(path)?));
            }
        }
        Ok(())
    }
    let mut entries = Vec::new();
    visit(root, root, &mut entries)?;
    entries.sort();
    Ok(entries)
}

#[test]
fn retention_does_not_retire_live_judge_when_pin_directory_is_replaced() -> Result {
    use std::os::unix::fs::symlink;
    for replacement in ["empty", "symlink"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        let state = root.join("state/ktask-dev");
        let original_db = Database::open_in(&state)?;
        held_provider(root)?;
        let mut cli = Running::start(root, evidence.failed)?;
        let call = calls(root, 1)?.remove(0);
        evidence.resolve(&mut writer)?;
        let directory = state.join("evidence-pins");
        let mut swapped = ReplacedPath::park(&directory, &root.join("original-pin-directory"))?;
        let replacement_directory = root.join("unrelated-pin-directory");
        fs::create_dir(&replacement_directory)?;
        fs::write(replacement_directory.join("sentinel"), "unrelated files")?;
        if replacement == "empty" {
            fs::create_dir(&directory)?;
        } else {
            symlink(&replacement_directory, &directory)?;
        }
        let live_id: String =
            original_db
                .connection()
                .query_row("SELECT id FROM evidence_pins", [], |row| row.get(0))?;
        let unrelated_id = if live_id == "00000000000000000000000000000000" {
            "11111111111111111111111111111111"
        } else {
            "00000000000000000000000000000000"
        };
        for role in ["owner", "guardian"] {
            fs::write(directory.join(format!("{unrelated_id}.{role}")), "")?;
        }
        if replacement == "empty" {
            fs::write(directory.join("sentinel"), "unrelated files")?;
        }
        let visible_before = tree_contents(&directory)?;
        let replacement_before = tree_contents(&replacement_directory)?;
        fail_closed(&mut writer, &stale)?;
        original_evidence_is_protected(&evidence, &original_db, &state)?;
        assert_eq!(
            tree_contents(&directory)?,
            visible_before,
            "retention changed unrelated orphan locks in a replacement directory"
        );
        assert_eq!(tree_contents(&replacement_directory)?, replacement_before);
        assert_eq!(
            fs::read_dir(&directory)?.count(),
            3,
            "retention changed replacement pin files"
        );
        swapped.restore()?;
        blocked(root, &evidence)?;
        fs::write(call.join("release"), "")?;
        let output = cli.finish()?;
        assert!(output.status.success(), "{output:?}");
        reaped(&call)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}

#[test]
fn retention_never_recreates_missing_or_substituted_live_pin_locks() -> Result {
    for replacement in [
        "owner-missing",
        "guardian-missing",
        "owner-empty",
        "guardian-empty",
        "both-empty",
    ] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        let state = root.join("state/ktask-dev");
        let original_db = Database::open_in(&state)?;
        held_provider(root)?;
        let mut cli = Running::start(root, evidence.failed)?;
        let call = calls(root, 1)?.remove(0);
        evidence.resolve(&mut writer)?;
        let directory = state.join("evidence-pins");
        let id: String =
            original_db
                .connection()
                .query_row("SELECT id FROM evidence_pins", [], |row| row.get(0))?;
        let mut swaps = Vec::new();
        for role in ["owner", "guardian"] {
            if !replacement.starts_with(role) && replacement != "both-empty" {
                continue;
            }
            let path = directory.join(format!("{id}.{role}"));
            swaps.push(ReplacedPath::park(
                &path,
                &root.join(format!("original-{role}-lock")),
            )?);
            if replacement.ends_with("empty") {
                fs::write(&path, "")?;
            }
        }
        fail_closed(&mut writer, &stale)?;
        original_evidence_is_protected(&evidence, &original_db, &state)?;
        for swapped in &swaps {
            if replacement.ends_with("missing") {
                assert!(
                    !swapped.original.exists(),
                    "retention recreated a missing lock as idle proof"
                );
            } else {
                assert_eq!(
                    fs::read(&swapped.original)?,
                    b"",
                    "retention modified a substituted lock"
                );
            }
        }
        for swapped in &mut swaps {
            swapped.restore()?;
        }
        blocked(root, &evidence)?;
        fs::write(call.join("release"), "")?;
        let output = cli.finish()?;
        assert!(output.status.success(), "{output:?}");
        reaped(&call)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}

#[test]
fn retention_and_pin_drop_preserve_substituted_instance_and_project_state_trees() -> Result {
    use std::os::unix::fs::symlink;
    for replacement in ["instance", "project"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        let state = root.join("state/ktask-dev");
        let original_db = Database::open_in(&state)?;
        held_provider(root)?;
        let mut cli = Running::start(root, evidence.failed)?;
        let call = calls(root, 1)?.remove(0);
        evidence.resolve(&mut writer)?;
        let replacement_state = if replacement == "instance" {
            root.join("state/ktask")
        } else {
            root.join("project-state")
        };
        fs::create_dir_all(replacement_state.join("evidence-pins"))?;
        // Include the same pin ID in a different database and idle decoy locks.
        // A pathname-based Drop would delete this unrelated row and its files.
        original_db.connection().execute(
            "VACUUM INTO ?1",
            [replacement_state.join("ktask.sqlite3").to_str().unwrap()],
        )?;
        for entry in fs::read_dir(state.join("evidence-pins"))? {
            fs::write(
                replacement_state
                    .join("evidence-pins")
                    .join(entry?.file_name()),
                "",
            )?;
        }
        for attempt in evidence.attempts {
            let directory = replacement_state.join(format!("evidence/attempt-{attempt}"));
            fs::create_dir_all(&directory)?;
            fs::write(
                directory.join("step-987.log"),
                "unrelated replacement transcript",
            )?;
        }
        fs::write(
            replacement_state.join("sentinel"),
            "unrelated replacement state",
        )?;
        let before = tree_contents(&replacement_state)?;
        let parked_state = root.join("original-state-directory");
        let mut swapped = ReplacedPath::park(&state, &parked_state)?;
        let visible_replacement = if replacement == "instance" {
            fs::rename(&replacement_state, &state)?;
            &state
        } else {
            symlink(&replacement_state, &state)?;
            &replacement_state
        };
        fail_closed(&mut writer, &stale)?;
        original_evidence_is_protected(&evidence, &original_db, &parked_state)?;
        assert_eq!(
            tree_contents(visible_replacement)?,
            before,
            "retention touched replacement state"
        );
        fs::write(call.join("release"), "")?;
        let output = cli.finish()?;
        assert!(output.status.success(), "{output:?}");
        reaped(&call)?;
        assert_eq!(
            tree_contents(visible_replacement)?,
            before,
            "pin Drop touched replacement state"
        );
        if replacement == "instance" {
            fs::rename(&state, &replacement_state)?;
        }
        swapped.restore()?;
        evidence.preserved(root)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}
