//! The shared storage budget as what a resource pause waits on: what it refused, where it
//! stands now and what raising it would take. The retention policy stays its only store.
use super::policy::{MOST_BYTES, MOST_FILES, Reserve, instance_limits, smallest_other_budget};
use super::*;
use crate::resource::{PauseLimit, StorageBudget, StorageRefusal};
use rusqlite::Connection;

const BYTES: StorageBudget = StorageBudget::StorageMaxBytes;
const FILES: StorageBudget = StorageBudget::StorageMaxFiles;

fn of(budget: StorageBudget, (bytes, files): (i64, i64)) -> i64 {
    match budget {
        StorageBudget::StorageMaxBytes => bytes,
        StorageBudget::StorageMaxFiles => files,
    }
}

/// The refusal when `after`, the allocation with the claimed `(bytes, files)`, would not
/// leave the work reserve of the `most` budgets free.
pub(super) fn refusal(
    after: (i64, i64),
    claim: (i64, i64),
    most: (i64, i64),
    reason: impl FnOnce() -> String,
) -> Option<StorageRefusal> {
    let budget = [BYTES, FILES]
        .into_iter()
        .find(|&budget| Reserve::Work.reached_in(budget, of(budget, after), of(budget, most)))?;
    Some(StorageRefusal {
        budget,
        value: of(budget, most) as u64,
        need: Reserve::Work.smallest_budget(budget, of(budget, after)) as u64,
        claim: of(budget, claim).max(0) as u64,
        reason: reason(),
    })
}

/// Where the limit of a resource pause stands now.
pub(in crate::application) struct Standing {
    /// The limit in force; for storage, the smallest budget among the instance's projects.
    pub current: u64,
    pub ceiling: u64,
    /// What raising this project's value would set, or why it cannot.
    pub proposal: std::result::Result<u64, String>,
    /// A stronger value governs, whatever this project stores: a test override, or
    /// another project's smaller budget.
    pub governed_elsewhere: bool,
    /// Reclaimed or aged-out evidence left room: a recorded storage refusal no longer holds.
    pub relieved: bool,
}

/// `need` and `claim` are what the pause recorded. A raise must hold the need and leave the
/// launch reserve free, or the resumed run would halt before it starts.
pub(in crate::application) fn storage_standing(
    db: &Connection,
    project: i64,
    budget: StorageBudget,
    need: u64,
    claim: u64,
) -> Result<Standing> {
    let limits = instance_limits(db)?;
    let current = of(budget, (limits.max_bytes, limits.max_files));
    let used = of(
        budget,
        db.query_row(
            "SELECT projected_bytes,projected_files FROM instance_storage_admission WHERE id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?,
    );
    let claim = i64::try_from(claim).unwrap_or(i64::MAX);
    let relieved = !Reserve::Launch.reached_in(budget, used, current)
        && !Reserve::Work.reached_in(budget, used.saturating_add(claim), current);
    let ceiling = of(budget, (MOST_BYTES, MOST_FILES));
    let floor = i64::try_from(need)
        .unwrap_or(i64::MAX)
        .max(Reserve::Launch.smallest_budget(budget, used));
    let other = smallest_other_budget(db, project, budget)?;
    let governed_elsewhere = other.is_some_and(|other| other < floor);
    let name = PauseLimit::Storage(budget);
    let proposal = if governed_elsewhere {
        Err(format!(
            "Another project's smaller {name} governs the shared allocation and does not cover {}; raise it in that project (ktask retention, or Configuration → Storage)",
            name.show(floor as u64)
        ))
    } else if floor > ceiling {
        Err(format!(
            "{name} cannot exceed {}, which does not cover the need of {}; reclaim evidence (Configuration → Storage)",
            name.show(ceiling as u64),
            name.show(floor as u64)
        ))
    } else {
        Ok(floor
            .max(current)
            .saturating_mul(2)
            .min(ceiling)
            .min(other.unwrap_or(ceiling)) as u64)
    };
    Ok(Standing {
        current: current as u64,
        ceiling: ceiling as u64,
        proposal,
        governed_elsewhere,
        relieved,
    })
}

impl Service {
    /// Raise one dimension of this project's storage budget through its retention policy.
    pub(in crate::application) fn raise_storage_budget(
        &mut self,
        budget: StorageBudget,
        to: u64,
    ) -> Result<()> {
        let policy = self.retention_policy()?;
        let to = i64::try_from(to).unwrap_or(i64::MAX);
        let (bytes, files) = match budget {
            StorageBudget::StorageMaxBytes => (to, policy.max_files),
            StorageBudget::StorageMaxFiles => (policy.max_bytes, to),
        };
        self.set_storage_policy(policy, policy.days, bytes, files)?;
        Ok(())
    }
}
