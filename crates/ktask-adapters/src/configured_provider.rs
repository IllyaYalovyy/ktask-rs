//! The one provider implementation used for definitions read from configuration.

use std::sync::Arc;

use ktask_core::{
    Output, Provider, ProviderCommand, ProviderDefinition, ProviderParser, ProviderUsage, StepCall,
    Usage,
};

/// Turns a named effective provider definition into the value the core runner executes.
#[must_use]
pub fn configured_provider(name: &str, definition: &ProviderDefinition) -> Provider {
    let command_definition = definition.clone();
    let session_definition = definition.clone();
    let usage_definition = definition.clone();
    let parser = definition.parser;
    Provider {
        name: name.to_owned(),
        command: Arc::new(move |prompt, call| Ok(build_command(&command_definition, prompt, call))),
        supports_resume: !definition.resume.is_empty(),
        read_session: Arc::new(move |output| read_session(&session_definition, output)),
        detect_limit: Arc::new(|_| None),
        read_usage: Arc::new(move |output| read_usage(&usage_definition, output)),
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
    if (event.get("session_id").is_some() || event.get("usage").is_some()) && !text.is_empty() {
        format!("{text}\n{event}")
    } else if text.is_empty() {
        event.to_string()
    } else {
        text
    }
}

/// Reads the configured JSON usage object from a streamed provider result. A provider that has
/// no configured usage path, such as `echo`, deliberately reports no figures.
fn read_usage(definition: &ProviderDefinition, output: &Output) -> ProviderUsage {
    let Some(path) = definition.usage.as_deref() else {
        return ProviderUsage::default();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find_map(|event| usage_in_event(&event, path))
        .unwrap_or_default()
}

fn usage_in_event(event: &serde_json::Value, path: &str) -> Option<ProviderUsage> {
    let usage = json_path(event, path)?;
    let input_tokens = number(usage, &["input_tokens", "inputTokens"]);
    let output_tokens = number(usage, &["output_tokens", "outputTokens"]);
    let cost_microusd = cost(usage);
    let model = string(usage, &["model", "model_name", "modelName"])
        .or_else(|| string(event, &["model", "model_name", "modelName"]));
    Some(ProviderUsage {
        usage: Usage {
            input_tokens,
            output_tokens,
            cost_microusd,
        },
        model,
    })
}

fn json_path<'a>(value: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    path.split('.')
        .filter(|part| !part.is_empty())
        .try_fold(value, |value, part| value.get(part))
}

fn number(value: &serde_json::Value, names: &[&str]) -> Option<u64> {
    names
        .iter()
        .find_map(|name| value.get(name))
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn string(value: &serde_json::Value, names: &[&str]) -> Option<String> {
    names
        .iter()
        .find_map(|name| value.get(*name)?.as_str().map(str::to_owned))
}

fn cost(value: &serde_json::Value) -> Option<u64> {
    ["cost_usd", "costUsd", "cost"]
        .iter()
        .find_map(|name| value.get(*name))
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_number().map(ToString::to_string))
        })
        .and_then(|value| decimal_microusd(&value))
}

/// Parses a non-negative decimal USD value into millionths exactly, rounding only beyond the
/// sixth decimal place. Provider costs are money, so a binary float would make persistence
/// depend on an implementation detail of the parser.
fn decimal_microusd(value: &str) -> Option<u64> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let whole = whole.parse::<u64>().ok()?.checked_mul(1_000_000)?;
    let mut fraction = fraction.chars().take(6).collect::<String>();
    fraction.extend(std::iter::repeat_n(
        '0',
        6_usize.saturating_sub(fraction.len()),
    ));
    let fraction = fraction.parse::<u64>().ok()?;
    let rounded = value
        .split_once('.')
        .and_then(|(_, decimals)| decimals.as_bytes().get(6))
        .is_some_and(|digit| *digit >= b'5');
    whole.checked_add(fraction)?.checked_add(u64::from(rounded))
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

    #[test]
    fn a_configured_usage_event_reports_tokens_cost_and_the_model_used() {
        let definition = ProviderDefinition {
            command: "agent".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            denied_tools: vec![],
            parser: ProviderParser::ClaudeStreamJson,
            session_id: None,
            usage: Some("result.usage".to_owned()),
            limit_message: None,
        };
        let output = parse_claude_stream(Output {
            stdout: br#"{"type":"result","result":{"usage":{"input_tokens":12,"output_tokens":34,"cost_usd":0.056789,"model":"asked"}}}"#.to_vec(),
            stderr: vec![],
            exit: Exit::Code(0),
        });
        assert_eq!(
            read_usage(&definition, &output),
            ProviderUsage {
                usage: Usage {
                    input_tokens: Some(12),
                    output_tokens: Some(34),
                    cost_microusd: Some(56_789),
                },
                model: Some("asked".to_owned()),
            }
        );
    }
}
