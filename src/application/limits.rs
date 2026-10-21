//! Optional time limits, both off by default. The task limit caps active time across all of a
//! task's attempts; the stage limit caps one agent turn and restarts with every result.
use std::result::Result;

/// The task limit when it is switched on.
pub const TASK_LIMIT_MS: i64 = 6 * 3_600_000;
/// The stage limit when it is switched on.
pub const STAGE_LIMIT_MS: i64 = 30 * 60_000;
pub const OFF: &str = "Off";

/// "Off", "30 min", "6 h", "1 h 30 min", "45 s" or "200 ms".
pub fn label(limit: Option<i64>) -> String {
    let Some(ms) = limit else {
        return OFF.into();
    };
    if ms % 1000 != 0 || ms < 1000 {
        return format!("{ms} ms");
    }
    let seconds = ms / 1000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds % 3600 / 60, seconds % 60);
    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours} h"));
    }
    if minutes > 0 {
        parts.push(format!("{minutes} min"));
    }
    if seconds > 0 {
        parts.push(format!("{seconds} s"));
    }
    parts.join(" ")
}

/// Inverse of [`label`]; also accepts compact forms such as "30m", "6h" or "90s".
pub fn parse(text: &str) -> Result<Option<i64>, String> {
    let text = text.trim();
    if text.is_empty() || text.eq_ignore_ascii_case(OFF) {
        return Ok(None);
    }
    let invalid = || format!("Time limit {text:?}: use Off, or a duration such as 30 min or 6 h");
    let mut total = 0_i64;
    let mut rest = text;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let number: i64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest = rest[digits..].trim_start();
        let unit = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let scale = match &rest[..unit] {
            "ms" => 1,
            "s" | "sec" => 1000,
            "m" | "min" => 60_000,
            "h" | "hr" | "hour" | "hours" => 3_600_000,
            _ => return Err(invalid()),
        };
        total = number
            .checked_mul(scale)
            .and_then(|n| total.checked_add(n))
            .ok_or_else(invalid)?;
        rest = rest[unit..].trim_start();
    }
    (total > 0).then_some(Some(total)).ok_or_else(invalid)
}

/// Off, the standard limit, and the current value when it is neither.
pub fn choices(current: Option<i64>, standard: i64) -> Vec<String> {
    let mut choices = vec![OFF.to_owned(), label(Some(standard))];
    if current.is_some_and(|ms| ms != standard) {
        choices.push(label(current));
    }
    choices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_round_trip_and_compact_forms_parse() {
        for limit in [
            None,
            Some(STAGE_LIMIT_MS),
            Some(TASK_LIMIT_MS),
            Some(5_400_000),
            Some(45_000),
            Some(200),
        ] {
            assert_eq!(parse(&label(limit)).unwrap(), limit, "{}", label(limit));
        }
        assert_eq!(label(Some(STAGE_LIMIT_MS)), "30 min");
        assert_eq!(label(Some(TASK_LIMIT_MS)), "6 h");
        assert_eq!(parse("30m").unwrap(), Some(STAGE_LIMIT_MS));
        assert_eq!(parse("6h").unwrap(), Some(TASK_LIMIT_MS));
        assert_eq!(parse("").unwrap(), None);
        assert!(parse("soon").is_err() && parse("0 min").is_err());
        assert_eq!(choices(None, STAGE_LIMIT_MS), ["Off", "30 min"]);
        assert_eq!(
            choices(Some(600_000), STAGE_LIMIT_MS),
            ["Off", "30 min", "10 min"]
        );
    }
}
