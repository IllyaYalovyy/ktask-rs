//! The one provider implementation used for definitions read from configuration.

use std::sync::Arc;

use ktask_core::{Output, Provider, ProviderCommand, ProviderDefinition, ProviderParser, StepCall};

/// Turns a named effective provider definition into the value the core runner executes.
#[must_use]
pub fn configured_provider(name: &str, definition: &ProviderDefinition) -> Provider {
    let command_definition = definition.clone();
    let session_definition = definition.clone();
    let parser = definition.parser;
    Provider {
        name: name.to_owned(),
        command: Arc::new(move |prompt, call| Ok(build_command(&command_definition, prompt, call))),
        supports_resume: !definition.resume.is_empty(),
        read_session: Arc::new(move |output| read_session(&session_definition, output)),
        detect_limit: Arc::new(|_| None),
        parse_output: Arc::new(move |output| parse_output(parser, output)),
    }
}

fn build_command(
    definition: &ProviderDefinition,
    prompt: &str,
    call: StepCall<'_>,
) -> ProviderCommand {
    let mut args = render(&definition.args, prompt, call);
    args.extend(render(&definition.prompt, prompt, call));
    if call.model.is_some() {
        args.extend(render(&definition.model, prompt, call));
    }
    if call.resume.is_some() {
        args.extend(render(&definition.resume, prompt, call));
    }
    ProviderCommand {
        program: definition.command.clone(),
        args,
        stdin: prompt.as_bytes().to_vec(),
    }
}

fn render(template: &[String], prompt: &str, call: StepCall<'_>) -> Vec<String> {
    template
        .iter()
        .map(|part| {
            part.replace("{prompt}", prompt)
                .replace("{model}", call.model.unwrap_or_default())
                .replace("{session}", call.resume.map_or("", |resume| resume.session))
                .replace("{denied-tools}", "")
        })
        .collect()
}

fn parse_output(parser: ProviderParser, output: Output) -> Output {
    match parser {
        ProviderParser::Plain => output,
        ProviderParser::ClaudeStreamJson => parse_claude_stream(output),
    }
}

fn parse_claude_stream(mut output: Output) -> Output {
    let raw = output.stdout.clone();
    let mut text = String::new();
    for line in String::from_utf8_lossy(&raw).lines() {
        let rendered = serde_json::from_str::<serde_json::Value>(line)
            .ok()
            .map_or_else(|| line.to_owned(), |event| claude_event_text(&event));
        text.push_str(&rendered);
        if !rendered.ends_with('\n') {
            text.push('\n');
        }
    }
    output.stdout = text.into_bytes();
    output
}

/// Keeps text from known event shapes, and the full JSON of any shape without text. This makes
/// new Claude event types visible rather than turning a harmless addition into a failed task.
fn claude_event_text(event: &serde_json::Value) -> String {
    let mut text = Vec::new();
    collect_text(event, &mut text);
    let text = text.join("");
    if event.get("session_id").is_some() && !text.is_empty() {
        format!("{text}\n{event}")
    } else if text.is_empty() {
        event.to_string()
    } else {
        text
    }
}

fn collect_text(value: &serde_json::Value, found: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(fields) => {
            for key in ["text", "result"] {
                if let Some(serde_json::Value::String(text)) = fields.get(key) {
                    found.push(text.clone());
                }
            }
            for (key, child) in fields {
                if key != "text" && key != "result" {
                    collect_text(child, found);
                }
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                collect_text(child, found);
            }
        }
        _ => {}
    }
}

fn read_session(definition: &ProviderDefinition, output: &Output) -> Option<String> {
    (definition.parser == ProviderParser::ClaudeStreamJson).then(|| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .find_map(|line| {
                serde_json::from_str::<serde_json::Value>(line)
                    .ok()
                    .and_then(|event| {
                        event
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .map(str::to_owned)
                    })
            })
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::Exit;

    #[test]
    fn claude_events_keep_known_text_and_unknown_events() {
        let output = parse_claude_stream(Output {
            stdout: br#"{"type":"system","subtype":"init","session_id":"s"}
{"type":"assistant","message":{"content":[{"type":"text","text":"working"}]}}
{"type":"content_block_delta","delta":{"type":"text_delta","text":" now"}}
{"type":"result","result":"finished","session_id":"s","usage":{"input_tokens":1}}
{"type":"future","value":7}
"#
            .to_vec(),
            stderr: vec![],
            exit: Exit::Code(0),
        });
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in ["working", " now", "finished", "\"type\":\"future\""] {
            assert!(text.contains(expected), "{text}");
        }
    }
}
