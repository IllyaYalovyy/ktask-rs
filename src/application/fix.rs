//! Agent fixes: owner-started repairs of one project by an agent, and the record they leave.
//! A fix shares the run record of agent reports ([`super::investigation`]): ID, state,
//! model, limits, requester, progress, usage and stop. Its question is the instruction.
//! These commands add what only a fix has: its steps, changes and verification probes.
use super::investigation::{
    AgentKind, Investigation, InvestigationLimits, InvestigationState, NewInvestigation, clip,
    insert_run,
};
use super::*;
use serde::Serialize;

/// Longest stored action, result or detail.
const ENTRY_CHARS: usize = 4000;
/// Entries of one kind a fix records, so a runaway agent cannot grow the database.
const ENTRIES: i64 = 1000;

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// Who performed a step of a fix.
    pub enum StepSource {
        Supervisor => "supervisor",
        Ktask => "ktask",
        Agent => "agent",
        Git => "git",
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// What came of a step. Refused and left-for-owner steps were not applied.
    pub enum StepStatus {
        Applied => "applied",
        Failed => "failed",
        Refused => "refused",
        LeftForOwner => "left_for_owner",
    }
}

impl StepStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Refused => "refused, left for the owner",
            Self::LeftForOwner => "left for the owner",
        }
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ChangeKind {
        File => "file",
        Setting => "setting",
        Stash => "stash",
        Commit => "commit",
        RecoveryRef => "recovery_ref",
        Uncommitted => "uncommitted",
    }
}

impl ChangeKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Setting => "setting",
            Self::Stash => "stash",
            Self::Commit => "commit",
            Self::RecoveryRef => "recovery ref",
            Self::Uncommitted => "uncommitted",
        }
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// What a verification repeats.
    pub enum ProbeKind {
        Check => "check",
        Dependency => "dependency",
        WorkTree => "work_tree",
        Command => "command",
    }
}

impl ProbeKind {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Check => "check",
            Self::Dependency => "dependency probe",
            Self::WorkTree => "work tree",
            Self::Command => "command",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FixEntry {
    pub sequence: i64,
    pub recorded_at_ms: i64,
    pub source: StepSource,
    pub action: String,
    pub result: String,
    pub status: StepStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Change {
    pub kind: ChangeKind,
    pub target: String,
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Probe {
    pub ordinal: i64,
    pub kind: ProbeKind,
    pub target: String,
    pub passed_before: Option<bool>,
    pub passed_after: Option<bool>,
    pub detail: Option<String>,
}

/// What only a fix records; the rest is its [`Investigation`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Fix {
    pub id: i64,
    pub report_id: Option<i64>,
    pub resume: bool,
    pub runner_was_running: bool,
    pub paused_task: Option<i64>,
    /// The queue was continued after a verified fix.
    pub continued: bool,
    pub summary: Option<String>,
    pub verification: Option<String>,
    pub steps: Vec<FixEntry>,
    pub changes: Vec<Change>,
    pub probes: Vec<Probe>,
}

/// A fix to record as running. `question` of `run` is the owner's instruction.
pub(crate) struct NewFix<'a> {
    pub run: NewInvestigation<'a>,
    pub report_id: Option<i64>,
    pub resume: bool,
    pub token: &'a str,
}

/// How a fix ended.
pub(crate) struct Ending<'a> {
    pub state: InvestigationState,
    /// Why it is not a verified fix; None only when it is.
    pub reason: Option<&'a str>,
    pub verification: Option<&'a str>,
    pub summary: Option<&'a str>,
    pub continued: bool,
}

fn present(text: &str) -> Option<String> {
    (!text.trim().is_empty()).then(|| clip(text, ENTRY_CHARS))
}

impl Service {
    /// The time and cost later fixes may use; shares its version with the report limits.
    pub fn fix_limits(&self) -> Result<InvestigationLimits> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT row_version,fix_time_limit_ms,fix_cost_limit_microusd FROM project_investigation_limits WHERE project_id=?1",
                [self.project_id],
                |r| {
                    Ok(InvestigationLimits {
                        version: r.get(0)?,
                        time_limit_ms: r.get(1)?,
                        cost_limit_microusd: r.get(2)?,
                    })
                },
            )
            .optional()?
            .unwrap_or(InvestigationLimits {
                version: 0,
                time_limit_ms: 30 * 60 * 1000,
                cost_limit_microusd: 5_000_000,
            }))
    }

    /// Replaces the limits later fixes start with; `expected` is the version read.
    pub fn save_fix_limits(
        &mut self,
        expected: i64,
        time_limit_ms: i64,
        cost_limit_microusd: i64,
    ) -> Result<InvestigationLimits> {
        require(
            (1000..=86_400_000).contains(&time_limit_ms),
            "Fix time limit must be from 1 second to 24 hours",
        )?;
        require(
            (10_000..=1_000_000_000).contains(&cost_limit_microusd),
            "Fix cost limit must be from 0.01 to 1000 USD",
        )?;
        let report = self.investigation_limits()?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx
            .query_row(
                "SELECT row_version FROM project_investigation_limits WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        require(
            current == expected,
            "Fix limits changed elsewhere; read them again",
        )?;
        tx.execute(
            "INSERT INTO project_investigation_limits(project_id,row_version,time_limit_ms,cost_limit_microusd,fix_time_limit_ms,fix_cost_limit_microusd) VALUES (?1,?2,?3,?4,?5,?6) ON CONFLICT(project_id) DO UPDATE SET row_version=excluded.row_version,fix_time_limit_ms=excluded.fix_time_limit_ms,fix_cost_limit_microusd=excluded.fix_cost_limit_microusd",
            params![
                self.project_id,
                current + 1,
                report.time_limit_ms,
                report.cost_limit_microusd,
                time_limit_ms,
                cost_limit_microusd
            ],
        )?;
        tx.commit()?;
        Ok(InvestigationLimits {
            version: current + 1,
            time_limit_ms,
            cost_limit_microusd,
        })
    }

    /// The finished report of this project whose proposed steps a fix may carry out.
    pub fn fixable_report(&self, id: i64) -> Result<Investigation> {
        let owner: Option<i64> = self
            .db
            .connection()
            .query_row(
                "SELECT project_id FROM investigations WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        require(
            owner.is_none_or(|owner| owner == self.project_id),
            &format!("Report #{id} belongs to another project; start the fix in that project"),
        )?;
        let found = self.investigation(id)?.ok_or_else(|| {
            Error::Conflict(format!("Report #{id} does not exist in this project"))
        })?;
        require(
            found.kind == AgentKind::Report,
            &format!("#{id} is an agent fix, not a report"),
        )?;
        require(
            found.state == InvestigationState::Finished,
            &format!(
                "Report #{id} is unfinished ({}); only a finished report can be carried out",
                found.state.label().to_lowercase()
            ),
        )?;
        Ok(found)
    }

    /// Records a running fix requested by this service's actor. Only the process that
    /// holds the project's agent lease calls this.
    pub(crate) fn begin_fix(&mut self, new: &NewFix<'_>) -> Result<i64> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let id = insert_run(&tx, self.project_id, &self.actor, AgentKind::Fix, &new.run)?;
        tx.execute(
            "INSERT INTO agent_fixes(investigation_id,report_id,resume,token) VALUES (?1,?2,?3,?4)",
            params![id, new.report_id, new.resume, new.token],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Refuses a command callback unless fix `id` of this project still runs and `token`
    /// is its own.
    pub(crate) fn require_running_fix(&self, id: i64, token: &str) -> Result<()> {
        let found: bool = self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM agent_fixes f JOIN investigations i ON i.id=f.investigation_id WHERE i.id=?1 AND i.project_id=?2 AND i.state='running' AND i.stop_requested_by IS NULL AND f.token=?3)",
            params![id, self.project_id, token],
            |r| r.get(0),
        )?;
        require(
            found,
            "This agent fix is no longer running; stop and return your reply",
        )
    }

    pub(crate) fn fix_step(
        &mut self,
        id: i64,
        source: StepSource,
        action: &str,
        result: &str,
        status: StepStatus,
    ) -> Result<()> {
        let Some(action) = present(action) else {
            return Ok(());
        };
        self.db.connection().execute(
            "INSERT INTO agent_fix_steps(investigation_id,sequence,recorded_at_ms,source,action,result,status)
             SELECT i.id, coalesce((SELECT max(sequence) FROM agent_fix_steps WHERE investigation_id=i.id),0)+1, ?3, ?4, ?5, ?6, ?7
             FROM investigations i WHERE i.id=?1 AND i.project_id=?2 AND i.kind='fix' AND i.state='running'
             AND (SELECT count(*) FROM agent_fix_steps WHERE investigation_id=i.id) < ?8",
            params![
                id,
                self.project_id,
                now_ms(),
                source,
                action,
                clip(result, ENTRY_CHARS),
                status,
                ENTRIES
            ],
        )?;
        Ok(())
    }

    pub(crate) fn fix_change(
        &mut self,
        id: i64,
        kind: ChangeKind,
        target: &str,
        detail: &str,
    ) -> Result<()> {
        let Some(target) = present(target) else {
            return Ok(());
        };
        self.db.connection().execute(
            "INSERT INTO agent_fix_changes(investigation_id,sequence,kind,target,detail)
             SELECT i.id, coalesce((SELECT max(sequence) FROM agent_fix_changes WHERE investigation_id=i.id),0)+1, ?3, ?4, ?5
             FROM investigations i WHERE i.id=?1 AND i.project_id=?2 AND i.kind='fix' AND i.state='running'
             AND (SELECT count(*) FROM agent_fix_changes WHERE investigation_id=i.id) < ?6",
            params![
                id,
                self.project_id,
                kind,
                target,
                clip(detail, ENTRY_CHARS),
                ENTRIES
            ],
        )?;
        Ok(())
    }

    /// Adds a probe with its result before the fix changed anything, and returns its
    /// ordinal. An identical probe keeps its first result.
    pub(crate) fn fix_probe(
        &mut self,
        id: i64,
        kind: ProbeKind,
        target: &str,
        passed_before: Option<bool>,
        detail: &str,
    ) -> Result<i64> {
        self.require_running_run(id)?;
        let target = clip(target, ENTRY_CHARS);
        let db = self.db.connection();
        let known: Option<i64> = db
            .query_row(
                "SELECT ordinal FROM agent_fix_probes WHERE investigation_id=?1 AND kind=?2 AND target=?3",
                params![id, kind, target],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(ordinal) = known {
            return Ok(ordinal);
        }
        let ordinal: i64 = db.query_row(
            "SELECT coalesce(max(ordinal),0)+1 FROM agent_fix_probes WHERE investigation_id=?1",
            [id],
            |r| r.get(0),
        )?;
        require(ordinal <= 100, "A fix repeats at most 100 probes")?;
        db.execute(
            "INSERT INTO agent_fix_probes(investigation_id,ordinal,kind,target,passed_before,detail) VALUES (?1,?2,?3,?4,?5,?6)",
            params![id, ordinal, kind, target, passed_before, present(detail)],
        )?;
        Ok(ordinal)
    }

    pub(crate) fn fix_probe_after(
        &mut self,
        id: i64,
        ordinal: i64,
        passed: bool,
        detail: &str,
    ) -> Result<()> {
        self.require_running_run(id)?;
        self.db.connection().execute(
            "UPDATE agent_fix_probes SET passed_after=?3, detail=coalesce(?4,detail) WHERE investigation_id=?1 AND ordinal=?2",
            params![id, ordinal, passed, present(detail)],
        )?;
        Ok(())
    }

    fn require_running_run(&self, id: i64) -> Result<()> {
        let running = self.running_agent()?;
        require(
            running == Some((id, AgentKind::Fix)),
            "This agent fix is no longer running",
        )
    }

    /// Whether a runner was running when the fix started and the task it paused.
    pub(crate) fn fix_paused_runner(
        &mut self,
        id: i64,
        was_running: bool,
        task: Option<i64>,
    ) -> Result<()> {
        self.require_running_run(id)?;
        self.db.connection().execute(
            "UPDATE agent_fixes SET runner_was_running=?2, paused_task_id=?3 WHERE investigation_id=?1",
            params![id, was_running, task],
        )?;
        Ok(())
    }

    /// Attributes a change to the fix in the project's events.
    pub(crate) fn fix_event(&mut self, id: i64, message: &str) -> Result<()> {
        let message = clip(
            &format!("{}: {message}", self.actor.name),
            ENTRY_CHARS.min(2000),
        );
        self.require_running_run(id)?;
        self.db.connection().execute(
            "INSERT INTO events(project_id,occurred_at_ms,kind,level,message) VALUES (?1,?2,'agent.fix','info',?3)",
            params![self.project_id, now_ms(), message],
        )?;
        Ok(())
    }

    /// Ends a running fix. Only a verified fix is `finished`.
    pub(crate) fn finish_fix(&mut self, id: i64, ending: &Ending<'_>) -> Result<()> {
        let verified = ending.state == InvestigationState::Finished;
        require(
            ending.state != InvestigationState::Running && verified == ending.reason.is_none(),
            "A fix that is not verified says why",
        )?;
        let project = self.project_id;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            tx.execute(
                "UPDATE investigations SET state=?3,finished_at_ms=max(?4,started_at_ms),reason=?5 WHERE id=?1 AND project_id=?2 AND kind='fix' AND state='running'",
                params![
                    id,
                    project,
                    ending.state,
                    now_ms(),
                    ending.reason.map(|reason| clip(reason, ENTRY_CHARS.min(2000)))
                ],
            )? == 1,
            "This agent fix is no longer running",
        )?;
        tx.execute(
            "UPDATE agent_fixes SET verification=?2,summary=coalesce(?3,summary),continued=?4 WHERE investigation_id=?1",
            params![
                id,
                ending.verification.and_then(present),
                ending.summary.and_then(present),
                ending.continued
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// One fix of this project with its steps, changes and probes.
    pub fn fix(&self, id: i64) -> Result<Option<Fix>> {
        let tx = self.db.connection().unchecked_transaction()?;
        let Some(mut fix) = tx
            .query_row(
                "SELECT f.report_id,f.resume,f.runner_was_running,f.paused_task_id,f.continued,f.summary,f.verification FROM agent_fixes f JOIN investigations i ON i.id=f.investigation_id WHERE i.id=?1 AND i.project_id=?2",
                params![id, self.project_id],
                |r| {
                    Ok(Fix {
                        id,
                        report_id: r.get(0)?,
                        resume: r.get(1)?,
                        runner_was_running: r.get(2)?,
                        paused_task: r.get(3)?,
                        continued: r.get(4)?,
                        summary: r.get(5)?,
                        verification: r.get(6)?,
                        ..Fix::default()
                    })
                },
            )
            .optional()?
        else {
            return Ok(None);
        };
        fix.steps = tx
            .prepare("SELECT sequence,recorded_at_ms,source,action,result,status FROM agent_fix_steps WHERE investigation_id=?1 ORDER BY sequence")?
            .query_map([id], |r| {
                Ok(FixEntry {
                    sequence: r.get(0)?,
                    recorded_at_ms: r.get(1)?,
                    source: r.get(2)?,
                    action: r.get(3)?,
                    result: r.get(4)?,
                    status: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        fix.changes = tx
            .prepare("SELECT kind,target,detail FROM agent_fix_changes WHERE investigation_id=?1 ORDER BY sequence")?
            .query_map([id], |r| {
                Ok(Change {
                    kind: r.get(0)?,
                    target: r.get(1)?,
                    detail: r.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        fix.probes = tx
            .prepare("SELECT ordinal,kind,target,passed_before,passed_after,detail FROM agent_fix_probes WHERE investigation_id=?1 ORDER BY ordinal")?
            .query_map([id], |r| {
                Ok(Probe {
                    ordinal: r.get(0)?,
                    kind: r.get(1)?,
                    target: r.get(2)?,
                    passed_before: r.get(3)?,
                    passed_after: r.get(4)?,
                    detail: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        tx.commit()?;
        Ok(Some(fix))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn service(state: &crate::scratch::Scratch, name: &str) -> Result<Service> {
        let root = state.path().join(name);
        std::fs::create_dir_all(&root)?;
        Service::open(Database::open_in(&state.path().join("state"))?, &root, None)
    }

    fn model() -> ModelConfig {
        ModelConfig {
            provider: "codex".into(),
            model: "operator".into(),
            effort: None,
        }
    }

    #[test]
    fn only_a_finished_report_of_this_project_is_carried_out_and_one_agent_runs() -> Result<()> {
        let sandbox =
            crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let mut own = service(&sandbox, "own")?;
        let mut other = service(&sandbox, "other")?;
        let (model, limits) = (model(), own.fix_limits()?);
        let run = || NewInvestigation {
            task_id: None,
            question: Some("repair it"),
            model: &model,
            limits: &limits,
        };
        let report = other.begin_investigation(&run())?;
        let refused = own.fixable_report(report).unwrap_err().to_string();
        assert!(refused.contains("belongs to another project"), "{refused}");
        let refused = other.fixable_report(report).unwrap_err().to_string();
        assert!(refused.contains("is unfinished (running)"), "{refused}");
        let refused = own.fixable_report(report + 100).unwrap_err().to_string();
        assert!(refused.contains("does not exist"), "{refused}");

        let other_report = NewFix {
            run: run(),
            report_id: None,
            resume: false,
            token: "other",
        };
        // Never alongside a running report.
        let refused = other.begin_fix(&other_report).unwrap_err().to_string();
        assert!(
            refused.contains(&format!("Investigation #{report} is already running")),
            "{refused}"
        );

        let new = NewFix {
            run: run(),
            report_id: None,
            resume: true,
            token: "secret",
        };
        let fix = own.begin_fix(&new)?;
        let refused = own.begin_investigation(&run()).unwrap_err().to_string();
        assert!(
            refused.contains(&format!("Agent fix #{fix} is already running")),
            "{refused}"
        );
        assert!(own.begin_fix(&new).is_err());
        own.require_running_fix(fix, "secret")?;
        assert!(own.require_running_fix(fix, "guess").is_err());
        assert!(other.require_running_fix(fix, "secret").is_err());
        let ordinal = own.fix_probe(fix, ProbeKind::Check, "build", Some(false), "")?;
        assert_eq!(
            own.fix_probe(fix, ProbeKind::Check, "build", Some(true), "")?,
            ordinal
        );
        own.fix_probe_after(fix, ordinal, true, "passed")?;
        own.fix_step(
            fix,
            StepSource::Ktask,
            "check-set build",
            "saved",
            StepStatus::Applied,
        )?;
        own.finish_fix(
            fix,
            &Ending {
                state: InvestigationState::Finished,
                reason: None,
                verification: Some("verified"),
                summary: None,
                continued: false,
            },
        )?;
        // Nothing is recorded once it ended, and its capability is gone.
        own.fix_step(fix, StepSource::Agent, "late", "", StepStatus::Applied)?;
        assert!(own.require_running_fix(fix, "secret").is_err());
        let record = own.fix(fix)?.unwrap();
        assert_eq!(record.steps.len(), 1);
        assert_eq!(record.probes[0].passed_before, Some(false));
        assert_eq!(record.probes[0].passed_after, Some(true));
        assert_eq!(other.fix(fix)?, None);
        assert_eq!(own.investigation(fix)?.unwrap().report, None);
        let refused = own.fixable_report(fix).unwrap_err().to_string();
        assert!(refused.contains("not a report"), "{refused}");

        let saved = own.save_fix_limits(0, 60_000, 500_000)?;
        assert_eq!(own.fix_limits()?, saved);
        let report_limits = own.investigation_limits()?;
        assert_eq!(report_limits.version, 1);
        assert_eq!(
            report_limits.time_limit_ms,
            InvestigationLimits::default().time_limit_ms
        );
        own.save_investigation_limits(1, 5_000, 20_000)?;
        assert_eq!(own.fix_limits()?.time_limit_ms, 60_000);
        Ok(())
    }
}
