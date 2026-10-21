//! Local wall-clock conversion for provider reset times and their display.
use chrono::{Datelike, Local, LocalResult, NaiveDate, TimeZone};

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Month number (1-12) for an English month name or its three-letter prefix.
pub(crate) fn month(name: &str) -> Option<u32> {
    let prefix = name.get(..3)?;
    MONTHS
        .iter()
        .position(|m| m.eq_ignore_ascii_case(prefix))
        .map(|i| i as u32 + 1)
}

fn local(ms: i64) -> chrono::DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .expect("epoch milliseconds map to one instant")
}

/// Epoch milliseconds for a local date and time, honouring the local time zone.
pub(crate) fn local_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> Option<i64> {
    let naive = NaiveDate::from_ymd_opt(year, month, day)?.and_hms_opt(hour, minute, 0)?;
    match Local.from_local_datetime(&naive) {
        LocalResult::Single(at) | LocalResult::Ambiguous(at, _) => Some(at.timestamp_millis()),
        LocalResult::None => None,
    }
}

/// The local date of an instant as (year, month, day).
pub(crate) fn local_date(ms: i64) -> (i32, u32, u32) {
    let at = local(ms);
    (at.year(), at.month(), at.day())
}

/// "Oct 26 23:32", with the year when it differs from `now_ms`'s year.
pub(crate) fn format_local(ms: i64, now_ms: i64) -> String {
    let at = local(ms);
    if at.year() == local(now_ms).year() {
        at.format("%b %-d %H:%M").to_string()
    } else {
        at.format("%b %-d %Y %H:%M").to_string()
    }
}

/// Compact remaining time: "45s", "12m", "3h 5m", "6d 4h".
pub(crate) fn format_remaining(seconds: i64) -> String {
    let (d, h, m) = (
        seconds / 86_400,
        seconds % 86_400 / 3600,
        seconds % 3600 / 60,
    );
    match (d, h, m) {
        (0, 0, 0) => format!("{}s", seconds.max(0)),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_round_trip_and_remaining() {
        let ms = local_ms(2026, 10, 26, 23, 32).unwrap();
        let now = local_ms(2026, 10, 20, 18, 0).unwrap();
        assert_eq!(format_local(ms, now), "Oct 26 23:32");
        assert_eq!(
            format_local(ms, local_ms(2025, 1, 1, 0, 0).unwrap()),
            "Oct 26 2026 23:32"
        );
        assert_eq!(local_date(ms), (2026, 10, 26));
        assert_eq!(month("october"), Some(10));
        assert_eq!(month("Xy"), None);
        assert_eq!(format_remaining(45), "45s");
        assert_eq!(format_remaining(3 * 3600 + 5 * 60), "3h 5m");
        assert_eq!(format_remaining(6 * 86_400 + 4 * 3600 + 59), "6d 4h");
    }
}
