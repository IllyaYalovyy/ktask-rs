//! Read projections for recorded provider usage. Missing fields stay unknown.
use crate::storage::Result;
use rusqlite::{Connection, params};

/// Sums of what providers reported. A call that reported nothing (it failed before the
/// provider sent usage, or the provider never reports cost) is counted, not guessed as zero,
/// and does not hide what the other calls reported.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct UsageTotals {
    pub calls: i64,
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
    pub cost_microusd: Option<i64>,
    /// Calls without token counts.
    pub unreported_calls: i64,
    /// Calls without a cost.
    pub uncosted_calls: i64,
}

impl UsageTotals {
    fn add(&mut self, fields: [Option<i64>; 5]) {
        self.calls += 1;
        self.unreported_calls += i64::from(fields[0].is_none());
        self.uncosted_calls += i64::from(fields[4].is_none());
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
            if let Some(value) = value {
                *target = Some(target.unwrap_or(0).saturating_add(value));
            }
        }
    }

    /// "$0.123456", or why there is no cost.
    pub fn cost_label(&self) -> String {
        match self.cost_microusd {
            Some(n) if self.uncosted_calls == 0 => format!("${:.6}", n as f64 / 1_000_000.0),
            Some(n) => format!(
                "${:.6} + {} call(s) without cost",
                n as f64 / 1_000_000.0,
                self.uncosted_calls
            ),
            None if self.calls == 0 => "none yet".into(),
            None => "not reported by the provider".into(),
        }
    }

    pub fn label(&self) -> String {
        fn number(value: Option<i64>) -> String {
            value.map_or("unknown".into(), grouped)
        }
        let unreported = if self.unreported_calls > 0 && self.input.is_some() {
            format!(" (+{} call(s) unreported)", self.unreported_calls)
        } else {
            String::new()
        };
        format!(
            "{} calls · uncached {} · cache read {} · cache write {} · output {}{unreported} · cost {}",
            self.calls,
            number(self.input),
            number(self.cache_read),
            number(self.cache_write),
            number(self.output),
            self.cost_label()
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

/// A token count as people read it: "?" when unreported, otherwise "980", "99k" or "4.8M".
pub fn count(value: Option<i64>) -> String {
    match value {
        None => "?".into(),
        Some(n) if n < 1_000 => n.to_string(),
        Some(n) if n < 1_000_000 => format!("{}k", n / 1_000),
        Some(n) if n < 10_000_000 => format!("{:.1}M", n as f64 / 1_000_000.0),
        Some(n) if n < 1_000_000_000 => format!("{}M", n / 1_000_000),
        Some(n) => format!("{}G", n / 1_000_000_000),
    }
}

/// An exact count with thousands separators: "14,081,664".
pub fn grouped(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3 + 1);
    if n < 0 {
        text.push('-');
    }
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_read_compactly_and_exactly() {
        for (n, short, exact) in [
            (0, "0", "0"),
            (980, "980", "980"),
            (36_898, "36k", "36,898"),
            (4_812_345, "4.8M", "4,812,345"),
            (14_081_664, "14M", "14,081,664"),
            (246_913_578_024, "246G", "246,913,578,024"),
            (-1_200, "-1200", "-1,200"),
        ] {
            assert_eq!(count(Some(n)), short);
            assert_eq!(grouped(n), exact);
        }
        assert_eq!(count(None), "?");
    }
}
