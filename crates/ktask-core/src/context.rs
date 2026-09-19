//! Prompt context assembly for AI agents.
//!
//! Assembles the complete prompt by combining task context, architectural records,
//! and the execution template. The assembly is deterministic for the same inputs.

use crate::{AttemptId, Project, Task};
use std::fmt::Write;
use std::path::Path;

/// Assemble a complete prompt for the AI agent.
///
/// Produces a prompt consisting of:
/// 1. An orchestrator header naming the task number, attempt, and report path
/// 2. The context document
/// 3. Every ADR recorded so far
/// 4. The template with `{{TASK}}` replaced by the task body
///
/// The assembly is deterministic for the same inputs and contains no runtime
/// state other than the task number, attempt ID, and total attempts.
///
/// # Arguments
///
/// * `task` - The task to be executed
/// * `context_doc` - The context document (e.g., system context, project overview)
/// * `adrs` - List of architecture decision records (ADRs) recorded so far
/// * `template` - The prompt template with `{{TASK}}` placeholder
/// * `attempt` - The current attempt number for this task
/// * `total` - Total number of attempts expected
///
/// # Returns
///
/// The assembled prompt as a string
#[must_use]
pub fn assemble(
    task: &Task,
    context_doc: &str,
    adrs: &[String],
    template: &str,
    attempt: AttemptId,
    total: usize,
) -> String {
    let task_id = task.id.get();
    let attempt_num = attempt.get();

    let mut result = String::new();

    // 1. Orchestrator header
    let _ = write!(
        result,
        "[Orchestrator context] Task {task_id} of {total} (attempt {attempt_num}/{total}). You are working in the project at $PWD. Your task report should be written to .ktask/queue/report-{task_id}.md before exiting.\n\n"
    );

    // 2. Context document
    if !context_doc.is_empty() {
        result.push_str(context_doc);
        result.push('\n');
        result.push('\n');
    }

    // 3. Every ADR recorded so far
    if !adrs.is_empty() {
        result.push_str("# Architecture Decision Records\n\n");
        for (i, adr) in adrs.iter().enumerate() {
            if i > 0 {
                result.push_str("\n---\n\n");
            }
            result.push_str(adr);
        }
        result.push('\n');
        result.push('\n');
    }

    // 4. Template with {{TASK}} replaced by task body
    let prompt = template.replace("{{TASK}}", &task.body);
    result.push_str(&prompt);

    result
}

/// Ensure default prompt templates exist, creating them if necessary.
///
/// Creates default templates in the global prompt library directory:
/// - `task.md`: A default task template containing `{{TASK}}` placeholder
/// - `context.md`: A default context document
///
/// The prompt library is located at `$XDG_CONFIG_HOME/ktask-rs/prompts/`
/// or `$HOME/.config/ktask-rs/prompts/` if `XDG_CONFIG_HOME` is not set.
///
/// This operation is idempotent: if templates already exist, they are not overwritten.
///
/// # Errors
///
/// Returns an error if the prompt library directory cannot be created or
/// if environment variables are misconfigured.
pub fn ensure_defaults() -> crate::Result<()> {
    ensure_defaults_with_prompt_lib(&crate::prompt_library)
}

fn ensure_defaults_with_prompt_lib(
    get_prompt_lib: &dyn Fn() -> crate::Result<std::path::PathBuf>,
) -> crate::Result<()> {
    let prompt_lib = get_prompt_lib()?;

    // Create the prompt library directory if it doesn't exist
    std::fs::create_dir_all(&prompt_lib)?;

    let task_template_path = prompt_lib.join("task.md");
    let context_template_path = prompt_lib.join("context.md");

    // Create default task.md if it doesn't exist
    if !task_template_path.exists() {
        let default_task_template = "# Task Template\n\n{{TASK}}\n";
        std::fs::write(&task_template_path, default_task_template)?;
    }

    // Create default context.md if it doesn't exist
    if !context_template_path.exists() {
        let default_context_template = "# Project Context\n\nAdd project context here.\n";
        std::fs::write(&context_template_path, default_context_template)?;
    }

    Ok(())
}

/// Load the task template, preferring per-project override over global default.
///
/// Searches for the template in this order:
/// 1. Per-project override at `$project.state_dir/prompts/task.md`
/// 2. Global default at `$XDG_CONFIG_HOME/ktask-rs/prompts/task.md`
///
/// If no template exists, `ensure_defaults()` is called to create the global default.
///
/// Templates are never read from inside the repository; they always come from
/// the XDG config directory or per-project state directory.
///
/// # Errors
///
/// Returns an error if the template cannot be read or if environment variables
/// are misconfigured.
pub fn load_template(project: &Project) -> crate::Result<String> {
    load_template_with(project, &crate::prompt_library)
}

fn load_template_with(
    project: &Project,
    get_prompt_lib: &dyn Fn() -> crate::Result<std::path::PathBuf>,
) -> crate::Result<String> {
    // Try per-project override first
    let project_override = project.state_dir.join("prompts").join("task.md");
    if project_override.exists() {
        return std::fs::read_to_string(&project_override).map_err(Into::into);
    }

    // Ensure global defaults exist
    ensure_defaults_with_prompt_lib(get_prompt_lib)?;

    // Load global default
    let prompt_lib = get_prompt_lib()?;
    let global_template = prompt_lib.join("task.md");

    std::fs::read_to_string(&global_template).map_err(Into::into)
}

/// Collect all architecture decision records (ADRs) from the repository.
///
/// Reads all ADR files from `docs/adr/` in the repository root, skipping the
/// template file (`0000-template.md`). Files are read in alphabetical order.
///
/// If the `docs/adr` directory does not exist, returns an empty list without error.
///
/// # Arguments
///
/// * `repo_root` - The root path of the repository
///
/// # Returns
///
/// A vector of ADR file contents as strings.
///
/// # Errors
///
/// Returns an error if reading the ADR directory or any ADR files fails.
pub fn collect_adrs(repo_root: &Path) -> crate::Result<Vec<String>> {
    let adr_dir = repo_root.join("docs/adr");

    // If the directory doesn't exist, return empty list
    if !adr_dir.exists() {
        return Ok(vec![]);
    }

    let mut adr_files = Vec::new();

    // Read all .md files from the adr directory
    for entry in std::fs::read_dir(&adr_dir)? {
        let entry = entry?;
        let path = entry.path();

        // Only process markdown files
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();

            // Skip the template file
            if file_name == "0000-template.md" {
                continue;
            }

            adr_files.push(path);
        }
    }

    // Sort files by name to ensure consistent order
    adr_files.sort();

    // Read each file and collect the content
    let mut adrs = Vec::new();
    for path in adr_files {
        let content = std::fs::read_to_string(&path)?;
        adrs.push(content);
    }

    Ok(adrs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{TaskId, TaskStatus};

    fn make_task(id: u32) -> Task {
        Task {
            id: TaskId::new(id),
            status: TaskStatus::Pending,
            body: "Test task\nwith multiple lines".to_string(),
            outcome: "Outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        }
    }

    #[test]
    fn context_assemble_with_minimal_inputs() {
        let task = make_task(1);
        let context_doc = "";
        let adrs = vec![];
        let template = "Task: {{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        // Should contain orchestrator header
        assert!(result.contains("Task 1 of 1 (attempt 1/1)"));
        assert!(result.contains(".ktask/queue/report-1.md"));
        // Should contain task body with replacement
        assert!(result.contains("Task: Test task"));
        assert!(result.contains("with multiple lines"));
    }

    #[test]
    fn context_assemble_with_context_doc() {
        let task = make_task(5);
        let context_doc = "# Project Context\n\nThis is the context.";
        let adrs = vec![];
        let template = "Template: {{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 3;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains("Task 5 of 3 (attempt 1/3)"));
        assert!(result.contains("# Project Context"));
        assert!(result.contains("This is the context."));
        assert!(result.contains("Template:"));
    }

    #[test]
    fn context_assemble_with_adrs() {
        let task = make_task(2);
        let context_doc = "";
        let adrs = vec![
            "ADR 1: Use Rust for implementation".to_string(),
            "ADR 2: Store state in SQLite".to_string(),
        ];
        let template = "Work on: {{TASK}}\n";
        let attempt = AttemptId::new(2);
        let total = 5;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains("Task 2 of 5 (attempt 2/5)"));
        assert!(result.contains("# Architecture Decision Records"));
        assert!(result.contains("ADR 1: Use Rust for implementation"));
        assert!(result.contains("ADR 2: Store state in SQLite"));
        assert!(result.contains("---"));
    }

    #[test]
    fn context_assemble_replaces_task_placeholder() {
        let task = Task {
            id: TaskId::new(10),
            status: TaskStatus::Pending,
            body: "Implementation task\nwith details".to_string(),
            outcome: "Outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };
        let context_doc = "";
        let adrs = vec![];
        let template = "Please work on:\n{{TASK}}\n\nThanks!";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        // {{TASK}} should be replaced, not present in output
        assert!(!result.contains("{{TASK}}"));
        // Task body should be in the result
        assert!(result.contains("Implementation task"));
        assert!(result.contains("with details"));
        assert!(result.contains("Thanks!"));
    }

    #[test]
    fn context_assemble_deterministic_same_inputs() {
        let task = make_task(3);
        let context_doc = "Context";
        let adrs = vec!["ADR 1".to_string(), "ADR 2".to_string()];
        let template = "Template: {{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 3;

        let result1 = assemble(&task, context_doc, &adrs, template, attempt, total);
        let result2 = assemble(&task, context_doc, &adrs, template, attempt, total);

        // Results should be identical for the same inputs
        assert_eq!(result1, result2);
    }

    #[test]
    fn context_assemble_report_path_format() {
        let task = make_task(42);
        let context_doc = "";
        let adrs = vec![];
        let template = "{{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains(".ktask/queue/report-42.md"));
    }

    #[test]
    fn context_assemble_attempt_numbering() {
        let task = make_task(7);
        let context_doc = "";
        let adrs = vec![];
        let template = "{{TASK}}\n";

        let result1 = assemble(&task, context_doc, &adrs, template, AttemptId::new(1), 3);
        let result2 = assemble(&task, context_doc, &adrs, template, AttemptId::new(2), 3);
        let result3 = assemble(&task, context_doc, &adrs, template, AttemptId::new(3), 3);

        assert!(result1.contains("attempt 1/3"));
        assert!(result2.contains("attempt 2/3"));
        assert!(result3.contains("attempt 3/3"));
    }

    #[test]
    fn context_assemble_single_adr() {
        let task = make_task(1);
        let context_doc = "";
        let adrs = vec!["ADR 1: Single decision".to_string()];
        let template = "{{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains("# Architecture Decision Records"));
        assert!(result.contains("ADR 1: Single decision"));
        // Should not have extra separators with only one ADR
        let adr_count = result.matches("---").count();
        assert_eq!(adr_count, 0); // Single ADR should have no separators
    }

    #[test]
    fn context_assemble_many_adrs() {
        let task = make_task(1);
        let context_doc = "";
        let adrs = (1..=5)
            .map(|i| format!("ADR {i}: Decision {i}"))
            .collect::<Vec<_>>();
        let template = "{{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains("# Architecture Decision Records"));
        for i in 1..=5 {
            assert!(result.contains(&format!("ADR {i}")));
        }
        // Should have 4 separators between 5 ADRs
        let adr_count = result.matches("---").count();
        assert_eq!(adr_count, 4);
    }

    #[test]
    fn context_assemble_empty_template() {
        let task = make_task(1);
        let context_doc = "Context";
        let adrs = vec![];
        let template = "";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        assert!(result.contains("Task 1 of 1"));
        assert!(result.contains("Context"));
        // Empty template should still work
        assert!(!result.contains("{{TASK}}"));
    }

    #[test]
    fn context_assemble_template_with_multiple_placeholders() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body: "The task".to_string(),
            outcome: "Outcome".to_string(),
            done_when: "Done".to_string(),
            verify: "Verify".to_string(),
            refs: "Refs".to_string(),
        };
        let context_doc = "";
        let adrs = vec![];
        // Template with multiple {{TASK}} placeholders
        let template = "First: {{TASK}}\nSecond: {{TASK}}\n";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        // All placeholders should be replaced
        assert!(!result.contains("{{TASK}}"));
        assert!(result.contains("First: The task"));
        assert!(result.contains("Second: The task"));
    }

    #[test]
    fn context_assemble_complex_task_body() {
        let task = Task {
            id: TaskId::new(1),
            status: TaskStatus::Pending,
            body:
                "## Implement feature\n\n**Requirements:**\n- Req 1\n- Req 2\n\n```rust\ncode\n```"
                    .to_string(),
            outcome: "Feature done".to_string(),
            done_when: "Tests pass".to_string(),
            verify: "cargo test".to_string(),
            refs: "Docs".to_string(),
        };
        let context_doc = "System context";
        let adrs = vec!["ADR 1".to_string()];
        let template = "Work on:\n{{TASK}}\n\nDone!";
        let attempt = AttemptId::new(1);
        let total = 1;

        let result = assemble(&task, context_doc, &adrs, template, attempt, total);

        // Should preserve complex formatting
        assert!(result.contains("## Implement feature"));
        assert!(result.contains("**Requirements:**"));
        assert!(result.contains("```rust"));
        assert!(result.contains("code"));
        assert!(result.contains("```"));
        assert!(result.contains("System context"));
        assert!(result.contains("ADR 1"));
    }

    #[test]
    fn context_ensure_defaults_creates_task_template() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        let result = ensure_defaults_with_prompt_lib(&get_prompt_lib);
        assert!(result.is_ok());

        let task_template = prompt_lib.join("task.md");
        assert!(task_template.exists());

        let content = std::fs::read_to_string(&task_template).unwrap();
        assert!(content.contains("{{TASK}}"));
    }

    #[test]
    fn context_ensure_defaults_creates_context_template() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        let result = ensure_defaults_with_prompt_lib(&get_prompt_lib);
        assert!(result.is_ok());

        let context_template = prompt_lib.join("context.md");
        assert!(context_template.exists());

        let content = std::fs::read_to_string(&context_template).unwrap();
        assert!(!content.is_empty());
    }

    #[test]
    fn context_ensure_defaults_is_idempotent() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        // Call ensure_defaults twice
        let result1 = ensure_defaults_with_prompt_lib(&get_prompt_lib);
        assert!(result1.is_ok());

        let task_template = prompt_lib.join("task.md");
        let content1 = std::fs::read_to_string(&task_template).unwrap();

        let result2 = ensure_defaults_with_prompt_lib(&get_prompt_lib);
        assert!(result2.is_ok());

        let content2 = std::fs::read_to_string(&task_template).unwrap();

        // Should not have changed
        assert_eq!(content1, content2);
    }

    #[test]
    fn context_load_template_uses_global_default() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let state_dir = temp.path().join("state");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        ensure_defaults_with_prompt_lib(&get_prompt_lib).unwrap();

        // Create a minimal project with state_dir
        let project = Project {
            root: temp.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let result = load_template_with(&project, &get_prompt_lib);
        assert!(result.is_ok());

        let template = result.unwrap();
        assert!(template.contains("{{TASK}}"));
    }

    #[test]
    fn context_load_template_prefers_project_override() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let state_dir = temp.path().join("state");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        ensure_defaults_with_prompt_lib(&get_prompt_lib).unwrap();

        // Create project state directory with override
        let prompts_dir = state_dir.join("prompts");
        std::fs::create_dir_all(&prompts_dir).unwrap();
        let override_template = "# Project-Specific Template\n{{TASK}}\n";
        std::fs::write(prompts_dir.join("task.md"), override_template).unwrap();

        let project = Project {
            root: temp.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let result = load_template_with(&project, &get_prompt_lib);
        assert!(result.is_ok());

        let template = result.unwrap();
        assert!(template.contains("# Project-Specific Template"));
        assert!(!template.contains("# Task Template"));
    }

    #[test]
    fn context_load_template_creates_defaults_on_demand() {
        let temp = tempfile::TempDir::new().unwrap();
        let config_dir = temp.path().join("config");
        let state_dir = temp.path().join("state");
        let prompt_lib = config_dir.join("ktask-rs/prompts");

        let get_prompt_lib = || Ok::<_, crate::Error>(prompt_lib.clone());

        // Don't call ensure_defaults; let load_template handle it
        assert!(!prompt_lib.join("task.md").exists());

        let project = Project {
            root: temp.path().to_path_buf(),
            id: "test-project".to_string(),
            state_dir: state_dir.clone(),
        };

        let result = load_template_with(&project, &get_prompt_lib);
        assert!(result.is_ok());

        // Defaults should have been created
        assert!(prompt_lib.join("task.md").exists());
    }

    #[test]
    fn context_collect_adrs_returns_empty_for_missing_directory() {
        let temp = tempfile::TempDir::new().unwrap();

        let result = collect_adrs(temp.path());
        assert!(result.is_ok());

        let empty: Vec<String> = vec![];
        assert_eq!(result.unwrap(), empty);
    }

    #[test]
    fn context_collect_adrs_skips_template_file() {
        let temp = tempfile::TempDir::new().unwrap();
        let adr_dir = temp.path().join("docs/adr");
        std::fs::create_dir_all(&adr_dir).unwrap();

        // Create template and other files
        std::fs::write(adr_dir.join("0000-template.md"), "# Template\n").unwrap();
        std::fs::write(adr_dir.join("0001-decision.md"), "# Decision 1\n").unwrap();

        let result = collect_adrs(temp.path());
        assert!(result.is_ok());
        let adrs = result.unwrap();

        // Should only have one ADR (not the template)
        assert_eq!(adrs.len(), 1);
        assert!(adrs[0].contains("# Decision 1"));
        assert!(!adrs[0].contains("# Template"));
    }

    #[test]
    fn context_collect_adrs_reads_in_alphabetical_order() {
        let temp = tempfile::TempDir::new().unwrap();
        let adr_dir = temp.path().join("docs/adr");
        std::fs::create_dir_all(&adr_dir).unwrap();

        // Create ADR files in non-alphabetical order
        std::fs::write(adr_dir.join("0003-third.md"), "# Third\n").unwrap();
        std::fs::write(adr_dir.join("0001-first.md"), "# First\n").unwrap();
        std::fs::write(adr_dir.join("0002-second.md"), "# Second\n").unwrap();

        let result = collect_adrs(temp.path());
        assert!(result.is_ok());
        let adrs = result.unwrap();

        assert_eq!(adrs.len(), 3);
        assert!(adrs[0].contains("# First"));
        assert!(adrs[1].contains("# Second"));
        assert!(adrs[2].contains("# Third"));
    }

    #[test]
    fn context_collect_adrs_ignores_non_markdown_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let adr_dir = temp.path().join("docs/adr");
        std::fs::create_dir_all(&adr_dir).unwrap();

        // Create mixed file types
        std::fs::write(adr_dir.join("0001-decision.md"), "# Decision\n").unwrap();
        std::fs::write(adr_dir.join("0002-note.txt"), "# Note\n").unwrap();
        std::fs::write(adr_dir.join("0003-readme"), "# Readme\n").unwrap();

        let result = collect_adrs(temp.path());
        assert!(result.is_ok());
        let adrs = result.unwrap();

        // Should only have one ADR (only .md files)
        assert_eq!(adrs.len(), 1);
        assert!(adrs[0].contains("# Decision"));
    }

    #[test]
    fn context_collect_adrs_multiple_files() {
        let temp = tempfile::TempDir::new().unwrap();
        let adr_dir = temp.path().join("docs/adr");
        std::fs::create_dir_all(&adr_dir).unwrap();

        // Create multiple ADR files
        std::fs::write(
            adr_dir.join("0001-architecture.md"),
            "# ADR 1\n\nUse Rust for implementation\n",
        )
        .unwrap();
        std::fs::write(
            adr_dir.join("0002-storage.md"),
            "# ADR 2\n\nUse SQLite for state\n",
        )
        .unwrap();
        std::fs::write(
            adr_dir.join("0000-template.md"),
            "# Template\n\nThis is the template\n",
        )
        .unwrap();

        let result = collect_adrs(temp.path());
        assert!(result.is_ok());
        let adrs = result.unwrap();

        // Should have two ADRs (template is skipped)
        assert_eq!(adrs.len(), 2);
        assert!(adrs[0].contains("ADR 1"));
        assert!(adrs[0].contains("Rust"));
        assert!(adrs[1].contains("ADR 2"));
        assert!(adrs[1].contains("SQLite"));
    }
}
