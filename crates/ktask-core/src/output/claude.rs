//! The operator-facing words in Claude Code's `stream-json` events.

use serde_json::Value;

/// Turns a Claude stream into one readable entry per visible event. Transport-only events,
/// including thinking blocks, are deliberately omitted: the retained raw stream remains
/// available to an operator who needs it.
#[must_use]
pub(super) fn render(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(render_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_line(line: &str) -> Option<String> {
    let event = serde_json::from_str::<Value>(line).ok()?;
    render_event(&event)
}

fn render_event(event: &Value) -> Option<String> {
    match event.get("type").and_then(Value::as_str) {
        Some("assistant") => assistant(event),
        Some("content_block_delta") => text(event.get("delta")?),
        Some("user") => tool_result(event),
        Some("result") => final_result(event),
        _ => None,
    }
}

fn assistant(event: &Value) -> Option<String> {
    let content = event.pointer("/message/content")?.as_array()?;
    let entries = content.iter().filter_map(content_entry).collect::<Vec<_>>();
    (!entries.is_empty()).then(|| entries.join("\n"))
}

fn content_entry(content: &Value) -> Option<String> {
    match content.get("type").and_then(Value::as_str) {
        Some("text") => text(content),
        Some("tool_use") => tool_use(content),
        _ => None,
    }
}

fn text(value: &Value) -> Option<String> {
    value
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(|text| format!("assistant: {text}"))
}

fn tool_use(content: &Value) -> Option<String> {
    let name = content.get("name")?.as_str()?;
    let input = content.get("input").unwrap_or(&Value::Null);
    let detail = tool_detail(input);
    Some(match detail {
        Some(detail) => format!("tool {name}: {detail}"),
        None => format!("tool {name}"),
    })
}

fn tool_detail(input: &Value) -> Option<&str> {
    ["command", "file_path", "path", "query", "pattern"]
        .into_iter()
        .find_map(|field| input.get(field).and_then(Value::as_str))
}

fn tool_result(event: &Value) -> Option<String> {
    let content = event.pointer("/message/content")?.as_array()?;
    let summaries = content
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("tool_result"))
        .filter_map(tool_result_summary)
        .collect::<Vec<_>>();
    (!summaries.is_empty()).then(|| format!("tool result: {}", summaries.join("; ")))
}

fn tool_result_summary(value: &Value) -> Option<String> {
    let content = value.get("content")?;
    let text = match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    };
    (!text.is_empty()).then(|| summarize(&text))
}

fn final_result(event: &Value) -> Option<String> {
    event
        .get("result")
        .and_then(Value::as_str)
        .filter(|result| !result.is_empty())
        .map(|result| format!("result: {result}"))
}

fn summarize(text: &str) -> String {
    const MAX: usize = 240;
    let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= MAX {
        return compact;
    }
    let shortened = compact
        .chars()
        .take(MAX.saturating_sub(1))
        .collect::<String>();
    format!("{shortened}…")
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_words_tools_results_and_final_result_without_json_framing() {
        let text = render(
            br#"{"type":"assistant","message":{"content":[{"type":"text","text":"I will inspect the file."},{"type":"tool_use","name":"Read","input":{"file_path":"src/lib.rs"}}]}}
{"type":"user","message":{"content":[{"type":"tool_result","content":"first line\nsecond line"}]}}
{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"c2VjcmV0"}]}}
{"type":"result","result":"Finished the task.","usage":{"output_tokens":2000}}"#,
        );
        assert_eq!(
            text,
            "assistant: I will inspect the file.\ntool Read: src/lib.rs\ntool result: first line second line\nresult: Finished the task."
        );
        assert!(!text.contains('{'));
        assert!(!text.contains("c2VjcmV0"));
    }

    #[test]
    fn text_deltas_are_visible_as_they_arrive() {
        assert_eq!(
            render(
                br#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"working"}}"#
            ),
            "assistant: working"
        );
    }
}
