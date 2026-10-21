use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(version, about = "TUI-first task workspace. Without a command, opens the TUI.", color = clap::ColorChoice::Never)]
pub struct Cli {
    /// Emit one versioned JSON response (errors on stderr).
    #[arg(long, global = true)]
    pub json: bool,
    /// Project directory; defaults to cwd. Never changes the instance data root.
    #[arg(long, global = true)]
    pub project: Option<PathBuf>,
    /// Unique name when registering a new project with add/import or the TUI.
    #[arg(long, global = true)]
    pub project_name: Option<String>,
    /// Attribution for mutations; ai requires an explicit --actor name.
    #[arg(long, global = true, value_enum, default_value = "human")]
    pub actor_kind: ActorKind,
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

#[derive(Clone, Copy, ValueEnum)]
pub enum ActorKind {
    Human,
    Ai,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Kind {
    Ai,
    Human,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Scope {
    Queue,
    History,
    All,
    Deleted,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Approval {
    Ready,
    Draft,
}
#[derive(Clone, Copy, ValueEnum)]
pub enum Outcome {
    SucceededOverride,
    FailedSkipped,
    Superseded,
}

#[derive(Subcommand)]
pub enum Command {
    /// Show project output usage and cleanup candidates; --apply removes previewed bytes.
    Storage {
        #[arg(long)]
        apply: bool,
    },
    /// Set the age of completed attempts eligible for storage cleanup (0 means immediately).
    Retention {
        days: i64,
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Execute/resume current. Failed checks invoke bounded remediation; never skips a failure.
    #[command(visible_aliases = ["resume", "retry"])]
    Run {
        /// Continue the queue (the default); overrides a saved Run one preference.
        #[arg(long)]
        queue: bool,
        /// Stop after current succeeds; failures still receive bounded remediation.
        #[arg(long, conflicts_with = "queue")]
        one: bool,
        /// Detach from this terminal; inspect progress in the TUI.
        #[arg(long)]
        background: bool,
        #[arg(long, hide = true)]
        notify: bool,
        /// Internal handoff after an intervention in an existing queue session.
        #[arg(long, hide = true)]
        continue_session: bool,
    },
    /// Request pause; state changes only after the worker is stopped and scratch cleaned.
    Pause {
        #[command(flatten)]
        target: Target,
    },
    /// Save an answer; an active queue session continues automatically.
    Answer {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser=nonempty)]
        text: String,
    },
    /// Give up on current: it moves to History as cancelled, keeping attempts and evidence.
    /// With --attempt only the paused or waiting attempt is interrupted; the task stays.
    Abandon {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = nonempty)]
        reason: String,
        /// Interrupt the paused or waiting attempt only, without spending budget.
        #[arg(long)]
        attempt: bool,
    },
    /// Recover an abandoned attempt, only when both supervisor and worker leases are free.
    Recover {
        /// Operator confirms all orphaned workers are stopped and useful work is preserved.
        #[arg(long)]
        confirm_worker_cleanup: bool,
    },
    /// Add/update a real shell check, run from the project root by the supervisor.
    CheckSet {
        #[arg(value_parser=nonempty)]
        name: String,
        #[arg(long, value_parser=nonempty)]
        command: String,
        #[arg(long, default_value_t=600000, value_parser=positive)]
        timeout_ms: i64,
    },
    /// List enabled project checks.
    Checks,
    /// Set a model default for NEW task revisions; existing revisions are unchanged.
    ModelDefault {
        #[arg(value_parser=nonempty)]
        role: String,
        /// codex, claude or opencode.
        #[arg(long, value_parser=nonempty)]
        provider: String,
        /// Optional for claude, which defaults to sonnet.
        #[arg(long, value_parser=nonempty)]
        model: Option<String>,
        #[arg(long, value_parser=nonempty)]
        effort: Option<String>,
    },
    /// List tasks in execution order (history: newest resolution first).
    List {
        #[arg(long, value_enum, default_value = "queue")]
        scope: Scope,
        #[arg(long)]
        search: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<Kind>,
    },
    /// Inspect a task, including revisions and recorded attempts. Includes deleted tasks.
    Show {
        #[arg(value_parser = positive)]
        id: i64,
    },
    /// Inspect the derived current task; returns null when the queue is empty.
    Current,
    /// Add a task. --criterion is repeatable; default approval is ready.
    Add(Add),
    /// File a follow-up draft from an active worker attempt.
    FollowUp {
        #[arg(long, value_parser = nonempty)]
        title: String,
        #[arg(long, value_parser = nonempty)]
        goal: String,
        #[arg(long, required = true, value_parser = nonempty)]
        criterion: Vec<String>,
    },
    /// Append a revision. Unspecified fields and approval state are preserved.
    Edit {
        #[command(flatten)]
        target: Target,
        /// Edit the latest pending draft rather than the effective definition.
        #[arg(long)]
        pending: bool,
        #[command(flatten)]
        changes: Changes,
    },
    /// Reorder pending work; omit --before to move to the end.
    Move {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = positive)]
        before: Option<i64>,
    },
    /// Approve the newest pending revision.
    Approve {
        #[command(flatten)]
        target: Target,
    },
    /// Reject the newest draft; an earlier ready revision remains usable.
    Reject {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_parser = nonempty)]
        reason: String,
    },
    /// Resolve failed current without changing its failed attempts.
    Resolve {
        #[command(flatten)]
        target: Target,
        #[arg(long, value_enum)]
        outcome: Outcome,
        #[arg(long, value_parser = nonempty)]
        reason: String,
    },
    /// Ask the configured failure judge for a judgment; --apply commits it.
    Remediate {
        #[arg(value_parser = positive)]
        id: i64,
        #[arg(long)]
        apply: bool,
    },
    /// Ask the planning model for ordered subtasks; --apply supersedes the original.
    Breakdown {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        apply: bool,
    },
    /// Acknowledge current Human work; creates no AI attempt.
    Ack {
        #[command(flatten)]
        target: Target,
    },
    /// Soft-delete pending work. Recover with restore ID.
    Delete {
        #[command(flatten)]
        target: Target,
    },
    /// Restore a deleted task at its retained execution position.
    Restore {
        #[command(flatten)]
        target: Target,
    },
    /// Preview or atomically import a ktask-tasks-v2 (or v1) TSV file.
    Import {
        file: PathBuf,
        #[arg(long)]
        preview: bool,
        #[arg(long, value_parser = positive)]
        before: Option<i64>,
    },
    /// Export definitions as TSV (not an attempt/history backup).
    Export {
        file: PathBuf,
        #[arg(long, value_enum, default_value = "queue")]
        scope: Scope,
        /// Explicit permission to replace an existing regular file.
        #[arg(long)]
        overwrite: bool,
    },
    /// Read an event page in ascending ID order.
    Events {
        #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i64).range(0..))]
        after: i64,
        #[arg(long, default_value_t = 100, value_parser = clap::value_parser!(i64).range(1..=1000))]
        limit: i64,
    },
    /// Project counts, active and waiting time, recorded token usage, and cost.
    Stats,
}

#[derive(Args)]
pub struct Target {
    #[arg(value_parser = positive)]
    pub id: i64,
    /// Reject if the task changed since the version returned by show/list.
    #[arg(long, value_parser = positive)]
    pub if_version: Option<i64>,
}

#[derive(Args)]
pub struct Add {
    #[arg(long, value_parser = nonempty)]
    pub title: String,
    #[arg(long, value_parser = nonempty)]
    pub goal: String,
    #[arg(long, required = true, value_parser = nonempty)]
    pub criterion: Vec<String>,
    #[arg(long, value_enum, default_value = "ai")]
    pub kind: Kind,
    #[arg(long, value_enum, default_value = "ready")]
    pub approval: Approval,
    #[arg(long, value_parser = positive)]
    pub before: Option<i64>,
    #[command(flatten)]
    pub settings: Settings,
}

#[derive(Args, Default)]
pub struct Changes {
    #[arg(long, value_parser = nonempty)]
    pub title: Option<String>,
    #[arg(long, value_parser = nonempty)]
    pub goal: Option<String>,
    /// Replaces the full criterion list when supplied; repeat for multiple criteria.
    #[arg(long, value_parser = nonempty)]
    pub criterion: Vec<String>,
    #[arg(long, value_enum)]
    pub kind: Option<Kind>,
    #[arg(long, value_enum)]
    pub approval: Option<Approval>,
    #[command(flatten)]
    pub settings: Settings,
}

#[derive(Args, Default)]
pub struct Settings {
    /// ROLE=PROVIDER/MODEL; roles: execution, review, validation, remediation.
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
