//! Facts Codex puts in its `--json` line-delimited event stream.

use ktask_core::{Output, ProviderUsage, Usage};

/// Reads the first thread identifier Codex reports for an invocation.
pub(crate) fn session(output: &Output) -> Option<String> {
    events(output).into_iter().find_map(|event| {
        (event.get("type")?.as_str()? == "thread.started")
            .then(|| event.get("thread_id")?.as_str())
            .flatten()
            .map(str::to_owned)
    })
}

/// Reads Codex's completed-turn token totals. Codex does not report cost or the actual model.
pub(crate) fn usage(output: &Output) -> ProviderUsage {
    let events = events(output);
    let usage = events
        .iter()
        .rev()
        .find(|event| {
            event.get("type").and_then(serde_json::Value::as_str) == Some("turn.completed")
        })
        .and_then(|event| event.get("usage"));
    ProviderUsage {
        usage: usage.map_or_else(Usage::default, |usage| Usage {
            input_tokens: number(usage, "input_tokens"),
            output_tokens: number(usage, "output_tokens"),
            cost_microusd: None,
        }),
        model: None,
        limit_warning: None,
    }
}

fn events(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn number(value: &serde_json::Value, name: &str) -> Option<u64> {
    value
        .get(name)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

#[cfg(test)]
mod tests {
    use ktask_core::Exit;

    use super::*;

    fn recorded_output() -> Output {
        Output {
            stdout: include_bytes!("../../../test-fixtures/codex/codex-0.160.0-success.jsonl")
                .to_vec(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        }
    }

    #[test]
    fn recorded_success_has_its_thread_and_turn_usage() {
        let output = recorded_output();
        assert_eq!(
            session(&output).as_deref(),
            Some("01a10555-6a4c-7f21-8ac4-aed0bf10dbb2")
        );
        assert_eq!(
            usage(&output),
            ProviderUsage {
                usage: Usage {
                    input_tokens: Some(13_282),
                    output_tokens: Some(5),
                    cost_microusd: None,
                },
                model: None,
                limit_warning: None,
            }
        );
    }
}
