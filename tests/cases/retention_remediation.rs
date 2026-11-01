use super::*;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::Command;

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");
const TRANSCRIPT: &str = "retention must preserve this judge transcript";
/// What an earlier owner wrote into the guardian lock before spawning its guardian.
const GUARDIAN_RESERVED: &[u8] = b"pending guardian\n";

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
    let run = writer.begin_run(target, token, "fixture-base", false)?;
    writer.output(&run, "stderr", "previously closed output")?;
    writer.finish_run(&run, "failed", Some("check"), "Fixture failure", None)?;
    let target = super::target(writer, id)?;
    writer.mark_done(target, "Reviewed fixture")?;
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

    fn close(&self, writer: &mut Service) -> Result {
        let target = target(writer, self.failed)?;
        writer.mark_done(target, "Closed while a pin is left")?;
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

fn pin_count(root: &Path) -> Result<i64> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    Ok(db
        .connection()
        .query_row("SELECT count(*) FROM evidence_pins", [], |row| row.get(0))?)
}

fn no_pins(root: &Path) -> Result {
    assert_eq!(pin_count(root)?, 0, "retention left a durable pin behind");
    let directory = root.join("state/ktask-dev/evidence-pins");
    if directory.exists() {
        assert_eq!(
            fs::read_dir(directory)?.count(),
            0,
            "retention left lock files behind"
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

/// The files and row an earlier ktask's interactive judge left while it read evidence:
/// the guard directory with its ownership record, the owner and guardian locks, and the
/// pin. Holding a lock stands in for that judge's process still running.
struct LeftoverPin {
    owner: File,
    guardian: File,
}

fn identity(path: &Path) -> Result<String> {
    let metadata = fs::metadata(path)?;
    Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
}

impl LeftoverPin {
    fn new(root: &Path) -> Result<Self> {
        let state = root.join("state/ktask-dev");
        let directory = state.join("evidence-pins");
        fs::create_dir(&directory)?;
        fs::write(state.join("evidence-pins.identity"), identity(&directory)?)?;
        let id = "0123456789abcdef0123456789abcdef";
        let lock = |role: &str| -> Result<File> {
            Ok(fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(directory.join(format!("{id}.{role}")))?)
        };
        let (owner, guardian) = (lock("owner")?, lock("guardian")?);
        let project = service(root)?.project_id;
        Database::open_in(&state)?.connection().execute(
            "INSERT INTO evidence_pins(id,project_id,directory_identity,owner_identity,guardian_identity) VALUES (?1,?2,?3,?4,?5)",
            rusqlite::params![
                id,
                project,
                identity(&directory)?,
                identity(&directory.join(format!("{id}.owner")))?,
                identity(&directory.join(format!("{id}.guardian")))?
            ],
        )?;
        Ok(Self { owner, guardian })
    }

    /// The pin as a judge that is still reading left it: its owner lock is held.
    fn live(root: &Path) -> Result<Self> {
        let pin = Self::new(root)?;
        pin.owner.lock()?;
        Ok(pin)
    }
}

#[test]
fn retention_waits_for_a_live_leftover_pin_and_is_instance_scoped() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let evidence = Evidence::new(root)?;
    let mut writer = service(root)?;
    let stale = writer.storage_preview()?;
    assert_eq!(stale.candidates.len(), 1);
    let pin = LeftoverPin::live(root)?;
    assert_eq!(pin_count(root)?, 1);
    // Reject a preview captured before the pin even though its closed task did not change.
    assert!(writer.cleanup_storage(&stale).is_err());
    evidence.close(&mut writer)?;
    blocked(root, &evidence)?;

    // A development pin must not defer retention in the production instance.
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

    drop(pin);
    evidence.preserved(root)?;
    evidence.cleanup(root)?;
    Ok(())
}

#[test]
fn retention_revokes_reservation_left_by_owner_that_died_before_guardian_claim() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let evidence = Evidence::new(root)?;
    let pin = LeftoverPin::live(root)?;
    // The owner persisted its reservation and had not yet spawned a guardian.
    (&pin.guardian).write_all(GUARDIAN_RESERVED)?;
    blocked(root, &evidence)?;
    evidence.close(&mut service(root)?)?;
    // A live guardian that already claimed keeps the pin even after its owner is gone.
    pin.guardian.lock()?;
    pin.owner.unlock()?;
    blocked(root, &evidence)?;
    assert_eq!(pin_count(root)?, 1);
    pin.guardian.unlock()?;
    // The owner died before any guardian claimed: recovery revokes and retires the pin.
    evidence.cleanup(root)?;
    let mut marker = Vec::new();
    (&pin.guardian).seek(SeekFrom::Start(0))?;
    (&pin.guardian).read_to_end(&mut marker)?;
    assert!(
        marker.is_empty(),
        "a delayed guardian could still claim the revoked reservation"
    );
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
fn retention_does_not_retire_live_pin_when_pin_directory_is_replaced() -> Result {
    use std::os::unix::fs::symlink;
    for replacement in ["empty", "symlink"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        let state = root.join("state/ktask-dev");
        let original_db = Database::open_in(&state)?;
        let pin = LeftoverPin::live(root)?;
        evidence.close(&mut writer)?;
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
        drop(pin);
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
        let pin = LeftoverPin::live(root)?;
        evidence.close(&mut writer)?;
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
        drop(pin);
        evidence.cleanup(root)?;
    }
    Ok(())
}

#[test]
fn retention_preserves_substituted_instance_and_project_state_trees() -> Result {
    use std::os::unix::fs::symlink;
    for replacement in ["instance", "project"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let evidence = Evidence::new(root)?;
        let mut writer = service(root)?;
        let stale = writer.storage_preview()?;
        let state = root.join("state/ktask-dev");
        let original_db = Database::open_in(&state)?;
        let pin = LeftoverPin::live(root)?;
        evidence.close(&mut writer)?;
        let replacement_state = if replacement == "instance" {
            root.join("state/ktask")
        } else {
            root.join("project-state")
        };
        fs::create_dir_all(replacement_state.join("evidence-pins"))?;
        // Include the same pin ID in a different database and idle decoy locks.
        // Recovery that followed the pathname would delete this unrelated row and its files.
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
        drop(pin);
        if replacement == "instance" {
            fs::rename(&state, &replacement_state)?;
        }
        swapped.restore()?;
        evidence.preserved(root)?;
        evidence.cleanup(root)?;
    }
    Ok(())
}
