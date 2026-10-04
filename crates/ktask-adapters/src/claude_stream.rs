//! Facts Claude Code puts in its `stream-json` event stream.

use std::time::{Duration, SystemTime};

use ktask_core::{LimitSignal, Output, ProviderUsage, Usage};

/// Reads a structured limit warning, rejection, or utilization at or above 90%.
pub(crate) fn limit(output: &Output) -> Option<LimitSignal> {
    events(output).into_iter().find_map(|event| {
        let info = (event.get("type")?.as_str() == Some("rate_limit_event"))
            .then(|| event.get("rate_limit_info"))??;
        let status = info.get("status")?.as_str()?;
        (status != "allowed" || utilization(info) >= Some(0.9)).then(|| LimitSignal {
            reset_at: reset_at(info),
        })
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
    }
}

fn utilization(info: &serde_json::Value) -> Option<f64> {
    let direct = info.get("utilization").and_then(serde_json::Value::as_f64);
    let windows = info
        .get("unifiedWindows")
        .and_then(serde_json::Value::as_object)
        .into_iter()
        .flat_map(|windows| windows.values())
        .filter_map(|window| window.get("utilization")?.as_f64())
        .max_by(f64::total_cmp);
    direct.into_iter().chain(windows).max_by(f64::total_cmp)
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

fn events(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}
