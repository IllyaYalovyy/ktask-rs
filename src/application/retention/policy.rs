//! Retention policy, storage limits and the capacity reserves derived from them.
use super::*;
use rusqlite::Connection;

/// A self-hosting instance's SQLite file alone can exceed 1 GiB, so the default budget is
/// several GiB. A project may still configure any value.
pub(super) const DEFAULT_MAX_BYTES: i64 = 8 * 1024 * 1024 * 1024;
const DEFAULT_MAX_FILES: i64 = 100_000;
const DEFAULT_DAYS: i64 = 30;
const DAY_MS: i64 = 86_400_000;

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

pub(super) fn policy(db: &Connection, project: i64) -> Result<Policy> {
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
        let byte_reserve = (limits.max_bytes / self.divisor()).min(256 * 1024 * 1024);
        let file_reserve = (limits.max_files / self.divisor()).clamp(1, 256);
        bytes.saturating_add(byte_reserve) >= limits.max_bytes
            || files.saturating_add(file_reserve) >= limits.max_files
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
        require(
            (0..=365000).contains(&days),
            "Retention days must be between 0 and 365000",
        )?;
        require(
            (1_048_576..=1_099_511_627_776).contains(&max_bytes),
            "Storage bytes must be between 1 MiB and 1 TiB",
        )?;
        require(
            (16..=10_000_000).contains(&max_files),
            "Storage files must be between 16 and 10000000",
        )?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            policy(&tx, self.project_id)? == expected,
            "Retention changed elsewhere; reopen and retry",
        )?;
        let next = Policy {
            days,
            max_bytes,
            max_files,
            version: expected.version + 1,
        };
        tx.execute("INSERT INTO project_retention(project_id,days,version) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET days=excluded.days,version=excluded.version", params![self.project_id, days, next.version])?;
        tx.execute("INSERT INTO project_storage_limits(project_id,max_bytes,max_files) VALUES (?1,?2,?3) ON CONFLICT(project_id) DO UPDATE SET max_bytes=excluded.max_bytes,max_files=excluded.max_files", params![self.project_id,max_bytes,max_files])?;
        tx.commit()?;
        Ok(next)
    }
}
