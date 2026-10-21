//! `claude -p --output-format stream-json`: init/assistant/user/rate_limit_event/result.
//! Usage arrives once per turn with uncached input and disjoint cache counts.
use super::{Access, Event, Launch, Stream, tool_text};
use crate::{
    application::ModelConfig,
    storage::{Result, usage::TokenUsage},
};
use serde_json::Value;

/// Reviewers may read and inspect history, never change the tree.
const READ_ONLY_TOOLS: &[&str] = &[
    "Read",
    "Grep",
    "Glob",
    "Bash(git diff *)",
    "Bash(git log *)",
    "Bash(git show *)",
    "Bash(git status *)",
    "Bash(ls *)",
    "Bash(cat *)",
    "Bash(head *)",
    "Bash(tail *)",
    "Bash(wc *)",
    "Bash(rg *)",
    "Bash(grep *)",
];

pub(super) fn launch(
    model: &ModelConfig,
    session: Option<&str>,
    access: Access,
    schema: &Value,
    prompt: String,
) -> Launch {
    let mut args: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        &model.model,
        "--json-schema",
        &schema.to_string(),
        "--strict-mcp-config",
        "--setting-sources",
        "project,local",
    ]
    .map(String::from)
    .into();
    if let Some(effort) = &model.effort {
        args.extend(["--effort".into(), effort.clone()]);
    }
    match access {
        Access::ReadOnly => {
            args.extend(
                [
                    "--permission-mode",
                    "dontAsk",
                    "--permission-prompts",
                    "none",
                    "--tools",
                    "Read,Grep,Glob,Bash",
                    "--allowedTools",
                ]
                .map(String::from),
            );
            args.extend(READ_ONLY_TOOLS.iter().map(|tool| tool.to_string()));
        }
        Access::Write => args.extend(["--permission-mode".into(), "bypassPermissions".into()]),
    }
    if let Some(session) = session {
        args.extend(["--resume".into(), session.into()]);
    }
    Launch {
        args,
        env: Vec::new(),
        schema_file: None,
        prompt,
    }
}

pub(super) fn event(stream: &mut Stream, value: &Value) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    match value["type"].as_str().unwrap_or("") {
        "system" if value["subtype"] == "init" => {
            events.extend(stream.session(value["session_id"].as_str()));
        }
        "assistant" => {
            for block in blocks(&value["message"]["content"]) {
                match block["type"].as_str() {
                    Some("text") => events.push(Event::Text(text(&block["text"]))),
                    // The structured result is reported once, by the final result event.
                    Some("tool_use") if block["name"] != "StructuredOutput" => {
                        events.push(Event::Text(tool_text(
                            text(&block["name"]).as_str(),
                            &block["input"],
                            "",
                        )))
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            for block in blocks(&value["message"]["content"]) {
                if block["type"] == "tool_result" {
                    let output = match &block["content"] {
                        Value::Array(parts) => parts
                            .iter()
                            .filter_map(|part| part["text"].as_str())
                            .collect::<Vec<_>>()
                            .join("\n"),
                        other => text(other),
                    };
                    if !output.is_empty() && output != "Structured output provided successfully" {
                        events.push(Event::Text(output));
                    }
                }
            }
        }
        "rate_limit_event" => {
            let info = &value["rate_limit_info"];
            if info["status"] == "rejected" {
                stream.limit(info["resetsAt"].as_i64().map(|seconds| seconds * 1000));
            }
        }
        "result" => events.extend(result(stream, value)?),
        _ => {}
    }
    Ok(events)
}

fn result(stream: &mut Stream, value: &Value) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let u = &value["usage"];
    if u.is_object() {
        events.push(Event::Usage(
            TokenUsage::from_uncached(
                u["input_tokens"].as_i64(),
                u["cache_read_input_tokens"].as_i64(),
                u["cache_creation_input_tokens"].as_i64(),
                u["output_tokens"].as_i64(),
            )?,
            value["total_cost_usd"]
                .as_f64()
                .map(|usd| (usd * 1_000_000.0).round() as i64),
        ));
    }
    let reply = match &value["structured_output"] {
        Value::Null => text(&value["result"]),
        structured => structured.to_string(),
    };
    if value["is_error"] == true || value["subtype"] != "success" {
        if value["api_error_status"] == 429 {
            stream.limit(super::super::reset::reset_at(
                &reply,
                crate::application::now_ms(),
            ));
        }
        stream.fail(&format!("{} {reply}", text(&value["subtype"])));
        events.push(Event::Text(value.to_string()));
    } else {
        stream.completed = true;
        stream.last_message = Some(reply.clone());
        events.push(Event::Message(reply));
    }
    Ok(events)
}

fn blocks(content: &Value) -> impl Iterator<Item = &Value> {
    content.as_array().into_iter().flatten()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::*;
    use serde_json::json;

    fn feed(stream: &mut Stream, events: &[Value]) -> Vec<Event> {
        let lines: String = events.iter().map(|e| format!("{e}\n")).collect();
        stream.feed(lines.as_bytes()).unwrap()
    }

    #[test]
    fn structured_result_session_and_uncached_usage() {
        let mut stream = Stream::new(Provider::Claude);
        let events = feed(
            &mut stream,
            &[
                json!({"type":"system","subtype":"init","session_id":"s-1"}),
                json!({"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"make check"}}]}}),
                json!({"type":"user","message":{"content":[{"type":"tool_result","content":"ok"}]}}),
                json!({"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1}}),
                json!({"type":"result","subtype":"success","is_error":false,"result":"","structured_output":{"outcome":"complete"},
                    "total_cost_usd":0.085004,"usage":{"input_tokens":4,"cache_creation_input_tokens":20428,"cache_read_input_tokens":20340,"output_tokens":125}}),
            ],
        );
        assert!(matches!(&events[0], Event::Session(id) if id == "s-1"));
        assert!(matches!(&events[1], Event::Text(t) if t == "$ make check"));
        assert!(matches!(&events[2], Event::Text(t) if t == "ok"));
        let Event::Usage(usage, cost) = &events[3] else {
            panic!("usage expected")
        };
        assert_eq!(
            *usage,
            TokenUsage::from_uncached(Some(4), Some(20340), Some(20428), Some(125)).unwrap()
        );
        assert_eq!(*cost, Some(85004));
        assert!(stream.completed && !stream.failed && !stream.rate_limited);
        assert_eq!(
            stream.last_message.as_deref(),
            Some(r#"{"outcome":"complete"}"#)
        );
    }

    #[test]
    fn rejected_limit_carries_its_reset_time() {
        let mut stream = Stream::new(Provider::Claude);
        feed(
            &mut stream,
            &[
                json!({"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1792969200}}),
                json!({"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"You've hit your limit"}),
            ],
        );
        assert!(stream.failed && stream.rate_limited);
        assert_eq!(stream.reset_at_ms, Some(1_792_969_200_000));
    }

    #[test]
    fn read_only_turns_cannot_edit_and_writers_never_ask() {
        let model = ModelConfig {
            provider: "claude".into(),
            model: "sonnet".into(),
            effort: Some("high".into()),
        };
        let read = launch(
            &model,
            Some("s-1"),
            Access::ReadOnly,
            &json!({}),
            "p".into(),
        )
        .args;
        assert!(
            read.windows(2)
                .any(|w| w == ["--permission-mode", "dontAsk"])
        );
        assert!(
            read.windows(2)
                .any(|w| w == ["--tools", "Read,Grep,Glob,Bash"])
        );
        assert!(
            !read
                .iter()
                .any(|a| a.contains("Edit") || a.contains("Write"))
        );
        assert!(read.windows(2).any(|w| w == ["--resume", "s-1"]));
        assert!(read.windows(2).any(|w| w == ["--effort", "high"]));
        let write = launch(&model, None, Access::Write, &json!({}), "p".into()).args;
        assert!(
            write
                .windows(2)
                .any(|w| w == ["--permission-mode", "bypassPermissions"])
        );
    }
}
