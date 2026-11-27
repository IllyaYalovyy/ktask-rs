//! Retention policy, storage limits and the capacity reserves derived from them.
use super::*;
use crate::resource::StorageBudget;
use rusqlite::Connection;

/// A self-hosting instance's SQLite file alone can exceed 1 GiB, so the default budget is
/// several GiB. A project may still configure any value.
pub(super) const DEFAULT_MAX_BYTES: i64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_MAX_FILES: i64 = 100_000;
const DEFAULT_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;
/// The largest budgets a policy accepts: the ceiling of raising one.
pub(super) const MOST_BYTES: i64 = 1_099_511_627_776;
pub(super) const MOST_FILES: i64 = 10_000_000;

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct Policy {
    pub days: i64,
    pub max_bytes: i64,
    pub max_files: i64,
    pub version: i64,
}

impl Policy {
    /// Closed evidence finished at or before this instant is past the minimum age.
    pub(super) fn cutoff(&self, now: i64) -> i64 {
        now.saturating_sub(self.retention_ms())
    }

    pub(super) fn retention_ms(&self) -> i64 {
        self.days.saturating_mul(DAY_MS)
    }
}

pub(in crate::application) fn policy(db: &Connection, project: i64) -> Result<Policy> {
    Ok(db
        .query_row(
            "SELECT r.days,coalesce(l.max_bytes,?2),coalesce(l.max_files,?3),r.version FROM project_retention r LEFT JOIN project_storage_limits l ON l.project_id=r.project_id WHERE r.project_id=?1",
            params![project, DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES],
            |r| {
                Ok(Policy {
                    days: r.get(0)?,
                    max_bytes: r.get(1)?,
                    max_files: r.get(2)?,
                    version: r.get(3)?,
                })
            },
        )
        .optional()?
        .unwrap_or(Policy {
            days: DEFAULT_DAYS,
            max_bytes: DEFAULT_MAX_BYTES,
            max_files: DEFAULT_MAX_FILES,
            version: 0,
        }))
}

pub(in crate::application) fn validate_policy(policy: Policy) -> Result<()> {
    require(
        (0..=365000).contains(&policy.days),
        "Retention days must be between 0 and 365000",
    )?;
    require(
        (1_048_576..=MOST_BYTES).contains(&policy.max_bytes),
        "Storage bytes must be between 1 MiB and 1 TiB",
    )?;
    require(
        (16..=MOST_FILES).contains(&policy.max_files),
        "Storage files must be between 16 and 10000000",
    )
}

/// Stores `policy`, with its version, as the project's retention and storage limits.
pub(in crate::application) fn write_policy(
    db: &Connection,
    project: i64,
    policy: Policy,
) -> Result<()> {
    db.execute("INSERT INTO project_retention(project_id,days,version) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET days=excluded.days,version=excluded.version", params![project, policy.days, policy.version])?;
    db.execute("INSERT INTO project_storage_limits(project_id,max_bytes,max_files) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET max_bytes=excluded.max_bytes,max_files=excluded.max_files", params![project,policy.max_bytes,policy.max_files])?;
    Ok(())
}

/// The instance shares one allocation, so the smallest project limit binds every project.
#[derive(Clone, Copy, Debug)]
pub(super) struct Limits {
    pub max_bytes: i64,
    pub max_files: i64,
    pub projects: i64,
}

pub(super) fn instance_limits(db: &Connection) -> Result<Limits> {
    Ok(db.query_row(
        "SELECT coalesce(min(coalesce(l.max_bytes,?1)),?1),coalesce(min(coalesce(l.max_files,?2)),?2),count(*) FROM projects p LEFT JOIN project_storage_limits l ON l.project_id=p.id",
        params![DEFAULT_MAX_BYTES, DEFAULT_MAX_FILES],
        |r| Ok(Limits { max_bytes: r.get(0)?, max_files: r.get(1)?, projects: r.get(2)? }),
    )?)
}

/// The smallest budget of the instance's other projects, which `project` cannot raise.
pub(super) fn smallest_other_budget(
    db: &Connection,
    project: i64,
    budget: StorageBudget,
) -> Result<Option<i64>> {
    let (column, default) = match budget {
        StorageBudget::StorageMaxBytes => ("max_bytes", DEFAULT_MAX_BYTES),
        StorageBudget::StorageMaxFiles => ("max_files", DEFAULT_MAX_FILES),
    };
    Ok(db.query_row(
        &format!("SELECT min(coalesce(l.{column},?1)) FROM projects p LEFT JOIN project_storage_limits l ON l.project_id=p.id WHERE p.id<>?2"),
        params![default, project],
        |r| r.get(0),
    )?)
}

/// Headroom kept free below the limits: work stops at a quarter, new launches at half,
/// each capped so a large budget does not waste most of its space.
#[derive(Clone, Copy, Debug)]
pub(super) enum Reserve {
    Work,
    Launch,
}

impl Reserve {
    fn divisor(self) -> i64 {
        match self {
            Self::Work => 4,
            Self::Launch => 2,
        }
    }

    pub(super) fn reached(self, bytes: i64, files: i64, limits: Limits) -> bool {
        self.reached_in(StorageBudget::StorageMaxBytes, bytes, limits.max_bytes)
            || self.reached_in(StorageBudget::StorageMaxFiles, files, limits.max_files)
    }

    /// Whether `used` leaves less than this reserve free of a budget of `most`.
    pub(super) fn reached_in(self, budget: StorageBudget, used: i64, most: i64) -> bool {
        used.saturating_add(self.headroom(budget, most)) >= most
    }

    fn headroom(self, budget: StorageBudget, most: i64) -> i64 {
        match budget {
            StorageBudget::StorageMaxBytes => (most / self.divisor()).min(256 * 1024 * 1024),
            StorageBudget::StorageMaxFiles => (most / self.divisor()).clamp(1, 256),
        }
    }

    /// The smallest budget that holds `used` with this reserve free.
    pub(super) fn smallest_budget(self, budget: StorageBudget, used: i64) -> i64 {
        let mut most = used.max(0).saturating_add(1);
        while most < i64::MAX && self.reached_in(budget, used, most) {
            most = used
                .saturating_add(self.headroom(budget, most))
                .saturating_add(1);
        }
        most
    }
}

impl Service {
    /// Two rotated segments per project fit inside one quarter of the smallest
    /// project budget, leaving the remaining allocation for evidence and recovery.
    pub(crate) fn runner_log_budget(&self) -> Result<u64> {
        let limits = instance_limits(self.db.connection())?;
        Ok((limits.max_bytes / limits.projects.max(1) / 8).clamp(4096, 8 * 1024 * 1024) as u64)
    }

    pub fn retention_policy(&self) -> Result<Policy> {
        policy(self.db.connection(), self.project_id)
    }

    pub fn set_retention(&mut self, expected: Policy, days: i64) -> Result<Policy> {
        self.set_storage_policy(expected, days, expected.max_bytes, expected.max_files)
    }

    pub fn set_storage_policy(
        &mut self,
        expected: Policy,
        days: i64,
        max_bytes: i64,
        max_files: i64,
    ) -> Result<Policy> {
        let mut next = Policy {
            days,
            max_bytes,
            max_files,
            version: expected.version,
        };
        validate_policy(next)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            policy(&tx, self.project_id)? == expected,
            "Retention changed elsewhere; reopen and retry",
        )?;
        next.version += 1;
        write_policy(&tx, self.project_id, next)?;
        tx.commit()?;
        Ok(next)
    }
}
