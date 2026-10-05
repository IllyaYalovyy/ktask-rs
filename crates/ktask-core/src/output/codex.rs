//! The operator-facing words in Codex's `--json` event stream.

use serde_json::Value;

/// Turns a Codex stream into readable entries. Events and completed items that are not yet
/// understood remain visible as their JSON text, so a newer Codex cannot silently erase the
/// evidence it emitted.
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
        "item.completed" => item(event.get("item")?).or_else(|| Some(line.to_owned())),
        _ => Some(line.to_owned()),
    }
}

fn item(item: &Value) -> Option<String> {
    match item.get("type")?.as_str()? {
        "agent_message" => item
            .get("text")?
            .as_str()
            .filter(|text| !text.is_empty())
            .map(|text| format!("assistant: {text}")),
        _ => Some(compact(item)),
    }
}

fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| value.to_string())
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
            "{\"type\":\"turn.started\"}\nassistant: OK"
        );
    }

    #[test]
    fn keeps_unknown_events_and_items_as_text() {
        let stream =
            br#"{"type":"item.completed","item":{"type":"command_execution","command":"pwd"}}
{"type":"new.event","detail":"still here"}"#;
        let rendered = render(stream);
        assert!(rendered.contains("command_execution"), "{rendered}");
        assert!(rendered.contains("new.event"), "{rendered}");
    }
}
