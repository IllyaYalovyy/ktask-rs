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

/// What a turn may do to the workspace. Only execution (and its repair turns) writes;
/// review, validation and every judge are read-only, because judges never run anything.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    Write,
}

impl Access {
    pub fn for_role(role: &str) -> Self {
        if role == "execution" {
            Self::Write
        } else {
            Self::ReadOnly
        }
    }
}

/// The provider's own words for a rate or usage limit, matched in lowercase text.
const RATE_LIMIT_MARKERS: [&str; 7] = [
    "rate_limit",
    "rate limit",
    "usage limit",
    "hit your limit",
    "too many requests",
    "http 429",
    "status 429",
];

fn names_rate_limit(lowercase: &str) -> bool {
    RATE_LIMIT_MARKERS
        .iter()
        .any(|marker| lowercase.contains(marker))
}

/// Everything the worker needs to start one provider turn.
pub struct Launch {
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    /// Codex reads the response schema from a file that the worker writes.
    pub schema_file: Option<Value>,
    pub prompt: String,
}

impl Launch {
    /// Permit only the supervisor's bounded reader, not its mutating commands.
    pub(super) fn allow_evidence(
        &mut self,
        provider: Provider,
        executable: &std::path::Path,
    ) -> Result<()> {
        let command = format!("{} evidence", executable.display());
        match provider {
            Provider::Claude => {
                let index = self
                    .args
                    .iter()
                    .position(|a| a == "--allowedTools")
                    .ok_or_else(|| Error::Conflict("Missing read-only tool policy".into()))?;
                self.args.insert(index + 1, format!("Bash({command} *)"));
            }
            Provider::OpenCode => {
                let (_, policy) = self
                    .env
                    .iter_mut()
                    .find(|(k, _)| k == "OPENCODE_PERMISSION")
                    .ok_or_else(|| Error::Conflict("Missing read-only tool policy".into()))?;
                let mut value: Value =
                    serde_json::from_str(policy).map_err(|e| Error::Conflict(e.to_string()))?;
                value["bash"][format!("{command} *")] = Value::String("allow".into());
                *policy = value.to_string();
            }
            Provider::Codex => {}
        }
        Ok(())
    }
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
    pub fn writable_network(self, enabled: bool) -> Result<()> {
        if !enabled && self != Self::Codex {
            return Err(Error::Conflict(format!(
                "{} cannot enforce Agent network Off for writable workers; set Agent network On or choose Codex for execution and repair",
                self.label()
            )));
        }
        Ok(())
    }
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

    /// `network` lets a writable turn use the network. Call `writable_network` before launch.
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

#[cfg(test)]
mod network_tests {
    use super::Provider;

    #[test]
    fn offline_writable_workers_require_provider_enforcement() {
        assert!(Provider::Codex.writable_network(false).is_ok());
        for provider in [Provider::Claude, Provider::OpenCode] {
            assert!(provider.writable_network(false).is_err());
            assert!(provider.writable_network(true).is_ok());
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
    pub configuration_error: Option<(String, String)>,
    error_context: String,
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
        self.note_error(text);
        if names_rate_limit(&text.to_lowercase()) {
            self.limit(super::reset::reset_at(text, crate::application::now_ms()));
        }
    }

    /// The provider's last error message, in its own words, for the failure reason.
    /// Providers nest error JSON inside JSON strings, so quotes are unescaped first.
    pub fn error_summary(&self) -> Option<String> {
        const KEY: &str = "\"message\":\"";
        let mut context = self.error_context.trim().to_owned();
        if context.is_empty() {
            return None;
        }
        while context.contains("\\\"") {
            context = context.replace("\\\"", "\"");
        }
        let message = context.rfind(KEY).map_or(context.as_str(), |start| {
            let rest = &context[start + KEY.len()..];
            &rest[..rest.find('"').unwrap_or(rest.len())]
        });
        Some(message.chars().take(400).collect())
    }

    pub fn note_error(&mut self, text: &str) {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            let status = error_field(
                &value,
                &[
                    "status",
                    "status_code",
                    "statusCode",
                    "api_error_status",
                    "code",
                ],
            )
            .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()));
            if status == Some(429) {
                self.limit(super::reset::reset_at(text, crate::application::now_ms()));
                return;
            }
            if status.is_some_and(|status| status >= 500) {
                return;
            }
            if let Some(setting) = structured_rejection(&value, status) {
                self.configuration_error = Some((setting.into(), text.chars().take(500).collect()));
                return;
            }
        }
        self.error_context.push_str(text);
        if self.error_context.len() > 4096 {
            let start = self.error_context.len() - 4096;
            self.error_context =
                self.error_context[self.error_context.floor_char_boundary(start)..].to_owned();
        }
        let lower = self.error_context.to_ascii_lowercase();
        if names_rate_limit(&lower) {
            return;
        }
        let setting = if lower.contains("effort") || lower.contains("variant") {
            Some("effort")
        } else if lower.contains("model") {
            Some("model")
        } else if lower.contains("invalid parameter") || lower.contains("invalid request") {
            Some("request settings")
        } else {
            None
        };
        let rejected = lower.contains("http 400")
            || lower.contains("error code: 400")
            || lower.contains("status code: 400")
            || lower.contains("status 400")
            || lower.contains("status\":400")
            || lower.contains("code\":400")
            || lower.contains("status_code\":400")
            || lower.contains("400 bad request")
            || lower.contains("unknown model")
            || lower.contains("model not found")
            || lower.contains("unsupported model")
            || lower.contains("invalid parameter")
            || lower.contains("invalid value")
            || lower.contains("unsupported value")
            || lower.contains("invalid model")
            || lower.contains("unrecognized model")
            || lower.contains("model does not exist")
            || (lower.contains("model")
                && lower.contains("does not exist or you do not have access"))
            || lower.contains("model_not_found")
            || lower.contains("invalid effort")
            || lower.contains("unsupported effort");
        if rejected && let Some(setting) = setting {
            self.configuration_error = Some((
                setting.into(),
                self.error_context.chars().take(500).collect(),
            ));
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

/// Inspect provider error fields before falling back to its human-readable message.
fn error_field<'a>(value: &'a Value, names: &[&str]) -> Option<&'a Value> {
    names.iter().find_map(|name| named_error_field(value, name))
}

fn named_error_field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Object(fields) => fields
            .get(name)
            .filter(|value| !value.is_null())
            .or_else(|| {
                fields
                    .values()
                    .find_map(|value| named_error_field(value, name))
            }),
        Value::Array(values) => values
            .iter()
            .find_map(|value| named_error_field(value, name)),
        _ => None,
    }
}

fn structured_rejection(value: &Value, status: Option<i64>) -> Option<&'static str> {
    let code = error_code(value).unwrap_or_default();
    // Event envelopes have their own type; inspect the nested API error first.
    let code = value
        .get("error")
        .filter(|error| error.is_object())
        .and_then(error_code)
        .unwrap_or(code)
        .to_ascii_lowercase();
    let parameter = error_field(value, &["param", "parameter"])
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if [
        "model_not_found",
        "invalid_model",
        "unsupported_model",
        "unknown_model",
    ]
    .contains(&code.as_str())
    {
        return Some("model");
    }
    let rejected_setting = [
        "invalid_request_error",
        "invalid_parameter",
        "invalid_value",
        "unsupported_value",
    ]
    .contains(&code.as_str());
    let invalid = matches!(status, Some(400 | 404 | 422)) || rejected_setting;
    if invalid && !parameter.is_empty() {
        return Some(
            if parameter.contains("effort") || parameter.contains("variant") {
                "effort"
            } else if parameter.contains("model") {
                "model"
            } else {
                "request settings"
            },
        );
    }
    if rejected_setting {
        let message = error_field(value, &["message", "result"])
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        return Some(
            if message.contains("effort") || message.contains("variant") {
                "effort"
            } else if message.contains("model") {
                "model"
            } else {
                "request settings"
            },
        );
    }
    None
}

fn error_code(value: &Value) -> Option<&str> {
    ["code", "type", "name"]
        .iter()
        .find_map(|name| named_error_field(value, name).and_then(Value::as_str))
}

#[cfg(test)]
mod configuration_tests {
    use super::*;

    #[test]
    fn explicit_setting_errors_without_parameter_do_not_require_http_status() {
        for code in [
            "invalid_request_error",
            "invalid_parameter",
            "invalid_value",
            "unsupported_value",
        ] {
            for status in [None, Some(400)] {
                let mut stream = Stream::new(Provider::Codex);
                let value = serde_json::json!({"type":"turn.failed","error":{
                    "type":code,"status":status,"message":"max_tokens must be a positive integer"
                }});
                stream.feed(format!("{value}\n").as_bytes()).unwrap();
                let rejection = stream.configuration_error.unwrap();
                assert_eq!(rejection.0, "request settings");
                assert!(
                    rejection
                        .1
                        .contains("max_tokens must be a positive integer")
                );
                assert!(!stream.rate_limited);
            }
        }
        let mut stream = Stream::new(Provider::Codex);
        let value = serde_json::json!({"type":"turn.failed","error":{
            "code":400,"type":"invalid_request_error","message":"max_tokens must be a positive integer"
        }});
        stream.feed(format!("{value}\n").as_bytes()).unwrap();
        assert_eq!(stream.configuration_error.unwrap().0, "request settings");
        for (message, setting) in [
            ("Unsupported effort ultra", "effort"),
            ("Unknown model foo", "model"),
        ] {
            let mut stream = Stream::new(Provider::Codex);
            let value = serde_json::json!({"type":"turn.failed","error":{
                "type":"invalid_request_error","message":message
            }});
            stream.feed(format!("{value}\n").as_bytes()).unwrap();
            assert_eq!(stream.configuration_error.unwrap().0, setting);
        }
    }

    #[test]
    fn unrelated_not_found_and_server_or_limit_statuses_are_not_configuration() {
        for status in [429, 503] {
            let mut stream = Stream::new(Provider::Codex);
            let value = serde_json::json!({"type":"turn.failed","error":{
                "type":"invalid_request_error","status":status,"message":"model service unavailable"
            }});
            stream.feed(format!("{value}\n").as_bytes()).unwrap();
            assert!(stream.configuration_error.is_none());
            assert_eq!(stream.rate_limited, status == 429);
        }
        let mut stream = Stream::new(Provider::Codex);
        let value = serde_json::json!({"type":"turn.failed","error":{
            "type":"not_found","status":404,"message":"endpoint unavailable"
        }});
        stream.feed(format!("{value}\n").as_bytes()).unwrap();
        assert!(stream.configuration_error.is_none());
        assert!(!stream.rate_limited);
    }

    #[test]
    fn structured_request_rejections_preserve_provider_details() {
        for (provider, value, setting) in [
            (
                Provider::Codex,
                serde_json::json!({"type":"turn.failed", "error":{"code":"model_not_found", "status":404,"message":"The model `typo` does not exist or you do not have access to it"}}),
                "model",
            ),
            (
                Provider::Claude,
                serde_json::json!({"type":"result", "subtype":"error_during_execution", "is_error":true,"api_error_status":400,"error":{"type":"invalid_request_error","param":"model"},"result":"Rejected"}),
                "model",
            ),
            (
                Provider::OpenCode,
                serde_json::json!({"type":"error","error":{"name":"APIError","data":{"statusCode":422,"code":"unsupported_value","param":"reasoning.effort","message":"bad setting"}}}),
                "effort",
            ),
        ] {
            let mut stream = Stream::new(provider);
            stream.feed(format!("{value}\n").as_bytes()).unwrap();
            let rejection = stream.configuration_error.unwrap();
            assert_eq!(rejection.0, setting);
            assert!(rejection.1.contains(if provider == Provider::Codex {
                "model_not_found"
            } else if provider == Provider::Claude {
                "invalid_request_error"
            } else {
                "unsupported_value"
            }));
            assert!(!stream.rate_limited);
        }
    }

    #[test]
    fn rejected_request_reports_the_providers_own_message() {
        let mut stream = Stream::new(Provider::Codex);
        assert_eq!(stream.error_summary(), None);
        let inner = r#"{"type":"error","error":{"message":"Invalid schema for response_format 'codex_output_schema': Missing 'models'.","type":"invalid_request_error","code":"invalid_json_schema"},"status":400}"#;
        let events = format!(
            "{}\n{}\n",
            serde_json::json!({"type":"error","message":inner}),
            serde_json::json!({"type":"turn.failed","error":{"message":inner}}),
        );
        stream.feed(events.as_bytes()).unwrap();
        assert!(stream.configuration_error.is_none());
        assert_eq!(
            stream.error_summary().as_deref(),
            Some("Invalid schema for response_format 'codex_output_schema': Missing 'models'.")
        );
    }

    #[test]
    fn access_wording_is_configuration_but_limits_and_transport_are_preserved() {
        let mut stream = Stream::new(Provider::Codex);
        stream.note_error("The model `typo` does not exist or you do not have access to it");
        assert_eq!(stream.configuration_error.unwrap().0, "model");
        for status in [429, 503] {
            let mut stream = Stream::new(Provider::Codex);
            let value = serde_json::json!({"type":"turn.failed", "error":{"status":status,"param":"model","message":"model service unavailable"}});
            stream.feed(format!("{value}\n").as_bytes()).unwrap();
            assert!(stream.configuration_error.is_none());
            assert_eq!(stream.rate_limited, status == 429);
            assert!(stream.failed);
        }
    }

    #[test]
    fn split_stderr_rejection_is_configuration_error() {
        let mut stream = Stream::new(Provider::Codex);
        stream.note_error("Error code: 400: Unsupported ");
        assert!(stream.configuration_error.is_none());
        stream.note_error("value 'hight' for model_reasoning_");
        stream.note_error("effort\n");
        assert_eq!(stream.configuration_error.as_ref().unwrap().0, "effort");
        assert!(
            stream
                .configuration_error
                .as_ref()
                .unwrap()
                .1
                .contains("hight")
        );
    }

    #[test]
    fn claude_status_400_unknown_model_is_configuration_error() {
        let mut stream = Stream::new(Provider::Claude);
        stream.feed(b"{\"type\":\"result\",\"is_error\":true,\"subtype\":\"error_during_execution\",\"api_error_status\":400,\"result\":\"Unknown model foo\"}\n").unwrap();
        assert_eq!(stream.configuration_error.as_ref().unwrap().0, "model");
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
        assert_eq!(Access::for_role("execution"), Access::Write);
    }

    #[test]
    fn only_execution_writes_and_judges_never_run_anything() {
        for role in crate::application::ROLES {
            let expected = if role == "execution" {
                Access::Write
            } else {
                Access::ReadOnly
            };
            assert_eq!(Access::for_role(role), expected, "{role}");
        }
        // A repair turn is an execution turn; the failure judge is the remediation role.
        assert_eq!(
            Access::for_role(crate::runner::agent::model_role("repair")),
            Access::Write
        );
        assert_eq!(Access::for_role("remediation"), Access::ReadOnly);
    }

    #[test]
    fn one_rate_limit_vocabulary_marks_limits_and_never_configuration() {
        for marker in RATE_LIMIT_MARKERS {
            let mut failed = Stream::new(Provider::Codex);
            failed.fail(&format!("Error: {}", marker.to_uppercase()));
            assert!(failed.rate_limited, "{marker}");
            let mut noted = Stream::new(Provider::Codex);
            noted.note_error(&format!("HTTP 400 invalid model: {marker}"));
            assert!(noted.configuration_error.is_none(), "{marker}");
        }
    }
}

#[cfg(test)]
mod evidence_tests {
    use super::*;
    #[test]
    fn reader_permission_does_not_allow_other_supervisor_commands() {
        let mut claude = Launch {
            args: vec!["--allowedTools".into(), "Read".into()],
            env: vec![],
            schema_file: None,
            prompt: String::new(),
        };
        claude
            .allow_evidence(Provider::Claude, std::path::Path::new("/installed/ktask"))
            .unwrap();
        assert_eq!(claude.args[1], "Bash(/installed/ktask evidence *)");
        let mut opencode = Launch {
            args: vec![],
            env: vec![(
                "OPENCODE_PERMISSION".into(),
                r#"{"bash":{"*":"deny"},"edit":"deny"}"#.into(),
            )],
            schema_file: None,
            prompt: String::new(),
        };
        opencode
            .allow_evidence(Provider::OpenCode, std::path::Path::new("/installed/ktask"))
            .unwrap();
        let policy: Value = serde_json::from_str(&opencode.env[0].1).unwrap();
        assert_eq!(policy["bash"]["*"], "deny");
        assert_eq!(policy["bash"]["/installed/ktask evidence *"], "allow");
        assert_eq!(policy["edit"], "deny");
        assert_eq!(policy["bash"].as_object().unwrap().len(), 2);
    }
}
