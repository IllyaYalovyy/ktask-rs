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

#[cfg(doc)]
use crate::Error;
use crate::project::Project;
use crate::{AttemptId, Result, Task, paths};
use std::fmt::Write as _;
use std::path::Path;

/// The default task template written by [`ensure_defaults`] on first use.
///
/// Contains `{{TASK}}`, the placeholder [`assemble`] replaces with the
/// task's body.
const DEFAULT_TASK_TEMPLATE: &str = "\
# Prompt template — {{TASK}} is replaced with the task body

## Your task

{{TASK}}
";

/// The default context document written by [`ensure_defaults`] on first use.
const DEFAULT_CONTEXT_DOC: &str = "\
# Project context

No project-specific context has been configured yet. Replace this file (or
add a per-project override) with what an agent should know about this
codebase before starting a task.
";

/// The file name the task template is stored under, both in the global
/// prompt library and in a project's per-project override.
const TASK_TEMPLATE_FILE: &str = "task.md";

/// The file name the context document is stored under in the global prompt
/// library.
const CONTEXT_DOC_FILE: &str = "context.md";

/// The path, relative to a repository's root, that ADRs are recorded under.
const ADR_DIR: &str = "docs/adr";

/// The template file inside [`ADR_DIR`] that `collect_adrs` never treats as
/// a recorded decision.
const ADR_TEMPLATE_FILE: &str = "0000-template.md";

/// Writes the default task template and context document into
/// [`paths::prompt_library`], if they are not already there.
///
/// Idempotent: an existing file is left untouched, so a user's edits to the
/// defaults survive being "ensured" again on a later run.
///
/// # Errors
///
/// Returns [`Error::Io`] when the prompt library directory cannot be created
/// or a default file cannot be written, and [`Error::Config`] when the
/// prompt library's path cannot be resolved (see [`paths::prompt_library`]).
pub fn ensure_defaults() -> Result<()> {
    ensure_defaults_with(&|key| std::env::var(key).ok())
}

fn ensure_defaults_with(env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    let dir = paths::prompt_library_with(env)?;
    std::fs::create_dir_all(&dir)?;
    write_if_absent(&dir.join(TASK_TEMPLATE_FILE), DEFAULT_TASK_TEMPLATE)?;
    write_if_absent(&dir.join(CONTEXT_DOC_FILE), DEFAULT_CONTEXT_DOC)?;
    Ok(())
}

fn write_if_absent(path: &Path, contents: &str) -> Result<()> {
    if !path.is_file() {
        std::fs::write(path, contents)?;
    }
    Ok(())
}

/// Loads the task template to use for `project`'s attempts.
///
/// Prefers a per-project override at `<project.state_dir>/task.md`; falls
/// back to the global default in [`paths::prompt_library`], creating it via
/// [`ensure_defaults`] first if this is the first run. Neither file is ever
/// read from inside the repository (VISION.md §11): the override lives in
/// the project's private state directory, and the default lives in the
/// global prompt library.
///
/// # Errors
///
/// Returns [`Error::Io`] when a present file cannot be read, and
/// [`Error::Config`] when the global prompt library's path cannot be
/// resolved.
pub fn load_template(project: &Project) -> Result<String> {
    load_template_with(project, &|key| std::env::var(key).ok())
}

fn load_template_with(project: &Project, env: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let override_path = project.state_dir.join(TASK_TEMPLATE_FILE);
    if override_path.is_file() {
        return Ok(std::fs::read_to_string(override_path)?);
    }

    ensure_defaults_with(env)?;
    let default_path = paths::prompt_library_with(env)?.join(TASK_TEMPLATE_FILE);
    Ok(std::fs::read_to_string(default_path)?)
}

/// Loads the static context document for `project`'s attempts (VISION.md
/// §6: "a static context document from the private prompt library").
///
/// Prefers a per-project override at `<project.state_dir>/context.md`;
/// falls back to the global default in [`paths::prompt_library`], creating
/// it via [`ensure_defaults`] first if this is the first run. Neither file
/// is ever read from inside the repository (VISION.md §11), mirroring
/// [`load_template`]'s own precedence for the task template.
///
/// # Errors
///
/// Returns [`Error::Io`] when a present file cannot be read, and
/// [`Error::Config`] when the global prompt library's path cannot be
/// resolved.
pub fn load_context_doc(project: &Project) -> Result<String> {
    load_context_doc_with(project, &|key| std::env::var(key).ok())
}

fn load_context_doc_with(
    project: &Project,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<String> {
    let override_path = project.state_dir.join(CONTEXT_DOC_FILE);
    if override_path.is_file() {
        return Ok(std::fs::read_to_string(override_path)?);
    }

    ensure_defaults_with(env)?;
    let default_path = paths::prompt_library_with(env)?.join(CONTEXT_DOC_FILE);
    Ok(std::fs::read_to_string(default_path)?)
}

/// Reads every recorded architecture decision record out of a repository, in
/// filename order, for [`assemble`]'s `adrs` argument.
///
/// Recorded decisions live at `docs/adr/*.md` (VISION.md §3 invariant 8:
/// "an unresolved product or technical decision is a first-class pause
/// state, and its resolution is recorded as a decision record (ADR)
/// available to future tasks"). `docs/adr/0000-template.md` is the blank
/// template every ADR is copied from, not a decision, so it is skipped.
/// Files are read in filename order, which is also ADR number order given
/// the `NNNN-slug.md` naming convention every ADR in this repository
/// follows, so a later decision always appears after an earlier one.
///
/// # Errors
///
/// Returns [`Error::Io`] when `docs/adr` exists but cannot be listed, or
/// when one of its files cannot be read. A repository with no `docs/adr`
/// directory at all is not an error: `resolve` (VISION.md §6, §3 invariant
/// 8) only creates it once the first decision is recorded, so most task
/// contexts hit this case.
pub fn collect_adrs(repo_root: &Path) -> Result<Vec<String>> {
    let dir = repo_root.join(ADR_DIR);
    if !dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut paths: Vec<_> = std::fs::read_dir(&dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    paths.retain(|path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("md")
            && path.file_name().and_then(|name| name.to_str()) != Some(ADR_TEMPLATE_FILE)
    });
    paths.sort();

    paths
        .into_iter()
        .map(|path| Ok(std::fs::read_to_string(path)?))
        .collect()
}

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

    fn env_with(dir: &tempfile::TempDir) -> impl Fn(&str) -> Option<String> {
        let config_home = dir.path().to_string_lossy().to_string();
        move |key| (key == "XDG_CONFIG_HOME").then(|| config_home.clone())
    }

    fn project_with_state_dir(state_dir: std::path::PathBuf) -> Project {
        Project {
            root: std::path::PathBuf::from("/repo"),
            id: "abc123".to_string(),
            state_dir,
        }
    }

    #[test]
    fn ensure_defaults_creates_the_prompt_library_on_first_use() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);

        ensure_defaults_with(&env).expect("ensure defaults");

        let dir = paths::prompt_library_with(&env).expect("prompt library path");
        assert!(dir.join(TASK_TEMPLATE_FILE).is_file());
        assert!(dir.join(CONTEXT_DOC_FILE).is_file());
    }

    #[test]
    fn ensure_defaults_writes_a_task_template_containing_the_placeholder() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);

        ensure_defaults_with(&env).expect("ensure defaults");

        let dir = paths::prompt_library_with(&env).expect("prompt library path");
        let template = std::fs::read_to_string(dir.join(TASK_TEMPLATE_FILE)).expect("read task.md");
        assert!(template.contains("{{TASK}}"));
    }

    #[test]
    fn ensure_defaults_does_not_overwrite_an_existing_file() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let dir = paths::prompt_library_with(&env).expect("prompt library path");
        std::fs::create_dir_all(&dir).expect("create prompt library");
        std::fs::write(dir.join(TASK_TEMPLATE_FILE), "custom {{TASK}} template")
            .expect("write custom template");

        ensure_defaults_with(&env).expect("ensure defaults");

        let template = std::fs::read_to_string(dir.join(TASK_TEMPLATE_FILE)).expect("read task.md");
        assert_eq!(template, "custom {{TASK}} template");
    }

    #[test]
    fn load_template_creates_the_defaults_on_a_first_run_instead_of_failing() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_with_state_dir(state_dir.path().to_path_buf());

        let template = load_template_with(&project, &env).expect("load template");

        assert!(template.contains("{{TASK}}"));
        let dir = paths::prompt_library_with(&env).expect("prompt library path");
        assert!(dir.join(TASK_TEMPLATE_FILE).is_file());
    }

    #[test]
    fn load_template_prefers_the_per_project_override_over_the_global_default() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        std::fs::write(
            state_dir.path().join(TASK_TEMPLATE_FILE),
            "override {{TASK}} template",
        )
        .expect("write override");
        let project = project_with_state_dir(state_dir.path().to_path_buf());

        let template = load_template_with(&project, &env).expect("load template");

        assert_eq!(template, "override {{TASK}} template");
    }

    #[test]
    fn load_template_never_reads_from_inside_the_repository() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        // `project.root` points at a directory that does not exist at all;
        // if `load_template` ever tried to read a template from inside it,
        // this would fail.
        let project = Project {
            root: std::path::PathBuf::from("/nonexistent/repo/root"),
            id: "abc123".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };

        let template = load_template_with(&project, &env).expect("load template");

        assert!(template.contains("{{TASK}}"));
    }

    #[test]
    fn load_context_doc_creates_the_defaults_on_a_first_run_instead_of_failing() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_with_state_dir(state_dir.path().to_path_buf());

        let doc = load_context_doc_with(&project, &env).expect("load context doc");

        assert!(doc.contains("No project-specific context"));
        let dir = paths::prompt_library_with(&env).expect("prompt library path");
        assert!(dir.join(CONTEXT_DOC_FILE).is_file());
    }

    #[test]
    fn load_context_doc_prefers_the_per_project_override_over_the_global_default() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        std::fs::write(
            state_dir.path().join(CONTEXT_DOC_FILE),
            "this project's own context",
        )
        .expect("write override");
        let project = project_with_state_dir(state_dir.path().to_path_buf());

        let doc = load_context_doc_with(&project, &env).expect("load context doc");

        assert_eq!(doc, "this project's own context");
    }

    #[test]
    fn load_context_doc_never_reads_from_inside_the_repository() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = Project {
            root: std::path::PathBuf::from("/nonexistent/repo/root"),
            id: "abc123".to_string(),
            state_dir: state_dir.path().to_path_buf(),
        };

        let doc = load_context_doc_with(&project, &env).expect("load context doc");

        assert!(doc.contains("No project-specific context"));
    }

    #[test]
    fn collect_adrs_yields_an_empty_list_when_docs_adr_does_not_exist() {
        let repo_root = tempfile::tempdir().expect("tempdir");

        let adrs = collect_adrs(repo_root.path()).expect("collect adrs");

        assert_eq!(adrs, Vec::<String>::new());
    }

    #[test]
    fn collect_adrs_reads_files_in_filename_order_skipping_the_template() {
        let repo_root = tempfile::tempdir().expect("tempdir");
        let adr_dir = repo_root.path().join("docs").join("adr");
        std::fs::create_dir_all(&adr_dir).expect("create docs/adr");
        std::fs::write(adr_dir.join(ADR_TEMPLATE_FILE), "TEMPLATE").expect("write template");
        std::fs::write(adr_dir.join("0002-second-decision.md"), "SECOND").expect("write second");
        std::fs::write(adr_dir.join("0001-first-decision.md"), "FIRST").expect("write first");
        std::fs::write(adr_dir.join("notes.txt"), "NOT AN ADR").expect("write non-md file");

        let adrs = collect_adrs(repo_root.path()).expect("collect adrs");

        assert_eq!(adrs, vec!["FIRST".to_string(), "SECOND".to_string()]);
    }

    #[test]
    fn an_adr_written_by_resolve_reaches_the_next_tasks_assembled_context_end_to_end() {
        let config_home = tempfile::tempdir().expect("tempdir");
        let env = env_with(&config_home);
        let repo_root = tempfile::tempdir().expect("tempdir");
        let state_dir = tempfile::tempdir().expect("state dir");
        let project = project_with_state_dir(state_dir.path().to_path_buf());

        // Simulates `ktask-rs resolve` recording a human's answer to a
        // `waiting_input` pause as an ADR (VISION.md §3 invariant 8, §6),
        // before the next task in the queue is assembled.
        let adr_dir = repo_root.path().join("docs").join("adr");
        std::fs::create_dir_all(&adr_dir).expect("create docs/adr");
        std::fs::write(
            adr_dir.join(ADR_TEMPLATE_FILE),
            "# NNNN. Short title\n\nblank template, not a decision",
        )
        .expect("write template");
        std::fs::write(
            adr_dir.join("0001-use-postgres.md"),
            "# 0001. Use Postgres for the journal\n\nDecided by the human via `resolve`.",
        )
        .expect("write adr");

        ensure_defaults_with(&env).expect("ensure defaults");
        let template = load_template_with(&project, &env).expect("load template");
        let library_dir = paths::prompt_library_with(&env).expect("prompt library path");
        let context_doc =
            std::fs::read_to_string(library_dir.join(CONTEXT_DOC_FILE)).expect("read context.md");
        let adrs = collect_adrs(repo_root.path()).expect("collect adrs");

        let next_task = task(81, "Do the next thing.");
        let prompt = assemble(
            &next_task,
            &context_doc,
            &adrs,
            &template,
            AttemptId::new(1),
            161,
        );

        assert!(
            prompt.contains("Use Postgres for the journal"),
            "the recorded decision must reach the next task's context: {prompt:?}"
        );
        assert!(
            !prompt.contains("blank template, not a decision"),
            "the ADR template itself must not be injected as a decision: {prompt:?}"
        );
    }
}
