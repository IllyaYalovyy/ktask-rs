//! Read projections for recorded provider usage. Missing fields stay unknown.
use crate::storage::Result;
use rusqlite::{Connection, params};

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct UsageTotals {
    pub calls: i64,
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
    pub cost_microusd: Option<i64>,
}

impl UsageTotals {
    fn add(&mut self, fields: [Option<i64>; 5]) {
        if self.calls == 0 {
            self.input = Some(0);
            self.cache_read = Some(0);
            self.cache_write = Some(0);
            self.output = Some(0);
            self.cost_microusd = Some(0);
        }
        self.calls += 1;
        for (target, value) in [
            &mut self.input,
            &mut self.cache_read,
            &mut self.cache_write,
            &mut self.output,
            &mut self.cost_microusd,
        ]
        .into_iter()
        .zip(fields)
        {
            *target = target.and_then(|sum| value.and_then(|n| sum.checked_add(n)));
        }
    }

    pub fn label(&self) -> String {
        fn number(value: Option<i64>) -> String {
            value.map_or("unknown".into(), |n| n.to_string())
        }
        let cost = self.cost_microusd.map_or("unknown".into(), |n| {
            format!("${:.6}", n as f64 / 1_000_000.0)
        });
        format!(
            "{} calls · uncached {} · cache read {} · cache write {} · output {} · cost {}",
            self.calls,
            number(self.input),
            number(self.cache_read),
            number(self.cache_write),
            number(self.output),
            cost
        )
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct RoleUsage {
    pub role: String,
    pub model: String,
    pub effort: Option<String>,
    pub totals: UsageTotals,
    /// Live provider calls advance until the call or its parent attempt finishes.
    pub provider_elapsed_ms: i64,
}

pub fn attempt(db: &Connection, id: i64) -> Result<(UsageTotals, Vec<RoleUsage>)> {
    let mut stmt = db.prepare_cached(
        "SELECT c.role,c.provider || '/' || c.model,c.effort,c.input_tokens,c.cache_read_tokens,c.cache_write_tokens,c.output_tokens,c.cost_microusd,c.started_at_ms,c.finished_at_ms,a.finished_at_ms
         FROM model_calls c JOIN attempt_steps s ON s.id=c.step_id JOIN attempts a ON a.id=s.attempt_id WHERE s.attempt_id=?1 ORDER BY c.id",
    )?;
    let rows = stmt.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            [r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?, r.get(7)?],
            r.get::<_, i64>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, Option<i64>>(10)?,
        ))
    })?;
    let mut total = UsageTotals::default();
    let mut roles: Vec<RoleUsage> = Vec::new();
    for row in rows {
        let (role, model, effort, fields, started, finished, attempt_finished) = row?;
        let elapsed = finished
            .or(attempt_finished)
            .unwrap_or_else(super::now_ms)
            .saturating_sub(started);
        total.add(fields);
        if let Some(existing) = roles
            .iter_mut()
            .find(|r| r.role == role && r.model == model && r.effort == effort)
        {
            existing.totals.add(fields);
            existing.provider_elapsed_ms = existing.provider_elapsed_ms.saturating_add(elapsed);
        } else {
            let mut totals = UsageTotals::default();
            totals.add(fields);
            roles.push(RoleUsage {
                role,
                model,
                effort,
                totals,
                provider_elapsed_ms: elapsed,
            });
        }
    }
    Ok((total, roles))
}

pub fn project(db: &Connection, project: i64) -> Result<UsageTotals> {
    let mut stmt = db.prepare_cached(
        "SELECT input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_microusd FROM model_calls WHERE project_id=?1 ORDER BY id",
    )?;
    let mut total = UsageTotals::default();
    for row in stmt.query_map(params![project], |r| {
        Ok([r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?])
    })? {
        total.add(row?);
    }
    Ok(total)
}
