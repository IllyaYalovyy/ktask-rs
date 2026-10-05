//! The one provider implementation used for definitions read from configuration.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::{claude_stream, codex_jsonl};
use ktask_core::{
    LimitSignal, Output, Provider, ProviderCommand, ProviderDefinition, ProviderParser,
    ProviderUsage, StepCall, Usage,
};
use regex::Regex;

/// Turns a named effective provider definition into the value the core runner executes.
#[must_use]
pub fn configured_provider(name: &str, definition: &ProviderDefinition) -> Provider {
    let command_definition = definition.clone();
    let limit_definition = definition.clone();
    let session_definition = definition.clone();
    let usage_definition = definition.clone();
    let parser = definition.parser;
    Provider {
        name: name.to_owned(),
        command: Arc::new(move |prompt, call| Ok(build_command(&command_definition, prompt, call))),
        supports_resume: !definition.resume.is_empty(),
        read_session: Arc::new(move |output| read_session(&session_definition, output)),
        detect_limit: Arc::new(move |output| detect_limit(&limit_definition, output)),
        read_usage: Arc::new(move |output| read_usage(&usage_definition, output)),
        parse_output: Arc::new(move |output| parse_output(parser, output)),
    }
}

/// Matches a configured limit pattern against the provider's normalized output. Patterns may
/// name their reset time with a `reset` capture containing Unix seconds; a matching message
/// without that capture is still a limit and uses the runner's ordinary back-off.
fn detect_limit(definition: &ProviderDefinition, output: &Output) -> Option<LimitSignal> {
    if definition.parser == ProviderParser::ClaudeStreamJson
        && let Some(signal) = claude_stream::limit(output)
    {
        return Some(signal);
    }
    text_limit(definition, output)
}

/// Falls back to the configured plain-text form for CLIs that do not carry a structured limit.
fn text_limit(definition: &ProviderDefinition, output: &Output) -> Option<LimitSignal> {
    let pattern = definition.limit_message.as_deref()?;
    let expression = Regex::new(pattern).ok()?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let matched = expression.captures(&text)?;
    let reset_at = matched
        .name("reset")
        .and_then(|capture| capture.as_str().parse::<u64>().ok())
        .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
    Some(LimitSignal { reset_at })
}

fn build_command(
    definition: &ProviderDefinition,
    prompt: &str,
    call: StepCall<'_>,
) -> ProviderCommand {
    if call.resume.is_some() && !definition.resume_command.is_empty() {
        return ProviderCommand {
            program: definition.command.clone(),
            args: render(&definition.resume_command, prompt, call),
            stdin: prompt.as_bytes().to_vec(),
        };
    }
    let mut args = render(&definition.args, prompt, call);
    if call.model.is_some() {
        args.extend(render(&definition.model, prompt, call));
    }
    args.extend(render(&definition.prompt, prompt, call));
    if call.resume.is_some() {
        args.extend(render(&definition.resume, prompt, call));
    }
    if !definition.denied_tools.is_empty() {
        args.push("--disallowedTools".to_owned());
        args.push(definition.denied_tools.join(","));
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
                .replace("{project-dir}", &call.project_dir.to_string_lossy())
        })
        .collect()
}

fn parse_output(parser: ProviderParser, output: Output) -> Output {
    match parser {
        ProviderParser::Plain | ProviderParser::CodexJsonl => output,
        ProviderParser::ClaudeStreamJson => parse_claude_stream(output),
    }
}

/// Leaves Claude's structured bytes available to the runner's usage, session and limit
/// readers. Operator-facing rendering happens from the separately retained raw stream, shared
/// by `output` and the terminal interface.
fn parse_claude_stream(output: Output) -> Output {
    output
}

/// Reads the configured JSON usage object from a streamed provider result. A provider that has
/// no configured usage path, such as `echo`, deliberately reports no figures.
fn read_usage(definition: &ProviderDefinition, output: &Output) -> ProviderUsage {
    if definition.parser == ProviderParser::ClaudeStreamJson {
        return claude_stream::usage(output);
    }
    if definition.parser == ProviderParser::CodexJsonl {
        return codex_jsonl::usage(output);
    }
    let Some(path) = definition.usage.as_deref() else {
        return ProviderUsage::default();
    };
    events(output)
        .into_iter()
        .find_map(|event| usage_in_event(&event, path))
        .unwrap_or_default()
}

fn events(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
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
        limit_warning: None,
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
pub(crate) fn decimal_microusd(value: &str) -> Option<u64> {
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

fn read_session(definition: &ProviderDefinition, output: &Output) -> Option<String> {
    match definition.parser {
        ProviderParser::ClaudeStreamJson => String::from_utf8_lossy(&output.stdout)
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
            }),
        ProviderParser::CodexJsonl => codex_jsonl::session(output),
        ProviderParser::Plain => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ktask_core::Exit;

    #[test]
    fn claude_events_remain_available_to_usage_and_session_readers() {
        let output = parse_claude_stream(Output {
            stdout: include_str!("../../../test-fixtures/claude/success.jsonl")
                .as_bytes()
                .to_vec(),
            stderr: vec![],
            exit: Exit::Code(0),
        });
        let text = String::from_utf8(output.stdout).unwrap();
        for expected in ["KTASK_RECORDING_SUCCESS", "\"total_cost_usd\":0.0110019"] {
            assert!(text.contains(expected), "{text}");
        }
    }

    #[test]
    fn recorded_claude_success_reports_result_cost_and_assistant_model() {
        let definition = ProviderDefinition {
            command: "agent".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            resume_command: vec![],
            denied_tools: vec![],
            parser: ProviderParser::ClaudeStreamJson,
            session_id: None,
            usage: Some("usage".to_owned()),
            limit_message: None,
        };
        let output = parse_claude_stream(Output {
            stdout: include_str!("../../../test-fixtures/claude/success.jsonl")
                .as_bytes()
                .to_vec(),
            stderr: vec![],
            exit: Exit::Code(0),
        });
        assert_eq!(
            read_usage(&definition, &output),
            ProviderUsage {
                usage: Usage {
                    input_tokens: Some(10),
                    output_tokens: Some(56),
                    cost_microusd: Some(11_002),
                },
                model: Some("claude-haiku-4-5-20251001".to_owned()),
                limit_warning: None,
            }
        );
    }

    #[test]
    fn recorded_claude_usage_warning_is_a_usage_fact_not_a_limit() {
        let definition = ProviderDefinition {
            command: "agent".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            resume_command: vec![],
            denied_tools: vec![],
            parser: ProviderParser::ClaudeStreamJson,
            session_id: None,
            usage: None,
            limit_message: Some(r"limit\|(?<reset>[0-9]+)".to_owned()),
        };
        let output = Output {
            stdout: include_str!("../../../test-fixtures/claude/usage-limit.jsonl")
                .as_bytes()
                .to_vec(),
            stderr: vec![],
            exit: Exit::Code(1),
        };
        assert_eq!(detect_limit(&definition, &output), None);
        assert_eq!(
            read_usage(&definition, &output).limit_warning,
            Some(ktask_core::LimitWarning {
                window: "7 days".to_owned(),
                utilization_percent: 91,
            })
        );
    }

    #[test]
    fn derived_claude_refusals_are_usage_limits() {
        let definition = ProviderDefinition {
            command: "agent".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            resume_command: vec![],
            denied_tools: vec![],
            parser: ProviderParser::ClaudeStreamJson,
            session_id: None,
            usage: None,
            limit_message: Some(r"(?i)Claude AI usage limit reached\|(?<reset>[0-9]+)".to_owned()),
        };
        let fixture =
            include_str!("../../../test-fixtures/claude/claude-2.1.283-derived-rejected.jsonl");
        let rejected = Output {
            stdout: fixture.as_bytes().to_vec(),
            stderr: vec![],
            exit: Exit::Code(1),
        };
        let rejected_limit = detect_limit(&definition, &rejected).expect("a rejection limit");
        assert_eq!(
            rejected_limit
                .reset_at
                .and_then(|reset| reset.duration_since(SystemTime::UNIX_EPOCH).ok())
                .map(|duration| duration.as_secs()),
            Some(1_791_154_800)
        );
        let result = Output {
            stdout: fixture
                .lines()
                .nth(2)
                .expect("the derived error result")
                .as_bytes()
                .to_vec(),
            stderr: vec![],
            exit: Exit::Code(1),
        };
        assert_eq!(
            detect_limit(&definition, &result),
            Some(LimitSignal { reset_at: None })
        );
    }

    #[test]
    fn plain_text_limit_remains_a_fallback() {
        let definition = ProviderDefinition {
            command: "agent".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            resume_command: vec![],
            denied_tools: vec![],
            parser: ProviderParser::ClaudeStreamJson,
            session_id: None,
            usage: None,
            limit_message: Some(r"limit\|(?<reset>[0-9]+)".to_owned()),
        };
        let output = Output {
            stdout: b"limit|42".to_vec(),
            stderr: vec![],
            exit: Exit::Code(1),
        };
        assert_eq!(
            detect_limit(&definition, &output),
            Some(LimitSignal {
                reset_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(42)),
            })
        );
    }
}
