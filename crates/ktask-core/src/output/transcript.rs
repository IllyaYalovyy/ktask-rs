//! The retained output of one attempt, split into one transcript per agent step.

use std::error::Error;
use std::fmt;

use crate::{ProviderParser, ProviderView, StatusEntry, StepLine, TaskId};

/// Port: the retained provider bytes of each agent step of an attempt.
pub trait StepOutputStore {
    /// Everything `step` of `attempt` of `task` has written so far; empty when it has written
    /// nothing.
    ///
    /// # Errors
    ///
    /// Fails, with the reason, when output exists but cannot be read.
    fn read_step_output(&self, task: TaskId, attempt: u32, step: &str) -> Result<Vec<u8>, String>;
}

/// The name of the file holding the output of `step` of `attempt` of `task`.
#[must_use]
pub fn step_output_file_name(task: TaskId, attempt: u32, step: &str) -> String {
    format!("{}{step}.log", attempt_output_file_prefix(task, attempt))
}

/// What every file holding output of `attempt` of `task` begins with.
#[must_use]
pub fn attempt_output_file_prefix(task: TaskId, attempt: u32) -> String {
    format!("{task}-{attempt}-")
}

/// Why the transcripts of an attempt cannot be produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptError {
    /// The attempt ran no agent step with this name.
    UnknownStep(TaskId, u32, String),
    /// The output or the configuration needed to read it is unavailable.
    Unavailable(String),
}

impl fmt::Display for TranscriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownStep(task, attempt, step) => {
                write!(
                    f,
                    "task {task} attempt {attempt} has no agent step {step:?}"
                )
            }
            Self::Unavailable(reason) => f.write_str(reason),
        }
    }
}

impl Error for TranscriptError {}

/// What one agent step of an attempt said, with who said it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepTranscript {
    /// The step's journal name.
    pub step: String,
    /// The provider that ran the step.
    pub provider: String,
    /// The model the step ran with, when it has one.
    pub model: Option<String>,
    parser: ProviderParser,
    raw: Vec<u8>,
}

impl StepTranscript {
    /// What `step`, run by `provider` with `model`, retained as `raw` in `parser`'s encoding.
    #[must_use]
    pub fn new(
        step: &str,
        provider: &str,
        model: Option<&str>,
        parser: ProviderParser,
        raw: &[u8],
    ) -> Self {
        Self {
            step: step.to_owned(),
            provider: provider.to_owned(),
            model: model.map(str::to_owned),
            parser,
            raw: raw.to_vec(),
        }
    }

    /// The provider's bytes exactly as they were received.
    #[must_use]
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    /// The provider's output as the entries an operator reads, safe to draw.
    #[must_use]
    pub fn readable(&self) -> String {
        super::sanitize_output(super::render_provider_output(self.parser, &self.raw).as_bytes())
    }
}

/// One transcript per agent step that `attempt` of `task` ran, in the order the steps ran —
/// or only the step called `step`. This is shared by the CLI and the TUI so both show the same
/// conversations.
///
/// # Errors
///
/// Fails when the attempt ran no such step, when a step's provider is not configured, or when a
/// step's retained output cannot be read.
pub fn attempt_transcripts(
    entries: &[StatusEntry],
    providers: &[ProviderView],
    store: &dyn StepOutputStore,
    task: TaskId,
    attempt: u32,
    step: Option<&str>,
) -> Result<Vec<StepTranscript>, TranscriptError> {
    agent_steps(entries, task, attempt, step)?
        .into_iter()
        .map(|(line, provider)| {
            let raw = store
                .read_step_output(task, attempt, &line.step)
                .map_err(TranscriptError::Unavailable)?;
            Ok(StepTranscript::new(
                &line.step,
                provider,
                line.model.as_deref(),
                parser_of(providers, provider)?,
                &raw,
            ))
        })
        .collect()
}

/// The steps of `attempt` that an agent ran, with their providers — only the one called
/// `wanted` when it is given.
fn agent_steps<'a>(
    entries: &'a [StatusEntry],
    task: TaskId,
    attempt: u32,
    wanted: Option<&str>,
) -> Result<Vec<(&'a StepLine, &'a str)>, TranscriptError> {
    let lines = super::find_attempt(entries, task, attempt)
        .map(|found| found.steps.as_slice())
        .unwrap_or_default();
    let found = lines
        .iter()
        .filter_map(|line| Some((line, line.provider.as_deref()?)))
        .filter(|(line, _)| wanted.is_none_or(|name| line.step == name))
        .collect::<Vec<_>>();
    match wanted {
        Some(name) if found.is_empty() => {
            Err(TranscriptError::UnknownStep(task, attempt, name.to_owned()))
        }
        _ => Ok(found),
    }
}

fn parser_of(providers: &[ProviderView], name: &str) -> Result<ProviderParser, TranscriptError> {
    providers
        .iter()
        .find(|candidate| candidate.name == name)
        .map(|candidate| candidate.definition.parser)
        .ok_or_else(|| {
            TranscriptError::Unavailable(format!("cannot find output parser for provider {name:?}"))
        })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crate::{
        AttemptLine, AttemptOutcome, ProviderDefinition, ProviderParser, StatusEntry, StepLine,
        TaskStatus, Usage, provider_views,
    };

    use super::*;

    struct Store(BTreeMap<String, Vec<u8>>);

    impl StepOutputStore for Store {
        fn read_step_output(
            &self,
            _task: TaskId,
            _attempt: u32,
            step: &str,
        ) -> Result<Vec<u8>, String> {
            Ok(self.0.get(step).cloned().unwrap_or_default())
        }
    }

    fn step(name: &str, provider: Option<&str>, model: Option<&str>) -> StepLine {
        StepLine {
            step: name.to_owned(),
            provider: provider.map(str::to_owned),
            model: model.map(str::to_owned),
            session: None,
            time_spent: Duration::ZERO,
            outcome: AttemptOutcome::Passed,
            reason: None,
            waiting: None,
            limit_wait: None,
            limit_warning: None,
            routed: None,
            more_time: None,
            usage: Usage::default(),
        }
    }

    fn entry(steps: Vec<StepLine>) -> Vec<StatusEntry> {
        vec![StatusEntry {
            task: TaskId(4),
            title: String::new(),
            status: TaskStatus::Done,
            attempt: AttemptLine {
                number: 2,
                step: String::new(),
                provider: None,
                model: None,
                session: None,
                time_spent: Duration::ZERO,
                outcome: AttemptOutcome::Passed,
                reason: None,
                waiting: None,
                limit_wait: None,
                limit_warning: None,
                routed: None,
                more_time: None,
                usage: Usage::default(),
                output_activity: None,
                steps,
            },
            history: vec![],
            done_by_user: None,
        }]
    }

    fn providers() -> Vec<ProviderView> {
        let plain = ProviderDefinition {
            command: "dummy".to_owned(),
            args: vec![],
            prompt: vec![],
            model: vec![],
            resume: vec![],
            resume_command: vec![],
            denied_tools: vec![],
            parser: ProviderParser::Plain,
            session_id: None,
            usage: None,
            limit_message: None,
        };
        provider_views(
            &BTreeMap::from([("dummy".to_owned(), plain)]),
            &BTreeMap::new(),
        )
        .expect("a complete provider")
    }

    fn store() -> Store {
        Store(BTreeMap::from([
            ("implementation".to_owned(), b"built it\n".to_vec()),
            ("review".to_owned(), b"looks fine".to_vec()),
        ]))
    }

    fn steps() -> Vec<StepLine> {
        vec![
            step("health check", None, None),
            step("implementation", Some("dummy"), Some("m1")),
            step("review", Some("dummy"), None),
            step("commit", None, None),
        ]
    }

    #[test]
    fn each_agent_step_is_one_transcript_in_the_order_the_steps_ran() {
        let found =
            attempt_transcripts(&entry(steps()), &providers(), &store(), TaskId(4), 2, None)
                .expect("transcripts");
        let names = found
            .iter()
            .map(|t| (t.step.as_str(), t.model.as_deref(), t.readable()))
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                ("implementation", Some("m1"), "built it\n".to_owned()),
                ("review", None, "looks fine".to_owned()),
            ]
        );
        assert_eq!(found[0].raw(), b"built it\n");
    }

    #[test]
    fn one_step_can_be_selected_and_an_unknown_or_command_step_is_refused() {
        let only = attempt_transcripts(
            &entry(steps()),
            &providers(),
            &store(),
            TaskId(4),
            2,
            Some("review"),
        )
        .expect("one transcript");
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].step, "review");
        for wanted in ["commit", "nothing"] {
            assert_eq!(
                attempt_transcripts(
                    &entry(steps()),
                    &providers(),
                    &store(),
                    TaskId(4),
                    2,
                    Some(wanted)
                ),
                Err(TranscriptError::UnknownStep(
                    TaskId(4),
                    2,
                    wanted.to_owned()
                ))
            );
        }
    }

    #[test]
    fn a_step_with_an_unconfigured_provider_is_unavailable() {
        let result = attempt_transcripts(
            &entry(vec![step("implementation", Some("gone"), None)]),
            &providers(),
            &store(),
            TaskId(4),
            2,
            None,
        );
        assert!(matches!(result, Err(TranscriptError::Unavailable(_))));
    }

    #[test]
    fn file_names_are_per_attempt_and_per_step() {
        assert_eq!(
            step_output_file_name(TaskId(4), 2, "review"),
            "4-2-review.log"
        );
        assert!(
            step_output_file_name(TaskId(4), 2, "review")
                .starts_with(&attempt_output_file_prefix(TaskId(4), 2))
        );
        assert!(
            !step_output_file_name(TaskId(41), 2, "review")
                .starts_with(&attempt_output_file_prefix(TaskId(4), 2))
        );
    }
}
