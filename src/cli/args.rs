use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// The guide printed after the command list by `--help`; `-h` keeps the short form.
const GUIDE: &str = include_str!("guide.txt");

/// Supervises AI coding agents on a Git project. Without a command, opens the TUI.
///
/// ktask RS keeps a queue of tasks for one Git repository. Each task is a Goal and
/// acceptance criteria. `run` gives the current task to an AI agent (Codex, Claude Code or
/// OpenCode). The supervisor then runs the project's checks, has independent read-only
/// models review and validate the change, repairs findings for up to ten rounds, and
/// commits the passing change and optionally delivers it by direct push or pull request.
/// Everything is recorded in SQLite: tasks, revisions,
/// attempts, events, token usage and cost. The TUI and this CLI share the same commands
/// and rules.
#[derive(Parser)]
#[command(version, after_long_help = GUIDE, color = clap::ColorChoice::Never)]
pub struct Cli {
    /// Print one JSON line on stdout ({"schema_version":1,"ok":true,"data":...}); errors go to
    /// stderr as JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Project directory (a Git repository); defaults to cwd. Never changes the instance's
    /// data root.
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,
    /// Unique name when add/import or the TUI registers a new project; checked otherwise.
    #[arg(long, global = true)]
    pub project_name: Option<String>,
    /// Who makes the change, for attribution. AI callers must pass ai with --actor NAME.
    #[arg(long, global = true, value_enum, default_value = "human")]
    pub actor_kind: ActorKind,
    /// Name recorded with changes, such as claude-code; defaults to owner.
    #[arg(long, global = true)]
    pub actor: Option<String>,
    /// Attempt capability for the restricted worker callback.
    #[arg(long, global = true, hide = true)]
    pub worker_token: Option<String>,
    /// Print this executable's instance-specific paths without opening storage.
    #[arg(long, conflicts_with = "demo")]
    pub paths: bool,
    /// Open the isolated simulator; never reads or writes the real database.
    #[arg(long)]
    pub demo: bool,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum CacheAction {
    /// Retire obsolete generations in owned caches and reclaim abandoned scratch. Previews
    /// unless --apply; an active build, open file or fresh output is always kept.
    Clean {
        /// Delete now instead of previewing.
        #[arg(long)]
        apply: bool,
    },
    /// Explicit opt-in: take ownership of an existing Cargo target directory (default:
    /// the project's target). Nothing unowned is ever cleaned without this.
    Adopt {
        /// A Cargo target directory; defaults to the project's own `target`.
        path: Option<PathBuf>,
    },
    /// Show or set the finite cache bounds.
    Limits {
        /// Allocated bytes one owned cache may hold.
        #[arg(long)]
        max_bytes: Option<u64>,
        /// Files and directories one owned cache may hold.
        #[arg(long)]
        max_files: Option<u64>,
        /// Newest generations of each local build artifact kept warm.
        #[arg(long)]
        keep_generations: Option<u64>,
        /// Output newer than this many seconds counts as active.
        #[arg(long)]
        grace_secs: Option<u64>,
        /// Free-space floor that raises a capacity-pressure warning.
        #[arg(long)]
        min_free_bytes: Option<u64>,
        /// Reject if the policy changed since this version.
        #[arg(long)]
        if_version: Option<u64>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ActorKind {
    Human,
    Ai,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    /// Run by AI agents.
    Ai,
    /// Done by the owner and confirmed with ack.
    Human,
    /// Documents and plans: AI-run and reviewed, but no project checks.
    Docs,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    /// Current and pending work, in execution order.
    Queue,
    /// Finished work, newest first.
    History,
    /// Queue, then History.
    All,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Approval {
    /// Approved: may run.
    Ready,
    /// Waits for approve or reject.
    Draft,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Switch {
    On,
    Off,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show or change how verified commits leave this project.
    Delivery {
        /// Local commits only, direct push to the base branch, or one project pull request.
        #[arg(long, value_parser=["local", "direct", "pr"])]
        mode: Option<String>,
        /// Configured Git remote name (default: origin).
        #[arg(long)]
        remote: Option<String>,
        /// Target branch (default: main).
        #[arg(long)]
        base: Option<String>,
        /// Reject a stale configuration edit.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Set up this project: register it, configure its check, report what is missing.
    ///
    /// A new project starts with the model defaults saved by model-default --new-projects.
    /// Without --check-command, a project that has no check gets one detected from its files:
    /// a Makefile check or test target, Cargo.toml, a package.json test script, go.mod, or
    /// Python test files. Rerunning is safe. Exits 4, after saving what it could, when the
    /// project still cannot run tasks; the message lists each missing piece and its fix.
    Init {
        /// Check command, run from the project root; exit status 0 means passed.
        #[arg(long, value_parser=nonempty)]
        check_command: Option<String>,
        /// Name of the check that --check-command sets.
        #[arg(long, default_value = "check", value_parser=nonempty)]
        check_name: String,
        /// Time allowed for one run of the check.
        #[arg(long, default_value_t=600000, value_parser=positive)]
        check_timeout_ms: i64,
        /// Let agents that change the project use the network, for example to install
        /// dependencies. Codex otherwise runs them without network. Unchanged when omitted.
        #[arg(long, value_enum)]
        agent_network: Option<Switch>,
        /// Run the project checks on a task's clean starting commit before any agent works,
        /// and stop without spending tokens when they fail. On by default. Unchanged when
        /// omitted.
        #[arg(long, value_enum)]
        check_before_task: Option<Switch>,
    },
    /// Show stored output and what retention would remove; --apply removes it.
    ///
    /// Lists retained bytes and the completed attempts that are eligible for cleanup.
    /// Queue evidence, unresolved failures and History records are always kept.
    Storage {
        /// Remove the previewed output now.
        #[arg(long)]
        apply: bool,
    },
    /// Inventory build caches and diagnostic trees; manage the ones this instance owns.
    ///
    /// Without a subcommand lists owned caches, unowned ones (inventoried only), provider
    /// session storage (never cleaned) and capacity pressure. Needs no registered project.
    Cache {
        #[command(subcommand)]
        action: Option<CacheAction>,
    },
    /// Set how many days completed attempts keep their output (0 means immediately).
    Retention {
        /// Age in days after which completed attempts become eligible for cleanup.
        days: i64,
        /// Maximum physical bytes allocated by this instance (shared SQLite and files).
        #[arg(long)]
        max_bytes: Option<i64>,
        /// Maximum files and directories allocated by this instance.
        #[arg(long)]
        max_files: Option<i64>,
        /// Reject if the retention policy changed since this version.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Move the current task forward: Run a pending task, Resume a paused one, or Retry a
    /// failed one; then continue the queue.
    ///
    /// Run starts fresh: the project checks run on the clean starting commit, then the
    /// agent works, and checks, review and validation follow, with up to ten repair rounds.
    /// Retry keeps the failed attempt's changes and continues from the checks. Run and
    /// Retry restart the task's attempt and time limits. Success commits the change and
    /// moves on to the next task. Without --background it blocks until the queue is done
    /// or the owner is needed, then prints why it stopped. resume and retry are aliases.
    ///
    /// Needs a Git repository with a commit, a clean worktree, at least one check
    /// (check-set) and a model for all four roles; otherwise it exits 4 and says which.
    #[command(visible_aliases = ["resume", "retry"])]
    Run {
        /// Continue the queue (the default); overrides a saved Run one preference.
        #[arg(long)]
        queue: bool,
        /// Stop after current succeeds; failures still receive bounded remediation.
        #[arg(long, conflicts_with = "queue")]
        one: bool,
        /// Start a detached supervisor and return once it is running.
        #[arg(long)]
        background: bool,
        #[arg(long, hide = true)]
        notify: bool,
        /// Internal handoff after an intervention in an existing queue session.
        #[arg(long, hide = true)]
        continue_session: bool,
    },
    /// Pause the runner and wait until it has exited; run resumes the same attempt.
    ///
    /// The worker stops and its scratch is cleaned; the attempt keeps its phase, changes
    /// and provider session. Nothing to pause is not an error.
    Pause,
    /// Stop a running, paused, waiting or failed task: it returns to Pending.
    ///
    /// The worker stops, the attempt ends and its uncommitted changes are saved as a git
    /// stash named "ktask: task #ID attempt #N" (restore them with git stash pop). The next
    /// run starts fresh with full limits.
    Stop {
        #[command(flatten)]
        target: Target,
        /// Why; recorded with the attempt. Defaults to who stopped it.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Mark a task done, whatever its state, overriding the supervisor's verdict.
    ///
    /// A live attempt is stopped first and its uncommitted changes are stashed as for stop;
    /// commit them before marking done to keep them in the tree.
    Done {
        #[command(flatten)]
        target: Target,
        /// Why; recorded with the task. Defaults to who marked it.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Cancel a pending task: it moves to History. copy brings it back as a new task.
    Cancel {
        #[command(flatten)]
        target: Target,
        /// Why; recorded with the task. Defaults to who cancelled it.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Mark a pending task as replaced by other tasks of this project.
    Supersede {
        #[command(flatten)]
        target: Target,
        /// Replacing task IDs, comma separated; at least one.
        #[arg(long, required = true, value_delimiter = ',', value_parser = positive)]
        by: Vec<i64>,
        /// Why; recorded with the task.
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Copy a done, cancelled or superseded task into the queue as a new pending task.
    Copy {
        #[command(flatten)]
        target: Target,
        /// Insert before this task ID instead of at the end.
        #[arg(long, value_parser = positive)]
        before: Option<i64>,
    },
    /// Answer the question a task needs input on; the task continues at once.
    ///
    /// Run one then stops after that task; an active queue session continues with the next.
    Answer {
        #[command(flatten)]
        target: Target,
        /// The answer passed to the agent.
        #[arg(long, value_parser=nonempty)]
        text: String,
    },
    /// Add or update a project check: a shell command run from the project root.
    ///
    /// Every attempt must pass all enabled checks, for example: check-set build --command
    /// 'make check'. Checks cannot change while an attempt is live.
    CheckSet {
        /// Check name; an existing name is replaced.
        #[arg(value_parser=nonempty)]
        name: String,
        /// Shell command; exit status 0 means passed.
        #[arg(long, value_parser=nonempty)]
        command: String,
        /// Time allowed for one run of the check.
        #[arg(long, default_value_t=600000, value_parser=positive)]
        timeout_ms: i64,
    },
    /// List enabled project checks.
    Checks,
    /// Set the project's default model for a role; only tasks added later use it.
    ///
    /// Roles: planning (used by breakdown), execution, review, validation, remediation.
    /// To change existing tasks, use edit --model ROLE=PROVIDER/MODEL.
    ModelDefault {
        /// planning, execution, review, validation or remediation.
        #[arg(value_parser=nonempty)]
        role: String,
        /// codex, claude or opencode.
        #[arg(long, value_parser=nonempty)]
        provider: String,
        /// Model name, such as gpt-6-sol. Optional for claude, which defaults to sonnet.
        #[arg(long, value_parser=nonempty)]
        model: Option<String>,
        /// Reasoning effort passed to the provider, such as high.
        #[arg(long, value_parser=nonempty)]
        effort: Option<String>,
        /// Also make it the default that projects registered later start with.
        #[arg(long)]
        new_projects: bool,
    },
    /// List tasks in execution order (history: newest resolution first).
    List {
        /// Which tasks to list.
        #[arg(long, value_enum, default_value = "queue")]
        scope: Scope,
        /// Case-insensitive match on "#ID STATUS TITLE".
        #[arg(long)]
        search: Option<String>,
        /// Only tasks of this kind.
        #[arg(long, value_enum)]
        kind: Option<Kind>,
    },
    /// Show one task: definition, revisions, attempts with steps, usage, cost and reports.
    ///
    /// "state" is the task's place in the state machine: needs_approval, pending, running,
    /// paused, needs_input, failed, done, cancelled or superseded.
    Show {
        /// Task ID.
        #[arg(value_parser = positive)]
        id: i64,
    },
    /// Open a retained text artifact by ID (listed by show/current).
    Artifact {
        /// Artifact ID listed by show or current.
        #[arg(value_parser = positive)]
        id: i64,
    },
    /// Show the current task like show; prints null when the queue is empty.
    Current,
    /// Add a task at the end of the queue (or --before ID). Ready unless --approval draft.
    ///
    /// Models and effort come from the project defaults (model-default) unless --model or
    /// --effort is given. New tasks allow 3 attempts and have no time limits.
    Add(Add),
    /// File a follow-up draft from inside an attempt (worker callback only).
    FollowUp {
        /// Short title.
        #[arg(long, value_parser = nonempty)]
        title: String,
        /// The outcome wanted.
        #[arg(long, value_parser = nonempty)]
        goal: String,
        /// One acceptance criterion; repeat for each.
        #[arg(long, required = true, value_parser = nonempty)]
        criterion: Vec<String>,
    },
    /// Change a task by appending a revision; unspecified fields are kept.
    ///
    /// The approval state is kept unless --approval is given. Running and finished tasks
    /// cannot be edited. A paused, waiting or failed task continues with the new revision
    /// in a new attempt.
    Edit {
        #[command(flatten)]
        target: Target,
        /// Edit the latest pending draft rather than the effective definition.
        #[arg(long)]
        pending: bool,
        #[command(flatten)]
        changes: Changes,
    },
    /// Move pending work before task --before; without it, to the end of the queue.
    Move {
        #[command(flatten)]
        target: Target,
        /// Task ID to move in front of.
        #[arg(long, value_parser = positive)]
        before: Option<i64>,
    },
    /// Approve the newest pending revision so it can run.
    Approve {
        #[command(flatten)]
        target: Target,
    },
    /// Reject the newest draft; an earlier ready revision remains usable.
    Reject {
        #[command(flatten)]
        target: Target,
        /// Why; recorded with the revision.
        #[arg(long, value_parser = nonempty)]
        reason: String,
    },
    /// Ask the planning model to split a task into ordered subtasks.
    ///
    /// Works on a pending task. With --apply the subtasks are added in its place and the
    /// original is superseded by them.
    Breakdown {
        #[command(flatten)]
        target: Target,
        /// Add the subtasks and supersede the original.
        #[arg(long)]
        apply: bool,
    },
    /// Preview or atomically import a ktask-tasks-v2 (or v1) TSV file.
    Import {
        /// TSV file written by export.
        file: PathBuf,
        /// Validate and list the tasks without changing anything.
        #[arg(long)]
        preview: bool,
        /// Insert before this task ID instead of at the end.
        #[arg(long, value_parser = positive)]
        before: Option<i64>,
    },
    /// Export task definitions as TSV (not an attempt or history backup).
    Export {
        /// Destination file.
        file: PathBuf,
        /// Which tasks to export.
        #[arg(long, value_enum, default_value = "queue")]
        scope: Scope,
        /// Explicit permission to replace an existing regular file.
        #[arg(long)]
        overwrite: bool,
    },
    /// Read events (progress, results, waits, changes) in ascending ID order.
    ///
    /// Poll with --after set to the previous reply's next_after to follow a run.
    Events {
        /// Return events with a larger ID than this.
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after: i64,
        /// Maximum events to return (1 to 1000).
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
    },
    /// Project counts, active and waiting time, recorded token usage, and cost.
    Stats,
}

#[derive(Args)]
pub struct Target {
    /// Task ID.
    #[arg(value_parser = positive)]
    pub id: i64,
    /// Reject if the task changed since the version returned by show/list.
    #[arg(long, value_parser = positive)]
    pub if_version: Option<i64>,
}

#[derive(Args)]
pub struct Add {
    /// Short title.
    #[arg(long, value_parser = nonempty)]
    pub title: String,
    /// The outcome wanted, not the implementation steps.
    #[arg(long, value_parser = nonempty)]
    pub goal: String,
    /// One verifiable acceptance criterion; repeat for each.
    #[arg(long, required = true, value_parser = nonempty)]
    pub criterion: Vec<String>,
    /// Who does the work.
    #[arg(long, value_enum, default_value = "ai")]
    pub kind: Kind,
    /// ready may run at once; draft waits for approve.
    #[arg(long, value_enum, default_value = "ready")]
    pub approval: Approval,
    /// Insert before this task ID instead of at the end.
    #[arg(long, value_parser = positive)]
    pub before: Option<i64>,
    #[command(flatten)]
    pub settings: Settings,
}

#[derive(Args, Default)]
pub struct Changes {
    /// Copy the current project model and effort defaults into a new task revision.
    #[arg(long)]
    pub apply_project_defaults: bool,
    /// Show each effective project default without changing the task.
    #[arg(long, conflicts_with = "apply_project_defaults")]
    pub preview_project_defaults: bool,
    /// New title.
    #[arg(long, value_parser = nonempty)]
    pub title: Option<String>,
    /// New goal.
    #[arg(long, value_parser = nonempty)]
    pub goal: Option<String>,
    /// Replaces the full criterion list when supplied; repeat for multiple criteria.
    #[arg(long, value_parser = nonempty)]
    pub criterion: Vec<String>,
    /// New kind.
    #[arg(long, value_enum)]
    pub kind: Option<Kind>,
    /// Store the revision as ready or draft; kept from the edited revision by default.
    #[arg(long, value_enum)]
    pub approval: Option<Approval>,
    #[command(flatten)]
    pub settings: Settings,
}

#[derive(Args, Default)]
pub struct Settings {
    /// ROLE=PROVIDER/MODEL, such as review=codex/gpt-6-sol; repeat per role.
    /// Roles: execution, review, validation, remediation.
    #[arg(long, value_parser = model_setting)]
    pub model: Vec<ModelSetting>,
    /// ROLE=EFFORT or ROLE=none. Omitted effort is preserved.
    #[arg(long, value_parser = effort_setting)]
    pub effort: Vec<EffortSetting>,
    /// Remove a role's model and effort. Execution needs all four roles configured.
    #[arg(long, value_parser = role)]
    pub clear_model: Vec<usize>,
    /// Positive count or unlimited. New tasks default to 3.
    #[arg(long, value_parser = limit)]
    pub attempt_limit: Option<Limit>,
    /// Positive task-wide active milliseconds or unlimited. New tasks default to unlimited.
    #[arg(long, value_parser = limit)]
    pub timeout_ms: Option<Limit>,
    /// Task limit, active time across all attempts: off, on (6 h) or a duration such as 8h.
    #[arg(long, value_parser = task_limit, conflicts_with = "timeout_ms")]
    pub task_limit: Option<Limit>,
    /// Stage limit, one agent turn without a result: off, on (30 min) or a duration such as 45m.
    #[arg(long, value_parser = stage_limit)]
    pub stage_limit: Option<Limit>,
}
#[derive(Clone)]
pub struct ModelSetting {
    pub role: usize,
    pub provider: String,
    pub model: String,
}
#[derive(Clone)]
pub struct EffortSetting {
    pub role: usize,
    pub effort: Option<String>,
}
#[derive(Clone, Copy)]
pub struct Limit(pub Option<i64>);
fn positive(s: &str) -> Result<i64, String> {
    s.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(|| "expected a positive integer".into())
}
fn nonempty(s: &str) -> Result<String, String> {
    if s.trim().is_empty() {
        Err("value must not be blank".into())
    } else {
        Ok(s.into())
    }
}
fn duration_limit(s: &str, standard: i64) -> Result<Limit, String> {
    if s.eq_ignore_ascii_case("on") {
        return Ok(Limit(Some(standard)));
    }
    crate::application::limits::parse(s).map(Limit)
}
fn task_limit(s: &str) -> Result<Limit, String> {
    duration_limit(s, crate::application::limits::TASK_LIMIT_MS)
}
fn stage_limit(s: &str) -> Result<Limit, String> {
    duration_limit(s, crate::application::limits::STAGE_LIMIT_MS)
}
fn limit(s: &str) -> Result<Limit, String> {
    if s == "unlimited" {
        Ok(Limit(None))
    } else {
        positive(s).map(|n| Limit(Some(n)))
    }
}
fn role(s: &str) -> Result<usize, String> {
    crate::application::ROLES
        .iter()
        .position(|r| *r == s)
        .ok_or_else(|| "expected execution, review, validation, or remediation".into())
}
fn model_setting(s: &str) -> Result<ModelSetting, String> {
    let (r, value) = s.split_once('=').ok_or("expected ROLE=PROVIDER/MODEL")?;
    let (provider, model) = crate::application::ModelConfig::split_identifier(value)?;
    Ok(ModelSetting {
        role: role(r)?,
        provider,
        model,
    })
}
fn effort_setting(s: &str) -> Result<EffortSetting, String> {
    let (r, value) = s
        .split_once('=')
        .ok_or("expected ROLE=EFFORT or ROLE=none")?;
    Ok(EffortSetting {
        role: role(r)?,
        effort: if value == "none" {
            None
        } else {
            Some(nonempty(value)?)
        },
    })
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    /// The help is the reference other tools and agents read, so nothing visible goes
    /// undocumented.
    #[test]
    fn every_visible_command_and_argument_has_help() {
        fn walk(command: &clap::Command, path: &str, missing: &mut Vec<String>) {
            for arg in command.get_arguments() {
                let builtin = matches!(arg.get_id().as_str(), "help" | "version");
                if !arg.is_hide_set() && !builtin && arg.get_help().is_none() {
                    missing.push(format!("{path} --{}", arg.get_id()));
                }
            }
            for sub in command.get_subcommands().filter(|s| !s.is_hide_set()) {
                let name = format!("{path} {}", sub.get_name());
                if sub.get_name() != "help" && sub.get_about().is_none() {
                    missing.push(name.clone());
                }
                walk(sub, &name, missing);
            }
        }
        let command = Cli::command();
        command.clone().debug_assert();
        let mut missing = Vec::new();
        walk(&command, "ktask", &mut missing);
        assert!(missing.is_empty(), "missing help: {missing:?}");
    }
}
