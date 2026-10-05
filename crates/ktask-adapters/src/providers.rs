//! Built-in provider configuration. Core deliberately knows only generic definitions.

use std::collections::BTreeMap;

use ktask_core::{ProviderDefinition, ProviderParser};

/// The provider definitions shipped by this binary.
#[must_use]
pub fn builtin_providers() -> BTreeMap<String, ProviderDefinition> {
    BTreeMap::from([
        ("claude".to_owned(), claude()),
        ("codex".to_owned(), codex()),
        ("echo".to_owned(), echo()),
    ])
}

/// The shipped Claude Code command-line definition.
fn claude() -> ProviderDefinition {
    ProviderDefinition {
        command: "claude".to_owned(),
        args: vec![
            "--print".to_owned(),
            "--output-format".to_owned(),
            "stream-json".to_owned(),
            "--verbose".to_owned(),
            "--permission-mode".to_owned(),
            "bypassPermissions".to_owned(),
        ],
        prompt: Vec::new(),
        model: vec!["--model".to_owned(), "{model}".to_owned()],
        resume: vec!["--resume".to_owned(), "{session}".to_owned()],
        denied_tools: vec![
            // Cannot work unattended: creates scheduled work beyond this invocation.
            "CronCreate".to_owned(),
            // Cannot work unattended: controls scheduled work outside this invocation.
            "CronDelete".to_owned(),
            // Cannot work unattended: reads schedules that change outside this invocation.
            "CronList".to_owned(),
            // Cannot work unattended: watches work that outlives this invocation.
            "Monitor".to_owned(),
            // Cannot work unattended: schedules a wake-up after this invocation ends.
            "ScheduleWakeup".to_owned(),
            // Cannot work unattended: reads output from work outside this invocation.
            "TaskOutput".to_owned(),
            // Cannot work unattended: controls work outside this invocation.
            "TaskStop".to_owned(),
        ],
        parser: ProviderParser::ClaudeStreamJson,
        session_id: Some("result.session_id".to_owned()),
        usage: Some("usage".to_owned()),
        limit_message: Some(r"(?i)Claude AI usage limit reached\|(?<reset>[0-9]+)".to_owned()),
    }
}

/// The shipped Codex command-line definition.
fn codex() -> ProviderDefinition {
    ProviderDefinition {
        command: "codex".to_owned(),
        args: vec![
            "exec".to_owned(),
            "--json".to_owned(),
            "--dangerously-bypass-approvals-and-sandbox".to_owned(),
            "--skip-git-repo-check".to_owned(),
            "-C".to_owned(),
            "{project-dir}".to_owned(),
        ],
        prompt: vec!["-".to_owned()],
        model: vec!["--model".to_owned(), "{model}".to_owned()],
        resume: Vec::new(),
        denied_tools: Vec::new(),
        parser: ProviderParser::CodexJsonl,
        session_id: Some("thread.started.thread_id".to_owned()),
        usage: Some("turn.completed.usage".to_owned()),
        limit_message: None,
    }
}

/// The shipped scripted provider definition used by hermetic tests.
fn echo() -> ProviderDefinition {
    ProviderDefinition {
        command: "bash".to_owned(),
        args: vec![
            "-s".to_owned(),
            "{token}".to_owned(),
            "{attempt}".to_owned(),
            "{step}".to_owned(),
        ],
        prompt: Vec::new(),
        model: Vec::new(),
        resume: Vec::new(),
        denied_tools: Vec::new(),
        parser: ProviderParser::Plain,
        session_id: Some("KTASK_SESSION: ".to_owned()),
        usage: None,
        limit_message: Some("KTASK_LIMIT: ".to_owned()),
    }
}
