//! Supported coding-agent CLIs: how to launch one turn and how to read its JSON events.
//! Every provider yields the same events, structured result, usage units and failure signals.
mod claude;
mod codex;
mod opencode;

use crate::{
    application::ModelConfig,
    storage::{Error, Result, usage::TokenUsage},
};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Provider {
    #[default]
    Codex,
    Claude,
    OpenCode,
}

pub const SUPPORTED: &str = "codex, claude or opencode";

/// What a turn may do to the workspace. Review, validation and every judge are read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    Write,
}

impl Access {
    pub fn for_role(role: &str) -> Self {
        if matches!(role, "execution" | "remediation") {
            Self::Write
        } else {
            Self::ReadOnly
        }
    }
}

/// Everything the worker needs to start one provider turn.
pub struct Launch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Codex reads the response schema from a file that the worker writes.
    pub schema_file: Option<Value>,
    pub prompt: String,
}

pub enum Event {
    Session(String),
    Usage(TokenUsage, Option<i64>),
    /// Tool activity and other progress, shown as written.
    Text(String),
    /// The agent's own reply; a structured assessment is summarized for display.
    Message(String),
}

impl Provider {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            "opencode" => Ok(Self::OpenCode),
            other => Err(Error::Conflict(format!(
                "Provider {other:?} is not supported; use {SUPPORTED}"
            ))),
        }
    }

    pub fn of(model: &ModelConfig) -> Result<Self> {
        Self::parse(&model.provider)
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
            Self::OpenCode => "opencode",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex",
            Self::Claude => "Claude Code",
            Self::OpenCode => "OpenCode",
        }
    }

    pub fn executable(self) -> Result<PathBuf> {
        super::executable(self.name())
    }

    /// Whose credentials a session belongs to: sessions are reused only within one scope.
    pub fn auth_scope(self) -> Result<String> {
        let var = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let home = || var("HOME");
        let scope = match self {
            Self::Codex => var("CODEX_HOME").or_else(|| home().map(|h| h.join(".codex"))),
            Self::Claude => var("CLAUDE_CONFIG_DIR").or_else(|| home().map(|h| h.join(".claude"))),
            Self::OpenCode => var("XDG_DATA_HOME")
                .or_else(|| home().map(|h| h.join(".local/share")))
                .map(|data| data.join("opencode")),
        };
        scope
            .map(|path| path.to_string_lossy().into_owned())
            .ok_or_else(|| Error::Conflict(format!("{} auth home is not configured", self.label())))
    }

    /// `network` lets a writable turn use the network. Only Codex sandboxes it; Claude Code
    /// and OpenCode commands always have it.
    pub fn launch(
        self,
        model: &ModelConfig,
        session: Option<&str>,
        access: Access,
        network: bool,
        schema: &Value,
        prompt: String,
    ) -> Launch {
        match self {
            Self::Codex => codex::launch(model, session, access, network, schema, prompt),
            Self::Claude => claude::launch(model, session, access, schema, prompt),
            Self::OpenCode => opencode::launch(model, session, access, schema, prompt),
        }
    }
}

// Command-completion events contain the entire captured command output.
const MAX_EVENT_BYTES: usize = 16 * 1024 * 1024;

/// What one turn reported so far, independent of the provider's event format.
#[derive(Default)]
pub struct Stream {
    pub provider: Provider,
    pub completed: bool,
    pub failed: bool,
    pub rate_limited: bool,
    /// The provider's own reset time for a limit, when it names one.
    pub reset_at_ms: Option<i64>,
    pub last_message: Option<String>,
    session: Option<String>,
    usage: Totals,
    pending: Vec<u8>,
}

/// Running totals for providers that report usage per step rather than per turn.
#[derive(Default)]
struct Totals {
    input: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    cost_microusd: Option<i64>,
}

impl Stream {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            ..Self::default()
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Event>> {
        let mut events = Vec::new();
        // Scan only new bytes, and bound individual records rather than a batch.
        for segment in bytes.split_inclusive(|b| *b == b'\n') {
            if segment.len() > MAX_EVENT_BYTES - self.pending.len() {
                return Err(Error::Conflict(format!(
                    "{} event exceeds 16 MiB",
                    self.provider.label()
                )));
            }
            self.pending.extend_from_slice(segment);
            if !segment.ends_with(b"\n") {
                continue;
            }
            let value: Value = serde_json::from_slice(&self.pending).map_err(|e| {
                Error::Conflict(format!("Invalid {} event: {e}", self.provider.label()))
            })?;
            self.pending.clear();
            events.extend(match self.provider {
                Provider::Codex => codex::event(self, &value)?,
                Provider::Claude => claude::event(self, &value)?,
                Provider::OpenCode => opencode::event(self, &value)?,
            });
        }
        Ok(events)
    }

    /// True once every byte the provider wrote formed complete events.
    pub fn drained(&self) -> bool {
        self.pending.is_empty()
    }

    /// A failed turn; limits are recognized from the provider's own words.
    fn fail(&mut self, text: &str) {
        self.failed = true;
        self.completed = false;
        let lower = text.to_lowercase();
        if [
            "rate_limit",
            "rate limit",
            "usage limit",
            "hit your limit",
            "too many requests",
        ]
        .iter()
        .any(|s| lower.contains(s))
        {
            self.limit(super::reset::reset_at(text, crate::application::now_ms()));
        }
    }

    fn limit(&mut self, reset_at_ms: Option<i64>) {
        self.rate_limited = true;
        self.reset_at_ms = self.reset_at_ms.or(reset_at_ms);
    }

    /// The first session ID the provider reports.
    fn session(&mut self, id: Option<&str>) -> Option<Event> {
        let id = id.filter(|id| !id.is_empty())?;
        if self.session.as_deref() == Some(id) {
            return None;
        }
        self.session = Some(id.into());
        Some(Event::Session(id.into()))
    }
}

/// "$ command\noutput" for shell activity, or "tool: target" otherwise.
fn tool_text(name: &str, input: &Value, output: &str) -> String {
    let command = input["command"].as_str();
    let head = match command {
        Some(command) => format!("$ {command}"),
        None => {
            let target = ["file_path", "filePath", "path", "pattern", "description"]
                .iter()
                .find_map(|key| input[key].as_str())
                .unwrap_or("");
            format!("{name}: {target}")
        }
    };
    if output.is_empty() {
        head
    } else {
        format!("{head}\n{output}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_bounds_records_instead_of_batches() {
        let record = format!(
            "{{\"type\":\"ignored\",\"text\":\"{}\"}}\n",
            "x".repeat(4096)
        );
        let batch = record.repeat(MAX_EVENT_BYTES / record.len() + 1);
        let mut stream = Stream::default();
        assert!(stream.feed(batch.as_bytes()).unwrap().is_empty());
        assert!(stream.pending.is_empty());
        // An exactly bounded event remains valid even when its newline arrives separately.
        let record = format!("{{\"text\":\"{}\"}}\n", "x".repeat(MAX_EVENT_BYTES - 12));
        assert_eq!(record.len(), MAX_EVENT_BYTES);
        stream.feed(&record.as_bytes()[..record.len() - 1]).unwrap();
        stream.feed(b"\n").unwrap();
        assert!(stream.pending.is_empty());
        // Reject oversized complete and unfinished events without growing past the bound.
        for tail in [b"\n".as_slice(), b"x".as_slice()] {
            let mut stream = Stream::default();
            stream.feed(&vec![b' '; MAX_EVENT_BYTES]).unwrap();
            assert!(stream.feed(tail).is_err());
            assert_eq!(stream.pending.len(), MAX_EVENT_BYTES);
        }
        assert!(
            Stream::default()
                .feed(&vec![b' '; MAX_EVENT_BYTES + 1])
                .is_err()
        );
    }

    #[test]
    fn providers_parse_by_name_and_reviewers_are_read_only() {
        assert_eq!(Provider::parse("claude").unwrap(), Provider::Claude);
        assert_eq!(Provider::parse("opencode").unwrap(), Provider::OpenCode);
        assert!(Provider::parse("gemini").is_err());
        for role in ["review", "validation"] {
            assert_eq!(Access::for_role(role), Access::ReadOnly);
        }
        for role in ["execution", "remediation"] {
            assert_eq!(Access::for_role(role), Access::Write);
        }
    }
}
