//! When a rate-limited or quota-exhausted provider says it will accept work again.
use crate::clock;

/// Parse "try again at Oct 26th, 2026 11:32 PM" (local time) or "try again in 1h 20m".
/// Returns epoch milliseconds, or None when the message names no future time.
pub(crate) fn reset_at(message: &str, now_ms: i64) -> Option<i64> {
    let lower = message.to_ascii_lowercase();
    let at = if let Some(i) = lower.find("try again at ") {
        absolute(&lower[i + "try again at ".len()..], now_ms)
    } else if let Some(i) = lower.find("try again in ") {
        relative(&lower[i + "try again in ".len()..]).map(|ms| now_ms + ms)
    } else {
        None
    };
    at.filter(|&at| at > now_ms)
}

fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| c.is_whitespace() || c == ',')
        .filter(|w| !w.is_empty())
        .map(|w| w.trim_end_matches(['.', ';', ')', '"', '\\']))
        .collect()
}

// "oct 26th, 2026 11:32 pm", "oct 26 11:32 pm" or "11:32 pm" / "23:32".
fn absolute(text: &str, now_ms: i64) -> Option<i64> {
    let words = words(text);
    let (today_year, today_month, today_day) = clock::local_date(now_ms);
    let (mut date, mut rest) = ((today_year, today_month, today_day), &words[..]);
    let mut explicit_date = false;
    if let [name, day, tail @ ..] = &words[..]
        && let Some(month) = clock::month(name)
    {
        let day = day
            .trim_end_matches(|c: char| c.is_ascii_alphabetic())
            .parse()
            .ok()?;
        date = (today_year, month, day);
        rest = tail;
        explicit_date = true;
        if let [year, tail @ ..] = rest
            && let Ok(year) = year.parse::<i32>()
            && year > 1900
        {
            date.0 = year;
            rest = tail;
        }
    }
    let (clock_text, meridiem) = match rest {
        [time, m, ..] if *m == "am" || *m == "pm" => (*time, Some(*m)),
        [time, ..] => (*time, None),
        [] => return None,
    };
    let (hour, minute) = clock_text.split_once(':')?;
    let (mut hour, minute): (u32, u32) = (hour.parse().ok()?, minute.get(..2)?.parse().ok()?);
    match meridiem {
        Some("pm") if hour < 12 => hour += 12,
        Some("am") if hour == 12 => hour = 0,
        _ => {}
    }
    if hour > 23 || minute > 59 {
        return None;
    }
    let at = clock::local_ms(date.0, date.1, date.2, hour, minute)?;
    // A bare time already past today means the same time tomorrow.
    if !explicit_date && at <= now_ms {
        return Some(at + 86_400_000);
    }
    Some(at)
}

// "20s", "1m30s", "6ms", "5 minutes", "2 hours and 3 minutes", "1.5s".
fn relative(text: &str) -> Option<i64> {
    let (mut total, mut found, mut rest) = (0.0, false, text);
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == ',');
        if found && let Some(tail) = rest.strip_prefix("and ") {
            rest = tail;
            continue;
        }
        if !rest.starts_with(|c: char| c.is_ascii_digit()) {
            break;
        }
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = rest[digits..].trim_start();
        let letters = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let scale = match &rest[..letters] {
            "ms" | "millisecond" | "milliseconds" => 1.0,
            "s" | "sec" | "secs" | "second" | "seconds" => 1000.0,
            "m" | "min" | "mins" | "minute" | "minutes" => 60_000.0,
            "h" | "hr" | "hrs" | "hour" | "hours" => 3_600_000.0,
            "d" | "day" | "days" => 86_400_000.0,
            _ => break,
        };
        total += number * scale;
        found = true;
        rest = &rest[letters..];
    }
    found.then_some(total as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::local_ms;

    #[test]
    fn codex_usage_limit_names_a_local_reset_time() {
        let now = local_ms(2026, 10, 20, 14, 27).unwrap();
        let message = r#"{"message":"You’ve hit your usage limit. Visit https://chatgpt.com/settings/usage to purchase more credits or try again at Oct 26th, 2026 11:32 PM.","type":"error"}"#;
        assert_eq!(reset_at(message, now), local_ms(2026, 10, 26, 23, 32));
        assert_eq!(
            reset_at("Try again at Nov 1st 9:05 AM", now),
            local_ms(2026, 11, 1, 9, 5)
        );
        assert_eq!(
            reset_at("try again at 2:00 PM.", now),
            local_ms(2026, 10, 21, 14, 0),
            "a time already past today is tomorrow"
        );
        assert_eq!(
            reset_at("try again at 11:59 PM", now),
            local_ms(2026, 10, 20, 23, 59)
        );
        assert_eq!(reset_at("try again at Oct 1st, 2026 1:00 AM", now), None);
    }

    #[test]
    fn relative_waits_and_unknown_messages() {
        let now = 1_000_000;
        assert_eq!(
            reset_at("Please try again in 20s.", now),
            Some(now + 20_000)
        );
        assert_eq!(reset_at("try again in 1m30s", now), Some(now + 90_000));
        assert_eq!(reset_at("try again in 6ms", now), Some(now + 6));
        assert_eq!(reset_at("try again in 1.5s", now), Some(now + 1500));
        assert_eq!(
            reset_at("try again in 2 hours and 3 minutes", now),
            Some(now + 7_380_000)
        );
        assert_eq!(reset_at("rate_limit reached", now), None);
        assert_eq!(reset_at("try again later", now), None);
    }
}
