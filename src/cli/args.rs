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
    /// Unique name when add, import, settings-import or the TUI registers a new project;
    /// checked otherwise.
    #[arg(long, global = true)]
    pub project_name: Option<String>,
    /// Who makes the change, for attribution. Defaults to human for CLI commands and AI for MCP.
    #[arg(long, global = true, value_enum)]
    pub actor_kind: Option<ActorKind>,
    /// Name recorded with changes, such as claude-code; defaults to owner for CLI and mcp for MCP.
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

#[derive(Subcommand)]
pub enum AgentAction {
    /// Start an investigation in the background and print its ID at once.
    ///
    /// Without QUESTION it explains the current state of the project and of its current
    /// task. One investigation runs per project; a second request exits 4 naming the
    /// running one. Read the result with agent show ID.
    Report {
        /// Ask about this task; defaults to the current task.
        #[arg(long, value_parser = positive)]
        task: Option<i64>,
        /// What to find out, as one quoted argument.
        #[arg(value_parser = nonempty)]
        question: Option<String>,
    },
    /// Have an agent repair this project, from a report or an instruction, and record it.
    ///
    /// Prints the fix ID at once; read it with agent show ID and end it with agent stop ID.
    /// Only the owner starts one (not --actor-kind ai); ktask never does. The fix first pauses the project runner and waits
    /// for it to exit, and no runner starts while it works. Work found in the tree that no
    /// task owns is saved as a git stash naming the fix; a paused or failed task's own work
    /// stays for it. The project's checks, unmet requirements and what the agent names as
    /// failing are repeated before and after: only a failure that now passes makes a
    /// verified fix, and --resume then continues the queue. Otherwise the runner stays
    /// paused. One fix or report runs per project (exit 4 names the running one).
    ///
    /// The agent may use ktask commands for this project (configuration, checks, models,
    /// limits, delivery, task edits, adding and moving tasks, stop, restart, retry, and
    /// answering a question the instruction answers), repair the project folder and its Git
    /// state without discarding work, and change this account's setup. It may not delete or
    /// move projects, touch other projects, shut ktask down, judge tasks done or cancelled,
    /// approve drafts unless the instruction says "approve #ID", gain privileges, or write
    /// product code (it adds tasks for that). Refused steps are listed as left for the owner.
    Fix {
        /// Carry out the proposed steps of this finished report of this project.
        #[arg(long, value_parser = positive, conflicts_with = "instruction", required_unless_present = "instruction")]
        report: Option<i64>,
        /// What to repair, as one quoted argument.
        #[arg(value_parser = nonempty)]
        instruction: Option<String>,
        /// Continue the queue if the fix is verified.
        #[arg(long)]
        resume: bool,
        /// Print the steps it would take and change nothing. With --report these are the
        /// report's steps; an instruction is planned by a read-only agent turn.
        #[arg(long)]
        dry_run: bool,
    },
    /// End a running report or fix, reap its processes and keep what it had done.
    Stop {
        /// Report or fix ID.
        #[arg(value_parser = positive)]
        id: i64,
    },
    /// List this project's reports and fixes, newest first.
    List,
    /// Show one report or fix: the report or record, or why it ended and what it had done.
    Show {
        /// Report or fix ID.
        #[arg(value_parser = positive)]
        id: i64,
    },
    /// Show or change the time and cost one report (default 15 min, $2) or fix (default
    /// 30 min, $5) may use.
    ///
    /// The cost limit ends an investigation whose provider reports a higher cost: Claude
    /// Code also enforces it itself, OpenCode reports cost per step, and Codex reports no
    /// cost, so only the time limit bounds a Codex investigation. Investigations that
    /// already run keep the limits they started with.
    Limits {
        /// Time one investigation may take, in milliseconds.
        #[arg(long, value_parser = positive)]
        time_limit_ms: Option<i64>,
        /// Cost one investigation may use, in US dollars (0.01 to 1000).
        #[arg(long, value_parser = usd)]
        cost_limit_usd: Option<Usd>,
        /// Time one fix may take, in milliseconds.
        #[arg(long, value_parser = positive)]
        fix_time_limit_ms: Option<i64>,
        /// Cost one fix may use, in US dollars (0.01 to 1000).
        #[arg(long, value_parser = usd)]
        fix_cost_limit_usd: Option<Usd>,
        /// Reject a stale edit of the limits.
        #[arg(long)]
        if_version: Option<i64>,
    },
}

/// US dollars as millionths.
#[derive(Clone, Copy, Debug)]
pub struct Usd(pub i64);

fn usd(text: &str) -> Result<Usd, String> {
    let value: f64 = text
        .parse()
        .map_err(|_| "expected an amount in US dollars, such as 2.50".to_owned())?;
    if !value.is_finite() || !(0.01..=1000.0).contains(&value) {
        return Err("expected an amount from 0.01 to 1000 US dollars".into());
    }
    Ok(Usd((value * 1_000_000.0).round() as i64))
}

#[derive(Subcommand)]
pub enum ToolAction {
    /// Show this instance's registered generations and this project's bindings.
    List,
    /// Show the fixed Android package, source, consent, capacity and destination plan.
    ProvisionPlan {
        /// Reviewed provisioning recipe name.
        recipe: String,
        /// Registered external Android SDK generation ID.
        source: i64,
    },
    /// Enable or disable managed provisioning for this project (human actor only).
    ProvisionMode {
        /// Manual or automatic provisioning policy for this project.
        #[arg(value_enum)]
        mode: ProvisionMode,
    },
    /// Record human consent for the exact license files of a registered SDK.
    ProvisionConsent {
        /// Reviewed provisioning recipe name.
        recipe: String,
        /// Registered external Android SDK generation ID.
        source: i64,
    },
    /// Run the fixed recipe in owned staging and bind the published generation.
    ProvisionInstall {
        /// Reviewed provisioning recipe name.
        recipe: String,
        /// Registered external Android SDK generation ID.
        source: i64,
    },
    /// Show the last provisioning progress or outcome for this project.
    ProvisionStatus,
    /// Cancel this project's active installer; its staging is reclaimed after reaping.
    ProvisionCancel,
    /// Register an existing installation. External trees stay user owned and read only.
    Register {
        /// Normalized tool name, such as rg or android-sdk.
        name: String,
        /// Concrete installed version.
        version: String,
        /// Target platform, such as linux.
        platform: String,
        /// Target architecture, such as x86_64.
        architecture: String,
        /// Existing installation directory.
        path: PathBuf,
        /// Installed component identifier; repeat for each component.
        #[arg(long = "component")]
        components: Vec<String>,
        /// The existing directory is below this instance's XDG data/tools root.
        #[arg(long)]
        managed: bool,
    },
    /// Explicitly accept a changed external installation after checking its version.
    Refresh {
        /// Registered generation ID from tool list.
        generation: i64,
        /// Concrete version now installed at the registered path.
        version: String,
        /// Installed component identifier; repeat for each component.
        #[arg(long = "component")]
        components: Vec<String>,
        /// Current generation version from tool list.
        #[arg(long)]
        if_version: i64,
    },
    /// Select one registered generation for this project.
    Bind {
        /// Tool name recorded on the generation.
        name: String,
        /// Registered generation ID from tool list.
        generation: i64,
        /// Binding version, or 0 if it has no binding yet.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Remove this project's binding, guarded by its current version.
    Unbind {
        /// Bound tool name from tool list.
        name: String,
        /// Current binding version from tool list.
        #[arg(long)]
        if_version: i64,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ProvisionMode {
    Manual,
    Automatic,
}

#[derive(Subcommand)]
pub enum ProfileAction {
    /// List profiles with what selects them.
    List,
    /// Show one profile's role models.
    Show {
        /// Profile name (case-insensitive).
        #[arg(value_parser = nonempty)]
        name: String,
    },
    /// Create a profile from role models, or from this project's effective models.
    ///
    /// Roles: planning, execution, review, validation, remediation, operator. Models are written
    /// ROLE=PROVIDER/MODEL with the providers and model names the provider CLI accepts; ktask
    /// checks the shape and effort spelling, never that a model exists. --from-project copies
    /// every effective model of this project, so a profile can capture the Claude/high
    /// setup the project already uses. Roles a profile leaves out inherit the user defaults.
    Create {
        /// Profile name, at most 64 characters; "none" is reserved.
        #[arg(value_parser = nonempty)]
        name: String,
        /// Start from this project's effective role models.
        #[arg(long)]
        from_project: bool,
        /// ROLE=PROVIDER/MODEL; repeat per role. Replaces a role copied by --from-project.
        #[arg(long, value_parser = profile_model)]
        model: Vec<ProfileModel>,
        /// ROLE=EFFORT or ROLE=none; the role needs a model.
        #[arg(long, value_parser = profile_effort)]
        effort: Vec<ProfileEffort>,
        /// Also select it for this project.
        #[arg(long)]
        select: bool,
        /// Also make it the profile new projects start with.
        #[arg(long)]
        make_default: bool,
    },
    /// Change a profile's name or role models; only tasks defined later use the change.
    ///
    /// A profile that a project or the new-project default selects may change a role's
    /// model but not drop the role.
    Edit {
        /// Profile name.
        #[arg(value_parser = nonempty)]
        name: String,
        /// New name.
        #[arg(long, value_parser = nonempty)]
        rename: Option<String>,
        /// ROLE=PROVIDER/MODEL; repeat per role.
        #[arg(long, value_parser = profile_model)]
        model: Vec<ProfileModel>,
        /// ROLE=EFFORT or ROLE=none. Omitted effort is preserved.
        #[arg(long, value_parser = profile_effort)]
        effort: Vec<ProfileEffort>,
        /// Remove a role from the profile so it inherits the user default.
        #[arg(long, value_parser = model_role)]
        clear_model: Vec<usize>,
        /// Reject a stale edit; the version comes from show or list.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Delete a profile that no project and no new-project default selects.
    Delete {
        /// Profile name.
        #[arg(value_parser = nonempty)]
        name: String,
        /// Reject a stale delete.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Select a profile for this project, or --none to inherit the user defaults alone.
    Select {
        /// Profile name.
        #[arg(value_parser = nonempty, required_unless_present = "none")]
        name: Option<String>,
        /// Stop using a profile.
        #[arg(long, conflicts_with = "name")]
        none: bool,
    },
    /// Choose the profile new projects select, or --none for no default profile.
    Default {
        /// Profile name.
        #[arg(value_parser = nonempty, required_unless_present = "none")]
        name: Option<String>,
        /// Clear the default profile.
        #[arg(long, conflicts_with = "name")]
        none: bool,
    },
    /// Show each role's effective model and which layer supplies it.
    ///
    /// Precedence: user defaults, the selected profile, this project's overrides, then a
    /// task's own models. With --task, shows that task's pinned models beside what a new
    /// revision would get now; pinned models never change on their own.
    Effective {
        /// A task whose pinned models to compare.
        #[arg(long, value_parser = positive)]
        task: Option<i64>,
        /// Show what a project registered now would get instead.
        #[arg(long, conflicts_with = "task")]
        new_project: bool,
    },
    /// Clear this project's own model overrides so the roles inherit again.
    Inherit {
        /// Roles to clear: planning, execution, review, validation, remediation or operator.
        #[arg(value_parser = model_role)]
        roles: Vec<usize>,
        /// Clear every role.
        #[arg(long, conflicts_with = "roles")]
        all: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub enum ActorKind {
    Human,
    Ai,
}
#[derive(Clone, Copy, Default, ValueEnum, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Run by AI agents.
    #[default]
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
pub enum LinkAction {
    /// On a child: save the parent this computer reports to, or --clear it.
    Parent {
        /// SSH destination of the parent: user@host or a Host alias from ~/.ssh/config.
        #[arg(required_unless_present = "clear")]
        destination: Option<String>,
        /// Forget the parent; a running link run then exits.
        #[arg(long, conflicts_with = "destination")]
        clear: bool,
        /// Name the parent lists this computer under; defaults to its host name.
        #[arg(long, conflicts_with = "clear")]
        name: Option<String>,
    },
    /// Show the role, peer, whether it is connected, last contact and last error.
    Status,
    /// Stay linked in the foreground, as a systemd user service would run it.
    ///
    /// With a parent configured it dials ssh PARENT ktask link accept, sends this
    /// computer's projects and their changes, and reconnects after any failure. Without
    /// one it is the parent: it holds the children's connections.
    Run,
    /// On the parent: the only command a child runs over SSH; pipes it to link run.
    Accept,
    /// On the parent: remove a disconnected host and its listed projects.
    Forget {
        /// Host name as listed by projects or link status.
        host: String,
    },
}

#[derive(Subcommand)]
pub enum Command {
    /// Inspect, register and bind reusable tools in this explicit instance.
    Tool {
        #[command(subcommand)]
        action: ToolAction,
    },
    /// Page or search an evidence file without opening the database (safe in judge sandboxes).
    Evidence {
        /// Regular evidence file to inspect.
        file: PathBuf,
        /// Start at this byte offset; continue with next_offset from the reply.
        #[arg(long, default_value_t = 0)]
        offset: u64,
        /// Maximum text bytes per page (at most 8192).
        #[arg(long, default_value_t = 4096, value_parser = clap::value_parser!(u32).range(1..=8192))]
        limit: u32,
        /// Find a literal string, scanning at most 1 MiB per call.
        #[arg(long)]
        search: Option<String>,
    },
    /// Show or change the settings every new project starts with; --apply applies them here.
    ///
    /// Saved once per user and instance, next to model-default --new-projects (the role
    /// models and effort). Only a project registered later inherits them; an existing project
    /// keeps its settings until --apply, and tasks and attempts keep their pinned inputs.
    /// Output lists, per setting, this project's value and what the defaults would set; the
    /// check command, remote and base branch are never part of them. --apply changes the
    /// project to the defaults shown, refusing during a live attempt or if they moved on.
    Defaults {
        /// Let writable agent turns use the network.
        #[arg(long, value_enum)]
        agent_network: Option<Switch>,
        /// Run the project checks on a task's clean starting commit first.
        #[arg(long, value_enum)]
        check_before_task: Option<Switch>,
        /// Time allowed for one run of a check.
        #[arg(long, value_parser=positive)]
        check_timeout_ms: Option<i64>,
        /// one runs a single task; queue continues until something needs the owner.
        #[arg(long, value_parser=["one", "queue"])]
        run_mode: Option<String>,
        /// Whether the planner's and remediator's tasks need the owner's approval.
        #[arg(long, value_parser=["automatic", "review-required"])]
        planner_approval: Option<String>,
        /// Local commits only, direct push to the base branch, or one pull request.
        #[arg(long, value_parser=["local", "direct", "pr"])]
        delivery_mode: Option<String>,
        /// Reject a stale edit of the defaults.
        #[arg(long)]
        if_version: Option<i64>,
        /// Apply the defaults to this project, as previewed.
        #[arg(long)]
        apply: bool,
    },
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
    /// A new project starts with the user defaults: the models saved by model-default
    /// --new-projects and the settings saved by the defaults command (agent network,
    /// baseline checks, check timeout, run mode, planner approval, delivery mode). Options
    /// given here override them for this project. Never copied from another project: the
    /// check command, directories, remote and base branch; the output lists what is left to
    /// set. Without --check-command, a project that has no check gets one detected from its files:
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
        /// Time allowed for one run of the check (default: the user default, 600000).
        #[arg(long, value_parser=positive)]
        check_timeout_ms: Option<i64>,
        /// Let agents that change the project use the network, for example to install
        /// dependencies. Codex otherwise runs them without network. Unchanged when omitted.
        #[arg(long, value_enum)]
        agent_network: Option<Switch>,
        /// Run the project checks on a task's clean starting commit before any agent works,
        /// and repair failures automatically while preserving the original goal.
        /// On by default. Unchanged when omitted.
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
    Pause {
        /// Pause runners for all registered projects in both ktask instances.
        #[arg(long, conflicts_with_all = ["project", "project_name"])]
        all: bool,
    },
    /// Pause every runner, close the TUIs and stop link run in both ktask instances for an
    /// upgrade.
    Shutdown,
    /// List every registered project of this instance, from any working directory.
    ///
    /// Each line: ID, state, unfinished tasks, name, root, then the current task. States:
    /// running, needs_owner (approval, answer, prerequisite, failure or Human task), paused,
    /// ready, idle, missing (the root is no longer a directory). Never registers a project.
    ///
    /// On a parent that has heard from a child (see link), the list is one for all hosts,
    /// sorted by name then host, and each line ends with the host ("local" for this
    /// computer). A disconnected host's projects show offline or incompatible and end with
    /// the time of last contact. Project and task IDs are per host. --json adds "host"
    /// (null for local) and "online" to every project.
    Projects,
    /// Link hosts over SSH: a child keeps one connection to a parent, which lists its
    /// projects read-only.
    ///
    /// Authentication and encryption are the hosts' SSH keys and known_hosts. Nothing is
    /// linked until a child names its parent and link run runs on both.
    Link {
        #[command(subcommand)]
        action: LinkAction,
    },
    /// Point a registered project at another folder; its tasks and history follow it.
    ///
    /// ktask moves no files: move or clone the repository first. Refused while the
    /// project's runner is alive or the folder already is a project.
    ProjectMove {
        /// Project ID, from `ktask projects`.
        id: i64,
        /// The existing folder the project now lives in.
        folder: PathBuf,
    },
    /// DANGER: permanently delete a project with all its tasks, attempts, output,
    /// evidence, usage and settings, and ktask's own files for it. Cannot be undone.
    ///
    /// Nothing in the project's folder is touched. Refused while its runner is alive.
    ProjectDelete {
        /// Project ID, from `ktask projects`.
        id: i64,
        /// The project's exact name, as confirmation.
        #[arg(long)]
        confirm: String,
    },
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
    /// Stop and reap the current task's worker, stash unfinished changes, then run it
    /// fresh from the current HEAD. A retained real checkpoint is rechecked before delivery.
    Restart {
        #[command(flatten)]
        target: Target,
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
    /// Declare a local prerequisite on the pending Current Task before its baseline.
    Require {
        #[command(flatten)]
        target: Target,
        #[command(flatten)]
        requirement: RequirementArgs,
    },
    /// Probe unmet prerequisites locally. A successful result enables an explicit Run.
    DependencyRecheck {
        #[command(flatten)]
        target: Target,
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
    /// Roles: planning (used by breakdown), execution, review, validation, remediation,
    /// operator (agent reports; unset, it uses the remediation model).
    /// To change existing tasks, use edit --model ROLE=PROVIDER/MODEL.
    ModelDefault {
        /// planning, execution, review, validation, remediation or operator.
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
    /// Manage named role/model/effort profiles and see where each model comes from.
    ///
    /// A profile maps roles to models for the existing providers (codex, claude,
    /// opencode); it adds no provider and promises no model's availability. Models resolve
    /// through the user defaults (model-default --new-projects), the project's selected
    /// profile, the project's own overrides (model-default), then a task's own models.
    /// Profile changes affect only tasks defined afterwards; existing revisions and attempts
    /// keep their pinned models, and pending tasks change only through
    /// edit --apply-project-defaults.
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// Manage work categories: the templates that tell AI turns how to execute, review,
    /// validate and remediate a kind of work.
    ///
    /// Built-in categories are code (project checks apply), documents and art (no checks).
    /// Save your own in the user scope, override any of them per project, pick one with
    /// `add --category NAME`, and see the effective prompt with `category preview`. A task
    /// revision pins the templates it was defined with; later edits reach only tasks
    /// defined afterwards. The supervisor's rules, evidence handling and response schema
    /// are not part of a category.
    Category {
        #[command(subcommand)]
        action: CategoryAction,
    },
    /// Manage review lenses: named specialist reviews a task can opt into.
    ///
    /// Built-in lenses are concurrency, security and simplicity. Save your own in the user
    /// scope, override any of them per project, choose the project's defaults with
    /// `lens defaults`, and select lenses on a task with `add --lens NAME` or
    /// `edit --lens NAME`. A task with lenses is reviewed once per lens, each in its own
    /// read-only session on the configured review model, instead of by the single ordinary
    /// review; all of them finish before one repair is requested. A revision pins the
    /// instructions it was defined with. The supervisor's rules are not part of a lens.
    Lens {
        #[command(subcommand)]
        action: LensAction,
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
    /// Serve every project of this instance over MCP on standard input/output (--project pins one).
    Mcp,
    /// Have an agent investigate this project and report what happened, or repair it.
    ///
    /// An agent fix (agent fix) repairs the project on request and records every change;
    /// see its help. An agent report is started by the owner and changes nothing: no file,
    /// task, attempt or setting. The agent runs on this host in a read-only sandbox with its own TMPDIR,
    /// under the time and cost limits of agent limits, with the operator model (model-default
    /// operator; unset, the remediation model). It reads ktask's records of the project, the
    /// project folder and Git state, and may probe the host. It works whether the runner is
    /// running, paused, failed or absent, and never touches a live attempt. The finished
    /// report states what happened, the cause with its references and proposed fix steps.
    Agent {
        #[command(subcommand)]
        action: AgentAction,
    },
    /// Add a task at the end of the queue (or --before ID). Ready unless --approval draft.
    ///
    /// Models and effort come from the project defaults (model-default) unless --model or
    /// --effort is given. New tasks allow 3 attempts and have no time limits.
    Add(Add),
    /// One command of a running agent fix (its callback only).
    #[command(hide = true)]
    #[command(name = "fix-command")]
    InFix {
        #[arg(last = true, required = true)]
        args: Vec<String>,
    },
    /// Append a follow-up draft to the queue from inside an attempt (worker callback only).
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
    /// Request a missing prerequisite from inside a live execution attempt (worker callback only).
    DependencyRequest {
        #[command(flatten)]
        requirement: RequirementArgs,
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
    /// Preview or atomically import a ktask-tasks-v4 (or v3, v2, v1) TSV file.
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
    /// Write this project's settings to one file, to move the project to another computer.
    ///
    /// Settings are everything configured that is not a task: checks, role models, profiles,
    /// custom categories and lenses with the default lenses, delivery, retention, agent
    /// report and fix limits, agent network, baseline checks, provisioning mode and
    /// preferences, plus what the project shares with the instance: user defaults, user
    /// categories and lenses, and build cache limits. Paths inside the project are written
    /// relative to its root. History, attempts, evidence, usage and cost are not part of
    /// the file, nor are installed tools, tool registrations and credentials; export tasks
    /// with `export`. Only reads, so it works while the runner is running.
    SettingsExport {
        /// Destination file, a readable, versioned JSON document. Keep it outside the
        /// project: an untracked file stops the next run.
        file: PathBuf,
        /// Explicit permission to replace an existing regular file.
        #[arg(long)]
        overwrite: bool,
    },
    /// Apply a settings file to the project of --project or the current directory.
    ///
    /// Registers the folder, which must be the root of a Git repository, when it is not a
    /// project yet. Applies everything or nothing. Replaces the settings the file names and
    /// keeps the rest; tasks and history are never touched, and existing tasks keep their
    /// revisions. Paths are resolved against this folder; absolute paths outside the project
    /// are kept and listed for you to confirm. Settings marked "shared" apply to every
    /// project of the instance. Changing what a running attempt uses (checks, delivery,
    /// agent network, baseline checks, run mode, planner approval, retention, provisioning)
    /// requires the runner to be paused.
    SettingsImport {
        /// File written by settings-export.
        file: PathBuf,
        /// Print what would be added, changed or left, the folder, the rewritten paths and
        /// the missing tools and models, without changing anything.
        #[arg(long)]
        preview: bool,
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
pub struct RequirementArgs {
    /// executable, sdk_component or capability.
    #[arg(long, value_parser = ["executable", "sdk_component", "capability"])]
    pub kind: String,
    /// Executable name/path, SDK semicolon component, or absolute capability path.
    #[arg(long, value_parser = nonempty)]
    pub item: String,
    /// Exact version token required from an executable's --version output.
    #[arg(long)]
    pub version: Option<String>,
    /// Where this prerequisite is needed: task, project or host.
    #[arg(long, value_parser = ["task", "project", "host"])]
    pub scope: String,
    /// Workflow stage affected by the missing prerequisite.
    #[arg(long, value_parser = ["baseline", "execution", "check", "review", "validation"])]
    pub stage: String,
    /// A bounded, concrete reproduction of the missing prerequisite.
    #[arg(long, value_parser = nonempty)]
    pub reproduction: String,
}

impl RequirementArgs {
    pub fn requirement(&self) -> crate::application::dependency::Requirement {
        crate::application::dependency::Requirement {
            kind: self.kind.clone(),
            item: self.item.clone(),
            version: self.version.clone(),
            scope: self.scope.clone(),
            stage: self.stage.clone(),
            reproduction: self.reproduction.clone(),
        }
    }
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
    /// Work category whose templates drive the AI turns (see `category list`); the default
    /// follows the kind: code for ai, documents for docs.
    #[arg(long, value_parser = nonempty)]
    pub category: Option<String>,
    #[command(flatten)]
    pub lenses: LensChoice,
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
    /// Copy the project's current effective models (user defaults, selected profile, project overrides) into a new task revision.
    #[arg(long)]
    pub apply_project_defaults: bool,
    /// Show each effective project model without changing the task.
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
    /// New kind. Without --category the kind's default category applies again.
    #[arg(long, value_enum)]
    pub kind: Option<Kind>,
    /// New work category, pinned with its current templates by the new revision.
    #[arg(long, value_parser = nonempty)]
    pub category: Option<String>,
    #[command(flatten)]
    pub lenses: LensChoice,
    /// Store the revision as ready or draft; kept from the edited revision by default.
    #[arg(long, value_enum)]
    pub approval: Option<Approval>,
    #[command(flatten)]
    pub settings: Settings,
}

/// A task's review lenses: opt-in specialist reviews that replace the ordinary single review.
#[derive(Args, Default)]
pub struct LensChoice {
    /// Review through this lens (see `lens list`); repeat for several, reviewed in the order
    /// given. Without --lens or --no-lenses a new task takes the project's default lenses
    /// (`lens defaults`) and an edited task keeps its own.
    #[arg(long = "lens", value_parser = nonempty)]
    pub lens: Vec<String>,
    /// Keep the ordinary single review, whatever the project's default lenses are.
    #[arg(long = "no-lenses", conflicts_with = "lens")]
    pub none: bool,
}

impl LensChoice {
    /// The selection this asks for; `None` leaves the choice to the default.
    pub fn selection(&self) -> Option<Vec<String>> {
        if self.none {
            Some(Vec::new())
        } else {
            (!self.lens.is_empty()).then(|| self.lens.clone())
        }
    }
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
pub struct ProfileModel {
    pub role: usize,
    pub provider: String,
    pub model: String,
}
#[derive(Clone)]
pub struct ProfileEffort {
    pub role: usize,
    pub effort: Option<String>,
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
fn model_role(s: &str) -> Result<usize, String> {
    crate::application::configuration::MODEL_ROLES
        .iter()
        .position(|r| *r == s)
        .ok_or_else(|| {
            "expected planning, execution, review, validation, remediation or operator".into()
        })
}
fn profile_model(s: &str) -> Result<ProfileModel, String> {
    let (r, value) = s.split_once('=').ok_or("expected ROLE=PROVIDER/MODEL")?;
    let (provider, model) = crate::application::ModelConfig::split_identifier(value)?;
    Ok(ProfileModel {
        role: model_role(r)?,
        provider,
        model,
    })
}
fn profile_effort(s: &str) -> Result<ProfileEffort, String> {
    let (r, value) = s
        .split_once('=')
        .ok_or("expected ROLE=EFFORT or ROLE=none")?;
    Ok(ProfileEffort {
        role: model_role(r)?,
        effort: if value == "none" {
            None
        } else {
            Some(nonempty(value)?)
        },
    })
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

#[derive(Subcommand)]
pub enum LensAction {
    /// List the lenses this project can choose, each from the scope that wins.
    List,
    /// Show a lens: where it comes from and its instructions.
    Show {
        /// Lens name (case-insensitive).
        #[arg(value_parser = nonempty)]
        name: String,
        /// Show one scope's definition instead of the effective one.
        #[arg(long, value_enum)]
        scope: Option<CategoryScope>,
    },
    /// Create or change a lens in the user or project scope.
    ///
    /// The first save of a name in a scope starts from the lens it would override (project
    /// over user over built-in) and a brand-new name needs --instructions. Instructions use
    /// {{title}}, {{goal}}, {{criteria}}, {{category}} and {{context}} (see `category
    /// placeholders`); pass @PATH to read them from a file. Only tasks defined afterwards use
    /// the change.
    Save {
        /// Lens name: letters, digits, spaces, '-', '_' or '.', at most 64 characters.
        #[arg(value_parser = nonempty)]
        name: String,
        /// Where to save it.
        #[arg(long, value_enum, default_value = "user")]
        scope: CategoryScope,
        /// One line saying what the lens looks for.
        #[arg(long)]
        description: Option<String>,
        /// The review instructions, or @PATH.
        #[arg(long)]
        instructions: Option<String>,
        /// Reject a stale save; the version comes from show or list.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Delete a user or project lens so the name inherits again.
    ///
    /// Tasks that already pinned the lens keep their copy.
    Delete {
        /// Lens name.
        #[arg(value_parser = nonempty)]
        name: String,
        /// The scope to delete from.
        #[arg(long, value_enum, default_value = "user")]
        scope: CategoryScope,
        /// Reject a stale delete.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Show or set the lenses new tasks of this project select, in review order.
    ///
    /// Without options the current defaults are shown. Existing tasks keep the lenses their
    /// revisions pinned; a task can always override the defaults.
    Defaults {
        /// Replace the defaults with these lenses, in review order.
        #[arg(long = "set", value_parser = nonempty, num_args = 1.., conflicts_with = "clear")]
        set: Vec<String>,
        /// Remove the defaults: new tasks use the ordinary single review.
        #[arg(long)]
        clear: bool,
        /// Reject the change if the defaults are no longer these lenses (comma separated,
        /// as shown by `lens defaults`); an empty value means none.
        #[arg(long, value_delimiter = ',')]
        if_current: Option<Vec<String>>,
    },
    /// Show the effective prompt of a lens review, with runtime-only parts labelled.
    ///
    /// With --task, the task's title, goal and criteria are filled in and its category is
    /// used; otherwise an example task is.
    Preview {
        /// Lens name; with --task, defaults to the task's first pinned lens.
        #[arg(value_parser = nonempty, required_unless_present = "task")]
        name: Option<String>,
        /// Preview this task's content and category.
        #[arg(long, value_parser = positive)]
        task: Option<i64>,
    },
}

/// A scope of work categories on the command line.
#[derive(Clone, Copy, ValueEnum)]
pub enum CategoryScope {
    /// Every project of this instance.
    User,
    /// This project only; overrides the user scope and the built-in categories.
    Project,
    /// Compiled-in defaults (read only).
    #[value(name = "built-in")]
    BuiltIn,
}

impl From<CategoryScope> for crate::application::categories::Scope {
    fn from(value: CategoryScope) -> Self {
        match value {
            CategoryScope::User => Self::User,
            CategoryScope::Project => Self::Project,
            CategoryScope::BuiltIn => Self::BuiltIn,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CategoryRole {
    Execution,
    Review,
    Validation,
    Remediation,
}

#[derive(Subcommand)]
pub enum CategoryAction {
    /// List the categories this project can choose, each from the scope that wins.
    List,
    /// Show a category: where it comes from, whether checks apply and every template.
    Show {
        /// Category name (case-insensitive).
        #[arg(value_parser = nonempty)]
        name: String,
        /// Show one scope's definition instead of the effective one.
        #[arg(long, value_enum)]
        scope: Option<CategoryScope>,
    },
    /// Create or change a category in the user or project scope.
    ///
    /// The first save of a name in a scope starts from the category it would override
    /// (project over user over built-in) or from --from, and a brand-new name must supply
    /// all four templates and --checks or --no-checks. Later saves change only what is
    /// given. Templates use {{title}}, {{goal}}, {{criteria}}, {{category}} and {{context}};
    /// see `category placeholders`. Pass @PATH to read a template from a file. Placeholders
    /// are checked on save. Only tasks defined afterwards use the change.
    Save {
        /// Category name: letters, digits, spaces, '-', '_' or '.', at most 64 characters.
        #[arg(value_parser = nonempty)]
        name: String,
        /// Where to save it.
        #[arg(long, value_enum, default_value = "user")]
        scope: CategoryScope,
        /// Start a new definition from this category.
        #[arg(long, value_parser = nonempty)]
        from: Option<String>,
        /// One line saying what the category is for.
        #[arg(long)]
        description: Option<String>,
        /// The project's checks gate this work.
        #[arg(long, conflicts_with = "no_checks")]
        checks: bool,
        /// The project's checks do not apply to this work.
        #[arg(long)]
        no_checks: bool,
        /// Template for the execution turn, or @PATH.
        #[arg(long)]
        execution: Option<String>,
        /// Template for review turns, or @PATH.
        #[arg(long)]
        review: Option<String>,
        /// Template for validation turns, or @PATH.
        #[arg(long)]
        validation: Option<String>,
        /// Template for the failure judge and repair turns, or @PATH.
        #[arg(long)]
        remediation: Option<String>,
        /// Reject a stale save; the version comes from show or list.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Delete a user or project category so the name inherits again.
    ///
    /// Tasks that already pinned the category keep their copy.
    Delete {
        /// Category name.
        #[arg(value_parser = nonempty)]
        name: String,
        /// The scope to delete from.
        #[arg(long, value_enum, default_value = "user")]
        scope: CategoryScope,
        /// Reject a stale delete.
        #[arg(long)]
        if_version: Option<i64>,
    },
    /// Show the effective prompt a role would get, with runtime-only parts labelled.
    ///
    /// With --task, the task's title, goal and criteria are filled in; otherwise an example
    /// task is used. The supervisor's rules are shown first because no category can edit
    /// them. Context and the worker callback only exist while a turn runs.
    Preview {
        /// Category name; with --task, defaults to the category the task pinned.
        #[arg(value_parser = nonempty, required_unless_present = "task")]
        name: Option<String>,
        /// Preview this task's content, and its pinned templates unless a name is given.
        #[arg(long, value_parser = positive)]
        task: Option<i64>,
        /// One role only; default all four.
        #[arg(long, value_enum)]
        role: Option<CategoryRole>,
        /// Preview one scope's definition instead of the effective one.
        #[arg(long, value_enum, conflicts_with = "task")]
        scope: Option<CategoryScope>,
    },
    /// List the placeholders templates may use.
    Placeholders,
}
