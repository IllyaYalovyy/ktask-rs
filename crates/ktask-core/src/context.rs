//! The prompt a provider is handed, assembled by the runner.
//!
//! VISION.md §6 makes context assembly the runner's job: "Task context is
//! assembled by the runner, never hand-injected per task: in v0.1 it is a static
//! context document from the private prompt library plus every ADR recorded so
//! far." [`assemble`] is that assembly, and it is a projection of what the caller
//! already holds rather than a reader of anything: the context document and the
//! template come from the private prompt library (VISION.md §11), the task body
//! comes from the queue, and the ADRs are the one input a caller may have read
//! from inside the repository — because a recorded decision is the one
//! operational document VISION.md §3 lets live there.
//!
//! The result belongs to the attempt it was assembled for.
//! [`crate::write_evidence`] files it as that attempt's `context.md`, which is
//! what lets a remediation, a failures screen or an operator with a shell read
//! back the exact words a session was given.
//!
//! Nothing here reads a clock, the environment or the filesystem, and that is the
//! whole of why the output is reproducible: a remediation is judged against the
//! attempt it replaces, and a prompt assembled from an instant or a directory
//! listing could not be. ADR-0075 records the decisions this file had to make,
//! including what the header can name as the report path when no project is in
//! scope to say where its state lives.

use std::path::PathBuf;

use crate::attempt::EVIDENCE_ROOT;
use crate::ids::{AttemptId, TaskId};
use crate::task::Task;

/// Where a prompt template wants the task body, as [`Task::body`].
const TASK_PLACEHOLDER: &str = "{{TASK}}";

/// The stand-in for a project's state directory in the path an agent is told to
/// write its report to.
///
/// It is a marker rather than a resolved directory because this signature carries
/// no [`crate::Project`], and a path resolved from `$XDG_STATE_HOME` and the
/// process's home directory would depend on something that is not one of the
/// prompt's inputs. A path beginning with a character no path may start with is
/// also the answer that fails loudly: an agent that tries to open it gets an
/// error, where a bare relative path would quietly write a run's report into the
/// repository, which is what VISION.md §3's invariant 6 forbids.
const STATE_DIR: &str = "<state_dir>";

/// The file an attempt's own report is written to, inside that attempt's evidence
/// directory.
///
/// Distinct from the `report.md` that [`crate::write_evidence`] generates from an
/// [`crate::AttemptRecord`]: one is what the runner concluded from the gates and
/// SHAs it watched, the other is what the agent said in its own words, and
/// VISION.md §3's invariant 4 keeps those two apart rather than letting the
/// second stand in for the first.
const AGENT_REPORT_FILE: &str = "agent-report.md";

/// The heading a template that never named [`TASK_PLACEHOLDER`] gets the task
/// under, so that a prompt is never handed over without the task it is about.
const TASK_HEADING: &str = "# The task";

/// The blank line that separates one part of a prompt from the next.
const SECTION_BREAK: &str = "\n\n";

/// The prompt one attempt is handed.
///
/// Four parts, in this order, joined by a blank line:
///
/// 1. a header naming the task number and how many tasks the queue holds, the
///    attempt within that task, and the path the report is to be written to;
/// 2. `context_doc`, the project's context document, as written;
/// 3. every ADR recorded so far, in the order the caller gave them, each under a
///    heading naming its position;
/// 4. `template`, with every `{{TASK}}` replaced by [`Task::body`].
///
/// # Determinism
///
/// Equal inputs are the same bytes: nothing here reads a clock, the environment
/// or the filesystem. That is what lets a later attempt be compared against the
/// one it replaced, and it is the determinism VISION.md §6 asks of assembly.
///
/// # What is passed through untouched
///
/// Each part keeps its own text; only trailing whitespace goes, so a context
/// document or an ADR ending in a newline does not open a gap where the next part
/// starts. Nothing inside a part is reflowed or escaped, and nothing is trimmed
/// at the front — indentation inside somebody's code fence is their text.
///
/// # A template that names no placeholder
///
/// The task body is appended under a `# The task` heading instead. A prompt with
/// no task in it is not a prompt, and this answers `String` rather than
/// [`crate::Result`] because a template that got here has already been read from
/// the prompt library, where a missing placeholder is a defect the runner can
/// only answer by handing something whole to the provider.
///
/// # What is deliberately absent
///
/// No instant and no absolute path: both are things a rerun of one task always
/// changes, and a prompt carrying them could not be compared against the attempt
/// it describes. Those are recorded beside the attempt instead, by
/// [`crate::write_evidence`].
#[must_use]
pub fn assemble(
    task: &Task,
    context_doc: &str,
    adrs: &[String],
    template: &str,
    attempt: AttemptId,
    total: usize,
) -> String {
    let parts = [
        header(task.id, attempt, total),
        context_doc.to_string(),
        decisions(adrs),
        filled(template, task.body.trim_end()),
    ];
    // Every part loses the whitespace it picked up at the end of the file it was
    // read out of, and a part that holds nothing contributes nothing — which is
    // the difference between a prompt with a section missing and a prompt that
    // stacked two separators where that section used to be.
    parts
        .into_iter()
        .map(|part| part.trim_end().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<String>>()
        .join(SECTION_BREAK)
}

/// The two lines a session reads first: which task, which attempt, and where its
/// report goes.
///
/// The queue length rides beside the task number because an agent that knows it
/// is 12 of 161 is working to a different sense of urgency from one that knows it
/// is 12 of 12, and the length is the one figure about the queue a prompt can
/// carry without a database in scope.
fn header(task: TaskId, attempt: AttemptId, total: usize) -> String {
    format!(
        "# ktask run — task {task} of {total}, attempt {attempt}\n\nReport: `{path}` — \
         `{STATE_DIR}` is this project's ktask state directory, outside the repository.",
        path = report_path(task, attempt).display(),
    )
}

/// Where one attempt's own report goes, below [`STATE_DIR`].
///
/// The two id levels are the ones [`crate::evidence_dir`] uses, so the file an
/// agent is told to write joins its attempt's directory rather than a third
/// layout nobody else reads.
fn report_path(task: TaskId, attempt: AttemptId) -> PathBuf {
    PathBuf::from(STATE_DIR)
        .join(EVIDENCE_ROOT)
        .join(task.to_string())
        .join(attempt.to_string())
        .join(AGENT_REPORT_FILE)
}

/// Every decision recorded so far, oldest first, under a heading that counts them.
///
/// The order is the caller's, unchanged: ADRs are recorded in the order they were
/// decided, and a later one supersedes an earlier one by number, so an assembly
/// that sorted them alphabetically would hand a session its history upside down.
/// Each one is numbered out of the total as well as counted in the heading, so a
/// session trimming its reading knows how much is left.
fn decisions(adrs: &[String]) -> String {
    let total = adrs.len();
    let heading = format!("# Decisions on record ({total})");
    if adrs.is_empty() {
        return format!("{heading}\n\nNone recorded yet.");
    }
    let blocks: Vec<String> = adrs
        .iter()
        .enumerate()
        .map(|(index, adr)| format!("## {} of {total}\n\n{}", index + 1, adr.trim_end()))
        .collect();
    format!("{heading}\n\n{}", blocks.join(SECTION_BREAK))
}

/// `template` with every [`TASK_PLACEHOLDER`] replaced by `body`.
///
/// Every occurrence, not the first: a template may name the task twice, and a
/// second placeholder left standing is a literal `{{TASK}}` in front of a
/// provider. A template that named none gets the task appended instead, because
/// the alternative is a prompt that asks for nothing.
fn filled(template: &str, body: &str) -> String {
    if template.contains(TASK_PLACEHOLDER) {
        return template.replace(TASK_PLACEHOLDER, body);
    }
    format!("{template}\n\n{TASK_HEADING}\n\n{body}")
}

#[cfg(test)]
mod tests {
    use super::{TASK_PLACEHOLDER, assemble};
    use crate::{
        AttemptId, AttemptRecord, Project, Task, TaskId, TaskStatus, Usage, evidence_dir,
        write_evidence,
    };
    use proptest::collection::vec;
    use proptest::prelude::*;
    use std::fs;
    use tempfile::tempdir;
    use time::macros::datetime;

    /// The queue position, the queue length and the attempt the fixtures use.
    const TASK_NUMBER: u32 = 12;
    const TOTAL: usize = 40;

    /// A context document in the shape `.ktask/context.md` is written in.
    const CONTEXT: &str = "# Project context\n\nRead VISION.md first.";

    /// A prompt template in the shape `.ktask/prompt.md` is written in.
    const TEMPLATE: &str = "# Prompt template\n\n## Your task\n\n{{TASK}}\n\n## Finishing\n\n\
                            Run ./scripts/quality.sh.";

    const ADR_ONE: &str = "# 0074. First decision\n\nRedaction runs inside the write path.\n";
    const ADR_TWO: &str = "# 0075. Second decision\n\nAssembly reads nothing.\n";

    /// The task as the queue holds it: its body is what the template is given.
    fn task() -> Task {
        Task {
            id: TaskId::new(TASK_NUMBER),
            status: TaskStatus::Pending,
            body: "## T080 Context assembly\n\n**Outcome:** the runner assembles the prompt."
                .to_owned(),
            outcome: "the prompt handed to a provider is assembled by the runner".to_owned(),
            done_when: "assembly is deterministic for the same inputs".to_owned(),
            verify: "cargo nextest run -p ktask-core -E 'test(/context/)'".to_owned(),
            refs: "VISION.md sections 6 and 11".to_owned(),
            gate: None,
            protocol: None,
        }
    }

    /// The same task holding `body` and nothing else changed.
    fn task_with(body: &str) -> Task {
        Task {
            body: body.to_owned(),
            ..task()
        }
    }

    /// The two decisions a project that has been running for a while holds.
    fn decisions() -> Vec<String> {
        vec![ADR_ONE.to_owned(), ADR_TWO.to_owned()]
    }

    /// The prompt the fixtures assemble, with both decisions on record.
    fn prompt() -> String {
        assemble(
            &task(),
            CONTEXT,
            &decisions(),
            TEMPLATE,
            AttemptId::new(2),
            TOTAL,
        )
    }

    #[test]
    fn the_prompt_is_the_header_the_context_the_decisions_then_the_template() {
        let expected = [
            "# ktask run — task 12 of 40, attempt 2",
            "",
            "Report: `<state_dir>/attempts/12/2/agent-report.md` — `<state_dir>` is this \
             project's ktask state directory, outside the repository.",
            "",
            "# Project context",
            "",
            "Read VISION.md first.",
            "",
            "# Decisions on record (2)",
            "",
            "## 1 of 2",
            "",
            "# 0074. First decision",
            "",
            "Redaction runs inside the write path.",
            "",
            "## 2 of 2",
            "",
            "# 0075. Second decision",
            "",
            "Assembly reads nothing.",
            "",
            "# Prompt template",
            "",
            "## Your task",
            "",
            "## T080 Context assembly",
            "",
            "**Outcome:** the runner assembles the prompt.",
            "",
            "## Finishing",
            "",
            "Run ./scripts/quality.sh.",
        ]
        .join("\n");
        assert_eq!(prompt(), expected);
    }

    #[test]
    fn the_report_path_names_the_attempt_it_was_assembled_for() {
        let prompt = assemble(&task(), CONTEXT, &[], TEMPLATE, AttemptId::new(11), TOTAL);
        assert!(
            prompt.contains("Report: `<state_dir>/attempts/12/11/agent-report.md`"),
            "the header has to name the attempt whose evidence the report joins: {prompt}"
        );
    }

    #[test]
    fn the_context_document_is_passed_through_word_for_word() {
        let odd = "# Context\n\n  indented code\n\ttab\n\ntrailing\n\n";
        let prompt = assemble(&task(), odd, &[], TEMPLATE, AttemptId::new(1), TOTAL);
        let starts = prompt
            .find("# Context")
            .expect("the context document is in the prompt");
        let decisions = prompt
            .find("# Decisions on record")
            .expect("the decisions are in the prompt");
        assert!(
            prompt[starts..].starts_with(odd.trim_end()),
            "the document keeps its own lines and its own indentation: {prompt}"
        );
        assert!(
            prompt.contains(&format!("{}\n\n# Decisions on record", odd.trim_end())),
            "a document's trailing newlines are its own, not a gap between sections: {prompt}"
        );
        assert!(
            starts < decisions,
            "the context document has to precede the decisions: {prompt}"
        );
    }

    #[test]
    fn the_task_body_lands_without_the_gap_it_was_read_in_with() {
        let gap = "**Outcome:** the runner assembles the prompt.\n\n";
        let prompt = assemble(
            &task_with(gap),
            CONTEXT,
            &[],
            "{{TASK}}\n\n## Finishing\n\nrun the gates",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with(
                "**Outcome:** the runner assembles the prompt.\n\n## Finishing\n\nrun \
                             the gates"
            ),
            "a body copied out of a plan file must not push the template's next heading away: \
             {prompt}"
        );
    }

    #[test]
    fn every_decision_recorded_reaches_the_prompt_in_its_own_order() {
        let three = vec![
            "ADR alpha".to_owned(),
            "ADR beta".to_owned(),
            "ADR gamma".to_owned(),
        ];
        let prompt = assemble(&task(), CONTEXT, &three, TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(prompt.contains("# Decisions on record (3)"));
        let mut place = 0;
        for (index, adr) in three.iter().enumerate() {
            let found = prompt[place..]
                .find(adr.as_str())
                .unwrap_or_else(|| panic!("decision {} is missing: {prompt}", index + 1));
            place += found + adr.len();
            assert!(
                prompt.contains(&format!("## {} of 3", index + 1)),
                "decision {} is not numbered where it stands: {prompt}",
                index + 1,
            );
        }
    }

    #[test]
    fn a_project_with_no_decisions_says_so_rather_than_leaving_the_slot_empty() {
        let prompt = assemble(&task(), CONTEXT, &[], TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(
            prompt.contains("# Decisions on record (0)\n\nNone recorded yet."),
            "an empty list is a fact about the project, not an omission: {prompt}"
        );
    }

    #[test]
    fn the_task_body_replaces_every_placeholder_the_template_named() {
        let template = "Do the task.\n\n{{TASK}}\n\nRestate it: {{TASK}}\nDone?";
        let prompt = assemble(&task(), CONTEXT, &[], template, AttemptId::new(1), TOTAL);
        assert!(
            !prompt.contains(TASK_PLACEHOLDER),
            "a placeholder that survives reaches the provider as a literal: {prompt}"
        );
        let body = task().body;
        assert_eq!(
            prompt.matches(body.as_str()).count(),
            2,
            "the template named the placeholder twice: {prompt}"
        );
    }

    #[test]
    fn a_template_that_never_named_the_placeholder_still_carries_the_task() {
        let prompt = assemble(
            &task(),
            CONTEXT,
            &[],
            "Work on whatever you find.",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with(
                "\n\n# The task\n\n## T080 Context assembly\n\n**Outcome:** the \
                            runner assembles the prompt."
            ),
            "a prompt with no task in it is not a prompt: {prompt}"
        );
    }

    #[test]
    fn an_empty_part_leaves_no_empty_section_behind() {
        let prompt = assemble(&task(), "", &[], TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(
            !prompt.contains("\n\n\n"),
            "an absent context document stacked two separators: {prompt}"
        );
    }

    #[test]
    fn the_prompt_ends_where_the_last_words_do() {
        let prompt = assemble(
            &task(),
            CONTEXT,
            &[],
            "{{TASK}}\n\nfin.\n\n\n",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with("\n\nfin.") && !prompt.ends_with(char::is_whitespace),
            "a prompt that ends in a gap invites the next section nobody wrote: {prompt}"
        );
    }

    #[test]
    fn the_same_inputs_assemble_the_same_bytes_twice() {
        let first = prompt();
        let second = assemble(
            &task(),
            CONTEXT,
            &decisions(),
            TEMPLATE,
            AttemptId::new(2),
            TOTAL,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn the_assembled_prompt_is_recorded_with_the_attempt() {
        let scratch = tempdir().expect("a scratch directory to hold a state directory");
        let state_dir = scratch.path().join("state").join("0123456789abcdef");
        fs::create_dir_all(&state_dir).expect("a state directory to file evidence below");
        let project = Project {
            root: scratch.path().join("repository"),
            id: "0123456789abcdef".to_owned(),
            state_dir,
        };
        let attempt = AttemptId::new(2);

        write_evidence(&project, &record(attempt), &prompt())
            .expect("an attempt's evidence accepts the prompt it was started with");
        let filed = evidence_dir(&project, TaskId::new(TASK_NUMBER), attempt).join("context.md");
        let filed =
            fs::read_to_string(&filed).unwrap_or_else(|why| panic!("{}: {why}", filed.display()));

        assert!(
            filed.contains("Report: `<state_dir>/attempts/12/2/agent-report.md`"),
            "the prompt an attempt was started with is the prompt its evidence holds: {filed}"
        );

        assert_eq!(
            filed,
            prompt(),
            "the prompt an attempt was started with is the prompt its evidence holds",
        );
    }

    /// The attempt the prompt is recorded against, with no gates and no usage.
    fn record(attempt: AttemptId) -> AttemptRecord {
        AttemptRecord {
            id: attempt,
            task: TaskId::new(TASK_NUMBER),
            started: datetime!(2026-09-21 09:14:03.5 UTC),
            ended: Some(datetime!(2026-09-21 09:41:47 UTC)),
            model_configured: Some("gpt-5.6-sol".to_owned()),
            model_reported: Some("gpt-5.6-sol-2026-09-01".to_owned()),
            session_id: Some("sess_01HQZK".to_owned()),
            exit_reason: "exited 0".to_owned(),
            gates: Vec::new(),
            usage: Some(Usage::unavailable()),
            base_sha: "0b78d3f1c2a4".to_owned(),
            candidate_sha: None,
        }
    }

    proptest! {
        /// Whatever the four parts hold, every one of them reaches the prompt, no
        /// placeholder is left standing, and two calls over equal inputs are the
        /// same bytes. The inputs are stripped of the placeholder so the question
        /// asked is about the assembly rather than about a body quoting it.
        #[test]
        fn every_part_reaches_the_prompt_and_the_answer_is_stable(
            body in any::<String>(),
            context in any::<String>(),
            adrs in vec(any::<String>(), 0..4),
            template in any::<String>(),
            attempt in 1u32..8,
            total in 0usize..500,
        ) {
            let body = body.replace(TASK_PLACEHOLDER, "");
            let context = context.replace(TASK_PLACEHOLDER, "");
            let template = template.replace(TASK_PLACEHOLDER, "");
            let adrs: Vec<String> = adrs
                .iter()
                .map(|adr| adr.replace(TASK_PLACEHOLDER, ""))
                .collect();
            let once = assemble(
                &task_with(&body),
                &context,
                &adrs,
                &template,
                AttemptId::new(attempt),
                total,
            );
            let twice = assemble(
                &task_with(&body),
                &context,
                &adrs,
                &template,
                AttemptId::new(attempt),
                total,
            );
            prop_assert_eq!(&once, &twice, "assembly is a function of its inputs");
            prop_assert!(once.contains(body.trim_end()), "the task body went missing");
            prop_assert!(!once.contains(TASK_PLACEHOLDER), "a placeholder reached the provider");
            prop_assert!(
                !once.ends_with(char::is_whitespace),
                "a prompt ending in whitespace joined parts that were not trimmed",
            );
            for adr in &adrs {
                prop_assert!(once.contains(adr.trim_end()), "a decision went missing");
            }
        }
    }
}
