//! The instructions gate: before a task's attempt begins, reads the instruction files its
//! agent steps open their prompts with — `VISION.md`, then the role's own — so the tool, not
//! the task text, tells every agent what the project is and how its role works. A file that
//! cannot be read refuses the run the way a failing health check does: no attempt is begun,
//! and the task stays `pending`.

use std::path::Path;

use crate::run::RunEnd;
use crate::{
    Clock, IMPLEMENTATION, InstructionFiles, Journal, RESOLVE_STEP, REVIEW_STEP, RunContext,
    RunError, TEST_STEP, TaskId,
};

/// The journal name of the instructions gate.
pub const INSTRUCTIONS_STEP: &str = "instructions";

const VISION_FILE: &str = "VISION.md";

/// The file the agent role of each step reads its own instructions from.
fn role_file(step: &str) -> Option<&'static str> {
    match step {
        IMPLEMENTATION => Some("CODER.md"),
        REVIEW_STEP => Some("REVIEWER.md"),
        TEST_STEP => Some("TESTER.md"),
        RESOLVE_STEP => Some("RESOLVER.md"),
        _ => None,
    }
}

/// The agent steps that will run for `context`, in the order they run.
fn agent_steps(context: RunContext<'_>) -> impl Iterator<Item = &'static str> + '_ {
    [IMPLEMENTATION, REVIEW_STEP, TEST_STEP, RESOLVE_STEP]
        .into_iter()
        .filter(move |step| {
            matches!(*step, IMPLEMENTATION | RESOLVE_STEP) || context.step_enabled(step)
        })
}

/// The text each agent step's prompt opens with, read once before the attempt begins.
#[derive(Debug, Default)]
pub(crate) struct Instructions {
    openings: Vec<(&'static str, String)>,
}

impl Instructions {
    /// What the prompt of `step` opens with: the vision, then the step's role file. Empty for
    /// a step whose files were not read.
    pub(crate) fn opening(&self, step: &str) -> &str {
        self.openings
            .iter()
            .find(|(name, _)| *name == step)
            .map_or("", |(_, text)| text)
    }
}

/// An instruction file that could not be read.
#[derive(Debug)]
struct Unreadable {
    path: String,
    reason: String,
}

/// `text`, with a newline added when it does not end in one.
fn with_trailing_newline(text: &str) -> String {
    if text.ends_with('\n') {
        text.to_owned()
    } else {
        format!("{text}\n")
    }
}

fn read_file(
    files: &dyn InstructionFiles,
    context: RunContext<'_>,
    name: &str,
) -> Result<String, Unreadable> {
    let dir = Path::new(context.instructions_dir);
    files
        .read(&context.project_dir.join(dir).join(name))
        .map_err(|error| Unreadable {
            path: dir.join(name).display().to_string(),
            reason: error.to_string(),
        })
}

fn read_instructions(
    files: &dyn InstructionFiles,
    context: RunContext<'_>,
) -> Result<Instructions, Unreadable> {
    let vision = with_trailing_newline(&read_file(files, context, VISION_FILE)?);
    let mut openings = Vec::new();
    for step in agent_steps(context) {
        let Some(file) = role_file(step) else {
            continue;
        };
        let role = with_trailing_newline(&read_file(files, context, file)?);
        openings.push((step, format!("{vision}{role}\n")));
    }
    Ok(Instructions { openings })
}

/// What failed and what is expected of the operator, on one line so it fits a status step's own
/// line — the words the queue screen shows for the stop.
fn gate_message(path: &str, reason: &str) -> String {
    format!("{path}: {reason}; add it or change instructions-dir")
}

/// Reads the instruction files for `task_id`'s attempt: the [`Instructions`] to open its
/// prompts with, or the [`RunEnd`] that stops the run when a file cannot be read — recording
/// why in the journal first, so a later `status` can show it even though the task stays
/// `pending`.
///
/// # Errors
///
/// Fails when the journal cannot be read or written.
pub(crate) fn run_gate(
    journal: &dyn Journal,
    files: &dyn InstructionFiles,
    clock: &dyn Clock,
    context: RunContext<'_>,
    task_id: TaskId,
) -> Result<Result<Instructions, RunEnd>, RunError> {
    match read_instructions(files, context) {
        Ok(instructions) => Ok(Ok(instructions)),
        Err(Unreadable { path, reason }) => {
            crate::attempt::record_gate_failure(
                journal,
                clock,
                task_id,
                INSTRUCTIONS_STEP,
                &gate_message(&path, &reason),
            )?;
            Ok(Err(RunEnd::InstructionsUnreadable {
                id: task_id,
                path,
                reason,
            }))
        }
    }
}
