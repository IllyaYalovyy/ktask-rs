//! Assembling the prompt handed to a provider for one attempt (VISION.md
//! §6, §11).
//!
//! "Task context is assembled by the runner, never hand-injected per task:
//! in v0.1 it is a static context document from the private prompt library
//! plus every ADR recorded so far" (VISION.md §6). The context document and
//! the prompt template both live outside the repository, per §11 ("Prompts
//! and templates live in a global, private prompt library; per-project
//! overrides also live outside the repo"); only the ADRs are read from
//! inside it, from `docs/adr/`. [`assemble`] takes all four as plain
//! strings rather than reading any of them itself, so nothing here ever
//! opens a file: the caller decides where each one came from, and this
//! function only ever concatenates what it is given.

use crate::{AttemptId, Task};
use std::fmt::Write as _;

/// Assembles the full prompt handed to a provider for one attempt at `task`.
///
/// The result is, in order:
///
/// 1. An orchestrator header naming the task's number (`task.id`, out of
///    `total`), the attempt number, and the path its report must be written
///    to: `.ktask/queue/report-<task.id>.md`, the convention every other
///    task report in this repository already follows.
/// 2. `context_doc` verbatim: the static context document from the private
///    prompt library.
/// 3. Every entry in `adrs`, in order: every ADR recorded so far.
/// 4. `template` with every occurrence of `{{TASK}}` replaced by
///    `task.body`.
///
/// Deterministic: the same arguments always produce the same string, since
/// nothing here reads the clock, the environment or the filesystem — it is
/// a pure function of its inputs. The returned string is exactly what
/// [`crate::write_evidence`] expects for its `context` argument, so a caller
/// records it with the attempt by passing it straight through.
#[must_use]
pub fn assemble(
    task: &Task,
    context_doc: &str,
    adrs: &[String],
    template: &str,
    attempt: AttemptId,
    total: usize,
) -> String {
    let mut out = String::new();

    let _ = write!(
        out,
        "[Orchestrator context] Task {} of {total} (attempt {attempt}). \
         Your task report should be written to \
         `.ktask/queue/report-{}.md` before exiting.\n\n",
        task.id, task.id,
    );

    push_trimmed(&mut out, context_doc);

    if !adrs.is_empty() {
        out.push_str("# Architecture Decision Records\n\n");
        for adr in adrs {
            push_trimmed(&mut out, adr);
        }
    }

    out.push_str(&template.replace("{{TASK}}", &task.body));

    out
}

/// Appends `text` to `out` with trailing whitespace trimmed and exactly one
/// blank line after it, so callers can join arbitrary sections without
/// worrying about how many newlines each one happens to end with.
fn push_trimmed(out: &mut String, text: &str) {
    out.push_str(text.trim_end());
    out.push_str("\n\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskId, TaskStatus};

    fn task(id: u32, body: &str) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: body.to_string(),
            outcome: "outcome".to_string(),
            done_when: "done".to_string(),
            verify: "verify".to_string(),
            refs: "refs".to_string(),
            protocol: None,
        }
    }

    #[test]
    fn assembly_is_deterministic_for_the_same_inputs() {
        let task = task(80, "Do the thing.\n\n**Outcome:** it is done.");
        let adrs = vec!["# ADR one".to_string(), "# ADR two".to_string()];

        let first = assemble(
            &task,
            "context doc",
            &adrs,
            "## Your task\n\n{{TASK}}\n",
            AttemptId::new(1),
            161,
        );
        let second = assemble(
            &task,
            "context doc",
            &adrs,
            "## Your task\n\n{{TASK}}\n",
            AttemptId::new(1),
            161,
        );

        assert_eq!(first, second);
    }

    #[test]
    fn the_header_names_the_task_number_total_attempt_and_report_path() {
        let task = task(80, "body");
        let result = assemble(&task, "doc", &[], "{{TASK}}", AttemptId::new(3), 161);

        assert!(
            result.starts_with(
                "[Orchestrator context] Task 80 of 161 (attempt 3). Your task report \
                 should be written to `.ktask/queue/report-80.md` before exiting.\n\n"
            ),
            "unexpected header in: {result:?}"
        );
    }

    #[test]
    fn the_context_document_is_included_verbatim() {
        let task = task(1, "body");
        let result = assemble(
            &task,
            "this is the static context document",
            &[],
            "{{TASK}}",
            AttemptId::new(1),
            1,
        );

        assert!(result.contains("this is the static context document"));
    }

    #[test]
    fn every_recorded_adr_is_included_in_order() {
        let task = task(1, "body");
        let adrs = vec![
            "# 0001. First decision".to_string(),
            "# 0002. Second decision".to_string(),
        ];
        let result = assemble(&task, "doc", &adrs, "{{TASK}}", AttemptId::new(1), 1);

        let first_pos = result
            .find("# 0001. First decision")
            .expect("first ADR present");
        let second_pos = result
            .find("# 0002. Second decision")
            .expect("second ADR present");
        assert!(
            first_pos < second_pos,
            "ADRs must appear in the order given: {result:?}"
        );
    }

    #[test]
    fn no_adr_section_is_added_when_no_adrs_have_been_recorded() {
        let task = task(1, "body");
        let result = assemble(&task, "doc", &[], "{{TASK}}", AttemptId::new(1), 1);

        assert!(!result.contains("Architecture Decision Records"));
    }

    #[test]
    fn the_template_placeholder_is_replaced_with_the_task_body() {
        let task = task(1, "## T001 Do the thing\n\n**Outcome:** it happens.");
        let result = assemble(
            &task,
            "doc",
            &[],
            "## Your task\n\n{{TASK}}\n\n## Finishing\n",
            AttemptId::new(1),
            1,
        );

        assert!(result.contains("## T001 Do the thing"));
        assert!(result.contains("**Outcome:** it happens."));
        assert!(!result.contains("{{TASK}}"));
        assert!(result.contains("## Finishing"));
    }

    #[test]
    fn every_occurrence_of_the_placeholder_is_replaced() {
        let task = task(1, "BODY");
        let result = assemble(
            &task,
            "doc",
            &[],
            "{{TASK}} and {{TASK}}",
            AttemptId::new(1),
            1,
        );

        assert_eq!(result.matches("{{TASK}}").count(), 0);
        assert_eq!(result.matches("BODY").count(), 2);
    }

    #[test]
    fn the_template_follows_the_context_document_and_adrs() {
        let task = task(1, "TASKBODY");
        let adrs = vec!["ADRTEXT".to_string()];
        let result = assemble(&task, "DOCTEXT", &adrs, "{{TASK}}", AttemptId::new(1), 1);

        let doc_pos = result.find("DOCTEXT").expect("context doc present");
        let adr_pos = result.find("ADRTEXT").expect("adr present");
        let task_pos = result.find("TASKBODY").expect("task body present");
        assert!(doc_pos < adr_pos, "context doc must precede ADRs");
        assert!(adr_pos < task_pos, "ADRs must precede the template");
    }
}
