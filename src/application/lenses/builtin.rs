//! The built-in review lenses: concurrency, security and simplicity. A user or a project
//! overrides one by saving a lens of the same name.
use super::{Lens, Scope};

pub const CONCURRENCY: &str = "concurrency";
pub const SECURITY: &str = "security";
pub const SIMPLICITY: &str = "simplicity";

const TASK_BLOCK: &str =
    "\nTask: {{title}}\nGoal: {{goal}}\nCriteria:\n{{criteria}}\nContext:\n{{context}}";

const SHARED_BAR: &str = "Review the Review scope once through this lens alone and leave every other concern to the other reviews. Report only a defect you can reach: name the file:line, the concrete trigger, the consequence and the smallest proportionate correction, never a redesign. Before answering, drop each finding you cannot trace from a real trigger to a real consequence in the current code. Mark a finding critical only when that consequence harms correctness, behavior, safety or data, performance, or long-term maintenance; record anything weaker as minor. Report all substantiated blockers together. Find no critical defect, and the review passes.";

const CONCURRENCY_TEXT: &str = "Specialist review through the concurrency lens: shared state and ordering. Look at what the change lets run at the same time: threads, async tasks, processes, signal handlers, database transactions and files or sockets that several actors touch. Check shared mutable state and its synchronization, lock scope and ordering (including locks held across blocking calls or I/O), check-then-act and read-modify-write gaps, operations that must be atomic but are not, lost wakeups, deadlock and starvation, cancellation, shutdown and cleanup of what was started, ordering assumptions about events, and retries that are not safe to repeat. For a race or deadlock, write out the interleaving step by step; a hazard you cannot interleave is not a finding.";
const SECURITY_TEXT: &str = "Specialist review through the security lens: trust boundaries. Look at where the change meets input or privileges it does not control: command-line, environment, files, network, database and model output. Check validation at those boundaries, injection into shells, SQL, paths and templates, path traversal and symlink following, authentication and authorization decisions, secrets in code, logs, argv or temporary files, file and process permissions, sandbox and isolation guarantees, unsafe deserialization, resource exhaustion by untrusted input, and new dependencies or privileges. For each finding, state who controls the input, what they can make happen and what it costs; a weakness no untrusted actor can reach is not a finding.";
const SIMPLICITY_TEXT: &str = "Specialist review through the simplicity lens: the least code that solves the task. Look for what makes the next change harder than it needs to be: logic the project already has written again, abstraction, configuration or generality nothing uses yet, indirection that hides a straight path, functions that do several things or mix levels of abstraction, dead or commented-out code, names that mislead, tests that cannot fail, and work beyond the task's scope. Compare with how the project already solves the same problem and prefer reusing it. A finding names the concrete maintenance cost, for example which future edit must now touch several places, and the smaller shape that would remove it; taste without a cost is not a finding.";

fn lens(name: &str, description: &str, text: &str) -> Lens {
    Lens {
        name: name.into(),
        description: description.into(),
        instructions: format!("{text} {SHARED_BAR}{TASK_BLOCK}"),
        scope: Scope::BuiltIn,
    }
}

/// The built-in lenses, in the order lists show them.
pub fn all() -> Vec<Lens> {
    vec![
        lens(
            CONCURRENCY,
            "Races, locking, atomicity, cancellation and cleanup, each with a concrete interleaving.",
            CONCURRENCY_TEXT,
        ),
        lens(
            SECURITY,
            "Trust boundaries, injection, permissions and secrets, each with a reachable abuse.",
            SECURITY_TEXT,
        ),
        lens(
            SIMPLICITY,
            "Duplication, speculative generality and needless complexity, each with its maintenance cost.",
            SIMPLICITY_TEXT,
        ),
    ]
}

pub fn named(name: &str) -> Option<Lens> {
    all()
        .into_iter()
        .find(|l| l.name.eq_ignore_ascii_case(name))
}
