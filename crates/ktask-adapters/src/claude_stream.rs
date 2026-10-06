//! Facts Claude Code puts in its `stream-json` event stream.

use std::time::{Duration, SystemTime};

use crate::json_lines::events;
use ktask_core::{LimitSignal, LimitWarning, Output, ProviderUsage, Usage};

/// Reads a structured rejection, or a rejected result that names a usage limit.
pub(crate) fn limit(output: &Output) -> Option<LimitSignal> {
    events(output).into_iter().find_map(|event| {
        if event.get("type").and_then(serde_json::Value::as_str) == Some("rate_limit_event") {
            let info = event.get("rate_limit_info")?;
            return (info.get("status")?.as_str() == Some("rejected")).then(|| LimitSignal {
                reset_at: reset_at(info),
            });
        }
        result_names_limit(&event).then_some(LimitSignal { reset_at: None })
    })
}

/// Reads result totals and the actual assistant model, falling back to `modelUsage` when an
/// assistant event did not arrive (for example, an interrupted run).
pub(crate) fn usage(output: &Output) -> ProviderUsage {
    let events = events(output);
    let model = events
        .iter()
        .rev()
        .filter_map(|event| event.pointer("/message/model")?.as_str())
        .find(|model| *model != "<synthetic>")
        .map(str::to_owned);
    let result = events
        .iter()
        .rev()
        .find(|event| event.get("type").and_then(serde_json::Value::as_str) == Some("result"));
    let usage = result.map_or_else(Usage::default, |event| Usage {
        input_tokens: event
            .pointer("/usage/input_tokens")
            .and_then(serde_json::Value::as_u64),
        output_tokens: event
            .pointer("/usage/output_tokens")
            .and_then(serde_json::Value::as_u64),
        cost_microusd: event.get("total_cost_usd").and_then(cost_microusd),
    });
    ProviderUsage {
        usage,
        model: model.or_else(|| result.and_then(model_from_usage)),
        limit_warning: events.iter().find_map(warning),
    }
}

/// True when an error result itself says Claude refused the call for a usage limit. Warnings
/// are a distinct `rate_limit_event`, so their text cannot make this branch wait.
fn result_names_limit(event: &serde_json::Value) -> bool {
    event.get("type").and_then(serde_json::Value::as_str) == Some("result")
        && event
            .get("is_error")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        && event
            .get("result")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| text.to_ascii_lowercase().contains("limit"))
}

/// Reads Claude's explicit `allowed_warning` fact. Its named window makes the warning useful
/// without assigning any policy threshold to it.
fn warning(event: &serde_json::Value) -> Option<LimitWarning> {
    let info = (event.get("type")?.as_str() == Some("rate_limit_event"))
        .then(|| event.get("rate_limit_info"))??;
    if info.get("status")?.as_str() != Some("allowed_warning") {
        return None;
    }
    Some(LimitWarning {
        window: window_name(info.get("rateLimitType")?.as_str()?),
        utilization_percent: percent(info.get("utilization")?.as_f64()?),
    })
}

fn window_name(name: &str) -> String {
    match name {
        "five_hour" => "5 hours".to_owned(),
        "seven_day" => "7 days".to_owned(),
        _ => name.replace('_', " "),
    }
}

fn percent(utilization: f64) -> u8 {
    format!("{:.0}", (utilization * 100.0).round().clamp(0.0, 100.0))
        .parse()
        .unwrap_or(100)
}

fn reset_at(info: &serde_json::Value) -> Option<SystemTime> {
    let seconds = info.get("resetsAt")?.as_u64()?;
    let seconds = if seconds >= 100_000_000_000 {
        seconds / 1_000
    } else {
        seconds
    };
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds))
}

fn model_from_usage(result: &serde_json::Value) -> Option<String> {
    let models = result.get("modelUsage")?.as_object()?;
    (models.len() == 1).then(|| {
        models.values().next().and_then(|usage| {
            usage
                .get("canonicalModel")
                .or_else(|| usage.get("model"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .or_else(|| models.keys().next().cloned())
        })
    })?
}

fn cost_microusd(value: &serde_json::Value) -> Option<u64> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_number().map(ToString::to_string))
        .and_then(|value| super::configured_provider::decimal_microusd(&value))
}
