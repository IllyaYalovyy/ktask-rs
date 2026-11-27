//! A storage limit that stopped its producer, the decision on it, and the limits in effect.
//!
//! The limits are stored settings (see `resource_limits`); an `extend` decision raises one
//! of them for one attempt, at most [`MAX_EXTENSIONS`] times, never above the owner's
//! ceiling and never into the storage reserve.
use super::{
    execution::{Run, close_unfinished},
    progress::Decision,
    *,
};
use crate::resource::{Breach, Limit, MAX_EXTENSIONS};

/// Free space kept on the scratch filesystem that no extension may use.
const SCRATCH_RESERVE_BYTES: u64 = 256 * 1024 * 1024;
const SCRATCH_RESERVE_ENTRIES: u64 = 256;
/// What one persisted output row costs the storage reserve.
const ROW_COST: u64 = 256;

/// A breach awaiting, or carrying, its decision.
pub(crate) struct Breached {
    pub id: i64,
    pub limit: Limit,
    pub configured: u64,
    pub measured: u64,
    pub cause: String,
    pub summary: String,
    /// The owner's text for this breach: the pause reason when nothing resolves it.
    pub report: String,
    /// Last output sequence of the attempt when the breach was recorded: the stopped work's transcript ends here.
    pub output_end: i64,
}

pub(crate) struct BreachDecision<'a> {
    pub id: i64,
    pub action: &'static str,
    pub reason: &'a str,
    pub extended_to: Option<u64>,
}

impl Decision for BreachDecision<'_> {
    fn record(&self, tx: &Transaction<'_>, run: &Run) -> Result<()> {
        let now = now_ms();
        require(
            tx.execute(
                "UPDATE resource_breaches SET decision=?3,reason=?4,extended_to=?5,decided_at_ms=?6
                 WHERE id=?1 AND attempt_id=?2 AND decision IS NULL",
                params![
                    self.id,
                    run.attempt,
                    self.action,
                    self.reason,
                    self.extended_to.map(|limit| limit as i64),
                    now
                ],
            )? == 1,
            "Resource breach changed before its decision was recorded",
        )?;
        let (limit, measured, configured): (String, i64, i64) = tx.query_row(
            "SELECT limit_name,measured,configured FROM resource_breaches WHERE id=?1",
            [self.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let project: i64 = tx.query_row(
            "SELECT project_id FROM attempts WHERE id=?1",
            [run.attempt],
            |r| r.get(0),
        )?;
        tx.execute(
            "INSERT INTO events(project_id,task_id,attempt_id,occurred_at_ms,kind,level,message)
             VALUES (?1,?2,?3,?4,'resource.breach',?5,?6)",
            params![
                project,
                run.task,
                run.attempt,
                now,
                if self.action == "escalate" { "warning" } else { "info" },
                format!(
                    "Resource limit {limit}: measured {measured} against {configured}; decision {}{}: {}",
                    self.action,
                    self.extended_to
                        .map_or_else(String::new, |to| format!(" to {to}")),
                    self.reason
                )
            ],
        )?;
        Ok(())
    }
}

/// How often each limit was breached and how the breach was decided, for the project.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LimitStats {
    pub limit: &'static str,
    pub repaired: i64,
    pub extended: i64,
    pub escalated: i64,
}

impl LimitStats {
    pub fn label(&self) -> String {
        format!(
            "{}: {} repaired · {} extended · {} escalated",
            self.limit, self.repaired, self.extended, self.escalated
        )
    }
}

/// The limits that were breached and how each breach was decided; empty when none was.
pub fn limit_lines(limits: &[LimitStats]) -> String {
    let breached: Vec<_> = limits
        .iter()
        .filter(|l| l.repaired + l.extended + l.escalated > 0)
        .collect();
    if breached.is_empty() {
        return String::new();
    }
    let mut text = String::from("Resource limit breaches:\n");
    for limit in breached {
        text.push_str(&format!("  {}\n", limit.label()));
    }
    text
}

/// The limit in effect for `attempt`: configured, raised by each `extend` decision, and for
/// an output limit with a breach still being diagnosed, with room for the diagnosis itself.
pub(crate) fn effective_limit(
    db: &rusqlite::Connection,
    attempt: i64,
    limit: Limit,
) -> Result<u64> {
    let raised = raised_limit(db, attempt, limit)?;
    let undiagnosed: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM resource_breaches WHERE attempt_id=?1 AND limit_name=?2 AND decision IS NULL)",
        params![attempt, limit.name()],
        |r| r.get(0),
    )?;
    Ok(
        if undiagnosed && matches!(limit, Limit::OutputBytes | Limit::OutputRows) {
            raised.saturating_add(raised / 8)
        } else {
            raised
        },
    )
}

/// The stored limit raised by the attempt's `extend` decisions.
pub(crate) fn raised_limit(db: &rusqlite::Connection, attempt: i64, limit: Limit) -> Result<u64> {
    let (extended, project): (u32, i64) = db.query_row(
        "SELECT (SELECT count(*) FROM resource_breaches WHERE attempt_id=?1 AND limit_name=?2 AND decision IS 'extend'),
                project_id FROM attempts WHERE id=?1",
        params![attempt, limit.name()],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(super::resource_limits::effective(db, project, limit)?.raised(extended))
}

impl Service {
    /// Records a breach and ends the unfinished steps it interrupted. A second report of a
    /// breach that is still undecided adds nothing.
    pub(crate) fn record_breach(&mut self, run: &Run, breach: &Breach) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM attempts WHERE id=?1 AND execution_token=?2 AND state='running')",
                params![run.attempt, run.token],
                |r| r.get(0),
            )?,
            "Attempt ownership lost",
        )?;
        let now = now_ms();
        close_unfinished(&tx, run.attempt, now, &breach.reason, "failed")?;
        let open: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_breaches WHERE attempt_id=?1 AND decision IS NULL)",
            [run.attempt],
            |r| r.get(0),
        )?;
        if !open {
            tx.execute(
                "INSERT INTO resource_breaches(attempt_id,limit_name,configured,measured,cause,summary,report,output_end,detected_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,(SELECT coalesce(max(sequence),0) FROM attempt_output WHERE attempt_id=?1),?8)",
                params![
                    run.attempt,
                    breach.limit.name(),
                    breach.configured as i64,
                    breach.measured as i64,
                    breach.cause,
                    breach.summary,
                    breach.reason,
                    now
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn pending_breach(&self, run: &Run) -> Result<Option<Breached>> {
        self.db
            .connection()
            .query_row(
                "SELECT id,limit_name,configured,measured,cause,summary,report,output_end FROM resource_breaches
                 WHERE attempt_id=?1 AND decision IS NULL ORDER BY id DESC LIMIT 1",
                [run.attempt],
                |r| {
                    let name: String = r.get(1)?;
                    Ok(Breached {
                        id: r.get(0)?,
                        limit: Limit::parse(&name).ok_or(rusqlite::Error::InvalidQuery)?,
                        configured: r.get::<_, i64>(2)? as u64,
                        measured: r.get::<_, i64>(3)? as u64,
                        cause: r.get(4)?,
                        summary: r.get(5)?,
                        report: r.get(6)?,
                        output_end: r.get(7)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// Whether this attempt already repaired or extended the same limit for the same cause.
    pub(crate) fn breach_repeats(&self, run: &Run, breach: &Breached) -> Result<bool> {
        Ok(self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM resource_breaches WHERE attempt_id=?1 AND limit_name=?2
               AND cause=?3 AND id<?4 AND decision IN ('repair','extend'))",
            params![run.attempt, breach.limit.name(), breach.cause, breach.id],
            |r| r.get(0),
        )?)
    }

    /// The limit as the attempt's own extensions leave it, ignoring any room for diagnosis.
    pub(crate) fn raised_limit(&self, run: &Run, limit: Limit) -> Result<u64> {
        raised_limit(self.db.connection(), run.attempt, limit)
    }

    /// The project's scratch limits, for workers that belong to no attempt.
    pub(crate) fn project_scratch_limits(&self) -> Result<(u64, u64)> {
        Ok((
            self.effective_setting(Limit::ScratchBytes)?.value,
            self.effective_setting(Limit::ScratchEntries)?.value,
        ))
    }

    /// Whether the attempt's last step before the breach was a project check.
    pub(crate) fn breached_in_check(&self, attempt: i64) -> Result<bool> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT role='check' FROM attempt_steps WHERE attempt_id=?1 AND role<>'remediation' ORDER BY id DESC LIMIT 1",
                [attempt],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    /// The scratch limits in force for the attempt's workers.
    pub(crate) fn scratch_limits(&self, attempt: i64) -> Result<(u64, u64)> {
        let db = self.db.connection();
        Ok((
            effective_limit(db, attempt, Limit::ScratchBytes)?,
            effective_limit(db, attempt, Limit::ScratchEntries)?,
        ))
    }

    /// The raised limit an `extend` decision would set, or None when the attempt has used its
    /// extensions, the owner's ceiling is reached, or the increase would enter the reserve of
    /// the filesystem or of the storage budget it draws on.
    pub(crate) fn extension_for(
        &self,
        run: &Run,
        limit: Limit,
        scratch: &std::path::Path,
    ) -> Result<Option<u64>> {
        let used: u32 = self.db.connection().query_row(
            "SELECT count(*) FROM resource_breaches WHERE attempt_id=?1 AND limit_name=?2 AND decision IS 'extend'",
            params![run.attempt, limit.name()],
            |r| r.get(0),
        )?;
        if used >= MAX_EXTENSIONS {
            return Ok(None);
        }
        let setting = self.effective_setting(limit)?;
        let (current, next) = (setting.raised(used), setting.raised(used + 1));
        if next <= current {
            return Ok(None);
        }
        let more = next - current;
        let fits = if limit.is_scratch() {
            scratch_has_room(scratch, limit, more)
        } else {
            let (bytes, files) = match limit {
                Limit::ArtifactAttemptFiles => (0, more),
                Limit::OutputRows => (more.saturating_mul(ROW_COST), 0),
                _ => (more, 0),
            };
            self.storage_admits(bytes, files)?
        };
        Ok(fits.then_some(next))
    }

    /// Per limit, how often breaches were repaired, extended and escalated in this project.
    pub fn resource_limit_stats(&self) -> Result<Vec<LimitStats>> {
        let mut stats = Vec::new();
        for limit in Limit::ALL {
            let counts: (i64, i64, i64) = self.db.connection().query_row(
                "SELECT coalesce(sum(b.decision IS 'repair'),0),coalesce(sum(b.decision IS 'extend'),0),coalesce(sum(b.decision IS 'escalate'),0)
                 FROM resource_breaches b JOIN attempts a ON a.id=b.attempt_id
                 WHERE a.project_id=?1 AND b.limit_name=?2",
                params![self.project_id, limit.name()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            stats.push(LimitStats {
                limit: limit.name(),
                repaired: counts.0,
                extended: counts.1,
                escalated: counts.2,
            });
        }
        Ok(stats)
    }
}

/// Whether `more` of `limit` still leaves the filesystem's reserve free.
fn scratch_has_room(scratch: &std::path::Path, limit: Limit, more: u64) -> bool {
    let Ok(stat) = rustix::fs::statvfs(scratch) else {
        return false;
    };
    let (free, reserve) = if limit.is_bytes() {
        (
            stat.f_bavail.saturating_mul(stat.f_frsize),
            SCRATCH_RESERVE_BYTES,
        )
    } else if stat.f_files == 0 {
        // The filesystem allocates entries dynamically and reports no inode count.
        return true;
    } else {
        (stat.f_favail, SCRATCH_RESERVE_ENTRIES)
    };
    free.saturating_sub(reserve) >= more
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_extension_never_enters_the_filesystem_reserve() {
        let dir = tempfile::tempdir().unwrap();
        assert!(scratch_has_room(dir.path(), Limit::ScratchBytes, 0));
        assert!(!scratch_has_room(
            dir.path(),
            Limit::ScratchBytes,
            u64::MAX / 2
        ));
        assert!(!scratch_has_room(
            &dir.path().join("missing"),
            Limit::ScratchBytes,
            0
        ));
    }
}
