//! The operator-facing words in Codex's `--json` event stream.

use serde_json::Value;

/// Turns a Codex stream into readable entries. Each completed item remains visible, including
/// a newer item kind, without exposing the stream's JSON framing to the operator.
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
    match event.get("type")?.as_str()? {
        "thread.started" | "turn.completed" => None,
        "turn.started" => Some("turn started".to_owned()),
        "item.completed" => item(event.get("item")?),
        kind => Some(format!("event: {}", readable_kind(kind))),
    }
}

fn item(item: &Value) -> Option<String> {
    let kind = item.get("type")?.as_str()?;
    match kind {
        "agent_message" => item
            .get("text")?
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| format!("assistant: {text}")),
        _ => Some(item_entry(kind, item)),
    }
}

fn item_entry(kind: &str, item: &Value) -> String {
    let label = readable_kind(kind);
    ["text", "command", "path", "file_path"]
        .into_iter()
        .find_map(|field| item.get(field).and_then(Value::as_str))
        .filter(|detail| !detail.is_empty())
        .map_or_else(|| label.clone(), |detail| format!("{label}: {detail}"))
}

fn readable_kind(kind: &str) -> String {
    kind.replace('_', " ")
}

#[cfg(test)]
mod tests {
    use super::render;

    #[test]
    fn renders_the_recorded_success_without_json_framing() {
        assert_eq!(
            render(include_bytes!(
                "../../../../test-fixtures/codex/codex-0.160.0-success.jsonl"
            )),
            "turn started\nassistant: OK"
        );
    }

    #[test]
    fn renders_other_items_and_events_as_readable_entries() {
        let stream =
            br#"{"type":"item.completed","item":{"type":"command_execution","command":"pwd"}}
{"type":"new.event","detail":"still here"}"#;
        assert_eq!(render(stream), "command execution: pwd\nevent: new.event");
    }
}
