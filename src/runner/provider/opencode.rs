//! `opencode run --format json`: step_start/text/tool_use/step_finish/error, each carrying
//! the session ID. Usage arrives per step (uncached input, disjoint cache counts) and is summed.
//! OpenCode has no response-schema flag, so the schema travels in the prompt.
use super::{Access, Event, Launch, Stream, tool_text};
use crate::{
    application::ModelConfig,
    storage::{Result, usage::TokenUsage},
};
use serde_json::{Value, json};

pub(super) fn launch(
    model: &ModelConfig,
    session: Option<&str>,
    access: Access,
    schema: &Value,
    prompt: String,
) -> Launch {
    let mut args: Vec<String> = ["run", "--format", "json", "--pure", "-m", &model.model]
        .map(String::from)
        .into();
    if let Some(effort) = &model.effort {
        args.extend(["--variant".into(), effort.clone()]);
    }
    if let Some(session) = session {
        args.extend(["--session".into(), session.into()]);
    }
    // Without --auto, `run` rejects every permission request that is not allowed outright.
    let env = match access {
        Access::ReadOnly => vec![("OPENCODE_PERMISSION".into(), read_only().to_string())],
        Access::Write => {
            args.push("--auto".into());
            Vec::new()
        }
    };
    Launch {
        args,
        env,
        schema_file: None,
        prompt: format!(
            "{prompt}\n\nYour final message must be exactly one JSON object, with no prose or code fences, valid against this JSON Schema:\n{schema}"
        ),
    }
}

/// Later rules win, so the blanket denial comes first.
fn read_only() -> Value {
    let mut bash = serde_json::Map::new();
    bash.insert("*".into(), json!("deny"));
    for prefix in [
        "git diff",
        "git log",
        "git show",
        "git status",
        "ls",
        "cat",
        "head",
        "tail",
        "wc",
        "rg",
        "grep",
    ] {
        bash.insert(prefix.into(), json!("allow"));
        bash.insert(format!("{prefix} *"), json!("allow"));
    }
    json!({
        "edit": "deny", "bash": bash, "task": "deny", "webfetch": "deny",
        "websearch": "deny", "external_directory": "deny", "question": "deny",
    })
}

pub(super) fn event(stream: &mut Stream, value: &Value) -> Result<Vec<Event>> {
    let mut events: Vec<Event> = stream
        .session(value["sessionID"].as_str())
        .into_iter()
        .collect();
    let part = &value["part"];
    match value["type"].as_str().unwrap_or("") {
        "step_start" => stream.completed = false,
        "text" => {
            let text = part["text"].as_str().unwrap_or_default();
            stream.last_message = Some(json_reply(text).into());
            events.push(Event::Message(text.into()));
        }
        "tool_use" => {
            let state = &part["state"];
            let output = state["output"]
                .as_str()
                .or_else(|| state["error"].as_str())
                .unwrap_or_default();
            events.push(Event::Text(tool_text(
                part["tool"].as_str().unwrap_or("tool"),
                &state["input"],
                output,
            )));
        }
        "step_finish" => {
            let tokens = &part["tokens"];
            let count = |value: &Value| value.as_i64().unwrap_or(0);
            let totals = &mut stream.usage;
            totals.input += count(&tokens["input"]);
            totals.output += count(&tokens["output"]) + count(&tokens["reasoning"]);
            totals.cache_read += count(&tokens["cache"]["read"]);
            totals.cache_write += count(&tokens["cache"]["write"]);
            if let Some(usd) = part["cost"].as_f64() {
                *totals.cost_microusd.get_or_insert(0) += (usd * 1_000_000.0).round() as i64;
            }
            events.push(Event::Usage(
                TokenUsage::from_uncached(
                    Some(totals.input),
                    Some(totals.cache_read),
                    Some(totals.cache_write),
                    Some(totals.output),
                )?,
                totals.cost_microusd,
            ));
            stream.completed = part["reason"] == "stop";
        }
        "error" => {
            let error = &value["error"];
            let retry_after = error["data"]["responseHeaders"]["retry-after"]
                .as_str()
                .and_then(|s| s.trim().parse::<f64>().ok());
            stream.fail(&error.to_string());
            if stream.rate_limited
                && let Some(seconds) = retry_after
            {
                stream.limit(Some(
                    crate::application::now_ms() + (seconds * 1000.0).ceil() as i64,
                ));
            }
            events.push(Event::Text(error.to_string()));
        }
        _ => {}
    }
    Ok(events)
}

/// The JSON object inside a reply that may still be wrapped in prose or code fences.
fn json_reply(text: &str) -> &str {
    match (text.find('{'), text.rfind('}')) {
        (Some(start), Some(end)) if start < end => &text[start..=end],
        _ => text,
    }
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::*;

    fn feed(stream: &mut Stream, events: &[Value]) -> Vec<Event> {
        let lines: String = events.iter().map(|e| format!("{e}\n")).collect();
        stream.feed(lines.as_bytes()).unwrap()
    }

    #[test]
    fn steps_sum_usage_and_the_last_text_is_the_reply() {
        let mut stream = Stream::new(Provider::OpenCode);
        let step = |reason: &str, cost: f64| {
            json!({"type":"step_finish","sessionID":"ses_1","part":{"reason":reason,"cost":cost,
                "tokens":{"input":10,"output":5,"reasoning":1,"cache":{"read":100,"write":20}}}})
        };
        let events = feed(
            &mut stream,
            &[
                json!({"type":"step_start","sessionID":"ses_1","part":{}}),
                json!({"type":"tool_use","sessionID":"ses_1","part":{"tool":"bash","state":{"status":"completed","input":{"command":"make check"},"output":"ok"}}}),
                step("tool-calls", 0.01),
                json!({"type":"text","sessionID":"ses_1","part":{"text":"```json\n{\"outcome\":\"complete\"}\n```"}}),
                step("stop", 0.02),
            ],
        );
        assert!(matches!(&events[0], Event::Session(id) if id == "ses_1"));
        assert!(matches!(&events[1], Event::Text(t) if t == "$ make check\nok"));
        let Some(Event::Usage(usage, cost)) = events.last() else {
            panic!("usage expected")
        };
        assert_eq!(
            *usage,
            TokenUsage::from_uncached(Some(20), Some(200), Some(40), Some(12)).unwrap()
        );
        assert_eq!(*cost, Some(30000));
        assert!(stream.completed && !stream.failed);
        assert_eq!(
            stream.last_message.as_deref(),
            Some(r#"{"outcome":"complete"}"#)
        );
    }

    #[test]
    fn rate_limit_error_uses_retry_after() {
        let mut stream = Stream::new(Provider::OpenCode);
        let before = crate::application::now_ms();
        feed(
            &mut stream,
            &[
                json!({"type":"error","sessionID":"ses_1","error":{"name":"APIError","data":{"message":"Rate limit exceeded","statusCode":429,"responseHeaders":{"retry-after":"120"}}}}),
            ],
        );
        assert!(stream.failed && stream.rate_limited);
        let reset = stream.reset_at_ms.unwrap();
        assert!(reset >= before + 120_000 && reset < before + 125_000);
    }

    #[test]
    fn readers_get_a_deny_first_permission_policy_and_writers_auto_approve() {
        let model = ModelConfig {
            provider: "opencode".into(),
            model: "anthropic/claude-sonnet-5-5".into(),
            effort: Some("high".into()),
        };
        let read = launch(
            &model,
            Some("ses_1"),
            Access::ReadOnly,
            &json!({"type":"object"}),
            "Do it".into(),
        );
        assert!(!read.args.contains(&"--auto".into()));
        assert!(
            read.args
                .windows(2)
                .any(|w| w == ["-m", "anthropic/claude-sonnet-5-5"])
        );
        assert!(read.args.windows(2).any(|w| w == ["--session", "ses_1"]));
        assert!(read.args.windows(2).any(|w| w == ["--variant", "high"]));
        let policy: Value = serde_json::from_str(&read.env[0].1).unwrap();
        assert_eq!(policy["edit"], "deny");
        let bash = policy["bash"].as_object().unwrap();
        assert_eq!(bash.keys().next().map(String::as_str), Some("*"));
        assert_eq!(bash["git diff *"], "allow");
        assert!(read.prompt.starts_with("Do it") && read.prompt.ends_with(r#"{"type":"object"}"#));
        let write = launch(&model, None, Access::Write, &json!({}), String::new());
        assert!(write.args.contains(&"--auto".into()) && write.env.is_empty());
    }
}
