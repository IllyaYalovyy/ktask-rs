//! Opt-in, fixed-recipe Android provisioning. No worker-supplied command is executed.
use super::{ActorKind, Service, now_ms, require, tools::Registration};
use crate::{
    instance::Paths,
    scratch::Scratch,
    storage::{Error, Result},
};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

pub const RECIPE: &str = "android-36-google-apis-x86_64";
const COMPONENTS: [&str; 4] = [
    "platform-tools",
    "emulator",
    "platforms;android-36",
    "system-images;android-36;google_apis;x86_64",
];
// Reviewed Linux x86_64 Android 36 package set. The modern Android CLI can
// request these revisions directly; sdkmanager is checked after installation.
const REVISIONS: [&str; 4] = ["37.0.1", "37.2.13", "2", "7"];
/// SHA-1 of the android-sdk-license text used by these exact stable packages in
/// Google's repository2-3.xml and sys-img/google_apis/sys-img2-1.xml. The Android
/// SDK manager stores this hash on a line in licenses/android-sdk-license.
pub const ANDROID_SDK_LICENSE_SHA1: &str = "efa68a6b3c661d18699d5c026771d5911cdc2f83";
const AVD_TEMPLATE: &str = r#"{"name":"ktask-api36-google-apis-x86_64","profile":"medium_phone","system_image":"system-images;android-36;google_apis;x86_64","api":36,"abi":"x86_64","tag":"google_apis"}"#;

pub(super) fn avd_template() -> &'static str {
    AVD_TEMPLATE
}
const MAX_STAGE_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 12 * 1024 * 1024 * 1024;
const MAX_DOWNLOAD_ENTRIES: u64 = 100_000;
const MAX_RETAINED_BYTES: u64 = 32 * 1024 * 1024 * 1024;
const MIN_FREE_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_STAGE_ENTRIES: u64 = 300_000;
const MAX_DURATION: Duration = Duration::from_secs(30 * 60);
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(0);

/// Data-only recipe contract. Adapters implement fixed argv and package probes;
/// requirements and callers can select an ID, never an executable or shell body.
struct RecipeContract {
    id: &'static str,
    packages: &'static [&'static str],
    repository: &'static str,
}

const ANDROID_RECIPE: RecipeContract = RecipeContract {
    id: RECIPE,
    packages: &COMPONENTS,
    repository: "https://dl.google.com/android/repository (stable channel via registered Android CLI/sdkmanager)",
};

#[derive(Clone, Copy)]
struct Limits {
    stage_bytes: u64,
    download_bytes: u64,
    download_entries: u64,
    retained_bytes: u64,
    min_free_bytes: u64,
    stage_entries: u64,
    duration: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            stage_bytes: MAX_STAGE_BYTES,
            download_bytes: MAX_DOWNLOAD_BYTES,
            download_entries: MAX_DOWNLOAD_ENTRIES,
            retained_bytes: MAX_RETAINED_BYTES,
            min_free_bytes: MIN_FREE_BYTES,
            stage_entries: MAX_STAGE_ENTRIES,
            duration: MAX_DURATION,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub recipe: String,
    pub source: PathBuf,
    pub source_version: String,
    pub packages: Vec<String>,
    pub package_revisions: Vec<String>,
    pub repository: &'static str,
    pub destination: PathBuf,
    pub stage_max_bytes: u64,
    pub download_cache_max_bytes: u64,
    pub download_cache_max_entries: u64,
    pub retained_max_bytes: u64,
    pub min_free_bytes: u64,
    pub timeout_seconds: u64,
    pub enabled: bool,
    pub consented: bool,
    pub license_sha256: Option<String>,
    pub bootstrap_sha256: Option<String>,
    pub issues: Vec<String>,
}

/// The exact dependency wait an installation may release. A new Run changes the
/// session generation even when it returns to the same task and attempt.
#[derive(Clone, Debug)]
pub struct ProvisionTarget {
    pub task: i64,
    pub attempt: Option<i64>,
    pub session_generation: Option<i64>,
    pub mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProvisionStatus {
    pub state: String,
    pub recipe: String,
    #[serde(default)]
    pub operation: String,
    pub stage_bytes: u64,
    pub stage_entries: u64,
    #[serde(default)]
    pub download_cache_bytes: u64,
    #[serde(default)]
    pub download_cache_entries: u64,
    pub message: String,
}

struct RunStatus {
    path: PathBuf,
    cancel: PathBuf,
    status: ProvisionStatus,
    completed: bool,
}

struct InstallerChild(Child);

/// Staging remains owned until the guardian and any orphaned descendants have
/// stopped. A failed recovery deliberately leaves its directory for a later
/// lease holder instead of deleting files beneath a live writer.
struct ProvisionStage {
    scratch: Option<Scratch>,
    guardian_lock: PathBuf,
    guardian_started: bool,
}

impl ProvisionStage {
    fn new(scratch: Scratch, guardian_lock: PathBuf) -> Self {
        Self {
            scratch: Some(scratch),
            guardian_lock,
            guardian_started: false,
        }
    }

    fn path(&self) -> &Path {
        self.scratch.as_ref().expect("owned stage").path()
    }
}

impl Drop for ProvisionStage {
    fn drop(&mut self) {
        if self.guardian_started
            && let Err(error) = crate::runner::process::recover_orphaned(&self.guardian_lock)
        {
            note!("provisioning stage preserved after guardian recovery failure: {error}");
            if let Some(scratch) = self.scratch.take() {
                let _ = scratch.preserve();
            }
        }
    }
}

impl Drop for InstallerChild {
    fn drop(&mut self) {
        let _ = killpg(Pid::from_raw(self.0.id() as i32), Signal::SIGKILL);
        let _ = self.0.wait();
    }
}

impl RunStatus {
    fn publish(&self) -> Result<()> {
        let temporary = self.path.with_extension("json.tmp");
        fs::write(
            &temporary,
            serde_json::to_vec(&self.status).expect("status JSON"),
        )?;
        fs::rename(temporary, &self.path)?;
        Ok(())
    }
}

impl Drop for RunStatus {
    fn drop(&mut self) {
        if !self.completed {
            self.status.state = "failed_or_interrupted".into();
            if self.status.message == "Installing fixed Android packages"
                || self.status.message == "Waiting for the shared Android generation lock"
            {
                self.status.message =
                    "Provisioning stopped; inspect the command error and retry explicitly".into();
            }
            let _ = self.publish();
        }
        let _ = fs::remove_file(&self.cancel);
    }
}

#[derive(Clone, Copy)]
struct Host {
    kvm: bool,
    java: bool,
}

impl Host {
    fn detect() -> Self {
        let mut java = java_probe();
        java.arg("-version");
        Self {
            kvm: OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_ok(),
            java: bounded_status(&mut java, Duration::from_secs(3)),
        }
    }
}

fn java_probe() -> Command {
    // A registered SDK can supply the only PATH entries in a restricted probe.
    // Honor an explicitly bound Java first, then the host's standard Java path.
    Command::new(crate::runner::executable("java").unwrap_or_else(|_| "/usr/bin/java".into()))
}

fn bounded_status(command: &mut Command, timeout: Duration) -> bool {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    let Ok(child) = command.spawn() else {
        return false;
    };
    let mut child = InstallerChild(child);
    let start = Instant::now();
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) => return status.success(),
            Err(_) => return false,
            Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
            Ok(None) => return false,
        }
    }
}

fn recipe(name: &str) -> Result<&'static RecipeContract> {
    require(
        name == ANDROID_RECIPE.id,
        "Unknown reviewed provisioning recipe",
    )?;
    Ok(&ANDROID_RECIPE)
}

fn license_digest(root: &Path) -> Result<String> {
    let licenses = root.join("licenses");
    require(
        licenses.is_dir(),
        "Accept Android SDK licenses in the registered source SDK first",
    )?;
    let mut names = fs::read_dir(&licenses)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    names.sort();
    require(
        names.len() <= 32 && !names.is_empty(),
        "Expected a bounded Android SDK license set",
    )?;
    let mut digest = Sha256::new();
    for path in names {
        let meta = fs::symlink_metadata(&path)?;
        require(
            meta.is_file() && meta.len() <= 16 * 1024,
            "License must be a small regular file",
        )?;
        let name = path.file_name().and_then(|v| v.to_str()).unwrap_or("");
        require(
            !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "Invalid license filename",
        )?;
        digest.update(name.as_bytes());
        digest.update(fs::read(&path)?);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_license_acceptance(root: &Path) -> Result<()> {
    let path = root.join("licenses/android-sdk-license");
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        Error::Conflict(
            "Accept the reviewed Android SDK license with sdkmanager --licenses, then Recheck"
                .into(),
        )
    })?;
    require(
        metadata.is_file() && metadata.len() <= 16 * 1024,
        "Android SDK license acceptance file must be a small regular file",
    )?;
    let bytes = fs::read(path)?;
    let accepted = std::str::from_utf8(&bytes).ok().is_some_and(|text| {
        text.lines()
            .any(|line| line.trim() == ANDROID_SDK_LICENSE_SHA1)
    });
    require(
        accepted,
        "Required Android SDK license hash is not accepted; run sdkmanager --licenses for this SDK, then Recheck",
    )
}

fn bootstrap(root: &Path) -> Option<(PathBuf, &'static str)> {
    let android = root.join("bin/android");
    if android.is_file() {
        return Some((android, "android"));
    }
    let packaged_android = root.join("cmdline-tools/latest/bin/android");
    if packaged_android.is_file() {
        return Some((packaged_android, "android-package"));
    }
    let legacy = root.join("cmdline-tools/latest/bin/sdkmanager");
    if legacy.is_file() {
        return Some((legacy, "sdkmanager"));
    }
    let mut packages = fs::read_dir(root.join("cmdline-tools"))
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    packages.sort();
    packages
        .iter()
        .find_map(|package| {
            let binary = package.join("bin/android");
            binary.is_file().then_some((binary, "android-package"))
        })
        .or_else(|| {
            packages.iter().find_map(|package| {
                let binary = package.join("bin/sdkmanager");
                binary.is_file().then_some((binary, "sdkmanager"))
            })
        })
}

fn bootstrap_digest(root: &Path) -> Result<String> {
    let (binary, _) = bootstrap(root).ok_or_else(|| {
        Error::Conflict("Source SDK needs a compatible Android CLI or sdkmanager".into())
    })?;
    let canonical = binary.canonicalize()?;
    require(
        canonical.starts_with(root.canonicalize()?),
        "Android bootstrap must stay inside the registered SDK",
    )?;
    let metadata = fs::symlink_metadata(&binary)?;
    require(
        metadata.is_file() && metadata.len() <= 128 * 1024 * 1024,
        "Android bootstrap must be a bounded regular executable",
    )?;
    let mut digest = Sha256::new();
    let mut file = File::open(binary)?;
    std::io::copy(&mut file, &mut digest)?;
    Ok(format!("{:x}", digest.finalize()))
}

fn destination(paths: &Paths, source: i64) -> PathBuf {
    paths
        .data
        .join("tools")
        .join(format!("{RECIPE}-source-{source}"))
}

impl Service {
    pub fn provision_target(&mut self) -> Result<Option<ProvisionTarget>> {
        let Some(task) = self.current_task()? else {
            return Ok(None);
        };
        if task.state() != super::lifecycle::State::NeedsDependency {
            return Ok(None);
        }
        let session = self.execution_session()?;
        Ok(Some(ProvisionTarget {
            task: task.id,
            attempt: task.attempts.last().map(|attempt| attempt.id),
            session_generation: session.as_ref().map(|session| session.generation),
            mode: session.map(|session| session.mode),
        }))
    }

    pub fn provision_target_is_current(&mut self, target: &ProvisionTarget) -> Result<bool> {
        let Some(task) = self.current_task()? else {
            return Ok(false);
        };
        let session = self.execution_session()?;
        Ok(task.id == target.task
            && task.state() == super::lifecycle::State::NeedsDependency
            && task.attempts.last().map(|attempt| attempt.id) == target.attempt
            && session.as_ref().map(|session| session.generation) == target.session_generation
            && session.map(|session| session.mode) == target.mode)
    }

    /// Called only at an SDK dependency wait. A failed or ineligible recipe leaves the
    /// wait intact; the caller makes at most one automatic attempt per run session.
    pub fn try_auto_provision_wait(&mut self, paths: &Paths) -> Result<bool> {
        if !self.provision_enabled()? {
            return Ok(false);
        }
        let Some(current) = self.current_task()? else {
            return Ok(false);
        };
        if current.state() != super::lifecycle::State::NeedsDependency {
            return Ok(false);
        }
        let target = self.provision_target()?.expect("current dependency wait");
        let requirements = self.dependencies(current.id)?;
        if !requirements.iter().any(|dependency| {
            dependency.state == "waiting"
                && dependency.requirement.kind == "sdk_component"
                && dependency.requirement.item == COMPONENTS[3]
        }) {
            return Ok(false);
        }
        let tools = self.tools()?;
        if tools
            .iter()
            .any(|tool| tool.name == "android-sdk" && tool.bound)
        {
            return Ok(false);
        }
        let source = tools
            .iter()
            .filter(|tool| {
                tool.name == "android-sdk" && tool.provenance == "external" && tool.ready
            })
            .find_map(|tool| {
                self.provision_plan(paths, RECIPE, tool.id)
                    .ok()
                    .filter(|plan| plan.issues.is_empty())
                    .map(|_| tool.id)
            });
        let Some(source) = source else {
            return Ok(false);
        };
        if self.provision_android(paths, RECIPE, source).is_err() {
            return Ok(false);
        }
        if !self.provision_target_is_current(&target)? {
            return Ok(false);
        }
        let current = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Current task disappeared after provisioning".into()))?;
        self.recheck_dependencies(
            super::VersionedTask {
                id: current.id,
                version: current.version,
            },
            &paths.cache.join("workers"),
            &paths.executable,
            paths,
        )
    }

    pub fn provision_status(&self, paths: &Paths) -> Result<Option<ProvisionStatus>> {
        let path = paths
            .state
            .join("provision")
            .join(format!("{}.json", self.project_id));
        match fs::read(path) {
            Ok(bytes) => {
                let mut status: ProvisionStatus = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::Conflict(format!("Invalid provisioning status: {e}")))?;
                if matches!(status.state.as_str(), "queued" | "running")
                    && !status.operation.is_empty()
                    && !operation_live(paths, self.project_id)?
                {
                    status.state = "failed_or_interrupted".into();
                    status.message =
                        "Installer exited without a completed result; retry explicitly".into();
                }
                Ok(Some(status))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn cancel_provision(&self, paths: &Paths) -> Result<()> {
        let status = self.provision_status(paths)?;
        require(
            status.as_ref().is_some_and(|status| {
                matches!(status.state.as_str(), "queued" | "running")
                    && !status.operation.is_empty()
            }),
            "No active provisioning to cancel",
        )?;
        let root = paths.data.join("provision-staging");
        fs::create_dir_all(&root)?;
        fs::write(
            root.join(format!("cancel-{}", status.expect("checked").operation)),
            b"cancel",
        )?;
        Ok(())
    }

    pub fn provision_enabled(&self) -> Result<bool> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT enabled FROM provision_policy WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn set_provision_enabled(&mut self, enabled: bool) -> Result<()> {
        require(
            matches!(self.actor.kind, ActorKind::Human),
            "Only a human actor may enable provisioning",
        )?;
        self.db.connection().execute("INSERT INTO provision_policy(project_id,enabled) VALUES (?1,?2) ON CONFLICT(project_id) DO UPDATE SET enabled=excluded.enabled,row_version=row_version+1", params![self.project_id,enabled])?;
        Ok(())
    }

    pub fn consent_to_provision(&mut self, recipe_name: &str, source: i64) -> Result<()> {
        recipe(recipe_name)?;
        require(
            matches!(self.actor.kind, ActorKind::Human),
            "Only a human actor may accept licenses",
        )?;
        let tool = self
            .tools()?
            .into_iter()
            .find(|tool| tool.id == source && tool.name == "android-sdk" && tool.ready)
            .ok_or_else(|| Error::Conflict("Register a ready Android SDK source first".into()))?;
        let hash = license_digest(&tool.path)?;
        verify_license_acceptance(&tool.path)?;
        let bootstrap_hash = bootstrap_digest(&tool.path)?;
        self.db.connection().execute("INSERT INTO provision_consents(project_id,recipe,source_id,license_sha256,bootstrap_sha256,actor_kind,actor_name,accepted_at_ms) VALUES (?1,?2,?3,?4,?5,'human',?6,?7) ON CONFLICT(project_id,recipe,source_id) DO UPDATE SET license_sha256=excluded.license_sha256,bootstrap_sha256=excluded.bootstrap_sha256,actor_name=excluded.actor_name,accepted_at_ms=excluded.accepted_at_ms", params![self.project_id,recipe_name,source,hash,bootstrap_hash,self.actor.name,now_ms()])?;
        Ok(())
    }

    pub fn provision_plan(&self, paths: &Paths, recipe_name: &str, source: i64) -> Result<Plan> {
        self.provision_plan_for(
            paths,
            recipe_name,
            source,
            Host::detect(),
            Limits::default(),
        )
    }

    fn provision_plan_for(
        &self,
        paths: &Paths,
        recipe_name: &str,
        source: i64,
        host: Host,
        limits: Limits,
    ) -> Result<Plan> {
        let contract = recipe(recipe_name)?;
        let tool = self
            .tools()?
            .into_iter()
            .find(|tool| tool.id == source && tool.name == "android-sdk")
            .ok_or_else(|| Error::Conflict("Unknown Android SDK source generation".into()))?;
        let mut issues = Vec::new();
        if !tool.ready {
            issues.push("Source SDK changed or disappeared; refresh or register it".into());
        }
        if tool.provenance != "external" {
            issues.push("Bootstrap must be a registered external Android SDK".into());
        }
        if self.tools()?.iter().any(|existing| {
            existing.name == "android-sdk" && existing.bound && existing.provenance == "external"
        }) {
            issues.push(
                "Unbind the project's explicit Android SDK before selecting a managed generation"
                    .into(),
            );
        }
        if std::env::consts::OS != "linux" || std::env::consts::ARCH != "x86_64" {
            issues.push("This recipe supports Linux x86_64 only".into());
        }
        if bootstrap(&tool.path).is_none() {
            issues.push("Source SDK needs a compatible android CLI or sdkmanager".into());
        }
        let license = license_digest(&tool.path).ok();
        if license.is_none() || verify_license_acceptance(&tool.path).is_err() {
            issues.push("Accept Android SDK licenses in the source SDK first".into());
        }
        let bootstrap_hash = bootstrap_digest(&tool.path).ok();
        if bootstrap_hash.is_none() {
            issues.push("Source Android command-line executable is missing or changed".into());
        }
        let accepted: Option<(String, String)> = self
            .db
            .connection()
            .query_row(
                "SELECT license_sha256,bootstrap_sha256 FROM provision_consents WHERE project_id=?1 AND recipe=?2 AND source_id=?3",
                params![self.project_id, recipe_name, source],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let consented =
            license
                .as_ref()
                .zip(bootstrap_hash.as_ref())
                .is_some_and(|(licenses, bootstrap)| {
                    accepted.as_ref() == Some(&(licenses.clone(), bootstrap.clone()))
                });
        if !consented {
            issues
                .push("A human must record consent for the exact source SDK license files".into());
        } else if let Some((binary, kind)) = bootstrap(&tool.path) {
            let mut command = Command::new(binary);
            command.arg(if kind.starts_with("android") {
                "-V"
            } else {
                "--version"
            });
            if !bounded_status(&mut command, Duration::from_secs(3)) {
                issues.push(
                    "Source Android command-line tool does not answer its version probe".into(),
                );
            }
        }
        let enabled = self.provision_enabled()?;
        if !enabled {
            issues.push("Automatic provisioning is disabled for this project".into());
        }
        if !host.kvm {
            issues.push("Emulator requires usable /dev/kvm; configure host virtualization or a separately authorized device".into());
        }
        if !host.java {
            issues.push(
                "A working Java runtime is required by Android SDK command-line tools".into(),
            );
        }
        Ok(Plan {
            recipe: recipe_name.into(),
            source: tool.path,
            source_version: tool.version,
            packages: contract.packages.iter().map(|v| (*v).into()).collect(),
            package_revisions: REVISIONS.iter().map(|v| (*v).into()).collect(),
            repository: contract.repository,
            destination: destination(paths, source),
            stage_max_bytes: limits.stage_bytes,
            download_cache_max_bytes: limits.download_bytes,
            download_cache_max_entries: limits.download_entries,
            retained_max_bytes: limits.retained_bytes,
            min_free_bytes: limits.min_free_bytes,
            timeout_seconds: limits.duration.as_secs(),
            enabled,
            consented,
            license_sha256: license,
            bootstrap_sha256: bootstrap_hash,
            issues,
        })
    }

    pub fn provision_android(
        &mut self,
        paths: &Paths,
        recipe_name: &str,
        source: i64,
    ) -> Result<i64> {
        self.provision_android_for(
            paths,
            recipe_name,
            source,
            Host::detect(),
            Limits::default(),
            None,
        )
    }

    pub fn provision_android_cancelable(
        &mut self,
        paths: &Paths,
        recipe_name: &str,
        source: i64,
        cancelled: &AtomicBool,
    ) -> Result<i64> {
        self.provision_android_for(
            paths,
            recipe_name,
            source,
            Host::detect(),
            Limits::default(),
            Some(cancelled),
        )
    }

    fn provision_android_for(
        &mut self,
        paths: &Paths,
        recipe_name: &str,
        source: i64,
        host: Host,
        limits: Limits,
        cancelled: Option<&AtomicBool>,
    ) -> Result<i64> {
        let binding_epoch = self.tool_epoch()?;
        let plan = self.provision_plan_for(paths, recipe_name, source, host, limits)?;
        require(
            plan.issues.is_empty(),
            &format!("Dependency setup needed: {}", plan.issues.join("; ")),
        )?;
        require(
            !self
                .current_task()?
                .is_some_and(|task| task.state() == super::lifecycle::State::Running),
            "Stop the live worker before provisioning",
        )?;
        let tools_root = paths.data.join("tools");
        fs::create_dir_all(&tools_root)?;
        let stage_root = paths.data.join("provision-staging");
        fs::create_dir_all(&stage_root)?;
        let download_root = paths.data.join("provision-download-cache");
        fs::create_dir_all(&download_root)?;
        let status_root = paths.state.join("provision");
        fs::create_dir_all(&status_root)?;
        let operation_lock =
            crate::scratch::open_lock(&status_root.join(format!("{}.lock", self.project_id)))?;
        require(
            operation_lock.try_lock().is_ok(),
            "This project already has an active provisioning operation",
        )?;
        let operation = format!(
            "{}-{}-{}-{}",
            self.project_id,
            std::process::id(),
            now_ms(),
            NEXT_OPERATION.fetch_add(1, Ordering::Relaxed)
        );
        let cancel = stage_root.join(format!("cancel-{operation}"));
        let mut run_status = RunStatus {
            path: status_root.join(format!("{}.json", self.project_id)),
            cancel,
            status: ProvisionStatus {
                state: "queued".into(),
                recipe: recipe_name.into(),
                operation,
                stage_bytes: 0,
                stage_entries: 0,
                download_cache_bytes: 0,
                download_cache_entries: 0,
                message: "Waiting for the shared Android generation lock".into(),
            },
            completed: false,
        };
        run_status.publish()?;
        let lock = crate::scratch::open_lock(&tools_root.join(".provision.lock"))?;
        let lock_started = Instant::now();
        loop {
            match lock.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    require(
                        !run_status.cancel.exists()
                            && !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
                        "Provisioning cancelled while waiting for generation lock",
                    )?;
                    require(
                        lock_started.elapsed() <= limits.duration,
                        "Provisioning timed out waiting for generation lock",
                    )?;
                    thread::sleep(Duration::from_millis(100));
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        require(
            !run_status.cancel.exists()
                && !cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed)),
            "Provisioning cancelled while waiting for generation lock",
        )?;
        run_status.status.state = "running".into();
        run_status.status.message = "Installing fixed Android packages".into();
        run_status.publish()?;
        let guardian_lock = tools_root.join(".provision-guardian.lock");
        crate::runner::process::recover_orphaned(&guardian_lock)?;
        if let Some(existing) = self
            .tools()?
            .into_iter()
            .find(|t| t.path == plan.destination && t.ready)
        {
            verify_android_metadata(&existing.path)?;
            verify_avd_template(&existing.path)?;
            require(
                license_digest(&existing.path).ok() == plan.license_sha256,
                "Reused SDK license digest differs from accepted source",
            )?;
            probe_android_payload(paths, &existing.path, Some(&run_status.cancel), cancelled)?;
            self.bind_tool_at_epoch("android-sdk", existing.id, None, Some(binding_epoch))?;
            run_status.status.state = "succeeded".into();
            run_status.status.message = format!("Reused generation #{}", existing.id);
            run_status.publish()?;
            run_status.completed = true;
            return Ok(existing.id);
        }
        require(
            !live_staging(&stage_root)?,
            "A live or orphaned Android installer still owns staging; wait for it to exit before retrying",
        )?;
        if plan.destination.exists() {
            require(
                plan.destination.is_dir()
                    && !fs::symlink_metadata(&plan.destination)?
                        .file_type()
                        .is_symlink(),
                "Unregistered managed generation is not an owned directory; inspect it",
            )?;
            require(
                license_digest(&plan.destination).ok() == plan.license_sha256,
                "Unregistered managed generation license digest differs; inspect it",
            )?;
            verify_android_metadata(&plan.destination)?;
            verify_avd_template(&plan.destination)?;
            probe_android_payload(
                paths,
                &plan.destination,
                Some(&run_status.cancel),
                cancelled,
            )?;
            let used = tree_usage(&plan.destination, limits)?;
            require(
                used.0 <= limits.stage_bytes && used.1 <= limits.stage_entries,
                "Unregistered managed generation exceeds capacity",
            )?;
            let id = self.register_tool(
                paths,
                &Registration {
                    name: "android-sdk",
                    version: &format!("{RECIPE}@{}", plan.source_version),
                    platform: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    components: &plan.packages,
                    path: &plan.destination,
                    managed: true,
                },
            )?;
            self.bind_tool_at_epoch("android-sdk", id, None, Some(binding_epoch))?;
            run_status.status.state = "succeeded".into();
            run_status.status.message = format!("Recovered published generation #{id}");
            run_status.publish()?;
            run_status.completed = true;
            return Ok(id);
        }
        let mut stage = ProvisionStage::new(Scratch::new(&stage_root)?, guardian_lock.clone());
        run_status.publish()?;
        let sdk = stage.path().join("sdk");
        fs::create_dir_all(&sdk)?;
        for directory in ["config", "data", "state"] {
            fs::create_dir_all(stage.path().join(directory))?;
        }
        fs::create_dir_all(download_root.join("android-user"))?;
        copy_licenses(&plan.source, &sdk)?;
        let (binary, kind) = bootstrap(&plan.source)
            .ok_or_else(|| Error::Conflict("Bootstrap disappeared".into()))?;
        require(
            bootstrap_digest(&plan.source).ok() == plan.bootstrap_sha256,
            "Source Android bootstrap changed since consent",
        )?;
        copy_bootstrap(&plan.source, &sdk, &binary, kind)?;
        let staged_binary = if kind == "android" {
            sdk.join("bin/android")
        } else if kind == "android-package" {
            sdk.join("cmdline-tools/latest/bin/android")
        } else {
            sdk.join("cmdline-tools/latest/bin/sdkmanager")
        };
        require(
            bootstrap_digest(&sdk).ok() == plan.bootstrap_sha256,
            "Staged Android bootstrap differs from reviewed source",
        )?;
        require(
            tree_usage(stage.path(), limits)?.0 <= limits.stage_bytes,
            "Bootstrap copy exceeds provisioning staging capacity",
        )?;
        let log = stage.path().join("installer.log");
        let mut args = Vec::new();
        if kind.starts_with("android") {
            args.extend([
                format!("--sdk={}", sdk.display()),
                "sdk".into(),
                "install".into(),
            ]);
            args.extend(
                COMPONENTS
                    .iter()
                    .zip(REVISIONS)
                    .map(|(component, revision)| {
                        format!("{}@{revision}", component.replace(';', "/"))
                    }),
            );
        } else {
            args.extend([
                format!("--sdk_root={}", sdk.display()),
                "--install".into(),
                "--channel=0".into(),
            ]);
            args.extend(COMPONENTS.iter().map(|item| (*item).to_owned()));
        }
        let mut spec = crate::runner::process::Invocation::new(
            staged_binary,
            args,
            stage.path().to_path_buf(),
            stage_root.clone(),
            guardian_lock,
        );
        spec.scratch_limits = Some((limits.stage_bytes, limits.stage_entries));
        spec.env = vec![
            ("ANDROID_HOME", sdk.clone()),
            ("ANDROID_SDK_ROOT", sdk.clone()),
            ("ANDROID_USER_HOME", download_root.join("android-user")),
            ("ANDROID_AVD_HOME", stage.path().join("avd")),
            ("ANDROID_SDK_HOME", download_root.clone()),
            ("HOME", stage.path().to_path_buf()),
            ("XDG_CONFIG_HOME", stage.path().join("config")),
            ("XDG_CACHE_HOME", download_root.clone()),
            ("XDG_DATA_HOME", stage.path().join("data")),
            ("XDG_STATE_HOME", stage.path().join("state")),
        ]
        .into_iter()
        .map(|(key, path)| (key.into(), path.to_string_lossy().into_owned()))
        .collect();
        spec.unset_env = [
            "SDK_TEST_BASE_URL",
            "SDK_TEST_URLS",
            "SDK_MIX_WITH_TEST_URLS",
            "SDKMANAGER_OPTS",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        let mut child = crate::runner::process::Worker::spawn(&paths.executable, &spec)?;
        stage.guardian_started = true;
        let mut log_file = File::create(&log)?;
        let mut logged = 0_usize;
        let start = Instant::now();
        let code = loop {
            let used = tree_usage(stage.path(), limits);
            let download_used = tree_usage(&download_root, limits);
            let free = free_bytes(&tools_root);
            let reason = if used
                .as_ref()
                .is_ok_and(|used| used.0 > limits.stage_bytes || used.1 > limits.stage_entries)
            {
                Some("staging capacity exceeded")
            } else if used.is_err() || free.is_err() {
                Some("capacity inspection failed")
            } else if download_used.as_ref().is_ok_and(|used| {
                used.0 > limits.download_bytes || used.1 > limits.download_entries
            }) {
                Some("download cache capacity exceeded")
            } else if download_used.is_err() {
                Some("download cache inspection failed")
            } else if free
                .as_ref()
                .is_ok_and(|free| *free < limits.min_free_bytes)
            {
                Some("free-space capacity exceeded")
            } else if run_status.cancel.exists()
                || cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
            {
                Some("cancelled")
            } else if start.elapsed() > limits.duration {
                Some("time limit exceeded")
            } else {
                None
            };
            if let Some(reason) = reason {
                run_status.status.message = format!("Provisioning stopped: {reason}");
                child.join()?;
                return Err(Error::Conflict(format!(
                    "Provisioning stopped: {reason}; stage {used:?}, free bytes {free:?}"
                )));
            }
            if let Ok((bytes, entries)) = used {
                run_status.status.stage_bytes = bytes;
                run_status.status.stage_entries = entries;
                if let Ok((bytes, entries)) = download_used {
                    run_status.status.download_cache_bytes = bytes;
                    run_status.status.download_cache_entries = entries;
                }
                let _ = run_status.publish();
            }
            match child.receive(Duration::from_millis(250))? {
                crate::runner::process::Received::Idle => {}
                crate::runner::process::Received::Output { bytes, .. } => {
                    let allowed = bytes.len().min((64 * 1024_usize).saturating_sub(logged));
                    if allowed > 0 {
                        use std::io::Write;
                        log_file.write_all(&bytes[..allowed])?;
                        logged += allowed;
                    }
                }
                crate::runner::process::Received::Exited(code) => break code,
                crate::runner::process::Received::Failed(error) => return Err(child.failure(error)),
                crate::runner::process::Received::Disconnected => {
                    return Err(Error::Conflict("Installer guardian disconnected".into()));
                }
                crate::runner::process::Received::Artifact => {
                    return Err(Error::Conflict("Unexpected installer artifact".into()));
                }
            }
        };
        drop(child);
        // A fast installer can finish between the last poll and its final write.
        let used = tree_usage(stage.path(), limits)?;
        let download_used = tree_usage(&download_root, limits)?;
        require(
            used.0 <= limits.stage_bytes && used.1 <= limits.stage_entries,
            "Installed SDK exceeds bounded staging capacity",
        )?;
        require(
            download_used.0 <= limits.download_bytes && download_used.1 <= limits.download_entries,
            "Download cache exceeds bounded provisioning capacity",
        )?;
        require(
            free_bytes(&tools_root)? >= limits.min_free_bytes,
            "Provisioning stopped at free-space capacity limit",
        )?;
        if code != 0 {
            run_status.status.message = format!("Android installer failed: {}", tail(&log)?);
            return Err(Error::Conflict(run_status.status.message.clone()));
        }
        require(
            license_digest(&plan.source).ok() == plan.license_sha256,
            "Source SDK licenses changed during installation",
        )?;
        require(
            license_digest(&sdk).ok() == plan.license_sha256,
            "Managed SDK licenses changed during installation",
        )?;
        for component in COMPONENTS {
            require(
                super::dependency::probe_sdk_component_at(&sdk, component, None).starts_with("OK:"),
                &format!("Installer omitted {component}"),
            )?;
        }
        if let Err(error) = verify_android_metadata(&sdk)
            .and_then(|_| probe_android_payload(paths, &sdk, Some(&run_status.cancel), cancelled))
        {
            run_status.status.message = error.to_string();
            return Err(error);
        }
        fs::write(sdk.join("ktask-avd-template.json"), avd_template())?;
        let used = tree_usage(&sdk, limits)?;
        require(
            used.0 <= limits.stage_bytes && used.1 <= limits.stage_entries,
            "Installed SDK exceeds bounded staging capacity",
        )?;
        require(
            tree_usage(&tools_root, limits)?.0.saturating_add(used.0) <= limits.retained_bytes,
            "Retained tool generations exceed provisioning capacity",
        )?;
        fs::rename(&sdk, &plan.destination)?;
        let id = match self.register_tool(
            paths,
            &Registration {
                name: "android-sdk",
                version: &format!("{RECIPE}@{}", plan.source_version),
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &plan.packages,
                path: &plan.destination,
                managed: true,
            },
        ) {
            Ok(id) => id,
            Err(error) => {
                let _ = fs::remove_dir_all(&plan.destination);
                return Err(error);
            }
        };
        self.bind_tool_at_epoch("android-sdk", id, None, Some(binding_epoch))?;
        run_status.status.state = "succeeded".into();
        run_status.status.message = format!("Published generation #{id}; recheck the waiting task");
        run_status.publish()?;
        run_status.completed = true;
        Ok(id)
    }
}

fn copy_licenses(source: &Path, destination: &Path) -> Result<()> {
    let from = source.join("licenses");
    let into = destination.join("licenses");
    fs::create_dir_all(&into)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        require(
            metadata.is_file() && metadata.len() <= 16 * 1024,
            "Invalid source license file",
        )?;
        fs::copy(entry.path(), into.join(entry.file_name()))?;
    }
    Ok(())
}

fn copy_bootstrap(source: &Path, sdk: &Path, binary: &Path, kind: &str) -> Result<()> {
    if kind == "android" {
        let destination = sdk.join("bin/android");
        fs::create_dir_all(destination.parent().expect("Android CLI bin parent"))?;
        fs::copy(binary, destination)?;
        return Ok(());
    }
    let relative = binary.strip_prefix(source).map_err(|_| {
        Error::Conflict("Android command-line tool escaped its registered SDK".into())
    })?;
    let source_package = source.join(
        relative
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| Error::Conflict("Invalid sdkmanager package layout".into()))?,
    );
    let destination = sdk.join("cmdline-tools/latest");
    copy_tree_bounded(&source_package, &destination, 1024 * 1024 * 1024, 50_000)
}

fn copy_tree_bounded(
    source: &Path,
    destination: &Path,
    max_bytes: u64,
    max_entries: u64,
) -> Result<()> {
    let mut todo = vec![(source.to_path_buf(), destination.to_path_buf())];
    let mut bytes = 0_u64;
    let mut entries = 0_u64;
    while let Some((from, to)) = todo.pop() {
        let metadata = fs::symlink_metadata(&from)?;
        require(
            !metadata.file_type().is_symlink(),
            "Bootstrap contains a symbolic link",
        )?;
        entries += 1;
        bytes = bytes.saturating_add(metadata.len());
        require(
            entries <= max_entries && bytes <= max_bytes,
            "Bootstrap exceeds its copy capacity",
        )?;
        if metadata.is_dir() {
            fs::create_dir_all(&to)?;
            for entry in fs::read_dir(from)? {
                let entry = entry?;
                todo.push((entry.path(), to.join(entry.file_name())));
            }
        } else {
            require(metadata.is_file(), "Bootstrap contains a special file")?;
            fs::copy(from, to)?;
        }
    }
    Ok(())
}

fn verify_android_metadata(sdk: &Path) -> Result<()> {
    for (component, revision, expected) in [
        ("platform-tools", REVISIONS[0], &[][..]),
        ("emulator", REVISIONS[1], &[][..]),
        (
            "platforms/android-36",
            REVISIONS[2],
            &[("AndroidVersion.ApiLevel", "36")][..],
        ),
        (
            "system-images/android-36/google_apis/x86_64",
            REVISIONS[3],
            &[
                ("AndroidVersion.ApiLevel", "36"),
                ("SystemImage.Abi", "x86_64"),
                ("SystemImage.TagId", "google_apis"),
            ][..],
        ),
    ] {
        let properties = sdk.join(component).join("source.properties");
        let meta = fs::symlink_metadata(&properties)?;
        require(
            meta.is_file() && meta.len() <= 4096,
            &format!("Missing bounded package metadata for {component}"),
        )?;
        let properties = fs::read_to_string(properties)?;
        let values = properties
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(key, value)| (key.trim(), value.trim()))
            .collect::<std::collections::BTreeMap<_, _>>();
        require(
            values.get("Pkg.Revision") == Some(&revision),
            &format!("Wrong Pkg.Revision in {component}; expected {revision}"),
        )?;
        for (key, value) in expected {
            require(
                values.get(key) == Some(value),
                &format!("Wrong {key} in {component}; expected {value}"),
            )?;
        }
    }
    Ok(())
}

fn verify_avd_template(sdk: &Path) -> Result<()> {
    require(
        fs::read_to_string(sdk.join("ktask-avd-template.json"))? == avd_template(),
        "Managed AVD template changed or is incompatible with the reviewed image",
    )
}

/// Runs inside the dependency guardian for every image admission, including
/// external SDKs. It reads the SDK and executes only bounded version/capability
/// probes with private Android home settings supplied by the caller.
pub(super) fn admit_android_image_at(sdk: &Path, item: &str) -> Result<()> {
    let parts = item.split(';').collect::<Vec<_>>();
    require(
        parts.len() == 4 && parts[0] == "system-images" && parts[1].starts_with("android-"),
        "Unsupported Android system image identifier",
    )?;
    let image = sdk.join(item.replace(';', "/"));
    let properties = image.join("source.properties");
    let metadata = fs::symlink_metadata(&properties)?;
    require(
        metadata.is_file() && metadata.len() <= 4096,
        "Android image has no bounded package metadata",
    )?;
    let values = fs::read_to_string(properties)?
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| (key.trim().to_owned(), value.trim().to_owned()))
        .collect::<std::collections::BTreeMap<_, _>>();
    require(
        values.get("AndroidVersion.ApiLevel").map(String::as_str) == Some(&parts[1][8..])
            && values.get("SystemImage.TagId").map(String::as_str) == Some(parts[2])
            && values.get("SystemImage.Abi").map(String::as_str) == Some(parts[3]),
        "Android image API, tag or ABI does not match its package path",
    )?;
    let payload = fs::symlink_metadata(image.join("system.img"))?;
    require(
        payload.is_file() && payload.len() > 0,
        "Android system image payload is missing or empty",
    )?;
    license_digest(sdk)?;
    verify_license_acceptance(sdk)?;
    let mut java = java_probe();
    java.arg("-version");
    require(
        bounded_status(&mut java, Duration::from_secs(3)),
        "A working Java runtime is required",
    )?;
    for (relative, argument, label) in [
        ("platform-tools/adb", "version", "adb"),
        ("emulator/emulator", "-version", "Android emulator"),
        (
            "emulator/emulator",
            "-accel-check",
            "Android emulator acceleration",
        ),
    ] {
        let mut probe = Command::new(sdk.join(relative));
        probe
            .arg(argument)
            .env("ANDROID_HOME", sdk)
            .env("ANDROID_SDK_ROOT", sdk);
        require(
            bounded_status(&mut probe, Duration::from_secs(3)),
            &format!("{label} is unavailable or unusable"),
        )?;
    }
    Ok(())
}

fn probe_android_payload(
    paths: &Paths,
    sdk: &Path,
    cancel_path: Option<&Path>,
    cancelled: Option<&AtomicBool>,
) -> Result<()> {
    let root = paths.data.join("provision-probes");
    fs::create_dir_all(&root)?;
    let guardian_lock = root.join(".guardian.lock");
    crate::runner::process::recover_orphaned(&guardian_lock)?;
    let mut home = ProvisionStage::new(Scratch::new(&root)?, guardian_lock.clone());
    let mut spec = crate::runner::process::Invocation::new(
        paths.executable.clone(),
        vec![
            "__dependency_probe".into(),
            "sdk_component".into(),
            COMPONENTS[3].into(),
            REVISIONS[3].into(),
        ],
        home.path().to_path_buf(),
        root,
        guardian_lock,
    );
    spec.env = [
        ("ANDROID_HOME", sdk.to_path_buf()),
        ("ANDROID_SDK_ROOT", sdk.to_path_buf()),
        ("ANDROID_AVD_HOME", home.path().join("avd")),
        ("ANDROID_USER_HOME", home.path().join("android-user")),
        ("HOME", home.path().to_path_buf()),
    ]
    .into_iter()
    .map(|(key, path)| (key.into(), path.to_string_lossy().into_owned()))
    .collect();
    spec.sandbox = Some(crate::runner::sandbox::Sandbox::for_installations(&[
        sdk.to_path_buf()
    ])?);
    spec.scratch_limits = Some((64 * 1024 * 1024, 10_000));
    let mut worker = crate::runner::process::Worker::spawn(&paths.executable, &spec)?;
    home.guardian_started = true;
    let started = Instant::now();
    let mut output = Vec::new();
    loop {
        if cancel_path.is_some_and(Path::exists)
            || cancelled.is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            worker.join()?;
            return Err(Error::Conflict("Android admission probe cancelled".into()));
        }
        if started.elapsed() > Duration::from_secs(15) {
            worker.join()?;
            return Err(Error::Conflict("Android admission probe timed out".into()));
        }
        match worker.receive(Duration::from_millis(50))? {
            crate::runner::process::Received::Idle => {}
            crate::runner::process::Received::Output { stream, bytes } => {
                if stream == "stdout" && output.len() + bytes.len() <= 4096 {
                    output.extend(bytes);
                }
            }
            crate::runner::process::Received::Exited(code) => {
                let observed = String::from_utf8_lossy(&output);
                require(
                    code == 0 && observed.contains("OK:"),
                    &format!("Android image admission failed: {}", observed.trim()),
                )?;
                return Ok(());
            }
            crate::runner::process::Received::Failed(error) => return Err(worker.failure(error)),
            crate::runner::process::Received::Disconnected => {
                return Err(Error::Conflict(
                    "Android probe guardian disconnected".into(),
                ));
            }
            crate::runner::process::Received::Artifact => {
                return Err(Error::Conflict("Unexpected Android probe artifact".into()));
            }
        }
    }
}

fn tree_usage(root: &Path, limits: Limits) -> Result<(u64, u64)> {
    let mut size = 0_u64;
    let mut count = 0_u64;
    let mut todo = vec![root.to_path_buf()];
    while let Some(path) = todo.pop() {
        let meta = fs::symlink_metadata(&path)?;
        require(
            !meta.file_type().is_symlink(),
            "Provisioning output contains a symbolic link",
        )?;
        count += 1;
        size = size.saturating_add(meta.len());
        require(
            count <= limits.stage_entries && size <= limits.retained_bytes,
            "Provisioning tree exceeds capacity",
        )?;
        if meta.is_dir() {
            for entry in fs::read_dir(path)? {
                todo.push(entry?.path());
            }
        } else {
            require(
                meta.is_file(),
                "Provisioning output contains a special file",
            )?;
        }
    }
    Ok((size, count))
}

fn free_bytes(path: &Path) -> Result<u64> {
    let stat = nix::sys::statvfs::statvfs(path).map_err(|e| Error::Conflict(e.to_string()))?;
    Ok(stat.blocks_available().saturating_mul(stat.fragment_size()))
}

fn live_staging(root: &Path) -> Result<bool> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_name().to_string_lossy().starts_with("run-") {
            continue;
        }
        let lease = match OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path().join(".lease"))
        {
            Ok(lease) => lease,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if lease.try_lock().is_err() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn operation_live(paths: &Paths, project_id: i64) -> Result<bool> {
    let lock = crate::scratch::open_lock(
        &paths
            .state
            .join("provision")
            .join(format!("{project_id}.lock")),
    )?;
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(std::fs::TryLockError::WouldBlock) => Ok(true),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

fn tail(path: &Path) -> Result<String> {
    let file = File::open(path)?;
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).chars().take(2000).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        application::{Actor, ActorKind},
        storage::Database,
    };
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        scratch: Scratch,
        paths: Paths,
        source: PathBuf,
        project: PathBuf,
    }

    impl Fixture {
        fn new() -> Result<Self> {
            let scratch = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
            let root = scratch.path();
            let paths = Paths {
                config: root.join("config"),
                state: root.join("state"),
                cache: root.join("cache"),
                data: root.join("data"),
                runtime: root.join("runtime"),
                executable: std::env::current_exe()?
                    .parent()
                    .and_then(Path::parent)
                    .ok_or_else(|| Error::Conflict("Test binary has no target directory".into()))?
                    .join("ktask-dev"),
            };
            let source = root.join("source-sdk");
            let project = root.join("project-one");
            fs::create_dir_all(source.join("cmdline-tools/latest/bin"))?;
            fs::create_dir_all(source.join("licenses"))?;
            fs::create_dir_all(&project)?;
            fs::write(
                source.join("licenses/android-sdk-license"),
                format!("{ANDROID_SDK_LICENSE_SHA1}\n"),
            )?;
            let script = source.join("cmdline-tools/latest/bin/sdkmanager");
            fs::write(
                &script,
                format!(
                    r#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then
  if [ "$(cat '{}/mode' 2>/dev/null || true)" = version-hold ]; then
    touch '{}/version-entered'
    while [ ! -f '{}/version-release' ]; do sleep .05; done
  fi
  printf '16.0\n'; exit 0
fi
printf '%s\n' "$@" > '{}/argv'
case "$1" in --sdk_root=*) ;; *) exit 23 ;; esac
[ "$ANDROID_HOME" = "$ANDROID_SDK_ROOT" ]
[ "$TMPDIR" != "$ANDROID_HOME" ]
[ "$XDG_CACHE_HOME" != "$TMPDIR" ]
case "$XDG_CACHE_HOME" in */data/provision-download-cache) ;; *) exit 24 ;; esac
[ "$XDG_CONFIG_HOME" = "$HOME/config" ]
[ -z "${{SDK_TEST_BASE_URL-}}" ]
case "$(cat '{}/mode' 2>/dev/null || true)" in
  fail) exit 17 ;;
  quota) mkdir -p "$ANDROID_HOME/platform-tools"; truncate -s 131072 "$ANDROID_HOME/platform-tools/huge"; touch '{}/quota-created'; exit 0 ;;
  quota-running) mkdir -p "$ANDROID_HOME/platform-tools"; truncate -s 131072 "$ANDROID_HOME/platform-tools/huge"; touch '{}/quota-created'; sleep 20 ;;
  download-quota) mkdir -p "$XDG_CACHE_HOME/archives"; truncate -s 131072 "$XDG_CACHE_HOME/archives/huge.zip"; sleep 20 ;;
  tampered-download) [ "$(cat "$XDG_CACHE_HOME/package.archive")" = verified ] || {{ echo 'repository archive checksum mismatch' >&2; exit 19; }} ;;
  interrupt) mkdir -p "$ANDROID_HOME/platform-tools"; touch "$ANDROID_HOME/platform-tools/partial"; sleep 20 ;;
  detached) HEARTBEAT_FILE='{}/heartbeat' setsid sh -c 'while :; do echo tick >> "$HEARTBEAT_FILE"; sleep .1; done' </dev/null >/dev/null 2>&1 & sleep 20 ;;
  kill-guardian) HEARTBEAT_FILE='{}/heartbeat' setsid sh -c 'while :; do echo tick >> "$HEARTBEAT_FILE"; sleep .1; done' </dev/null >/dev/null 2>&1 & kill -9 "$PPID"; sleep 20 ;;
  slow) sleep 1 ;;
  slow-lock) sleep 3 ;;
esac
mkdir -p "$ANDROID_HOME/platform-tools" "$ANDROID_HOME/emulator" "$ANDROID_HOME/platforms/android-36" "$ANDROID_HOME/system-images/android-36/google_apis/x86_64"
printf 'Pkg.Revision=37.0.1\n' > "$ANDROID_HOME/platform-tools/source.properties"
printf 'Pkg.Revision=37.2.13\n' > "$ANDROID_HOME/emulator/source.properties"
printf 'Pkg.Revision=2\nAndroidVersion.ApiLevel=36\n' > "$ANDROID_HOME/platforms/android-36/source.properties"
printf 'Pkg.Revision=7\nAndroidVersion.ApiLevel=36\nSystemImage.Abi=x86_64\nSystemImage.TagId=google_apis\n' > "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/source.properties"
printf '#!/bin/sh\nexit 0\n' > "$ANDROID_HOME/platform-tools/adb"
printf '#!/bin/sh\nexit 0\n' > "$ANDROID_HOME/emulator/emulator"
chmod +x "$ANDROID_HOME/platform-tools/adb" "$ANDROID_HOME/emulator/emulator"
printf 'fixture image' > "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/system.img"
if [ "$(cat '{}/mode' 2>/dev/null || true)" = large-image ]; then
  truncate -s 131072 "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/system.img"
fi
if [ "$(cat '{}/mode' 2>/dev/null || true)" = tamper ]; then
  printf 'Pkg.Revision=7\nAndroidVersion.ApiLevel=35\nSystemImage.Abi=x86_64\nSystemImage.TagId=google_apis\n' > "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/source.properties"
fi
if [ "$(cat '{}/mode' 2>/dev/null || true)" = wrong-revision ]; then
  printf 'Pkg.Revision=8\nAndroidVersion.ApiLevel=36\nSystemImage.Abi=x86_64\nSystemImage.TagId=google_apis\n' > "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/source.properties"
fi
if [ "$(cat '{}/mode' 2>/dev/null || true)" = bad-emulator ]; then
  printf '#!/bin/sh\nexit 17\n' > "$ANDROID_HOME/emulator/emulator"
fi
if [ "$(cat '{}/mode' 2>/dev/null || true)" = kill-probe-guardian ]; then
  cat > "$ANDROID_HOME/emulator/emulator" <<'PROBE'
#!/bin/sh
if [ "$1" = -accel-check ]; then
  HEARTBEAT_FILE='{}/probe-heartbeat' setsid sh -c 'while :; do echo tick >> "$HEARTBEAT_FILE"; sleep .1; done' </dev/null >/dev/null 2>&1 &
  i=0; until [ -s '{}/probe-heartbeat' ] || [ "$i" -ge 50 ]; do i=$((i+1)); sleep .01; done
  guardian=$(sed -n 's/^PPid:[[:space:]]*//p' "/proc/$PPID/status")
  kill -9 "$guardian"
  sleep 20
fi
exit 0
PROBE
  chmod +x "$ANDROID_HOME/emulator/emulator"
fi
"#,
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display(),
                    root.display()
                ),
            )?;
            fs::set_permissions(script, fs::Permissions::from_mode(0o755))?;
            Ok(Self {
                scratch,
                paths,
                source,
                project,
            })
        }

        fn service(&self, project: &Path) -> Result<Service> {
            Service::open(Database::open_in(&self.paths.state)?, project, None)
        }

        fn source_id(&self, service: &mut Service) -> Result<i64> {
            service.register_tool(
                &self.paths,
                &Registration {
                    name: "android-sdk",
                    version: "cmdline-tools-16",
                    platform: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    components: &[],
                    path: &self.source,
                    managed: false,
                },
            )
        }

        fn alternate_source(&self, service: &mut Service) -> Result<i64> {
            let source = self.scratch.path().join("source-two");
            fs::create_dir_all(source.join("cmdline-tools/latest/bin"))?;
            fs::create_dir_all(source.join("licenses"))?;
            fs::copy(
                self.source.join("cmdline-tools/latest/bin/sdkmanager"),
                source.join("cmdline-tools/latest/bin/sdkmanager"),
            )?;
            fs::copy(
                self.source.join("licenses/android-sdk-license"),
                source.join("licenses/android-sdk-license"),
            )?;
            service.register_tool(
                &self.paths,
                &Registration {
                    name: "android-sdk",
                    version: "cmdline-tools-16",
                    platform: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    components: &[],
                    path: &source,
                    managed: false,
                },
            )
        }
    }

    #[test]
    fn manual_default_consent_and_host_gates_are_explicit() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        let plan = service.provision_plan_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: false,
                java: false,
            },
            Limits::default(),
        )?;
        assert!(!plan.enabled && !plan.consented);
        assert!(plan.issues.iter().any(|issue| issue.contains("/dev/kvm")));
        assert!(plan.issues.iter().any(|issue| issue.contains("Java")));
        assert!(
            service
                .provision_plan_for(
                    &f.paths,
                    "unreviewed",
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default()
                )
                .is_err()
        );
        service.set_actor(Actor {
            kind: ActorKind::Ai,
            name: "worker".into(),
        })?;
        assert!(service.set_provision_enabled(true).is_err());
        assert!(service.consent_to_provision(RECIPE, source).is_err());
        service.set_actor(Actor {
            kind: ActorKind::Human,
            name: "owner".into(),
        })?;
        service.consent_to_provision(RECIPE, source)?;
        service.set_provision_enabled(true)?;
        assert!(
            service
                .provision_plan_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default()
                )?
                .issues
                .is_empty()
        );
        service.bind_tool("android-sdk", source, Some(0))?;
        assert!(
            service
                .provision_plan_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default(),
                )?
                .issues
                .iter()
                .any(|issue| issue.contains("Unbind"))
        );
        fs::write(f.source.join("licenses/android-sdk-license"), "changed")?;
        assert!(
            !service
                .provision_plan_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default()
                )?
                .consented
        );
        fs::write(
            f.source.join("licenses/android-sdk-license"),
            format!("{ANDROID_SDK_LICENSE_SHA1}\n"),
        )?;
        fs::OpenOptions::new()
            .append(true)
            .open(f.source.join("cmdline-tools/latest/bin/sdkmanager"))?
            .write_all(b"\n# changed after consent\n")?;
        assert!(
            !service
                .provision_plan_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default(),
                )?
                .consented
        );
        Ok(())
    }

    #[test]
    fn required_license_acceptance_rejects_missing_empty_and_unrelated_hashes() -> Result<()> {
        let f = Fixture::new()?;
        let license = f.source.join("licenses/android-sdk-license");
        assert!(verify_license_acceptance(&f.source).is_ok());
        fs::remove_file(&license)?;
        assert!(verify_license_acceptance(&f.source).is_err());
        fs::write(
            f.source.join("licenses/unrelated-license"),
            ANDROID_SDK_LICENSE_SHA1,
        )?;
        assert!(verify_license_acceptance(&f.source).is_err());
        fs::write(&license, "")?;
        assert!(verify_license_acceptance(&f.source).is_err());
        fs::write(&license, "0123456789abcdef0123456789abcdef01234567\n")?;
        assert!(verify_license_acceptance(&f.source).is_err());
        fs::write(&license, format!("{ANDROID_SDK_LICENSE_SHA1}\n"))?;
        assert!(verify_license_acceptance(&f.source).is_ok());
        Ok(())
    }

    #[test]
    fn fixed_recipe_publishes_once_and_reuses_across_projects() -> Result<()> {
        let f = Fixture::new()?;
        let mut first = f.service(&f.project)?;
        let source = f.source_id(&mut first)?;
        first.set_provision_enabled(true)?;
        first.consent_to_provision(RECIPE, source)?;
        let id = first.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let generated = first
            .tools()?
            .into_iter()
            .find(|tool| tool.id == id)
            .unwrap();
        assert!(generated.ready && generated.bound && generated.provenance == "managed");
        let template: serde_json::Value =
            serde_json::from_slice(&fs::read(generated.path.join("ktask-avd-template.json"))?)
                .expect("valid reviewed template");
        assert_eq!(template["system_image"], COMPONENTS[3]);
        assert!(generated.path.starts_with(f.paths.data.join("tools")));
        assert!(!generated.path.starts_with(f.paths.cache.join("workers")));
        let argv = fs::read_to_string(f.scratch.path().join("argv"))?;
        assert!(
            argv.contains("--sdk_root=")
                && argv.contains("--channel=0")
                && argv.contains("system-images;android-36;google_apis;x86_64")
        );
        let second_project = f.scratch.path().join("project-two");
        fs::create_dir_all(&second_project)?;
        let mut second = f.service(&second_project)?;
        second.set_provision_enabled(true)?;
        second.consent_to_provision(RECIPE, source)?;
        assert_eq!(
            second.provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true
                },
                Limits::default(),
                None
            )?,
            id
        );
        assert!(
            second
                .tools()?
                .into_iter()
                .find(|tool| tool.id == id)
                .unwrap()
                .bound
        );
        Ok(())
    }

    #[test]
    fn current_android_cli_receives_explicit_sdk_and_slash_package_paths() -> Result<()> {
        let f = Fixture::new()?;
        let android = f.source.join("cmdline-tools/latest/bin/android");
        fs::write(
            &android,
            format!(
                "#!/bin/sh\nset -eu\nif [ \"$1\" = -V ]; then echo 1.0; exit 0; fi\nprintf '%s\\n' \"$@\" > '{}/android-argv'\nexec '{}' \"--sdk_root=$ANDROID_HOME\" --install platform-tools emulator 'platforms;android-36' 'system-images;android-36;google_apis;x86_64'\n",
                f.scratch.path().display(),
                f.source
                    .join("cmdline-tools/latest/bin/sdkmanager")
                    .display()
            ),
        )?;
        fs::set_permissions(android, fs::Permissions::from_mode(0o755))?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        service.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let argv = fs::read_to_string(f.scratch.path().join("android-argv"))?;
        assert!(argv.lines().next().unwrap().starts_with("--sdk="));
        assert!(argv.contains("sdk\ninstall\nplatform-tools@37.0.1\nemulator@37.2.13\nplatforms/android-36@2\nsystem-images/android-36/google_apis/x86_64@7"));
        Ok(())
    }

    #[test]
    fn concurrent_projects_wait_for_one_atomic_generation() -> Result<()> {
        let f = Fixture::new()?;
        let mut first = f.service(&f.project)?;
        let source = f.source_id(&mut first)?;
        first.set_provision_enabled(true)?;
        first.consent_to_provision(RECIPE, source)?;
        let other_project = f.scratch.path().join("project-two");
        fs::create_dir_all(&other_project)?;
        let mut second = f.service(&other_project)?;
        second.set_provision_enabled(true)?;
        second.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "slow")?;
        let paths = Paths {
            config: f.paths.config.clone(),
            state: f.paths.state.clone(),
            cache: f.paths.cache.clone(),
            data: f.paths.data.clone(),
            runtime: f.paths.runtime.clone(),
            executable: f.paths.executable.clone(),
        };
        let first_run = thread::spawn(move || {
            first.provision_android_for(
                &paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
        });
        let marker = f.scratch.path().join("argv");
        let started = Instant::now();
        while !marker.exists() && started.elapsed() < Duration::from_secs(5) {
            thread::sleep(Duration::from_millis(20));
        }
        require(marker.exists(), "First installer did not start")?;
        assert!(!destination(&f.paths, source).exists());
        let reused = second.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let installed = first_run.join().expect("first installer thread")?;
        assert_eq!(installed, reused);
        assert!(
            second
                .tools()?
                .into_iter()
                .any(|tool| tool.id == reused && tool.bound && tool.ready)
        );
        Ok(())
    }

    #[test]
    fn completed_atomic_publication_recovers_after_registration_interruption() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        let first = service.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let alternate = f.alternate_source(&mut service)?;
        service.consent_to_provision(RECIPE, alternate)?;
        copy_tree_bounded(
            &destination(&f.paths, source),
            &destination(&f.paths, alternate),
            1024 * 1024,
            1000,
        )?;
        fs::write(f.scratch.path().join("mode"), "fail")?;
        let recovered = service.provision_android_for(
            &f.paths,
            RECIPE,
            alternate,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        assert_ne!(first, recovered);
        let tools = service.tools()?;
        assert!(tools.iter().any(|tool| tool.id == first && tool.ready));
        assert!(
            tools
                .iter()
                .any(|tool| tool.id == recovered && tool.ready && tool.bound)
        );
        assert_eq!(
            service.provision_status(&f.paths)?.expect("status").state,
            "succeeded"
        );
        Ok(())
    }

    #[test]
    fn wrong_reviewed_package_revision_is_not_published() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "wrong-revision")?;
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("Wrong Pkg.Revision"), "{error}");
        assert!(!destination(&f.paths, source).exists());
        Ok(())
    }

    #[test]
    fn unusable_emulator_is_not_published() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "bad-emulator")?;
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Android emulator is unavailable or unusable"),
            "{error}"
        );
        assert!(!destination(&f.paths, source).exists());
        Ok(())
    }

    #[test]
    fn failure_does_not_publish_or_replace_working_generation() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        let tiny = Limits {
            stage_bytes: 64 * 1024,
            retained_bytes: 1024 * 1024,
            min_free_bytes: 0,
            ..Limits::default()
        };
        let first = service.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            tiny,
            None,
        )?;
        let alternate = f.alternate_source(&mut service)?;
        service.consent_to_provision(RECIPE, alternate)?;
        fs::write(f.scratch.path().join("mode"), "tamper")?;
        assert!(
            service
                .provision_android_for(
                    &f.paths,
                    RECIPE,
                    alternate,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default(),
                    None
                )
                .unwrap_err()
                .to_string()
                .contains("Wrong AndroidVersion.ApiLevel")
        );
        assert!(!destination(&f.paths, alternate).exists());
        fs::write(f.scratch.path().join("mode"), "tampered-download")?;
        fs::write(
            f.paths
                .data
                .join("provision-download-cache/package.archive"),
            b"tampered",
        )?;
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                alternate,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("checksum mismatch"), "{error}");
        assert!(!destination(&f.paths, alternate).exists());
        fs::write(f.scratch.path().join("mode"), "quota")?;
        let plan = service.provision_plan_for(
            &f.paths,
            RECIPE,
            alternate,
            Host {
                kvm: true,
                java: true,
            },
            tiny,
        )?;
        assert_eq!(plan.stage_max_bytes, tiny.stage_bytes);
        assert_eq!(plan.retained_max_bytes, tiny.retained_bytes);
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                alternate,
                Host {
                    kvm: true,
                    java: true,
                },
                tiny,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("staging capacity"), "{error}");
        assert!(f.scratch.path().join("quota-created").exists());
        assert!(!destination(&f.paths, alternate).exists());
        assert!(
            fs::read_dir(f.paths.data.join("provision-staging"))?
                .all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".sweep.lock"))
        );
        assert!(
            service
                .tools()?
                .into_iter()
                .find(|tool| tool.id == first)
                .unwrap()
                .ready
        );
        assert!(
            service
                .tools()?
                .into_iter()
                .find(|tool| tool.id == first)
                .unwrap()
                .bound
        );
        Ok(())
    }

    #[test]
    fn running_installer_exceeding_stage_budget_is_stopped() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "quota-running")?;
        let tiny = Limits {
            stage_bytes: 64 * 1024,
            retained_bytes: 1024 * 1024,
            min_free_bytes: 0,
            ..Limits::default()
        };
        let start = Instant::now();
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                tiny,
                None,
            )
            .unwrap_err();
        assert!(error.to_string().contains("staging capacity"), "{error}");
        assert!(f.scratch.path().join("quota-created").exists());
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "installer was not stopped promptly"
        );
        assert!(!destination(&f.paths, source).exists());
        assert!(
            fs::read_dir(f.paths.data.join("provision-staging"))?
                .all(|entry| entry.is_ok_and(|entry| entry.file_name() == ".sweep.lock"))
        );
        Ok(())
    }

    #[test]
    fn download_cache_quota_is_separate_from_unpack_staging() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "download-quota")?;
        let tiny = Limits {
            download_bytes: 64 * 1024,
            ..Limits::default()
        };
        let error = service
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                tiny,
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("download cache capacity"),
            "{error}"
        );
        assert!(
            f.paths
                .data
                .join("provision-download-cache/archives/huge.zip")
                .exists()
        );
        assert!(!destination(&f.paths, source).exists());
        Ok(())
    }

    #[test]
    fn cancellation_reaps_partial_extraction_and_keeps_staging_separate() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "interrupt")?;
        std::thread::scope(|scope| -> Result<()> {
            let handle = scope.spawn(|| {
                service.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            let cancel = f.service(&f.project)?;
            for _ in 0..100 {
                if cancel
                    .provision_status(&f.paths)?
                    .is_some_and(|status| status.state == "running")
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            cancel.cancel_provision(&f.paths)?;
            assert!(handle.join().unwrap().is_err());
            Ok(())
        })?;
        assert!(!destination(&f.paths, source).exists());
        assert_eq!(
            f.service(&f.project)?
                .provision_status(&f.paths)?
                .unwrap()
                .state,
            "failed_or_interrupted"
        );
        Ok(())
    }

    #[test]
    fn concurrent_explicit_binding_is_not_replaced_by_publication() -> Result<()> {
        let f = Fixture::new()?;
        let mut installer = f.service(&f.project)?;
        let source = f.source_id(&mut installer)?;
        installer.set_provision_enabled(true)?;
        installer.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "slow")?;
        std::thread::scope(|scope| -> Result<()> {
            let handle = scope.spawn(|| {
                installer.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            let mut other = f.service(&f.project)?;
            for _ in 0..100 {
                if other
                    .provision_status(&f.paths)?
                    .is_some_and(|status| status.state == "running")
                {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            other.bind_tool("android-sdk", source, Some(0))?;
            let error = handle.join().expect("installer thread").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Tool binding changed during provisioning"),
                "{error}"
            );
            let tools = other.tools()?;
            assert!(tools.iter().any(|tool| tool.id == source && tool.bound));
            assert!(
                tools
                    .iter()
                    .any(|tool| tool.path == destination(&f.paths, source)
                        && tool.ready
                        && !tool.bound)
            );
            Ok(())
        })
    }

    #[test]
    fn binding_during_blocked_version_probe_cannot_be_overwritten() -> Result<()> {
        let f = Fixture::new()?;
        let mut installer = f.service(&f.project)?;
        let source = f.source_id(&mut installer)?;
        installer.set_provision_enabled(true)?;
        installer.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "version-hold")?;
        std::thread::scope(|scope| -> Result<()> {
            let handle = scope.spawn(|| {
                installer.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            let entered = f.scratch.path().join("version-entered");
            for _ in 0..100 {
                if entered.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(entered.exists(), "version probe did not start");
            let mut other = f.service(&f.project)?;
            other.bind_tool("android-sdk", source, Some(0))?;
            fs::write(f.scratch.path().join("version-release"), [])?;
            let error = handle.join().expect("installer thread").unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("Tool binding changed during provisioning"),
                "{error}"
            );
            assert!(
                other
                    .tools()?
                    .iter()
                    .any(|tool| tool.id == source && tool.bound)
            );
            assert!(
                other
                    .tools()?
                    .iter()
                    .any(|tool| tool.path == destination(&f.paths, source)
                        && tool.ready
                        && !tool.bound)
            );
            Ok(())
        })
    }

    #[test]
    fn queued_second_project_can_cancel_without_stopping_first() -> Result<()> {
        let f = Fixture::new()?;
        let mut first = f.service(&f.project)?;
        let source = f.source_id(&mut first)?;
        first.set_provision_enabled(true)?;
        first.consent_to_provision(RECIPE, source)?;
        let second_project = f.scratch.path().join("second-project");
        fs::create_dir_all(&second_project)?;
        let mut second = f.service(&second_project)?;
        second.set_provision_enabled(true)?;
        second.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "slow-lock")?;
        std::thread::scope(|scope| -> Result<()> {
            let first_run = scope.spawn(|| {
                first.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            let observer = f.service(&f.project)?;
            for _ in 0..100 {
                if observer
                    .provision_status(&f.paths)?
                    .is_some_and(|status| status.state == "running")
                {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                observer
                    .provision_status(&f.paths)?
                    .expect("first installer status")
                    .state,
                "running"
            );
            let second_observer = f.service(&second_project)?;
            let second_run = scope.spawn(|| {
                second.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            for _ in 0..100 {
                if second_observer
                    .provision_status(&f.paths)?
                    .is_some_and(|status| status.state == "queued")
                {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert_eq!(
                second_observer
                    .provision_status(&f.paths)?
                    .expect("queued status")
                    .state,
                "queued"
            );
            second_observer.cancel_provision(&f.paths)?;
            let error = second_run.join().expect("second installer").unwrap_err();
            assert!(error.to_string().contains("cancelled"), "{error}");
            assert!(first_run.join().expect("first installer").is_ok());
            Ok(())
        })
    }

    #[test]
    fn image_larger_than_task_scratch_budget_stays_in_shared_data() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "large-image")?;
        service.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let image = destination(&f.paths, source)
            .join("system-images/android-36/google_apis/x86_64/system.img");
        assert_eq!(fs::metadata(&image)?.len(), 128 * 1024);
        assert!(crate::resource::scratch_exceeded(
            image.parent().expect("image directory"),
            64 * 1024,
            100
        )?);
        assert!(image.starts_with(f.paths.data.join("tools")));
        assert!(!image.starts_with(f.paths.cache.join("workers")));
        Ok(())
    }

    #[test]
    fn reused_generation_rejects_broken_emulator_and_license() -> Result<()> {
        let f = Fixture::new()?;
        let mut first = f.service(&f.project)?;
        let source = f.source_id(&mut first)?;
        first.set_provision_enabled(true)?;
        first.consent_to_provision(RECIPE, source)?;
        first.provision_android_for(
            &f.paths,
            RECIPE,
            source,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let second_project = f.scratch.path().join("reuse-project");
        fs::create_dir(&second_project)?;
        let mut second = f.service(&second_project)?;
        second.set_provision_enabled(true)?;
        second.consent_to_provision(RECIPE, source)?;
        let managed = destination(&f.paths, source);
        fs::write(managed.join("emulator/emulator"), b"#!/bin/sh\nexit 17\n")?;
        let error = second
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Android emulator is unavailable or unusable"),
            "{error}"
        );
        fs::write(managed.join("emulator/emulator"), b"#!/bin/sh\nexit 0\n")?;
        fs::write(managed.join("licenses/android-sdk-license"), b"changed")?;
        let error = second
            .provision_android_for(
                &f.paths,
                RECIPE,
                source,
                Host {
                    kvm: true,
                    java: true,
                },
                Limits::default(),
                None,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("license digest differs"),
            "{error}"
        );
        assert!(
            first
                .tools()?
                .iter()
                .any(|tool| tool.bound && tool.path == managed)
        );
        Ok(())
    }

    #[test]
    fn dead_guardian_recovers_detached_installer_before_stage_cleanup() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "kill-guardian")?;
        assert!(
            service
                .provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default(),
                    None
                )
                .is_err()
        );
        let heartbeat = f.scratch.path().join("heartbeat");
        let before = fs::metadata(&heartbeat).map(|meta| meta.len()).unwrap_or(0);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            fs::metadata(&heartbeat).map(|meta| meta.len()).unwrap_or(0),
            before,
            "detached installer continued writing after guardian recovery"
        );
        assert!(!destination(&f.paths, source).exists());
        Ok(())
    }

    #[test]
    fn dead_payload_probe_reaps_detached_writer_and_keeps_old_generation() -> Result<()> {
        let f = Fixture::new()?;
        let mut first = f.service(&f.project)?;
        let original = f.source_id(&mut first)?;
        first.set_provision_enabled(true)?;
        first.consent_to_provision(RECIPE, original)?;
        first.provision_android_for(
            &f.paths,
            RECIPE,
            original,
            Host {
                kvm: true,
                java: true,
            },
            Limits::default(),
            None,
        )?;
        let other_project = f.scratch.path().join("other-project");
        fs::create_dir(&other_project)?;
        let mut other = f.service(&other_project)?;
        let second = f.alternate_source(&mut other)?;
        other.set_provision_enabled(true)?;
        other.consent_to_provision(RECIPE, second)?;
        fs::write(f.scratch.path().join("mode"), "kill-probe-guardian")?;
        assert!(
            other
                .provision_android_for(
                    &f.paths,
                    RECIPE,
                    second,
                    Host {
                        kvm: true,
                        java: true
                    },
                    Limits::default(),
                    None
                )
                .is_err()
        );
        let heartbeat = f.scratch.path().join("probe-heartbeat");
        let before = fs::metadata(&heartbeat)?.len();
        assert!(before > 0, "detached probe did not start writing");
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            fs::metadata(&heartbeat)?.len(),
            before,
            "detached probe continued writing after guardian recovery"
        );
        assert!(!destination(&f.paths, second).exists());
        assert!(
            first
                .tools()?
                .iter()
                .any(|tool| tool.bound && tool.path == destination(&f.paths, original))
        );
        Ok(())
    }

    #[test]
    fn cancelling_installer_reaps_detached_descendant() -> Result<()> {
        let f = Fixture::new()?;
        let mut service = f.service(&f.project)?;
        let source = f.source_id(&mut service)?;
        service.set_provision_enabled(true)?;
        service.consent_to_provision(RECIPE, source)?;
        fs::write(f.scratch.path().join("mode"), "detached")?;
        std::thread::scope(|scope| -> Result<()> {
            let run = scope.spawn(|| {
                service.provision_android_for(
                    &f.paths,
                    RECIPE,
                    source,
                    Host {
                        kvm: true,
                        java: true,
                    },
                    Limits::default(),
                    None,
                )
            });
            let observer = f.service(&f.project)?;
            let heartbeat = f.scratch.path().join("heartbeat");
            for _ in 0..100 {
                if heartbeat.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(heartbeat.exists(), "detached fixture did not start");
            observer.cancel_provision(&f.paths)?;
            assert!(run.join().expect("installer thread").is_err());
            let before = fs::metadata(&heartbeat)?.len();
            thread::sleep(Duration::from_millis(300));
            assert_eq!(
                fs::metadata(&heartbeat)?.len(),
                before,
                "detached installer continued after cancellation"
            );
            Ok(())
        })
    }
}
