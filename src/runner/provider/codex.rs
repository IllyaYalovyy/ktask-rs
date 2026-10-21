//! `codex exec --json`: thread/turn/item events; the schema is read from a file.
use super::{Access, Event, Launch, Stream};
use crate::{
    application::ModelConfig,
    storage::{Result, usage::TokenUsage},
};
use serde_json::Value;

pub(super) fn launch(
    model: &ModelConfig,
    session: Option<&str>,
    access: Access,
    schema: &Value,
    prompt: String,
) -> Launch {
    let sandbox = match access {
        Access::ReadOnly => "read-only",
        Access::Write => "workspace-write",
    };
    let mut args: Vec<String> = [
        "exec",
        "--json",
        "--ignore-user-config",
        "--ignore-rules",
        "-m",
        &model.model,
        "-c",
        "approval_policy=\"never\"",
        "-c",
        &format!("sandbox_mode=\"{sandbox}\""),
        "-c",
        "features.multi_agent=false",
        "-c",
        "sandbox_workspace_write.exclude_slash_tmp=true",
        "-c",
        "sandbox_workspace_write.exclude_tmpdir_env_var=false",
    ]
    .map(String::from)
    .into();
    if let Some(effort) = &model.effort {
        args.extend([
            "-c".into(),
            format!(
                "model_reasoning_effort={}",
                serde_json::to_string(effort).expect("string JSON")
            ),
        ]);
    }
    if let Some(session) = session {
        args.extend(["resume".into(), session.into()]);
    }
    args.push("-".into());
    Launch {
        args,
        env: Vec::new(),
        schema_file: Some(schema.clone()),
        prompt,
    }
}

pub(super) fn event(stream: &mut Stream, value: &Value) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    match value["type"].as_str().unwrap_or("") {
        "thread.started" => events.extend(stream.session(value["thread_id"].as_str())),
        "turn.completed" => {
            stream.completed = true;
            let u = &value["usage"];
            events.push(Event::Usage(
                TokenUsage::from_inclusive(
                    u["input_tokens"].as_i64(),
                    u["cached_input_tokens"].as_i64(),
                    Some(0),
                    u["output_tokens"].as_i64(),
                )?,
                None,
            ));
        }
        "turn.failed" | "error" => {
            stream.fail(&value.to_string());
            events.push(Event::Text(value.to_string()));
        }
        "item.completed" => {
            let item = &value["item"];
            if item["type"] == "agent_message"
                && let Some(text) = item["text"].as_str()
            {
                stream.last_message = Some(text.into());
                events.push(Event::Message(text.into()));
            } else {
                events.push(Event::Text(item_text(item)));
            }
        }
        "item.started" => events.push(Event::Text(item_text(&value["item"]))),
        _ => {}
    }
    Ok(events)
}

fn item_text(item: &Value) -> String {
    match item["type"].as_str().unwrap_or("activity") {
        "command_execution" => format!(
            "$ {}\n{}{}",
            item["command"].as_str().unwrap_or("command"),
            item["aggregated_output"].as_str().unwrap_or(""),
            item["exit_code"]
                .as_i64()
                .map(|n| format!("\nexit {n}"))
                .unwrap_or_default()
        ),
        "file_change" => format!(
            "Files changed: {}",
            item["changes"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|c| c["path"].as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        ),
        kind => format!(
            "{kind}: {}",
            item["text"]
                .as_str()
                .or_else(|| item["status"].as_str())
                .unwrap_or("updated")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::super::Provider;
    use super::*;
    use serde_json::json;

    #[test]
    fn large_command_event_preserves_output_across_pipe_reads() {
        let output = format!("START\n{}\nEND", "x".repeat(1250000));
        let record = format!(
            "{}\n",
            json!({"type":"item.completed","item":{
                "type":"command_execution","command":"make check",
                "aggregated_output":output,"exit_code":0
            }})
        );
        let mut stream = Stream::new(Provider::Codex);
        let mut events = Vec::new();
        for chunk in record.as_bytes().chunks(4096) {
            events.extend(stream.feed(chunk).unwrap());
        }
        assert_eq!(events.len(), 1);
        assert!(
            matches!(&events[0], Event::Text(text) if text == &format!("$ make check\n{output}\nexit 0"))
        );
        stream
            .feed(
                format!(
                    "{}\n{}\n",
                    json!({"type":"item.completed","item":{"type":"agent_message","text":"{}"}}),
                    json!({"type":"turn.completed","usage":{}})
                )
                .as_bytes(),
            )
            .unwrap();
        assert!(stream.completed && !stream.failed);
        assert_eq!(stream.last_message.as_deref(), Some("{}"));
    }

    #[test]
    fn worker_sandbox_retains_owned_tmp_but_excludes_general_tmp() {
        let model = ModelConfig {
            provider: "codex".into(),
            model: "test".into(),
            effort: None,
        };
        for access in [Access::Write, Access::ReadOnly] {
            let args = launch(&model, None, access, &json!({}), String::new()).args;
            assert!(args.contains(&"sandbox_workspace_write.exclude_slash_tmp=true".into()));
            assert!(args.contains(&"sandbox_workspace_write.exclude_tmpdir_env_var=false".into()));
            let mode = if access == Access::ReadOnly {
                "read-only"
            } else {
                "workspace-write"
            };
            assert!(args.contains(&format!("sandbox_mode=\"{mode}\"")));
        }
    }
}
