//! Resource limits as project data. A limit exists to stop a runaway producer from filling
//! the host; the owner sees and changes it, it follows the project as it grows, and a pause
//! it caused is never repeated unchanged.
//!
//! Instance defaults and per-project values are stored; every read resolves them again, so a
//! change applies to the next step of a paused attempt however its runner was started.
use super::retention::{Standing, storage_standing};
use super::*;
use crate::resource::{Effective, Limit, PauseLimit, StorageRefusal, Stored};
use rusqlite::Connection;

/// Where a stored value applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LimitScope {
    Project,
    Instance,
}

impl LimitScope {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::Instance => "instance",
        }
    }
}

/// A change to one scope's stored limit: `None` keeps a field, `Some(None)` inherits it.
#[derive(Clone, Copy, Debug, Default)]
pub struct LimitChange {
    pub value: Option<Option<u64>>,
    pub ceiling: Option<Option<u64>>,
}

/// One limit as Configuration and the CLI show it.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct LimitSetting {
    pub limit: Limit,
    pub value: u64,
    pub ceiling: u64,
    /// env, project, automatic, instance or default.
    pub source: &'static str,
    pub default: u64,
    pub project: Stored,
    pub instance: Stored,
    /// The most a successful attempt of this project needed.
    pub high_water: Option<u64>,
    pub warning: Option<String>,
}

/// Why an attempt is paused on a limit or the storage budget, with what Raise and resume
/// would set.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ResourcePause {
    pub limit: PauseLimit,
    /// The limit in force when the attempt paused.
    pub value: u64,
    /// The measured need, when known.
    pub measured: Option<u64>,
    /// Repeating the same work under the same limit would pause it again.
    pub deterministic: bool,
    pub source_tree: Option<String>,
    /// The limit in force now.
    pub current: u64,
    pub ceiling: u64,
    /// What Raise and resume sets; None when it cannot raise, with `blocked` saying why.
    pub proposal: Option<u64>,
    pub blocked: Option<String>,
    /// Storing a larger value here would not change the limit in force.
    pub unraisable: bool,
    /// Capacity was reclaimed since: the recorded storage refusal no longer holds.
    pub relieved: bool,
}

impl ResourcePause {
    /// The limit was raised since the attempt paused.
    pub fn raised(&self) -> bool {
        self.current > self.value
    }

    /// The same work would pause again: neither the limit nor the room under it changed.
    pub fn repeats(&self) -> bool {
        self.deterministic && !self.raised() && !self.relieved
    }

    /// The limit, its value and the measured need.
    pub fn describe(&self) -> String {
        format!(
            "{} is {}{}",
            self.limit,
            self.limit.show(self.current),
            self.measured.map_or_else(String::new, |need| format!(
                ", needed {}",
                self.limit.show(need)
            ))
        )
    }

    /// What the owner can do about it.
    pub fn offer(&self) -> String {
        match (&self.proposal, &self.blocked) {
            (Some(to), _) => format!(
                "Raise and resume sets {} to {} (TUI + on Current, or ktask run --raise)",
                self.limit,
                self.limit.show(*to)
            ),
            (None, Some(why)) => why.clone(),
            (None, None) => String::new(),
        }
    }
}

fn stored(db: &Connection, project: Option<i64>, limit: Limit) -> Result<Stored> {
    Ok(db
        .query_row(
            "SELECT value,extended,ceiling FROM resource_limit_settings
             WHERE ifnull(project_id,0)=ifnull(?1,0) AND limit_name=?2",
            params![project, limit.name()],
            |r| {
                let number = |index| -> rusqlite::Result<Option<u64>> {
                    Ok(r.get::<_, Option<i64>>(index)?.map(|v| v as u64))
                };
                Ok(Stored {
                    value: number(0)?,
                    extended: number(1)?,
                    ceiling: number(2)?,
                })
            },
        )
        .optional()?
        .unwrap_or_default())
}

fn save(db: &Connection, project: Option<i64>, limit: Limit, row: Stored) -> Result<()> {
    db.execute(
        "DELETE FROM resource_limit_settings WHERE ifnull(project_id,0)=ifnull(?1,0) AND limit_name=?2",
        params![project, limit.name()],
    )?;
    if row != Stored::default() {
        let number = |value: Option<u64>| value.map(|v| v as i64);
        db.execute(
            "INSERT INTO resource_limit_settings(project_id,limit_name,value,extended,ceiling) VALUES (?1,?2,?3,?4,?5)",
            params![
                project,
                limit.name(),
                number(row.value),
                number(row.extended),
                number(row.ceiling)
            ],
        )?;
    }
    Ok(())
}

/// The limit in force for a project, read from its stored settings.
pub(crate) fn effective(db: &Connection, project: i64, limit: Limit) -> Result<Effective> {
    Ok(limit.resolve(
        (limit.env_value(), limit.env_ceiling()),
        stored(db, Some(project), limit)?,
        stored(db, None, limit)?,
    ))
}

/// A warning when the limit has less than twice the headroom the project has used. The
/// runner log rotates at its limit, so it has no high-water mark.
fn warning(limit: Limit, value: u64, high_water: Option<u64>) -> Option<String> {
    let need = high_water?;
    (need.saturating_mul(2) > value).then(|| {
        format!(
            "{limit} is {}, less than twice the {} a successful task needed",
            limit.show(value),
            limit.show(need)
        )
    })
}

/// The value Raise and resume sets: twice the larger of the need and the current limit,
/// never above the ceiling and never below the need.
fn proposal(
    limit: Limit,
    effective: Effective,
    current: u64,
    measured: Option<u64>,
) -> std::result::Result<u64, String> {
    if effective.source == "env" {
        return Err(format!(
            "{} overrides the stored {limit}; change or unset that variable",
            limit.env()
        ));
    }
    let need = measured.unwrap_or(0).max(current.saturating_add(1));
    let proposal = need.max(current).saturating_mul(2).min(effective.ceiling);
    if proposal < need {
        return Err(format!(
            "The owner ceiling of {limit} is {}, which does not cover the need of {}; raise the ceiling first (Configuration → Resource limits, or ktask limits {limit} --ceiling N)",
            limit.show(effective.ceiling),
            limit.show(need)
        ));
    }
    Ok(proposal)
}

fn limit_event(db: &Connection, project: i64, attempt: Option<i64>, message: &str) -> Result<()> {
    db.execute(
        "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
         VALUES (?1,(SELECT task_id FROM attempts WHERE id=?2),?2,?3,'resource.limit','info',?4)",
        params![project, attempt, now_ms(), message],
    )?;
    Ok(())
}

/// Keeps the largest need of a successful attempt for each measured limit.
pub(super) fn record_high_water(db: &Connection, attempt: i64) -> Result<()> {
    // Archived files and files a truncated collection left out are both measured need.
    const MEASURED: [(Limit, &str); 7] = [
        (
            Limit::ScratchBytes,
            "SELECT bytes FROM attempt_scratch_peaks WHERE attempt_id=?1",
        ),
        (
            Limit::ScratchEntries,
            "SELECT entries FROM attempt_scratch_peaks WHERE attempt_id=?1",
        ),
        (
            Limit::ArtifactFileBytes,
            "SELECT max(size) FROM (SELECT size_bytes AS size FROM artifacts WHERE attempt_id=?1
               UNION ALL SELECT m.size_bytes FROM evidence_manifest m JOIN evidence_collections c ON c.id=m.collection_id
               WHERE c.attempt_id=?1 AND m.omitted IS NOT NULL)",
        ),
        (
            Limit::ArtifactAttemptBytes,
            "SELECT (SELECT coalesce(sum(size_bytes),0) FROM artifacts WHERE attempt_id=?1)
               + (SELECT coalesce(sum(m.size_bytes),0) FROM evidence_manifest m JOIN evidence_collections c ON c.id=m.collection_id
               WHERE c.attempt_id=?1 AND m.omitted IS NOT NULL)",
        ),
        (
            Limit::ArtifactAttemptFiles,
            "SELECT (SELECT count(*) FROM artifacts WHERE attempt_id=?1)
               + (SELECT count(*) FROM evidence_manifest m JOIN evidence_collections c ON c.id=m.collection_id
               WHERE c.attempt_id=?1 AND m.omitted IS NOT NULL)",
        ),
        (
            Limit::OutputBytes,
            "SELECT bytes FROM attempt_output_usage WHERE attempt_id=?1",
        ),
        (
            Limit::OutputRows,
            "SELECT rows FROM attempt_output_usage WHERE attempt_id=?1",
        ),
    ];
    for (limit, query) in MEASURED {
        let value: Option<i64> = db
            .query_row(query, [attempt], |r| r.get(0))
            .optional()?
            .flatten();
        let Some(value) = value.filter(|&value| value > 0) else {
            continue;
        };
        db.execute(
            "INSERT INTO resource_high_water(project_id,limit_name,value,attempt_id)
             SELECT project_id,?2,?3,id FROM attempts WHERE id=?1
             ON CONFLICT(project_id,limit_name) DO UPDATE SET value=excluded.value,attempt_id=excluded.attempt_id
             WHERE excluded.value>resource_high_water.value",
            params![attempt, limit.name(), value],
        )?;
    }
    Ok(())
}

impl Service {
    /// Every limit with its stored layers, the value in force, and the project's high-water
    /// mark with a warning when the headroom is small.
    pub fn resource_limits(&self) -> Result<Vec<LimitSetting>> {
        Limit::SETTINGS
            .into_iter()
            .map(|limit| self.resource_limit(limit))
            .collect()
    }

    pub fn resource_limit(&self, limit: Limit) -> Result<LimitSetting> {
        let db = self.db.connection();
        let project = stored(db, Some(self.project_id), limit)?;
        let instance = stored(db, None, limit)?;
        let effective = effective(db, self.project_id, limit)?;
        let high_water = db
            .query_row(
                "SELECT value FROM resource_high_water WHERE project_id=?1 AND limit_name=?2",
                params![self.project_id, limit.name()],
                |r| r.get::<_, i64>(0),
            )
            .optional()?
            .map(|value| value as u64);
        Ok(LimitSetting {
            limit,
            value: effective.value,
            ceiling: effective.ceiling,
            source: effective.source,
            default: limit.default_value(),
            project,
            instance,
            high_water,
            warning: warning(limit, effective.value, high_water),
        })
    }

    /// The limits whose headroom is less than twice what the project has needed.
    pub fn limit_warnings(&self) -> Result<Vec<String>> {
        Ok(self
            .resource_limits()?
            .into_iter()
            .filter_map(|setting| setting.warning)
            .collect())
    }

    /// Change one scope's stored limit. `expected` is the setting the owner was shown; a
    /// change since then is refused.
    pub fn set_resource_limit(
        &mut self,
        scope: LimitScope,
        limit: Limit,
        change: LimitChange,
        expected: Option<&LimitSetting>,
    ) -> Result<LimitSetting> {
        self.set_resource_limits(limit, &[(scope, change)], expected)
    }

    /// Change several scopes of one limit in one transaction.
    pub fn set_resource_limits(
        &mut self,
        limit: Limit,
        changes: &[(LimitScope, LimitChange)],
        expected: Option<&LimitSetting>,
    ) -> Result<LimitSetting> {
        for (_, change) in changes {
            for number in [change.value, change.ceiling]
                .into_iter()
                .flatten()
                .flatten()
            {
                require(
                    number > 0 && number <= i64::MAX as u64,
                    "A resource limit is a positive integer",
                )?;
            }
            require(
                change.value.is_some() || change.ceiling.is_some(),
                "Give a value or a ceiling",
            )?;
        }
        let actor = format!("{} {}", self.actor.kind.as_str(), self.actor.name);
        let mine = self.project_id;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(expected) = expected {
            require(
                stored(&tx, Some(mine), limit)? == expected.project
                    && stored(&tx, None, limit)? == expected.instance,
                "The limit changed; read it again before saving",
            )?;
        }
        for (scope, change) in changes {
            let project = (*scope == LimitScope::Project).then_some(mine);
            let mut row = stored(&tx, project, limit)?;
            if let Some(value) = change.value {
                row.value = value;
                // The owner's choice replaces an automatic raise.
                row.extended = None;
            }
            if let Some(ceiling) = change.ceiling {
                row.ceiling = ceiling;
            }
            if let (Some(value), Some(ceiling)) = (row.value, row.ceiling) {
                require(value <= ceiling, "The ceiling cannot be below the limit")?;
            }
            save(&tx, project, limit, row)?;
            if *scope == LimitScope::Instance && change.value.is_some() {
                // An instance default the owner sets also replaces this project's automatic raise.
                let mut own = stored(&tx, Some(mine), limit)?;
                own.extended = None;
                save(&tx, Some(mine), limit, own)?;
            }
            let shown = |value: Option<u64>| value.map_or("inherited".into(), |v| v.to_string());
            limit_event(
                &tx,
                mine,
                None,
                &format!(
                    "Resource limit {limit} ({}) set to {} with ceiling {} by {actor}",
                    scope.name(),
                    shown(row.value),
                    shown(row.ceiling)
                ),
            )?;
        }
        tx.commit()?;
        self.resource_limit(limit)
    }

    /// The limit in force for this project's attempts.
    pub(crate) fn effective_setting(&self, limit: Limit) -> Result<Effective> {
        effective(self.db.connection(), self.project_id, limit)
    }

    /// Steady growth: a need of at most twice the limit in force and within the owner's
    /// ceiling raises the project's limit, kept for later tasks, without a model turn.
    pub(crate) fn extend_steadily(&self, attempt: i64, limit: Limit, need: u64) -> Result<bool> {
        let db = self.db.connection();
        let tx = Transaction::new_unchecked(db, TransactionBehavior::Immediate)?;
        let current = super::resource_breach::effective_limit(&tx, attempt, limit)?;
        let setting = effective(&tx, self.project_id, limit)?;
        let doubled = current.saturating_mul(crate::resource::EXTENSION_FACTOR);
        if setting.source == "env" || need <= current || need > doubled || need > setting.ceiling {
            return Ok(false);
        }
        let to = doubled.min(setting.ceiling);
        let mut row = stored(&tx, Some(self.project_id), limit)?;
        row.extended = Some(to);
        save(&tx, Some(self.project_id), limit, row)?;
        limit_event(
            &tx,
            self.project_id,
            Some(attempt),
            &format!(
                "Resource limit {limit} extended from {} to {} for this project: evidence grew steadily to {}",
                limit.show(current),
                limit.show(to),
                limit.show(need)
            ),
        )?;
        tx.commit()?;
        Ok(true)
    }

    pub(crate) fn record_scratch_peak(&self, attempt: i64, bytes: u64, entries: u64) -> Result<()> {
        self.db.connection().execute(
            "INSERT INTO attempt_scratch_peaks(attempt_id,bytes,entries) VALUES (?1,?2,?3)
             ON CONFLICT(attempt_id) DO UPDATE SET bytes=max(bytes,excluded.bytes),entries=max(entries,excluded.entries)",
            params![
                attempt,
                i64::try_from(bytes).unwrap_or(i64::MAX),
                i64::try_from(entries).unwrap_or(i64::MAX)
            ],
        )?;
        Ok(())
    }

    /// Records why the attempt pauses on a limit, replacing an earlier record.
    pub(crate) fn record_resource_pause(&self, attempt: i64, pause: &PauseCause<'_>) -> Result<()> {
        let number = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
        self.db.connection().execute(
            "INSERT OR REPLACE INTO resource_pauses(attempt_id,limit_name,value,measured,deterministic,source_tree,claim)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                attempt,
                pause.limit.name(),
                number(pause.value),
                pause.measured.map(number),
                pause.deterministic,
                pause.source_tree,
                number(pause.claim)
            ],
        )?;
        Ok(())
    }

    /// A refusal of the storage budget repeats until the budget or the room under it changes,
    /// whatever the source holds.
    pub(crate) fn record_storage_pause(
        &self,
        attempt: i64,
        refusal: &StorageRefusal,
    ) -> Result<()> {
        self.record_resource_pause(
            attempt,
            &PauseCause {
                limit: PauseLimit::Storage(refusal.budget),
                value: refusal.value,
                measured: Some(refusal.need),
                deterministic: true,
                source_tree: None,
                claim: refusal.claim,
            },
        )
    }

    pub(crate) fn clear_resource_pause(&self, attempt: i64) -> Result<()> {
        self.db
            .connection()
            .execute("DELETE FROM resource_pauses WHERE attempt_id=?1", [attempt])?;
        Ok(())
    }

    /// The limit a paused attempt waits on.
    pub fn resource_pause(&self, attempt: i64) -> Result<Option<ResourcePause>> {
        pause_of(self.db.connection(), attempt)
    }

    /// Refuse a plain Resume that would only repeat the work: the pause is deterministic and
    /// neither its limit nor, when the pause depends on it, the source has changed.
    pub(crate) fn require_changed_cause(&self, attempt: i64, tree: &str) -> Result<()> {
        let Some(pause) = self.resource_pause(attempt)? else {
            return Ok(());
        };
        if !pause.repeats() || pause.source_tree.as_deref().is_some_and(|at| at != tree) {
            return Ok(());
        }
        Err(Error::Conflict(format!(
            "Resume refused: {} and nothing that caused the pause has changed, so the same work would pause again. {}",
            pause.describe(),
            pause.offer()
        )))
    }

    /// Raise the limit the current paused task waits on, to the proposal or to `to`. The
    /// caller then resumes the task as usual.
    pub fn raise_paused_limit(
        &mut self,
        task: i64,
        to: Option<u64>,
    ) -> Result<(PauseLimit, u64, u64)> {
        let current = self
            .current_task()?
            .filter(|current| current.id == task)
            .ok_or_else(|| Error::Conflict("Only the current task can be resumed".into()))?;
        current.require(lifecycle::Action::Resume)?;
        let pause = current
            .attempts
            .last()
            .and_then(|attempt| attempt.resource_pause.clone())
            .ok_or_else(|| {
                Error::Conflict(format!("Task #{task} is not paused on a resource limit"))
            })?;
        let blocked = || Error::Conflict(pause.blocked.clone().unwrap_or_default());
        if pause.unraisable {
            return Err(blocked());
        }
        let to = match to {
            Some(to) => to,
            None => pause.proposal.ok_or_else(blocked)?,
        };
        require(
            to > pause.current && pause.measured.is_none_or(|need| to >= need),
            &format!(
                "The new {} must be above {} and cover the measured need",
                pause.limit,
                pause.limit.show(pause.current)
            ),
        )?;
        require(
            to <= pause.ceiling,
            &format!(
                "{} exceeds the owner ceiling of {}; raise the ceiling first",
                pause.limit.show(to),
                pause.limit.show(pause.ceiling)
            ),
        )?;
        match pause.limit {
            PauseLimit::Resource(limit) => {
                let change = LimitChange {
                    value: Some(Some(to)),
                    ceiling: None,
                };
                self.set_resource_limit(LimitScope::Project, limit, change, None)?;
            }
            PauseLimit::Storage(budget) => self.raise_storage_budget(budget, to)?,
        }
        Ok((pause.limit, pause.current, to))
    }
}

/// What a runner records when it pauses an attempt on a limit.
pub(crate) struct PauseCause<'a> {
    pub limit: PauseLimit,
    pub value: u64,
    pub measured: Option<u64>,
    pub deterministic: bool,
    /// Set when a change of the source could change the outcome.
    pub source_tree: Option<&'a str>,
    /// What a refused storage write would have added; zero otherwise.
    pub claim: u64,
}

pub(super) fn pause_of(db: &Connection, attempt: i64) -> Result<Option<ResourcePause>> {
    type Row = (String, i64, Option<i64>, bool, Option<String>, i64, i64);
    let row: Option<Row> = db
        .query_row(
            "SELECT p.limit_name,p.value,p.measured,p.deterministic,p.source_tree,p.claim,a.project_id
             FROM resource_pauses p JOIN attempts a ON a.id=p.attempt_id
             WHERE p.attempt_id=?1 AND a.state='paused'",
            [attempt],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                    r.get(6)?,
                ))
            },
        )
        .optional()?;
    let Some((name, value, measured, deterministic, source_tree, claim, project)) = row else {
        return Ok(None);
    };
    let limit = PauseLimit::parse(&name)
        .ok_or_else(|| Error::Conflict(format!("Unknown resource limit {name}")))?;
    let measured = measured.map(|n| n as u64);
    let standing = match limit {
        PauseLimit::Resource(limit) => {
            let setting = effective(db, project, limit)?;
            // Without the allowance an undiagnosed output breach has: that is no owner raise.
            let current = super::resource_breach::raised_limit(db, attempt, limit)?;
            Standing {
                current,
                ceiling: setting.ceiling,
                proposal: proposal(limit, setting, current, measured),
                governed_elsewhere: setting.source == "env",
                relieved: false,
            }
        }
        PauseLimit::Storage(budget) => storage_standing(
            db,
            project,
            budget,
            measured.unwrap_or(0),
            claim.max(0) as u64,
        )?,
    };
    let (proposal, blocked) = match standing.proposal {
        Ok(to) => (Some(to), None),
        Err(why) => (None, Some(why)),
    };
    Ok(Some(ResourcePause {
        limit,
        value: value as u64,
        measured,
        deterministic,
        source_tree,
        current: standing.current,
        ceiling: standing.ceiling,
        proposal,
        blocked,
        unraisable: standing.governed_elsewhere,
        relieved: standing.relieved,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setting(value: u64, ceiling: u64, source: &'static str) -> Effective {
        Effective {
            value,
            ceiling,
            source,
        }
    }

    #[test]
    fn a_proposal_covers_the_need_with_headroom_under_the_ceiling() {
        let limit = Limit::ArtifactAttemptFiles;
        let propose =
            |ceiling, measured| proposal(limit, setting(100, ceiling, "project"), 100, measured);
        assert_eq!(propose(1000, Some(150)), Ok(300));
        assert_eq!(propose(1000, None), Ok(202));
        assert_eq!(propose(200, Some(150)), Ok(200));
        assert!(propose(120, Some(150)).unwrap_err().contains("ceiling"));
        assert!(propose(100, None).is_err());
        let overridden = proposal(limit, setting(100, 400, "env"), 100, Some(150));
        assert!(
            overridden
                .unwrap_err()
                .contains("KTASK_ARTIFACT_ATTEMPT_FILES")
        );
    }

    #[test]
    fn warnings_need_less_than_twice_the_headroom() {
        let limit = Limit::OutputRows;
        assert_eq!(warning(limit, 100, None), None);
        assert_eq!(warning(limit, 100, Some(50)), None);
        assert!(warning(limit, 100, Some(51)).is_some());
    }
}
