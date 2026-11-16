//! Durable local prerequisites. Only bounded probes clear a dependency wait.
use super::{Service, VersionedTask, execution::Run, lifecycle, now_ms, require};
use crate::instance::Paths;
use crate::storage::{Error, Result};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Read,
    os::unix::fs::{FileTypeExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub kind: String,
    pub item: String,
    pub version: Option<String>,
    pub scope: String,
    pub stage: String,
    pub reproduction: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Dependency {
    pub id: i64,
    pub requirement: Requirement,
    pub state: String,
    pub observed: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbePurpose {
    Admission,
    ExplicitRecheck,
}

impl Requirement {
    fn validate(&self) -> Result<()> {
        require(
            matches!(
                self.kind.as_str(),
                "executable" | "sdk_component" | "capability"
            ),
            "Requirement kind must be executable, sdk_component or capability",
        )?;
        require(
            matches!(self.scope.as_str(), "task" | "project" | "host"),
            "Requirement scope must be task, project or host",
        )?;
        require(
            matches!(
                self.stage.as_str(),
                "baseline" | "execution" | "check" | "review" | "validation"
            ),
            "Invalid affected stage",
        )?;
        for (value, limit) in [(&self.item, 160), (&self.reproduction, 1024)] {
            require(
                !value.trim().is_empty() && value.len() <= limit && !value.contains('\0'),
                "Requirement field is empty or too long",
            )?;
        }
        if let Some(version) = &self.version {
            require(
                !version.trim().is_empty() && version.len() <= 80 && !version.contains('\0'),
                "Invalid requirement version",
            )?;
        }
        require(
            !self.item.contains('\n') && !self.reproduction.contains('\0'),
            "Invalid requirement text",
        )?;
        if self.kind == "executable" {
            require(
                !self.item.contains('/') || Path::new(&self.item).is_absolute(),
                "Executable must be a name on PATH or an absolute path",
            )?;
        }
        if self.kind == "sdk_component" {
            require(
                self.item.split(';').all(|part| {
                    !part.is_empty() && part != "." && part != ".." && !part.contains('/')
                }),
                "SDK component must use sdkmanager semicolon segments",
            )?;
        }
        if self.kind == "capability" {
            require(
                Path::new(&self.item).is_absolute(),
                "Capability must be an absolute local path",
            )?;
            require(
                self.version.is_none(),
                "Capability paths do not have a version; declare a versioned executable or SDK component separately",
            )?;
        }
        Ok(())
    }
}

impl Service {
    /// Record a live environment loss before the runner settles into dependency wait.
    /// The guardian has already reaped any subprocess before that settlement.
    pub fn request_tool_environment_wait(&mut self, run: &Run, reason: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2 AND project_id=?3 AND execution_token=?4 AND state='running')",params![run.attempt,run.task,self.project_id,run.token],|r|r.get::<_,bool>(0))?,"Tool environment wait lost its attempt")?;
        tx.execute("INSERT INTO task_requirements(project_id,task_id,attempt_id,kind,item,scope,stage,reproduction,requested_by_kind,requested_by,state,observed,created_at_ms) VALUES (?1,?2,?3,'registered_tool','environment','project','execution',?4,'system','ktask','waiting',?4,?5) ON CONFLICT DO UPDATE SET attempt_id=excluded.attempt_id,reproduction=excluded.reproduction,state='waiting',observed=excluded.observed,checked_at_ms=NULL",params![self.project_id,run.task,run.attempt,reason,now_ms()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn dependencies(&self, task: i64) -> Result<Vec<Dependency>> {
        let mut statement = self.db.connection().prepare("SELECT id,kind,item,required_version,scope,stage,reproduction,state,observed FROM task_requirements WHERE project_id=?1 AND task_id=?2 ORDER BY id")?;
        let rows = statement.query_map(params![self.project_id, task], |r| {
            Ok(Dependency {
                id: r.get(0)?,
                requirement: Requirement {
                    kind: r.get(1)?,
                    item: r.get(2)?,
                    version: r.get(3)?,
                    scope: r.get(4)?,
                    stage: r.get(5)?,
                    reproduction: r.get(6)?,
                },
                state: r.get(7)?,
                observed: r.get(8)?,
            })
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// A declaration is allowed before the first run. A live worker uses the separate
    /// capability method, which proves task, attempt and execution token in one transaction.
    pub fn declare_requirement(&mut self, target: VersionedTask, req: &Requirement) -> Result<i64> {
        req.validate()?;
        let task = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        require(
            task.id == target.id
                && task.version == target.version
                && task.state() == lifecycle::State::Pending,
            "Declare prerequisites on the pending current task before execution",
        )?;
        self.insert_requirement(task.id, None, None, Some(target.version), req, None)
    }

    pub fn request_live_requirement(
        &mut self,
        task: i64,
        attempt: i64,
        token: &str,
        req: &Requirement,
        environment: &str,
        effective_path: &str,
    ) -> Result<i64> {
        req.validate()?;
        require(
            req.kind != "executable" || !req.item.contains('/'),
            "Worker executable requests must name a tool on the supervisor PATH",
        )?;
        let current = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        require(
            current.id == task
                && current
                    .attempts
                    .last()
                    .is_some_and(|a| a.id == attempt && a.state == "running"),
            "Worker requirement is stale",
        )?;
        let owns: bool = self.db.connection().query_row("SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND task_id=?2 AND project_id=?3 AND execution_token=?4 AND state='running')",params![attempt,task,self.project_id,token],|r|r.get(0))?;
        require(owns, "Worker requirement capability is stale")?;
        let lower = req.reproduction.to_ascii_lowercase();
        let denied = lower.contains("permission denied")
            || lower.contains("operation not permitted")
            || lower.contains("read-only file system")
            || lower.contains("command not found");
        let bound = if denied { self.tools()? } else { Vec::new() };
        let host_present = if req.kind == "executable" {
            executable_on_path_in(
                &req.item,
                &self.project_root()?,
                std::ffi::OsStr::new(effective_path),
            )
            .is_ok()
        } else if req.kind == "sdk_component" {
            std::env::var_os("ANDROID_SDK_ROOT")
                .or_else(|| std::env::var_os("ANDROID_HOME"))
                .is_some_and(|root| {
                    PathBuf::from(root)
                        .join(req.item.replace(';', "/"))
                        .exists()
                })
                || bound.iter().any(|tool| {
                    tool.bound
                        && tool.ready
                        && tool.name == "android-sdk"
                        && tool.components.contains(&req.item)
                })
        } else {
            false
        };
        let observed = (denied && host_present).then(|| {
            format!(
                "PROVIDER_DENIED|{environment}|Host tool is present but inaccessible to the worker"
            )
        });
        self.insert_requirement(
            task,
            Some(attempt),
            Some(token),
            None,
            req,
            observed.as_deref(),
        )
    }

    fn insert_requirement(
        &mut self,
        task: i64,
        attempt: Option<i64>,
        token: Option<&str>,
        expected_version: Option<i64>,
        req: &Requirement,
        observed: Option<&str>,
    ) -> Result<i64> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(version) = expected_version {
            require(tx.query_row("SELECT EXISTS(SELECT 1 FROM current_tasks WHERE id=?1 AND project_id=?2 AND row_version=?3 AND status='ready')",params![task,self.project_id,version],|r|r.get::<_,bool>(0))?,"Current task changed before requirement declaration")?;
        }
        if let Some(token) = token {
            require(tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts a JOIN current_tasks c ON c.latest_attempt_id=a.id WHERE a.id=?1 AND a.task_id=?2 AND a.project_id=?3 AND a.execution_token=?4 AND a.state='running' AND c.id=?2)",params![attempt,task,self.project_id,token],|r|r.get::<_,bool>(0))?,"Worker requirement capability is stale")?;
        }
        let existing: Option<(i64,String)> = tx.query_row("SELECT id,state FROM task_requirements WHERE task_id=?1 AND kind=?2 AND item=?3 AND coalesce(required_version,'')=coalesce(?4,'') AND scope=?5 AND stage=?6",params![task,req.kind,req.item,req.version,req.scope,req.stage],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
        let count: i64 = tx.query_row(
            "SELECT count(*) FROM task_requirements WHERE task_id=?1",
            [task],
            |r| r.get(0),
        )?;
        require(
            existing.is_some() || count < 16,
            "Task already has 16 requirements",
        )?;
        let changed = tx.execute("INSERT INTO task_requirements(project_id,task_id,attempt_id,kind,item,required_version,scope,stage,reproduction,requested_by_kind,requested_by,state,observed,created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'unverified',?13,?12) ON CONFLICT DO UPDATE SET attempt_id=excluded.attempt_id,reproduction=excluded.reproduction,requested_by_kind=excluded.requested_by_kind,requested_by=excluded.requested_by,created_at_ms=excluded.created_at_ms,state='unverified',observed=excluded.observed,checked_at_ms=NULL WHERE task_requirements.state='satisfied'",params![self.project_id,task,attempt,req.kind,req.item,req.version,req.scope,req.stage,req.reproduction,self.actor.kind.as_str(),self.actor.name,now_ms(),observed])?;
        let id: i64 = tx.query_row("SELECT id FROM task_requirements WHERE task_id=?1 AND kind=?2 AND item=?3 AND coalesce(required_version,'')=coalesce(?4,'') AND scope=?5 AND stage=?6",params![task,req.kind,req.item,req.version,req.scope,req.stage],|r|r.get(0))?;
        if changed > 0 {
            tx.execute(
                "UPDATE tasks SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
                params![task, now_ms()],
            )?;
            tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'dependency.requested','warning',?5)",params![self.project_id,task,attempt,now_ms(),format!("{} {} required for {}: {}",req.kind,req.item,req.stage,req.reproduction)])?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn pending_dependency(&self, attempt: i64) -> Result<Option<String>> {
        pending_reason(self.db.connection(), attempt)
    }

    /// Every launch checks declared prerequisites in its own environment. A recorded
    /// dependency wait still needs an explicit Recheck before another launch is allowed.
    pub fn admit_dependencies_for_run(
        &mut self,
        target: VersionedTask,
        scratch_root: &Path,
        supervisor: &Path,
        paths: &Paths,
    ) -> Result<bool> {
        let current = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        require(
            current.id == target.id && current.version == target.version,
            "Current task changed before prerequisite admission",
        )?;
        let dependencies = self.dependencies(target.id)?;
        if current.state() == lifecycle::State::NeedsDependency
            || dependencies.iter().any(|d| d.state == "waiting")
        {
            return Ok(false);
        }
        require(
            matches!(
                current.state(),
                lifecycle::State::Pending
                    | lifecycle::State::Paused
                    | lifecycle::State::Failed
                    | lifecycle::State::Running
                    | lifecycle::State::NeedsInput
            ),
            "Prerequisite admission needs a runnable current task",
        )?;
        self.ensure_requirement_bindings(target.id, scratch_root, supervisor, paths)?;
        if let Some((name, id)) = self.unready_bound_tool()? {
            self.wait_for_registered_tool(target,&name,&format!("Bound tool {name} generation #{id} is missing or changed; refresh or rebind it, then Recheck"))?;
            return Ok(false);
        }
        if dependencies.is_empty() {
            return Ok(true);
        }
        self.probe_dependencies(
            target,
            scratch_root,
            supervisor,
            paths,
            ProbePurpose::Admission,
        )
    }

    /// Called only after the guardian has stopped and reaped the worker. This freezes the
    /// active clock and preserves the attempt phase, repair count and provider session.
    pub fn wait_for_dependency(&mut self, attempt: i64, token: &str, reason: &str) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let task: i64 = tx.query_row(
            "SELECT task_id FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running'",
            params![attempt, token],
            |r| r.get(0),
        )?;
        settle_dependency_wait(&tx, self.project_id, attempt, task, now, reason)?;
        tx.commit()?;
        Ok(())
    }

    /// A failed deterministic probe retains its observed result. A successful probe moves a
    /// live attempt to Paused; only an explicit Run/Resume may continue it.
    pub fn recheck_dependencies(
        &mut self,
        target: VersionedTask,
        scratch_root: &Path,
        supervisor: &Path,
        paths: &Paths,
    ) -> Result<bool> {
        let current = self
            .current_task()?
            .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
        require(
            current.id == target.id && current.version == target.version,
            "Current task changed before recheck",
        )?;
        require(
            matches!(
                current.state(),
                lifecycle::State::Pending
                    | lifecycle::State::Paused
                    | lifecycle::State::NeedsDependency
            ),
            "Wait until the worker stops before rechecking",
        )?;
        require(
            !self.dependencies(target.id)?.is_empty(),
            "No requirements to recheck",
        )?;
        self.ensure_requirement_bindings(target.id, scratch_root, supervisor, paths)?;
        self.probe_dependencies(
            target,
            scratch_root,
            supervisor,
            paths,
            ProbePurpose::ExplicitRecheck,
        )
    }

    fn probe_dependencies(
        &mut self,
        target: VersionedTask,
        scratch_root: &Path,
        supervisor: &Path,
        paths: &Paths,
        purpose: ProbePurpose,
    ) -> Result<bool> {
        let dependencies = self.dependencies(target.id)?;
        let scratch = crate::scratch::Scratch::new(scratch_root)?;
        let project = self.project_root()?;
        let resolved = self.resolve_tools(paths);
        let worker_network = self.agent_network()?;
        let mut all_pass = true;
        let mut results = Vec::new();
        for dependency in dependencies {
            let result: String = if dependency.requirement.kind == "registered_tool" {
                resolved.as_ref().map_or_else(
                    |error| format!("MISSING: {error}"),
                    |_| "OK: bound tool generation verified".into(),
                )
            } else if let Ok(tools) = &resolved {
                let provider_denial = dependency
                    .observed
                    .as_deref()
                    .and_then(|value| value.strip_prefix("PROVIDER_DENIED|"))
                    .and_then(|value| value.split_once('|'))
                    .is_some();
                probe(
                    &dependency.requirement,
                    scratch.path(),
                    &project,
                    supervisor,
                    &tools.env,
                    if provider_denial {
                        Some(ProbePolicy::Restricted {
                            writable_roots: &tools.writable_roots,
                            installations: &tools.installations,
                            network: worker_network,
                        })
                    } else if tools.installations.is_empty() {
                        None
                    } else {
                        Some(ProbePolicy::Protected(&tools.installations))
                    },
                )
            } else {
                format!(
                    "MISSING: {}",
                    resolved.as_ref().err().expect("tool resolution failed")
                )
            };
            let result = preserve_provider_denial(
                result,
                dependency.observed.as_deref(),
                resolved
                    .as_ref()
                    .ok()
                    .map(|tools| tools.fingerprint.as_str()),
            );
            let result: String = result.chars().take(1024).collect();
            let passed = result.starts_with("OK:");
            all_pass &= passed;
            results.push((dependency.id, result, passed));
        }
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        require(tx.execute("UPDATE tasks SET row_version=row_version+1,updated_at_ms=?3 WHERE id=?1 AND row_version=?2",params![target.id,target.version,now])? == 1,"Current task changed during recheck")?;
        for (id, result, passed) in results {
            tx.execute(
                "UPDATE task_requirements SET state=?2,observed=?3,checked_at_ms=?4 WHERE id=?1",
                params![
                    id,
                    if passed { "satisfied" } else { "waiting" },
                    result,
                    now
                ],
            )?;
        }
        if all_pass && purpose == ProbePurpose::ExplicitRecheck {
            tx.execute("UPDATE attempts SET state='paused',wait_reason=NULL,waiting_since_ms=NULL,report='Dependencies verified; Resume continues this attempt' WHERE task_id=?1 AND state='waiting' AND wait_reason='dependency'",[target.id])?;
        } else {
            if !all_pass {
                tx.execute("UPDATE attempts SET state='waiting',wait_reason='dependency',waiting_since_ms=?2,resume_at_ms=NULL,report='Dependency probe failed; recheck after external setup' WHERE task_id=?1 AND (state='paused' OR (state='waiting' AND wait_reason<>'dependency'))",params![target.id,now])?;
            }
        }
        tx.execute("INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,'dependency.rechecked',?4,?5)",params![self.project_id,target.id,now,if all_pass {"info"} else {"warning"},if all_pass {"All prerequisites verified; Run resumes the task"} else {"Prerequisites still missing; inspect Current readiness"}])?;
        tx.commit()?;
        Ok(all_pass)
    }

    fn wait_for_registered_tool(
        &mut self,
        target: VersionedTask,
        name: &str,
        reason: &str,
    ) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        require(tx.execute("UPDATE tasks SET row_version=row_version+1,updated_at_ms=?3 WHERE id=?1 AND row_version=?2",params![target.id,target.version,now])?==1,"Current task changed before tool wait")?;
        tx.execute("INSERT INTO task_requirements(project_id,task_id,kind,item,scope,stage,reproduction,requested_by_kind,requested_by,state,observed,checked_at_ms,created_at_ms) VALUES (?1,?2,'registered_tool',?3,'project','baseline',?4,'system','ktask','waiting',?4,?5,?5) ON CONFLICT DO UPDATE SET state='waiting',observed=excluded.observed,checked_at_ms=excluded.checked_at_ms,reproduction=excluded.reproduction",params![self.project_id,target.id,name,reason,now])?;
        tx.execute("UPDATE attempts SET state='waiting',wait_reason='dependency',waiting_since_ms=?2,resume_at_ms=NULL,report=?3 WHERE task_id=?1 AND (state='paused' OR (state='waiting' AND wait_reason IN ('rate_limit','provider_unavailable')))",params![target.id,now,reason])?;
        tx.execute("UPDATE execution_sessions SET state='waiting',reason=?2,updated_at_ms=?3 WHERE project_id=?1 AND state IN ('running','waiting','resume_requested')",params![self.project_id,reason,now])?;
        tx.execute("INSERT INTO events(project_id,task_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,'dependency.requested','warning',?4)",params![self.project_id,target.id,now,reason])?;
        tx.commit()?;
        Ok(())
    }
}

fn preserve_provider_denial(
    result: String,
    observed: Option<&str>,
    environment: Option<&str>,
) -> String {
    let denied_environment = observed
        .and_then(|value| value.strip_prefix("PROVIDER_DENIED|"))
        .and_then(|value| value.split_once('|'))
        .map(|(fingerprint, _)| fingerprint);
    match (denied_environment, environment) {
        (Some(_), Some(_)) if result.starts_with("OK:") => result,
        (Some(_), Some(current)) => format!(
            "PROVIDER_DENIED|{current}|{}",
            "Worker access is still denied; repair the tool or its permitted runtime directory, then Recheck"
        ),
        (Some(previous), None) => format!(
            "PROVIDER_DENIED|{previous}|Tool environment is unavailable; repair or rebind it, then Recheck"
        ),
        _ => result,
    }
}

fn pending_reason(db: &rusqlite::Connection, attempt: i64) -> Result<Option<String>> {
    db.query_row("SELECT kind || ' ' || item || coalesce(' ' || required_version,'') || ': ' || reproduction FROM task_requirements WHERE attempt_id=?1 AND state<>'satisfied' ORDER BY id LIMIT 1",[attempt],|r|r.get(0)).optional().map_err(Into::into)
}

/// Shared by normal worker settlement and orphan recovery, both after workers are reaped.
pub(super) fn settle_dependency_wait(
    tx: &Transaction<'_>,
    project: i64,
    attempt: i64,
    task: i64,
    now: i64,
    reason: &str,
) -> Result<()> {
    require(
        pending_reason(tx, attempt)?.is_some(),
        "Dependency request disappeared before settlement",
    )?;
    require(tx.execute("UPDATE attempts SET state='waiting',wait_reason='dependency',waiting_since_ms=?2,resume_at_ms=NULL,report=?3 WHERE id=?1 AND state='running'",params![attempt,now,reason])? == 1,"Attempt changed before dependency wait")?;
    // A baseline failure provisionally opened round one before the worker could identify
    // an undeclared tool. A wait settles that discovery, not a completed code repair.
    let refunded = tx.execute(
        "UPDATE attempt_progress SET repair_round=0,
            context=replace(context,'Supervisor checks failed, repair round 1/10.',
                'Supervisor checks failed before implementation; dependency wait recorded.')
         WHERE attempt_id=?1 AND phase='fix' AND repair_round=1
           AND instr(context,?2)>0",
        params![attempt, super::progress::BASELINE_FAILURE_NOTE],
    )?;
    if refunded > 0 {
        tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'workflow.progress','info','Dependency discovery returned the provisional baseline repair round; repair round 0/10')",params![project,task,attempt,now])?;
    }
    tx.execute("UPDATE attempt_steps SET state='failed',finished_at_ms=?2,report=?3 WHERE attempt_id=?1 AND state='running'",params![attempt,now,reason])?;
    tx.execute(
        "UPDATE tasks SET row_version=row_version+1,updated_at_ms=?2 WHERE id=?1",
        params![task, now],
    )?;
    tx.execute("INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,?3,?4,'attempt.waiting','warning',?5)",params![project,task,attempt,now,reason])?;
    Ok(())
}

pub(super) fn orphan_dependency_reason(
    tx: &Transaction<'_>,
    attempt: i64,
) -> Result<Option<String>> {
    pending_reason(tx, attempt)
}

pub(super) enum ProbePolicy<'a> {
    Protected(&'a [PathBuf]),
    Restricted {
        writable_roots: &'a [PathBuf],
        installations: &'a [PathBuf],
        network: bool,
    },
}

pub(super) fn probe(
    req: &Requirement,
    scratch: &Path,
    project: &Path,
    supervisor: &Path,
    env: &[(String, String)],
    policy: Option<ProbePolicy<'_>>,
) -> String {
    let mut args = vec![
        "__dependency_probe".into(),
        req.kind.clone(),
        req.item.clone(),
    ];
    if let Some(version) = &req.version {
        args.push(version.clone());
    }
    let mut spec = crate::runner::process::Invocation::new(
        supervisor.to_owned(),
        args,
        project.to_owned(),
        scratch.join("guardian"),
        scratch.join("worker.lock"),
    );
    spec.env.extend(env.iter().cloned());
    let sandbox = match policy {
        None => None,
        Some(ProbePolicy::Protected(installations)) => Some(
            crate::runner::sandbox::Sandbox::for_installations(installations),
        ),
        Some(ProbePolicy::Restricted {
            writable_roots,
            installations,
            network,
        }) => Some(
            crate::runner::sandbox::WorkspaceWrite::new(
                project,
                writable_roots,
                installations,
                network,
            )
            .probe_mount(),
        ),
    };
    if let Some(sandbox) = sandbox {
        spec.sandbox = match sandbox {
            Ok(sandbox) => Some(sandbox),
            Err(error) => return format!("DENIED: provider context unavailable: {error}"),
        };
    }
    let (code, stdout, stderr) = match crate::runner::process::capture_probe(supervisor, &spec) {
        Ok(result) => result,
        Err(error) => return format!("TRANSIENT: {error}"),
    };
    if req.kind != "executable" {
        return if code == 0 {
            String::from_utf8_lossy(&stdout).trim().to_owned()
        } else {
            format!(
                "TRANSIENT: local probe exited {code}: {}",
                String::from_utf8_lossy(&stderr).trim()
            )
        };
    }
    if code == 125
        && let Some(observed) = String::from_utf8_lossy(&stdout)
            .trim()
            .strip_prefix("KTASK_PROBE_LOOKUP_ERROR: ")
    {
        return observed.to_owned();
    }
    let output = format!(
        "{} {}",
        String::from_utf8_lossy(&stdout),
        String::from_utf8_lossy(&stderr)
    );
    classify_version(code, &output, req.version.as_deref())
}

/// Runs inside the guardian deadline. The selected path remains a symlink so a tool
/// that dispatches on argv[0] observes the same invocation name as the task would.
pub fn probe_executable(item: &str) -> ExitCode {
    let selected = std::env::current_dir()
        .map_err(|error| format!("TRANSIENT: project directory: {error}"))
        .and_then(|project| {
            if item.contains('/') {
                executable_candidate(PathBuf::from(item))
            } else {
                executable_on_path(item, &project)
            }
        });
    let path = match selected {
        Ok(path) => path,
        Err(observed) => {
            println!("KTASK_PROBE_LOOKUP_ERROR: {observed}");
            return ExitCode::from(125);
        }
    };
    match Command::new(path).arg("--version").status() {
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            let category = match error.kind() {
                std::io::ErrorKind::PermissionDenied => "DENIED",
                std::io::ErrorKind::NotFound => "MISSING",
                _ => "TRANSIENT",
            };
            println!("KTASK_PROBE_LOOKUP_ERROR: {category}: {error}");
            ExitCode::from(125)
        }
    }
}

fn classify_version(code: i32, output: &str, expected: Option<&str>) -> String {
    let output = output.trim();
    let brief: String = output.chars().take(256).collect();
    if code != 0 {
        return format!("FAILED: --version exited {code}: {brief}");
    }
    if let Some(version) = expected
        && !output
            .split_whitespace()
            .any(|word| word.trim_start_matches('v').trim_end_matches(',') == version)
    {
        return format!("WRONG_VERSION: expected {version}; observed {brief}");
    }
    format!("OK: {brief}")
}

fn executable_candidate(path: PathBuf) -> std::result::Result<PathBuf, String> {
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_file() && metadata.permissions().mode() & 0o111 != 0 => {
            Ok(path)
        }
        Ok(_) => Err(format!("DENIED: {} is not executable", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(format!("DENIED: {e}")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!("MISSING: {e}")),
        Err(e) => Err(format!("TRANSIENT: executable metadata: {e}")),
    }
}

fn executable_on_path(name: &str, project: &Path) -> std::result::Result<PathBuf, String> {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".into());
    executable_on_path_in(name, project, &path)
}

fn executable_on_path_in(
    name: &str,
    project: &Path,
    path: &std::ffi::OsStr,
) -> std::result::Result<PathBuf, String> {
    let mut denied = false;
    for directory in std::env::split_paths(path) {
        let directory = if directory.is_absolute() {
            directory
        } else {
            project.join(directory)
        };
        match executable_candidate(directory.join(name)) {
            Ok(path) => return Ok(path),
            Err(observed) if observed.starts_with("DENIED:") => denied = true,
            Err(observed) if observed.starts_with("MISSING:") => {}
            Err(observed) => return Err(observed),
        }
    }
    Err(if denied {
        format!("DENIED: {name} exists on PATH but cannot be executed")
    } else {
        format!("MISSING: {name} is not executable on PATH")
    })
}

/// Runs inside the managed guardian, so even a blocking device or metadata filesystem
/// cannot hold the owner's Recheck past the guardian deadline.
pub fn probe_local_file(kind: &str, item: &str, version: Option<&str>) -> String {
    match kind {
        "sdk_component" => probe_sdk_component(item, version),
        "capability" => probe_capability(item),
        _ => "FAILED: unknown local probe kind".into(),
    }
}

fn probe_sdk_component(item: &str, version: Option<&str>) -> String {
    let root = std::env::var_os("ANDROID_SDK_ROOT").or_else(|| std::env::var_os("ANDROID_HOME"));
    let Some(root) = root else {
        return "MISSING: set ANDROID_SDK_ROOT or ANDROID_HOME to an installed SDK".into();
    };
    probe_sdk_component_at(&PathBuf::from(root), item, version)
}

pub(super) fn probe_sdk_component_at(root: &Path, item: &str, version: Option<&str>) -> String {
    let path = root.join(item.replace(';', "/"));
    match fs::metadata(&path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return format!("MISSING: {} is not an SDK directory", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return format!("DENIED: {e}");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!(
                "MISSING: {} is absent; install externally with sdkmanager '{}': {e}",
                path.display(),
                item
            );
        }
        Err(e) => return format!("TRANSIENT: SDK metadata: {e}"),
    }
    if let Some(expected) = version {
        let properties = path.join("source.properties");
        let metadata = match fs::metadata(&properties) {
            Ok(metadata) if metadata.is_file() => metadata,
            Ok(_) => return "FAILED: SDK version metadata is not a regular file".into(),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return format!("DENIED: {e}");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return format!("MISSING: SDK version metadata: {e}");
            }
            Err(e) => return format!("TRANSIENT: SDK version metadata: {e}"),
        };
        if metadata.len() > 4096 {
            return "TRANSIENT: SDK version metadata exceeds 4 KiB".into();
        }
        let file = match fs::OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_NONBLOCK)
            .open(&properties)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return format!("DENIED: {e}");
            }
            Err(e) => return format!("TRANSIENT: SDK version metadata: {e}"),
        };
        let mut bytes = Vec::new();
        if let Err(e) = file.take(4097).read_to_end(&mut bytes) {
            return format!("TRANSIENT: SDK version metadata: {e}");
        }
        if bytes.len() > 4096 {
            return "TRANSIENT: SDK version metadata exceeds 4 KiB".into();
        }
        let properties = match std::str::from_utf8(&bytes) {
            Ok(properties) => properties,
            Err(e) => return format!("FAILED: SDK version metadata is not UTF-8: {e}"),
        };
        let installed = properties
            .lines()
            .find_map(|line| line.trim().strip_prefix("Pkg.Revision=").map(str::trim));
        if installed != Some(expected) {
            return format!(
                "WRONG_VERSION: expected {expected}; observed {}",
                installed.unwrap_or("unknown")
            );
        }
    }
    "OK: installed SDK component found locally".into()
}

fn probe_capability(item: &str) -> String {
    let metadata = match fs::metadata(item) {
        Ok(metadata) => metadata,
        Err(e) => return capability_error(item, e),
    };
    if metadata.file_type().is_fifo() {
        return "FAILED: a FIFO is not a stable local capability".into();
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).custom_flags(nix::libc::O_NONBLOCK);
    if item.starts_with("/dev/") {
        options.write(true);
    }
    match options.open(item) {
        Ok(_) => "OK: local capability is accessible".into(),
        Err(e) => capability_error(item, e),
    }
}

fn capability_error(item: &str, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        format!("DENIED: {error}")
    } else if error.kind() == std::io::ErrorKind::NotFound && item.starts_with("/dev/") {
        format!("ABSENT_HARDWARE: {error}")
    } else if error.kind() == std::io::ErrorKind::NotFound
        && item.to_ascii_lowercase().contains("license")
    {
        format!("MISSING_LICENSE: {error}")
    } else if error.kind() == std::io::ErrorKind::NotFound {
        format!("MISSING: {error}")
    } else {
        format!("TRANSIENT: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn provider_denial_requires_a_successful_restricted_recheck() {
        let denied = "PROVIDER_DENIED|old|Host tool is present but inaccessible to the worker";
        assert_eq!(
            preserve_provider_denial("OK: rg found".into(), Some(denied), Some("old")),
            "OK: rg found"
        );
        assert_eq!(
            preserve_provider_denial("OK: rg found".into(), Some(denied), Some("new")),
            "OK: rg found"
        );
        assert!(
            preserve_provider_denial("MISSING: rg".into(), Some(denied), Some("new"))
                .starts_with("PROVIDER_DENIED|new|")
        );
        assert!(
            preserve_provider_denial("MISSING: rg".into(), Some(denied), None)
                .starts_with("PROVIDER_DENIED|old|")
        );
    }

    #[test]
    fn local_probes_distinguish_denied_license_hardware_and_failed_command() {
        let root = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let mut req = Requirement {
            kind: "capability".into(),
            item: root
                .path()
                .join("licenses/android-sdk-license")
                .to_string_lossy()
                .into_owned(),
            version: None,
            scope: "host".into(),
            stage: "execution".into(),
            reproduction: "license absent".into(),
        };
        assert!(probe_local_file("capability", &req.item, None).starts_with("MISSING_LICENSE:"));
        req.item = format!("/dev/ktask-test-absent-{}", std::process::id());
        assert!(probe_local_file("capability", &req.item, None).starts_with("ABSENT_HARDWARE:"));
        let tool = root.path().join("tool");
        fs::write(
            &tool,
            "#!/bin/sh\necho 'rg: command not found' >&2\nexit 3\n",
        )
        .unwrap();
        assert!(
            executable_candidate(tool.clone())
                .unwrap_err()
                .starts_with("DENIED:")
        );
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(classify_version(3, "rg: command not found", None).starts_with("FAILED:"));
        assert!(classify_version(0, "tool 2.0", Some("1.0")).starts_with("WRONG_VERSION:"));
    }
}
