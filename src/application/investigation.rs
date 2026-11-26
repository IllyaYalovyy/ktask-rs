//! Agent reports: owner-started, read-only investigations of one project and the reports
//! they leave. An investigation is not a task. Its commands write only its own rows, so no
//! task, revision, attempt or setting changes because one ran. Agent fixes
//! ([`super::fix`]) share the run record: ID, state, limits, progress, usage and stop.
use super::*;
use serde::Serialize;

/// Progress entries kept per investigation; tool activity stops earlier so that findings
/// reported late in a long run are still kept.
const PROGRESS_ENTRIES: i64 = 500;
const ACTIVITY_ENTRIES: i64 = 400;
const PROGRESS_CHARS: usize = 2000;
/// The role whose model investigates; unset, it is the remediation model.
pub const OPERATOR_ROLE: &str = "operator";

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// What an agent run is: a read-only report or a fix.
    pub enum AgentKind {
        Report => "report",
        Fix => "fix",
    }
}

impl AgentKind {
    /// "Agent report #3" or "Agent fix #4".
    pub fn named(self, id: i64) -> String {
        format!("Agent {self} #{id}")
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// How an agent run stands. Only a `finished` report has a report; a `finished` fix is
    /// verified.
    pub enum InvestigationState {
        Running => "running",
        Finished => "finished",
        Failed => "failed",
        TimedOut => "timed_out",
        OverBudget => "over_budget",
        Stopped => "stopped",
        Unverified => "unverified",
    }
}

impl InvestigationState {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Finished => "Finished",
            Self::Failed => "Failed",
            Self::TimedOut => "Timed out",
            Self::OverBudget => "Over cost limit",
            Self::Stopped => "Stopped",
            Self::Unverified => "Unverified",
        }
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum ProgressKind {
        Status => "status",
        Finding => "finding",
        Activity => "activity",
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// What a report cites as showing the cause.
    pub enum ReferenceKind {
        Event => "event",
        Attempt => "attempt",
        Step => "step",
        Evidence => "evidence",
        Log => "log",
        File => "file",
        Command => "command",
    }
}

stored_names! {
    #[derive(Serialize)]
    #[serde(rename_all = "snake_case")]
    /// Who or what a proposed fix step changes.
    pub enum FixClass {
        KtaskAction => "ktask_action",
        Configuration => "configuration",
        Repository => "repository",
        Host => "host",
        Owner => "owner",
    }
}

impl FixClass {
    pub const fn label(self) -> &'static str {
        match self {
            Self::KtaskAction => "ktask action",
            Self::Configuration => "configuration change",
            Self::Repository => "repository change",
            Self::Host => "host change",
            Self::Owner => "owner only",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct InvestigationLimits {
    /// 0 until the project saves its own.
    pub version: i64,
    pub time_limit_ms: i64,
    pub cost_limit_microusd: i64,
}

impl Default for InvestigationLimits {
    fn default() -> Self {
        Self {
            version: 0,
            time_limit_ms: 15 * 60 * 1000,
            cost_limit_microusd: 2_000_000,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Reference {
    pub kind: ReferenceKind,
    pub target: String,
    pub detail: String,
    /// Whether the cited event, attempt or step exists in the project; None: not checked.
    pub verified: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FixStep {
    pub class: FixClass,
    pub action: String,
    /// It gets around the problem without repairing it.
    pub workaround: bool,
}

/// A finished report, in the order it is read: what happened, the cause and what shows it,
/// then the proposed fix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Report {
    pub what_happened: String,
    pub cause: String,
    /// Set when the cause is a defect in ktask itself.
    pub ktask_defect: Option<String>,
    pub references: Vec<Reference>,
    pub fix_steps: Vec<FixStep>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub sequence: i64,
    pub recorded_at_ms: i64,
    pub kind: ProgressKind,
    pub message: String,
}

/// What the provider reported using; None means it reported nothing, not zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct InvestigationUsage {
    pub input_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cost_microusd: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Investigation {
    pub id: i64,
    pub kind: AgentKind,
    pub project_id: i64,
    pub task_id: Option<i64>,
    pub question: Option<String>,
    pub requester_kind: String,
    pub requester: String,
    pub model: ModelConfig,
    pub state: InvestigationState,
    pub time_limit_ms: i64,
    pub cost_limit_microusd: i64,
    pub stop_requested_by: Option<String>,
    pub started_at_ms: i64,
    pub finished_at_ms: Option<i64>,
    /// Why it ended without a report.
    pub reason: Option<String>,
    pub usage: InvestigationUsage,
    /// Only a finished investigation; loaded by [`Service::investigation`].
    pub report: Option<Report>,
    /// Loaded by [`Service::investigation`].
    pub progress: Vec<Progress>,
}

/// An investigation to record as running.
pub struct NewInvestigation<'a> {
    pub task_id: Option<i64>,
    pub question: Option<&'a str>,
    pub model: &'a ModelConfig,
    pub limits: &'a InvestigationLimits,
}

const COLUMNS: &str = "id,project_id,task_id,question,requester_kind,requester,provider,model,effort,state,time_limit_ms,cost_limit_microusd,stop_requested_by,started_at_ms,finished_at_ms,reason,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_microusd,kind";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Investigation> {
    Ok(Investigation {
        id: r.get(0)?,
        project_id: r.get(1)?,
        task_id: r.get(2)?,
        question: r.get(3)?,
        requester_kind: r.get(4)?,
        requester: r.get(5)?,
        model: ModelConfig {
            provider: r.get(6)?,
            model: r.get(7)?,
            effort: r.get(8)?,
        },
        state: r.get(9)?,
        time_limit_ms: r.get(10)?,
        cost_limit_microusd: r.get(11)?,
        stop_requested_by: r.get(12)?,
        started_at_ms: r.get(13)?,
        finished_at_ms: r.get(14)?,
        reason: r.get(15)?,
        usage: InvestigationUsage {
            input_tokens: r.get(16)?,
            cache_read_tokens: r.get(17)?,
            cache_write_tokens: r.get(18)?,
            output_tokens: r.get(19)?,
            cost_microusd: r.get(20)?,
        },
        kind: r.get(21)?,
        report: None,
        progress: Vec::new(),
    })
}

pub(super) fn clip(text: &str, limit: usize) -> String {
    let mut clipped: String = text.trim().chars().take(limit).collect();
    if text.trim().chars().count() > limit {
        clipped.push('…');
    }
    clipped
}

impl Service {
    /// The model that investigates: the operator role, else the remediation model.
    pub fn operator_model(&self) -> Result<Option<ModelConfig>> {
        Ok(self
            .effective_models()?
            .into_iter()
            .find(|row| row.role == OPERATOR_ROLE)
            .and_then(|row| row.model))
    }

    pub fn investigation_limits(&self) -> Result<InvestigationLimits> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT row_version,time_limit_ms,cost_limit_microusd FROM project_investigation_limits WHERE project_id=?1",
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
            .unwrap_or_default())
    }

    /// Replaces the limits later investigations start with; `expected` is the version read.
    pub fn save_investigation_limits(
        &mut self,
        expected: i64,
        time_limit_ms: i64,
        cost_limit_microusd: i64,
    ) -> Result<InvestigationLimits> {
        require(
            (1000..=86_400_000).contains(&time_limit_ms),
            "Investigation time limit must be from 1 second to 24 hours",
        )?;
        require(
            (10_000..=1_000_000_000).contains(&cost_limit_microusd),
            "Investigation cost limit must be from 0.01 to 1000 USD",
        )?;
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
            "Investigation limits changed elsewhere; read them again",
        )?;
        tx.execute(
            "INSERT INTO project_investigation_limits(project_id,row_version,time_limit_ms,cost_limit_microusd) VALUES (?1,?2,?3,?4) ON CONFLICT(project_id) DO UPDATE SET row_version=excluded.row_version,time_limit_ms=excluded.time_limit_ms,cost_limit_microusd=excluded.cost_limit_microusd",
            params![self.project_id, current + 1, time_limit_ms, cost_limit_microusd],
        )?;
        tx.commit()?;
        Ok(InvestigationLimits {
            version: current + 1,
            time_limit_ms,
            cost_limit_microusd,
        })
    }

    /// The report or fix recorded as running, whether or not its process is still alive.
    pub fn running_investigation(&self) -> Result<Option<i64>> {
        Ok(self.running_agent()?.map(|(id, _)| id))
    }

    /// Like [`Self::running_investigation`], with what runs.
    pub fn running_agent(&self) -> Result<Option<(i64, AgentKind)>> {
        running(self.db.connection(), self.project_id)
    }

    /// Records a running investigation requested by this service's actor. Only the process
    /// that holds the project's investigation lease calls this.
    pub(crate) fn begin_investigation(&mut self, new: &NewInvestigation<'_>) -> Result<i64> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(task) = new.task_id {
            require(
                tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM tasks WHERE id=?1 AND project_id=?2)",
                    params![task, self.project_id],
                    |r| r.get::<_, bool>(0),
                )?,
                &format!("Task #{task} does not exist in this project"),
            )?;
        }
        let id = insert_run(&tx, self.project_id, &self.actor, AgentKind::Report, new)?;
        tx.commit()?;
        Ok(id)
    }

    /// Appends one entry while the investigation runs, up to the entry limits.
    pub(crate) fn investigation_progress(
        &mut self,
        id: i64,
        kind: ProgressKind,
        message: &str,
    ) -> Result<()> {
        let message = clip(message, PROGRESS_CHARS);
        if message.is_empty() {
            return Ok(());
        }
        let entries = if kind == ProgressKind::Activity {
            ACTIVITY_ENTRIES
        } else {
            PROGRESS_ENTRIES
        };
        self.db.connection().execute(
            "INSERT INTO investigation_progress(investigation_id,sequence,recorded_at_ms,kind,message)
             SELECT i.id, coalesce((SELECT max(sequence) FROM investigation_progress WHERE investigation_id=i.id),0)+1, ?3, ?4, ?5
             FROM investigations i WHERE i.id=?1 AND i.project_id=?2 AND i.state='running'
             AND (SELECT count(*) FROM investigation_progress WHERE investigation_id=i.id) < ?6",
            params![id, self.project_id, now_ms(), kind, message, entries],
        )?;
        Ok(())
    }

    pub(crate) fn investigation_usage(
        &mut self,
        id: i64,
        usage: &InvestigationUsage,
    ) -> Result<()> {
        self.db.connection().execute(
            "UPDATE investigations SET input_tokens=?3,cache_read_tokens=?4,cache_write_tokens=?5,output_tokens=?6,cost_microusd=coalesce(?7,cost_microusd) WHERE id=?1 AND project_id=?2 AND state='running'",
            params![
                id,
                self.project_id,
                usage.input_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                usage.output_tokens,
                usage.cost_microusd
            ],
        )?;
        Ok(())
    }

    /// Asks the investigator to end; it stops its agent and records the state itself.
    pub fn request_investigation_stop(&mut self, id: i64) -> Result<()> {
        let changed = self.db.connection().execute(
            "UPDATE investigations SET stop_requested_by=coalesce(stop_requested_by,?3) WHERE id=?1 AND project_id=?2 AND state='running'",
            params![id, self.project_id, self.actor.name],
        )?;
        if changed == 1 {
            return Ok(());
        }
        let found = self.investigation(id)?.ok_or_else(|| {
            Error::Conflict(format!(
                "Investigation #{id} does not exist in this project"
            ))
        })?;
        Err(Error::Conflict(format!(
            "{} is not running ({})",
            match found.kind {
                AgentKind::Report => format!("Investigation #{id}"),
                AgentKind::Fix => AgentKind::Fix.named(id),
            },
            found.state.label().to_lowercase()
        )))
    }

    pub(crate) fn investigation_stop_requested(&self, id: i64) -> Result<Option<String>> {
        Ok(self.db.connection().query_row(
            "SELECT stop_requested_by FROM investigations WHERE id=?1 AND project_id=?2",
            params![id, self.project_id],
            |r| r.get(0),
        )?)
    }

    /// Records the report of a running investigation and marks which of the cited events,
    /// attempts and steps exist in this project.
    pub(crate) fn finish_investigation(&mut self, id: i64, report: &Report) -> Result<()> {
        let project = self.project_id;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            tx.execute(
                "UPDATE investigations SET state='finished',finished_at_ms=max(?3,started_at_ms),what_happened=?4,cause=?5,ktask_defect=?6 WHERE id=?1 AND project_id=?2 AND state='running'",
                params![
                    id,
                    project,
                    now_ms(),
                    report.what_happened.trim(),
                    report.cause.trim(),
                    report.ktask_defect.as_deref().map(str::trim)
                ],
            )? == 1,
            "Investigation is no longer running",
        )?;
        for (ordinal, reference) in (1i64..).zip(&report.references) {
            let table = match reference.kind {
                ReferenceKind::Event => Some("events"),
                ReferenceKind::Attempt => Some("attempts"),
                ReferenceKind::Step => Some("attempt_steps"),
                _ => None,
            };
            let verified = match table {
                Some(table) => {
                    let cited: Option<i64> = reference
                        .target
                        .split(|c: char| !c.is_ascii_digit())
                        .find(|part| !part.is_empty())
                        .and_then(|part| part.parse().ok());
                    Some(match cited {
                        Some(cited) => tx.query_row(
                            &format!(
                                "SELECT EXISTS(SELECT 1 FROM {table} WHERE id=?1 AND project_id=?2)"
                            ),
                            params![cited, project],
                            |r| r.get::<_, bool>(0),
                        )?,
                        None => false,
                    })
                }
                None => None,
            };
            tx.execute(
                "INSERT INTO investigation_references(investigation_id,ordinal,kind,target,detail,verified) VALUES (?1,?2,?3,?4,?5,?6)",
                params![id, ordinal, reference.kind, reference.target.trim(), reference.detail.trim(), verified],
            )?;
        }
        for (ordinal, step) in (1i64..).zip(&report.fix_steps) {
            tx.execute(
                "INSERT INTO investigation_fix_steps(investigation_id,ordinal,class,action,workaround) VALUES (?1,?2,?3,?4,?5)",
                params![id, ordinal, step.class, step.action.trim(), step.workaround],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Ends a running investigation without a report; what it found stays as progress.
    pub(crate) fn end_investigation(
        &mut self,
        id: i64,
        state: InvestigationState,
        reason: &str,
    ) -> Result<()> {
        require(
            !matches!(
                state,
                InvestigationState::Running | InvestigationState::Finished
            ),
            "An investigation without a report ends as failed, timed out, over budget or stopped",
        )?;
        self.db.connection().execute(
            "UPDATE investigations SET state=?3,finished_at_ms=max(?4,started_at_ms),reason=?5 WHERE id=?1 AND project_id=?2 AND state='running'",
            params![id, self.project_id, state, now_ms(), clip(reason, PROGRESS_CHARS)],
        )?;
        Ok(())
    }

    /// This project's investigations, newest first, without progress and reports.
    pub fn investigations(&self) -> Result<Vec<Investigation>> {
        Ok(self
            .db
            .connection()
            .prepare(&format!(
                "SELECT {COLUMNS} FROM investigations WHERE project_id=?1 ORDER BY id DESC"
            ))?
            .query_map([self.project_id], row)?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// One investigation of this project with its progress and report.
    pub fn investigation(&self, id: i64) -> Result<Option<Investigation>> {
        let db = self.db.connection();
        // One read transaction: the state, its report and its progress belong together.
        let tx = db.unchecked_transaction()?;
        let Some(mut found) = tx
            .query_row(
                &format!("SELECT {COLUMNS} FROM investigations WHERE id=?1 AND project_id=?2"),
                params![id, self.project_id],
                row,
            )
            .optional()?
        else {
            return Ok(None);
        };
        found.progress = tx
            .prepare("SELECT sequence,recorded_at_ms,kind,message FROM investigation_progress WHERE investigation_id=?1 ORDER BY sequence")?
            .query_map([id], |r| {
                Ok(Progress {
                    sequence: r.get(0)?,
                    recorded_at_ms: r.get(1)?,
                    kind: r.get(2)?,
                    message: r.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        if found.kind == AgentKind::Report && found.state == InvestigationState::Finished {
            let references = tx
                .prepare("SELECT kind,target,detail,verified FROM investigation_references WHERE investigation_id=?1 ORDER BY ordinal")?
                .query_map([id], |r| {
                    Ok(Reference {
                        kind: r.get(0)?,
                        target: r.get(1)?,
                        detail: r.get(2)?,
                        verified: r.get(3)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            let fix_steps = tx
                .prepare("SELECT class,action,workaround FROM investigation_fix_steps WHERE investigation_id=?1 ORDER BY ordinal")?
                .query_map([id], |r| {
                    Ok(FixStep {
                        class: r.get(0)?,
                        action: r.get(1)?,
                        workaround: r.get(2)?,
                    })
                })?
                .collect::<rusqlite::Result<_>>()?;
            found.report = Some(tx.query_row(
                "SELECT what_happened,cause,ktask_defect FROM investigations WHERE id=?1",
                [id],
                |r| {
                    Ok(Report {
                        what_happened: r.get(0)?,
                        cause: r.get(1)?,
                        ktask_defect: r.get(2)?,
                        references,
                        fix_steps,
                    })
                },
            )?);
        }
        tx.commit()?;
        Ok(Some(found))
    }
}

/// The refusal a second request gets, naming the report or fix that runs.
pub(crate) fn already_running(id: i64, kind: AgentKind) -> String {
    let name = match kind {
        AgentKind::Report => format!("Investigation #{id}"),
        AgentKind::Fix => AgentKind::Fix.named(id),
    };
    format!(
        "{name} is already running for this project; read it with ktask agent show {id} or end it with ktask agent stop {id}"
    )
}

fn running(db: &rusqlite::Connection, project: i64) -> Result<Option<(i64, AgentKind)>> {
    Ok(db
        .query_row(
            "SELECT id,kind FROM investigations WHERE project_id=?1 AND state='running'",
            [project],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?)
}

/// Records one running report or fix; refused while either runs.
pub(super) fn insert_run(
    tx: &Transaction<'_>,
    project: i64,
    actor: &Actor,
    kind: AgentKind,
    new: &NewInvestigation<'_>,
) -> Result<i64> {
    if let Some((id, kind)) = running(tx, project)? {
        return Err(Error::Conflict(already_running(id, kind)));
    }
    tx.execute(
        "INSERT INTO investigations(project_id,task_id,question,requester_kind,requester,provider,model,effort,state,time_limit_ms,cost_limit_microusd,started_at_ms,kind) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'running',?9,?10,?11,?12)",
        params![
            project,
            new.task_id,
            new.question.map(str::trim).filter(|q| !q.is_empty()),
            actor.kind.as_str(),
            actor.name,
            new.model.provider,
            new.model.model,
            new.model.effort,
            new.limits.time_limit_ms,
            new.limits.cost_limit_microusd,
            now_ms(),
            kind
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Database;

    fn service() -> Result<(Service, crate::scratch::Scratch)> {
        let sandbox =
            crate::scratch::Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path().join("project");
        std::fs::create_dir_all(&root)?;
        let service = Service::open(
            Database::open_in(&sandbox.path().join("state"))?,
            &root,
            None,
        )?;
        Ok((service, sandbox))
    }

    fn model() -> ModelConfig {
        ModelConfig {
            provider: "codex".into(),
            model: "operator".into(),
            effort: None,
        }
    }

    #[test]
    fn one_runs_at_a_time_and_only_a_finished_one_has_a_report() -> Result<()> {
        let (mut service, _sandbox) = service()?;
        let limits = service.investigation_limits()?;
        assert_eq!(limits, InvestigationLimits::default());
        let (model, new_limits) = (model(), limits.clone());
        let new = NewInvestigation {
            task_id: None,
            question: Some("  "),
            model: &model,
            limits: &new_limits,
        };
        let first = service.begin_investigation(&new)?;
        let refused = service.begin_investigation(&new).unwrap_err().to_string();
        assert!(refused.contains(&format!("#{first} is already running")));
        service.investigation_progress(first, ProgressKind::Finding, "found a thing")?;
        service.end_investigation(first, InvestigationState::Stopped, "Stopped by owner")?;
        // Nothing is appended or replaced once it ended.
        service.investigation_progress(first, ProgressKind::Finding, "late")?;
        let stopped = service.investigation(first)?.unwrap();
        assert_eq!(stopped.state, InvestigationState::Stopped);
        assert_eq!(stopped.question, None);
        assert_eq!(stopped.report, None);
        assert_eq!(stopped.progress.len(), 1);
        assert!(service.request_investigation_stop(first).is_err());

        let second = service.begin_investigation(&new)?;
        let report = Report {
            what_happened: "Nothing".into(),
            cause: "Idle".into(),
            ktask_defect: None,
            references: vec![
                Reference {
                    kind: ReferenceKind::Event,
                    target: "#999999".into(),
                    detail: "missing".into(),
                    verified: None,
                },
                Reference {
                    kind: ReferenceKind::Log,
                    target: "runner.log:1".into(),
                    detail: "line".into(),
                    verified: None,
                },
            ],
            fix_steps: vec![FixStep {
                class: FixClass::Owner,
                action: "Nothing to do".into(),
                workaround: false,
            }],
        };
        service.finish_investigation(second, &report)?;
        let finished = service.investigation(second)?.unwrap();
        assert_eq!(finished.state, InvestigationState::Finished);
        let stored = finished.report.unwrap();
        assert_eq!(stored.references[0].verified, Some(false));
        assert_eq!(stored.references[1].verified, None);
        assert_eq!(stored.fix_steps, report.fix_steps);
        assert_eq!(service.investigations()?.len(), 2);
        Ok(())
    }

    #[test]
    fn limits_are_versioned_and_the_operator_defaults_to_remediation() -> Result<()> {
        let (mut service, _sandbox) = service()?;
        assert!(
            service
                .save_investigation_limits(1, 60_000, 500_000)
                .is_err()
        );
        assert!(service.save_investigation_limits(0, 10, 500_000).is_err());
        let saved = service.save_investigation_limits(0, 60_000, 500_000)?;
        assert_eq!(saved.version, 1);
        assert_eq!(service.investigation_limits()?, saved);

        assert_eq!(service.operator_model()?, None);
        let judge = ModelConfig {
            provider: "codex".into(),
            model: "judge".into(),
            effort: Some("high".into()),
        };
        service.set_default("remediation", &judge, false)?;
        assert_eq!(service.operator_model()?, Some(judge.clone()));
        service.set_default(OPERATOR_ROLE, &model(), false)?;
        assert_eq!(service.operator_model()?, Some(model()));
        Ok(())
    }
}
