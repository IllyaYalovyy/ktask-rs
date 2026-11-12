-- Unreleased V1 schema. Edit this definition in place until the first release.
-- No historical data conversions or compatibility-only fields.

-- Named role/model/effort mappings shared by every project in the instance. A profile only
-- maps roles to models the existing provider adapters run; it never names a new provider.
CREATE TABLE model_profiles (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE COLLATE NOCASE CHECK (length(trim(name)) > 0 AND length(name) <= 64),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK (row_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

CREATE TABLE model_profile_roles (
    profile_id INTEGER NOT NULL REFERENCES model_profiles(id),
    role TEXT NOT NULL CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0),
    PRIMARY KEY (profile_id, role)
) STRICT;

CREATE TABLE projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE CHECK (length(trim(name)) > 0),
    root_path BLOB NOT NULL UNIQUE CHECK (length(root_path) > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    -- A task starts only after the project checks pass on its clean starting commit.
    check_before_task INTEGER NOT NULL DEFAULT 1 CHECK (check_before_task IN (0, 1)),
    -- Writable agent turns may use the network (offline mode requires provider enforcement).
    agent_network INTEGER NOT NULL DEFAULT 0 CHECK (agent_network IN (0, 1)),
    -- Run mode and planner approval the project started with. They apply until the project
    -- saves its own preferences file; later changes to the user defaults never reach it.
    initial_continuation TEXT NOT NULL DEFAULT 'queue' CHECK (initial_continuation IN ('single', 'queue')),
    initial_planner_approval TEXT NOT NULL DEFAULT 'automatic' CHECK (initial_planner_approval IN ('automatic', 'review_required')),
    -- The model profile this project inherits from; NULL inherits the user defaults alone.
    -- A profile a project selects cannot be deleted.
    profile_id INTEGER REFERENCES model_profiles(id)
) STRICT;

-- Project delivery is guarded independently of task and preference versions.
CREATE TABLE project_delivery (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK (row_version > 0),
    mode TEXT NOT NULL DEFAULT 'local' CHECK (mode IN ('local', 'direct', 'pr')),
    remote TEXT NOT NULL DEFAULT 'origin' CHECK (length(trim(remote)) > 0),
    base_branch TEXT NOT NULL DEFAULT 'main' CHECK (length(trim(base_branch)) > 0),
    branch TEXT CHECK (length(trim(branch)) > 0),
    owned_tip TEXT CHECK (length(owned_tip) IN (40,64))
) STRICT;

-- Execution intent is project scoped and survives terminals and interventions.
-- Attempt state remains solely on attempts; this records the scheduler's policy.
CREATE TABLE execution_sessions (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    mode TEXT NOT NULL CHECK (mode IN ('queue', 'one')),
    state TEXT NOT NULL CHECK (state IN ('running', 'waiting', 'resume_requested', 'paused', 'stopped', 'completed')),
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;

-- A control request belongs to the runner's lease, not to the issuing terminal.
CREATE TABLE pending_controls (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    action TEXT NOT NULL CHECK (action IN ('stop', 'done')),
    reason TEXT NOT NULL,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ai')),
    actor TEXT NOT NULL CHECK (length(trim(actor)) > 0),
    state TEXT NOT NULL CHECK (state IN ('requested', 'completed')),
    stash TEXT,
    stash_checked INTEGER NOT NULL DEFAULT 0 CHECK (stash_checked IN (0, 1)),
    completed_version INTEGER
) STRICT;

-- This project's own model overrides, the layer above its profile. A missing role inherits the
-- selected profile, then the user defaults. Resolved values are copied into revision_models,
-- never followed through a mutable reference.
CREATE TABLE project_models (
    project_id INTEGER NOT NULL REFERENCES projects(id),
    role TEXT NOT NULL CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0),
    PRIMARY KEY (project_id, role)
) STRICT;

-- Work categories: how to execute, review, validate and remediate a kind of work. A row with a
-- NULL project_id is user scope (every project of the instance); a row with a project_id
-- overrides the user scope and the built-in code, documents and art categories of that
-- project by name. Resolved values are copied into revision_categories, never followed.
CREATE TABLE work_categories (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER REFERENCES projects(id),
    name TEXT NOT NULL COLLATE NOCASE CHECK (length(trim(name)) > 0 AND length(name) <= 64),
    description TEXT NOT NULL DEFAULT '',
    -- Whether the project's checks gate this work: a baseline check and a check step.
    run_checks INTEGER NOT NULL CHECK (run_checks IN (0, 1)),
    execution_template TEXT NOT NULL CHECK (length(trim(execution_template)) > 0),
    review_template TEXT NOT NULL CHECK (length(trim(review_template)) > 0),
    validation_template TEXT NOT NULL CHECK (length(trim(validation_template)) > 0),
    remediation_template TEXT NOT NULL CHECK (length(trim(remediation_template)) > 0),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK (row_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= 0)
) STRICT;
CREATE UNIQUE INDEX ux_work_categories_user ON work_categories(name) WHERE project_id IS NULL;
CREATE UNIQUE INDEX ux_work_categories_project ON work_categories(project_id, name) WHERE project_id IS NOT NULL;

-- The user-wide model defaults, the lowest layer every project inherits.
CREATE TABLE default_models (
    role TEXT PRIMARY KEY CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0)
) STRICT;

-- Reusable settings a newly registered project starts with (one row, versioned). Never a
-- command, path, remote or branch: those belong to each project.
CREATE TABLE user_defaults (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    row_version INTEGER NOT NULL CHECK (row_version > 0),
    agent_network INTEGER NOT NULL CHECK (agent_network IN (0, 1)),
    check_before_task INTEGER NOT NULL CHECK (check_before_task IN (0, 1)),
    check_timeout_ms INTEGER NOT NULL CHECK (check_timeout_ms > 0),
    continuation TEXT NOT NULL CHECK (continuation IN ('single', 'queue')),
    planner_approval TEXT NOT NULL CHECK (planner_approval IN ('automatic', 'review_required')),
    delivery_mode TEXT NOT NULL CHECK (delivery_mode IN ('local', 'direct', 'pr')),
    -- The profile a newly registered project selects.
    default_profile_id INTEGER REFERENCES model_profiles(id)
) STRICT;

-- Commands are executed by the supervisor, never inferred from a model's claim.
CREATE TABLE project_checks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    command TEXT NOT NULL CHECK (length(trim(command)) > 0),
    working_directory TEXT NOT NULL CHECK (length(working_directory) > 0),
    timeout_ms INTEGER NOT NULL CHECK (timeout_ms > 0),
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    UNIQUE (project_id, name),
    UNIQUE (id, project_id)
) STRICT;

CREATE TABLE tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    order_key TEXT COLLATE BINARY NOT NULL CHECK (length(order_key) > 0
        AND order_key NOT GLOB '*[^0-9a-f]*' AND substr(order_key, -1) <> '0'),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('new', 'ready', 'blocked', 'succeeded', 'succeeded_override', 'failed_skipped', 'superseded', 'cancelled', 'deleted')),
    origin TEXT NOT NULL CHECK (origin IN ('human', 'planner', 'remediator', 'import', 'worker')),
    origin_task_id INTEGER REFERENCES tasks(id),
    origin_attempt_id INTEGER,
    recovery_root_id INTEGER REFERENCES tasks(id),
    automatic_replans INTEGER NOT NULL DEFAULT 0 CHECK (automatic_replans BETWEEN 0 AND 10),
    -- Set by Stop and by a decided revision: the next Run starts fresh, not from the last attempt.
    reset_pending INTEGER NOT NULL DEFAULT 0 CHECK (reset_pending IN (0, 1)),
    reset_worktree TEXT,
    -- Run and Retry restart the limits: only attempts numbered above this one count.
    counted_from INTEGER NOT NULL DEFAULT 0 CHECK (counted_from >= 0),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK (row_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    closed_at_ms INTEGER CHECK (closed_at_ms >= created_at_ms),
    resolution_reason TEXT,
    resolved_by_kind TEXT CHECK (resolved_by_kind IN ('human', 'ai', 'system')),
    resolved_by TEXT,
    UNIQUE (id, project_id),
    UNIQUE (project_id, order_key),
    FOREIGN KEY (origin_attempt_id, origin_task_id) REFERENCES attempts(id, task_id),
    CHECK ((origin = 'worker' AND origin_task_id IS NOT NULL AND origin_attempt_id IS NOT NULL)
        OR (origin <> 'worker' AND origin_task_id IS NULL AND origin_attempt_id IS NULL)),
    CHECK ((lifecycle IN ('new', 'ready', 'blocked') AND closed_at_ms IS NULL)
        OR (lifecycle IN ('succeeded', 'succeeded_override', 'failed_skipped', 'superseded', 'cancelled', 'deleted') AND closed_at_ms IS NOT NULL)),
    CHECK (lifecycle NOT IN ('succeeded_override', 'failed_skipped') OR
        (resolution_reason IS NOT NULL AND length(trim(resolution_reason)) > 0
         AND resolved_by_kind IS NOT NULL AND resolved_by IS NOT NULL AND length(trim(resolved_by)) > 0)),
    CHECK ((resolved_by_kind IS NULL AND resolved_by IS NULL) OR
        (resolved_by_kind IS NOT NULL AND resolved_by IS NOT NULL AND length(trim(resolved_by)) > 0))
) STRICT;

CREATE TABLE task_lifecycle_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    action TEXT NOT NULL CHECK (action IN ('closed', 'reopened')),
    outcome TEXT CHECK (outcome IN ('succeeded', 'succeeded_override', 'failed_skipped', 'superseded', 'cancelled')),
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('human', 'ai', 'system')),
    actor TEXT NOT NULL CHECK (length(trim(actor)) > 0),
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms >= 0),
    CHECK ((action = 'closed') = (outcome IS NOT NULL))
) STRICT;
CREATE INDEX ix_task_lifecycle_events_task ON task_lifecycle_events(task_id, id);

-- No selected-revision pointer/circular FK. Latest ready revision is derived; attempts pin inputs.
CREATE TABLE task_revisions (
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('draft', 'ready', 'rejected', 'retired')),
    kind TEXT NOT NULL CHECK (kind IN ('ai', 'human', 'docs')),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    goal TEXT NOT NULL CHECK (length(trim(goal)) > 0),
    change_reason TEXT,
    created_by_kind TEXT NOT NULL CHECK (created_by_kind IN ('human', 'ai', 'system')),
    created_by TEXT NOT NULL CHECK (length(trim(created_by)) > 0),
    attempt_limit INTEGER CHECK (attempt_limit > 0),
    attempt_timeout_ms INTEGER CHECK (attempt_timeout_ms > 0),
    stage_timeout_ms INTEGER CHECK (stage_timeout_ms > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    decided_at_ms INTEGER CHECK (decided_at_ms >= created_at_ms),
    decided_by_kind TEXT CHECK (decided_by_kind IN ('human', 'ai', 'system')),
    decided_by TEXT,
    PRIMARY KEY (task_id, revision),
    CHECK ((lifecycle = 'draft' AND decided_at_ms IS NULL AND decided_by_kind IS NULL AND decided_by IS NULL)
        OR (lifecycle <> 'draft' AND decided_at_ms IS NOT NULL AND decided_by_kind IS NOT NULL
            AND decided_by IS NOT NULL AND length(trim(decided_by)) > 0))
) STRICT;

CREATE TABLE revision_models (
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0),
    PRIMARY KEY (task_id, revision, role),
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions(task_id, revision)
) STRICT;

-- A category exactly as a revision pinned it: name, where it came from, whether checks gate the
-- work and the four role templates. Revisions share a snapshot while the category is unchanged,
-- so a long queue stores each distinct set of templates once. Snapshots are never changed.
CREATE TABLE category_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    digest TEXT NOT NULL UNIQUE CHECK (length(digest) = 64),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    scope TEXT NOT NULL CHECK (scope IN ('built_in', 'user', 'project')),
    run_checks INTEGER NOT NULL CHECK (run_checks IN (0, 1)),
    execution_template TEXT NOT NULL CHECK (length(trim(execution_template)) > 0),
    review_template TEXT NOT NULL CHECK (length(trim(review_template)) > 0),
    validation_template TEXT NOT NULL CHECK (length(trim(validation_template)) > 0),
    remediation_template TEXT NOT NULL CHECK (length(trim(remediation_template)) > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;

-- The category snapshot a revision was defined with. Later edits of the category never reach the
-- revision. A revision without a row (defined before categories existed, or Human work) runs the
-- built-in category of its kind, or none.
CREATE TABLE revision_categories (
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    snapshot_id INTEGER NOT NULL REFERENCES category_snapshots(id),
    PRIMARY KEY (task_id, revision),
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions(task_id, revision)
) WITHOUT ROWID, STRICT;

CREATE TABLE acceptance_criteria (
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    description TEXT NOT NULL CHECK (length(trim(description)) > 0),
    PRIMARY KEY (task_id, revision, ordinal),
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions(task_id, revision)
) STRICT;

-- Provenance/dependencies between normal tasks, not a second population of proposed tasks.
CREATE TABLE task_links (
    project_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    related_task_id INTEGER NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('repairs', 'replaces', 'depends_on', 'derived_from')),
    PRIMARY KEY (task_id, related_task_id, kind),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    FOREIGN KEY (related_task_id, project_id) REFERENCES tasks(id, project_id),
    CHECK (task_id <> related_task_id)
) STRICT;

CREATE TABLE attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    number INTEGER NOT NULL CHECK (number > 0),
    state TEXT NOT NULL CHECK (state IN ('running', 'paused', 'waiting', 'succeeded', 'failed', 'interrupted', 'cancelled')),
    execution_token TEXT NOT NULL UNIQUE CHECK (length(trim(execution_token)) > 0),
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    finished_at_ms INTEGER CHECK (finished_at_ms >= started_at_ms),
    active_ms INTEGER NOT NULL DEFAULT 0 CHECK (active_ms >= 0),
    rate_waits INTEGER NOT NULL DEFAULT 0 CHECK (rate_waits BETWEEN 0 AND 3),
    wait_reason TEXT CHECK (wait_reason IN ('question', 'rate_limit', 'provider_unavailable')),
    waiting_since_ms INTEGER CHECK (waiting_since_ms >= started_at_ms),
    resume_at_ms INTEGER CHECK (resume_at_ms >= waiting_since_ms),
    outcome TEXT,
    failure_category TEXT CHECK (failure_category IN ('rate_limit', 'timeout', 'transport', 'configuration', 'environment', 'protocol', 'check', 'review', 'validation', 'agent', 'interrupted', 'unknown')),
    base_commit TEXT,
    head_commit TEXT,
    report TEXT,
    CHECK ((state = 'waiting' AND wait_reason IS NOT NULL AND waiting_since_ms IS NOT NULL)
        OR (state <> 'waiting' AND wait_reason IS NULL AND waiting_since_ms IS NULL AND resume_at_ms IS NULL)),
    UNIQUE (task_id, number),
    UNIQUE (id, task_id),
    UNIQUE (id, project_id, task_id, revision),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions(task_id, revision),
    CHECK ((state IN ('running', 'paused', 'waiting') AND finished_at_ms IS NULL AND outcome IS NULL AND failure_category IS NULL)
        OR (state IN ('succeeded', 'cancelled') AND finished_at_ms IS NOT NULL AND outcome IS NOT NULL AND length(trim(outcome)) > 0 AND failure_category IS NULL)
        OR (state IN ('failed', 'interrupted') AND finished_at_ms IS NOT NULL AND outcome IS NOT NULL AND length(trim(outcome)) > 0 AND failure_category IS NOT NULL))
) STRICT;
-- A running attempt pins delivery policy and records each external boundary.
-- After a crash, the supervisor reads remote state before retrying a push or PR.
CREATE TABLE attempt_delivery (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    mode TEXT NOT NULL CHECK (mode IN ('local', 'direct', 'pr')),
    remote TEXT NOT NULL,
    destination_url TEXT NOT NULL CHECK (mode = 'local' OR length(trim(destination_url)) > 0),
    base_branch TEXT NOT NULL,
    branch TEXT,
    stage TEXT NOT NULL CHECK (stage IN ('preparing', 'work', 'checkpointing', 'checkpointed', 'integrating', 'verified', 'pushed', 'pr_ready')),
    checkpoint_head TEXT CHECK (length(checkpoint_head) IN (40,64)),
    fetched_base TEXT CHECK (length(fetched_base) IN (40,64)),
    pushed_head TEXT CHECK (length(pushed_head) IN (40,64)),
    pr_url TEXT,
    pending_head TEXT,
    pending_tip TEXT,
    handoff_target INTEGER REFERENCES tasks(id),
    handoff_claimed_by INTEGER REFERENCES attempts(id),
    repair_origin_task INTEGER REFERENCES tasks(id),
    sync_cycles INTEGER NOT NULL DEFAULT 0 CHECK (sync_cycles BETWEEN 0 AND 6),
    restart_sync INTEGER NOT NULL DEFAULT 0 CHECK (restart_sync IN (0,1)),
    retargeted INTEGER NOT NULL DEFAULT 0 CHECK (retargeted IN (0,1))
) STRICT;
CREATE UNIQUE INDEX ux_live_attempt_task ON attempts(task_id) WHERE state IN ('running', 'paused', 'waiting');
CREATE UNIQUE INDEX ux_running_attempt_project ON attempts(project_id) WHERE state = 'running';

-- Normal implementation/review/fix/validation progress survives supervisor restarts.
CREATE TABLE attempt_progress (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    repair_round INTEGER NOT NULL DEFAULT 0 CHECK (repair_round BETWEEN 0 AND 10),
    phase TEXT NOT NULL CHECK (phase IN ('baseline','implement','judge','replanned','fix','integrate','check','review','validate','decide','complete')),
    context TEXT NOT NULL DEFAULT '',
    evidence TEXT NOT NULL DEFAULT '',
    verified_tree TEXT NOT NULL DEFAULT '',
    reset_fresh INTEGER NOT NULL DEFAULT 0 CHECK (reset_fresh IN (0,1)),
    -- One readable line: why the current fix round runs (failing check or judge's reason).
    why TEXT NOT NULL DEFAULT ''
) STRICT;

-- Provider session != worker process != attempt. Waiting may have no process at all.
CREATE TABLE ai_sessions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    task_id INTEGER,
    role TEXT NOT NULL CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    provider_scope TEXT NOT NULL CHECK (length(trim(provider_scope)) > 0),
    provider_session_id TEXT CHECK (length(trim(provider_session_id)) > 0),
    state TEXT NOT NULL CHECK (state IN ('open', 'closed', 'unavailable')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    last_used_at_ms INTEGER NOT NULL CHECK (last_used_at_ms >= created_at_ms),
    UNIQUE (provider, provider_scope, provider_session_id),
    UNIQUE (id, project_id, provider),
    UNIQUE (id, project_id, task_id, role, provider),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id)
) STRICT;

CREATE TABLE attempt_steps (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL,
    project_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    role TEXT NOT NULL CHECK (role IN ('execution', 'check', 'review', 'validation', 'remediation')),
    state TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'changes_requested', 'failed', 'interrupted', 'skipped')),
    session_id INTEGER,
    provider TEXT,
    model TEXT,
    effort TEXT CHECK (length(trim(effort)) > 0),
    invocation TEXT CHECK (invocation IN ('start', 'resume', 'nudge')),
    project_check_id INTEGER,
    check_name TEXT,
    command TEXT,
    working_directory TEXT,
    timeout_ms INTEGER CHECK (timeout_ms > 0),
    source_tree TEXT CHECK (source_tree IS NULL OR length(source_tree) IN (40,64)),
    reused_from_step_id INTEGER REFERENCES attempt_steps(id),
    exit_code INTEGER,
    passed_count INTEGER CHECK (passed_count >= 0),
    failed_count INTEGER CHECK (failed_count >= 0),
    ignored_count INTEGER CHECK (ignored_count >= 0),
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    finished_at_ms INTEGER CHECK (finished_at_ms >= started_at_ms),
    report TEXT,
    UNIQUE (attempt_id, ordinal),
    UNIQUE (id, project_id, task_id, revision),
    UNIQUE (id, task_id, revision),
    FOREIGN KEY (attempt_id, project_id, task_id, revision) REFERENCES attempts(id, project_id, task_id, revision),
    FOREIGN KEY (session_id, project_id, task_id, role, provider) REFERENCES ai_sessions(id, project_id, task_id, role, provider),
    FOREIGN KEY (project_check_id, project_id) REFERENCES project_checks(id, project_id),
    CHECK ((state = 'running' AND finished_at_ms IS NULL) OR (state <> 'running' AND finished_at_ms IS NOT NULL)),
    CHECK ((passed_count IS NULL AND failed_count IS NULL AND ignored_count IS NULL)
        OR (role = 'check' AND passed_count IS NOT NULL AND failed_count IS NOT NULL AND ignored_count IS NOT NULL)),
    CHECK ((role IN ('execution', 'review', 'validation', 'remediation') AND session_id IS NOT NULL
                AND provider IS NOT NULL AND length(trim(provider)) > 0
                AND model IS NOT NULL AND length(trim(model)) > 0
                AND invocation IS NOT NULL
                AND project_check_id IS NULL AND check_name IS NULL AND command IS NULL AND working_directory IS NULL AND timeout_ms IS NULL AND source_tree IS NULL AND reused_from_step_id IS NULL AND exit_code IS NULL)
        OR (role = 'check' AND session_id IS NULL AND provider IS NULL AND model IS NULL AND effort IS NULL AND invocation IS NULL
                AND command IS NOT NULL AND length(trim(command)) > 0 AND working_directory IS NOT NULL AND timeout_ms IS NOT NULL))
) STRICT;

CREATE TABLE criterion_results (
    step_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    criterion_ordinal INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running', 'passed', 'needs_work', 'failed', 'skipped')),
    evidence TEXT,
    PRIMARY KEY (step_id, criterion_ordinal),
    FOREIGN KEY (step_id, task_id, revision) REFERENCES attempt_steps(id, task_id, revision),
    FOREIGN KEY (task_id, revision, criterion_ordinal) REFERENCES acceptance_criteria(task_id, revision, ordinal),
    CHECK (state = 'running' OR (evidence IS NOT NULL AND length(trim(evidence)) > 0))
) STRICT;

CREATE TABLE model_calls (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    task_id INTEGER,
    revision INTEGER,
    step_id INTEGER,
    session_id INTEGER,
    role TEXT NOT NULL CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0),
    request_key TEXT NOT NULL UNIQUE CHECK (length(trim(request_key)) > 0),
    state TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'failed', 'interrupted')),
    started_at_ms INTEGER NOT NULL CHECK (started_at_ms >= 0),
    finished_at_ms INTEGER CHECK (finished_at_ms >= started_at_ms),
    -- Uncached input for EVERY provider. Inclusive reports must be converted before insertion.
    input_tokens INTEGER CHECK (input_tokens >= 0),
    cache_read_tokens INTEGER CHECK (cache_read_tokens >= 0),
    cache_write_tokens INTEGER CHECK (cache_write_tokens >= 0),
    output_tokens INTEGER CHECK (output_tokens >= 0),
    cost_microusd INTEGER CHECK (cost_microusd >= 0),
    error TEXT,
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    FOREIGN KEY (task_id, revision) REFERENCES task_revisions(task_id, revision),
    FOREIGN KEY (step_id, project_id, task_id, revision) REFERENCES attempt_steps(id, project_id, task_id, revision),
    FOREIGN KEY (session_id, project_id, provider) REFERENCES ai_sessions(id, project_id, provider),
    CHECK ((task_id IS NULL AND revision IS NULL AND step_id IS NULL) OR (task_id IS NOT NULL AND revision IS NOT NULL)),
    CHECK ((state = 'running' AND finished_at_ms IS NULL AND error IS NULL)
        OR (state = 'succeeded' AND finished_at_ms IS NOT NULL AND error IS NULL)
        OR (state IN ('failed', 'interrupted') AND finished_at_ms IS NOT NULL AND error IS NOT NULL AND length(trim(error)) > 0))
) STRICT;

CREATE TABLE questions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    prompt TEXT NOT NULL CHECK (length(trim(prompt)) > 0),
    state TEXT NOT NULL CHECK (state IN ('open', 'answered', 'cancelled')),
    answer TEXT,
    answered_by TEXT,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    resolved_at_ms INTEGER CHECK (resolved_at_ms >= created_at_ms),
    CHECK ((state = 'open' AND answer IS NULL AND answered_by IS NULL AND resolved_at_ms IS NULL)
        OR (state = 'answered' AND answer IS NOT NULL AND length(trim(answer)) > 0 AND answered_by IS NOT NULL AND length(trim(answered_by)) > 0 AND resolved_at_ms IS NOT NULL)
        OR (state = 'cancelled' AND answer IS NULL AND answered_by IS NULL AND resolved_at_ms IS NOT NULL))
) STRICT;

CREATE TABLE events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project_id INTEGER NOT NULL REFERENCES projects(id),
    task_id INTEGER,
    attempt_id INTEGER,
    occurred_at_ms INTEGER NOT NULL CHECK (occurred_at_ms >= 0),
    kind TEXT NOT NULL CHECK (length(trim(kind)) > 0),
    level TEXT NOT NULL CHECK (level IN ('info', 'warning', 'error')),
    message TEXT NOT NULL CHECK (length(trim(message)) > 0),
    FOREIGN KEY (task_id, project_id) REFERENCES tasks(id, project_id),
    FOREIGN KEY (attempt_id, task_id) REFERENCES attempts(id, task_id),
    CHECK (attempt_id IS NULL OR task_id IS NOT NULL)
) STRICT;

CREATE TABLE attempt_output (
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    stream TEXT NOT NULL CHECK (stream IN ('stdout', 'stderr', 'system')),
    recorded_at_ms INTEGER NOT NULL CHECK (recorded_at_ms >= 0),
    content TEXT NOT NULL,
    PRIMARY KEY (attempt_id, sequence)
) STRICT;

-- Every attempt has a row: new attempts start at zero; attempts recorded before the
-- counters existed are backfilled from their retained rows when storage opens.
CREATE TABLE attempt_output_usage (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    rows INTEGER NOT NULL CHECK (rows >= 0)
) STRICT;

-- Counters follow every output mutation in the same transaction.
CREATE TRIGGER attempt_output_usage_insert AFTER INSERT ON attempt_output
BEGIN
    UPDATE attempt_output_usage SET
        bytes=bytes+length(CAST(NEW.content AS BLOB)), rows=rows+1
    WHERE attempt_id=NEW.attempt_id;
END;

CREATE TRIGGER attempt_output_usage_delete AFTER DELETE ON attempt_output
BEGIN
    UPDATE attempt_output_usage SET
        bytes=bytes-length(CAST(OLD.content AS BLOB)), rows=rows-1
    WHERE attempt_id=OLD.attempt_id;
END;

CREATE TRIGGER attempt_output_usage_update AFTER UPDATE OF content,attempt_id ON attempt_output
BEGIN
    UPDATE attempt_output_usage SET
        bytes=bytes-length(CAST(OLD.content AS BLOB)), rows=rows-1
    WHERE attempt_id=OLD.attempt_id AND OLD.attempt_id<>NEW.attempt_id;
    UPDATE attempt_output_usage SET
        bytes=bytes+length(CAST(NEW.content AS BLOB)), rows=rows+1
    WHERE attempt_id=NEW.attempt_id AND OLD.attempt_id<>NEW.attempt_id;
    UPDATE attempt_output_usage SET
        bytes=bytes+length(CAST(NEW.content AS BLOB))-length(CAST(OLD.content AS BLOB))
    WHERE attempt_id=NEW.attempt_id AND OLD.attempt_id=NEW.attempt_id;
END;

-- The history remains inspectable after output bytes are deliberately removed.
CREATE TABLE output_pruning (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    pruned_at_ms INTEGER NOT NULL CHECK (pruned_at_ms >= 0),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0)
) STRICT;

-- Bytes removed in bounded batches before an attempt's output is fully pruned.
CREATE TABLE output_pruning_progress (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    size_bytes INTEGER NOT NULL DEFAULT 0 CHECK (size_bytes >= 0),
    removed_records INTEGER NOT NULL DEFAULT 0 CHECK (removed_records >= 0)
) STRICT;

-- Attempts already admitted to retention remain eligible for directory cleanup
-- if the retention policy changes before cleanup finishes.
CREATE TABLE evidence_cleanup_admissions (
    attempt_id INTEGER PRIMARY KEY REFERENCES attempts(id),
    admitted_at_ms INTEGER NOT NULL CHECK (admitted_at_ms >= 0)
) STRICT;

CREATE TABLE project_retention (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    days INTEGER NOT NULL CHECK (days BETWEEN 0 AND 365000),
    version INTEGER NOT NULL CHECK (version > 0)
) STRICT;

-- Limits apply to the whole instance allocation because SQLite and its WAL are
-- shared by projects. Each project can choose when its closed evidence is shed.
CREATE TABLE project_storage_limits (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    max_bytes INTEGER NOT NULL CHECK (max_bytes BETWEEN 1048576 AND 1099511627776),
    max_files INTEGER NOT NULL CHECK (max_files BETWEEN 16 AND 10000000)
) STRICT;

-- A conservative instance-wide admission watermark. Writers advance it in the
-- same transaction as output or artifact reservations; housekeeping reconciles
-- it against physical allocation only if no writer changed its generation.
CREATE TABLE instance_storage_admission (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    projected_bytes INTEGER NOT NULL CHECK (projected_bytes >= 0),
    projected_files INTEGER NOT NULL CHECK (projected_files >= 0),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    known_bytes INTEGER NOT NULL DEFAULT 0 CHECK (known_bytes >= 0),
    known_files INTEGER NOT NULL DEFAULT 0 CHECK (known_files >= 0)
) STRICT;

-- Runner segments are probed by project, so their baselines must not be
-- compared with another project's logs or the shared SQLite/root baseline.
CREATE TABLE instance_runner_log_baselines (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    files INTEGER NOT NULL CHECK (files >= 0)
) STRICT;

-- Automatic accounting scans a bounded number of directory entries per turn.
-- Its cursor is durable. A walk in progress is never restarted: when it settles,
-- the claims admitted since its base (the projection at its start, less the claims
-- of reservations then pending) are added to the measurement, and a changed
-- admission generation schedules the next walk.
CREATE TABLE instance_allocation_scan (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    generation INTEGER NOT NULL CHECK (generation >= 0),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    frames_json TEXT NOT NULL,
    bytes INTEGER NOT NULL CHECK (bytes >= 0),
    files INTEGER NOT NULL CHECK (files >= 0),
    database_bytes INTEGER NOT NULL CHECK (database_bytes >= 0),
    wal_bytes INTEGER NOT NULL CHECK (wal_bytes >= 0),
    runner_log_bytes INTEGER NOT NULL CHECK (runner_log_bytes >= 0),
    evidence_bytes INTEGER NOT NULL CHECK (evidence_bytes >= 0),
    evidence_entries INTEGER NOT NULL CHECK (evidence_entries >= 0),
    directory_entries INTEGER NOT NULL CHECK (directory_entries >= 0),
    entries_visited INTEGER NOT NULL CHECK (entries_visited >= 0),
    complete INTEGER NOT NULL CHECK (complete IN (0,1,2)),
    base_bytes INTEGER NOT NULL DEFAULT 0,
    base_files INTEGER NOT NULL DEFAULT 0
) STRICT;

-- A bounded pass over closed attempts can resume without revisiting the same
-- preserved directories on every storage cleanup.
CREATE TABLE evidence_sweep (
    project_id INTEGER PRIMARY KEY REFERENCES projects(id),
    last_attempt_id INTEGER NOT NULL DEFAULT 0 CHECK (last_attempt_id >= 0),
    active_attempt_id INTEGER NOT NULL DEFAULT 0 CHECK (active_attempt_id >= 0),
    last_step_id INTEGER NOT NULL DEFAULT 0 CHECK (last_step_id >= 0),
    last_artifact_id INTEGER NOT NULL DEFAULT 0 CHECK (last_artifact_id >= 0),
    dirty INTEGER NOT NULL DEFAULT 0 CHECK (dirty IN (0,1)),
    complete INTEGER NOT NULL DEFAULT 0 CHECK (complete IN (0,1)),
    pressure INTEGER NOT NULL DEFAULT 0 CHECK (pressure IN (0,1)),
    next_eligible_at_ms INTEGER NOT NULL DEFAULT 0 CHECK (next_eligible_at_ms >= 0)
) STRICT;

-- Interactive read-only providers retain evidence without creating execution attempts.
CREATE TABLE evidence_pins (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) = 32 AND id NOT GLOB '*[^0-9a-f]*'),
    project_id INTEGER NOT NULL REFERENCES projects(id),
    directory_identity TEXT NOT NULL,
    owner_identity TEXT NOT NULL,
    guardian_identity TEXT NOT NULL
) STRICT;
CREATE INDEX ix_evidence_pins_project ON evidence_pins(project_id);

CREATE TABLE artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    media_type TEXT NOT NULL CHECK (length(trim(media_type)) > 0),
    relative_path TEXT NOT NULL UNIQUE CHECK (length(relative_path) > 0),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    checksum_sha256 TEXT CHECK (checksum_sha256 IS NULL OR length(checksum_sha256) = 64),
    source_tree TEXT CHECK (source_tree IS NULL OR length(source_tree) IN (40,64)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    pruning_requested_at_ms INTEGER CHECK (pruning_requested_at_ms >= created_at_ms),
    pruned_at_ms INTEGER CHECK (pruned_at_ms >= created_at_ms)
) STRICT;

-- A reservation is committed before bytes enter state storage. Recovery can finish
-- a complete copy or discard a partial one without losing its source attribution.
CREATE TABLE artifact_reservations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    name TEXT NOT NULL,
    media_type TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    checksum_sha256 TEXT NOT NULL CHECK (length(checksum_sha256) = 64),
    source_tree TEXT NOT NULL CHECK (length(source_tree) IN (40,64)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;

-- Eligibility can change behind a bounded sweep cursor between Storage applies.
-- Mark that pass stale; the next sweep restarts at the oldest attempt.
CREATE TRIGGER evidence_sweep_task_closed AFTER UPDATE OF closed_at_ms ON tasks
WHEN OLD.closed_at_ms IS NOT NEW.closed_at_ms
BEGIN
    UPDATE evidence_sweep SET dirty=1 WHERE project_id=NEW.project_id;
END;
CREATE TRIGGER evidence_sweep_reservation_released AFTER DELETE ON artifact_reservations
BEGIN
    UPDATE evidence_sweep SET dirty=1 WHERE project_id=(SELECT project_id FROM attempts WHERE id=OLD.attempt_id);
END;
CREATE TRIGGER evidence_sweep_admitted AFTER INSERT ON evidence_cleanup_admissions
BEGIN
    UPDATE evidence_sweep SET dirty=1 WHERE project_id=(SELECT project_id FROM attempts WHERE id=NEW.attempt_id);
END;
CREATE TRIGGER evidence_sweep_policy_added AFTER INSERT ON project_retention
BEGIN
    UPDATE evidence_sweep SET dirty=1 WHERE project_id=NEW.project_id;
END;
CREATE TRIGGER evidence_sweep_policy_changed AFTER UPDATE OF days,version ON project_retention
BEGIN
    UPDATE evidence_sweep SET dirty=1 WHERE project_id=NEW.project_id;
END;

CREATE INDEX ix_tasks_history ON tasks(project_id, closed_at_ms DESC, id DESC);
CREATE INDEX ix_tasks_origin ON tasks(origin_task_id, order_key) WHERE origin_task_id IS NOT NULL;
CREATE INDEX ix_revisions_ready ON task_revisions(task_id, lifecycle, revision DESC);
CREATE INDEX ix_task_links_related ON task_links(related_task_id, project_id);
CREATE INDEX ix_attempts_project ON attempts(project_id, id);
CREATE INDEX ix_attempts_revision ON attempts(task_id, revision);
CREATE INDEX ix_sessions_task ON ai_sessions(task_id, project_id);
CREATE INDEX ix_steps_session ON attempt_steps(session_id);
CREATE INDEX ix_steps_check ON attempt_steps(project_check_id);
CREATE INDEX ix_criteria_results_definition ON criterion_results(task_id, revision, criterion_ordinal);
CREATE INDEX ix_calls_project ON model_calls(project_id, id);
CREATE INDEX ix_calls_task ON model_calls(task_id, revision, id);
CREATE INDEX ix_calls_step ON model_calls(step_id);
CREATE INDEX ix_calls_session ON model_calls(session_id);
CREATE INDEX ix_questions_attempt ON questions(attempt_id, id);
CREATE INDEX ix_events_project ON events(project_id, id);
CREATE INDEX ix_events_task ON events(task_id, id);
CREATE INDEX ix_events_attempt ON events(attempt_id, id);
CREATE INDEX ix_artifacts_attempt ON artifacts(attempt_id, id);
CREATE INDEX ix_artifact_reservations_attempt ON artifact_reservations(attempt_id);

CREATE VIEW task_details AS
SELECT t.*, r.revision, r.lifecycle AS revision_lifecycle,
    coalesce(r.kind, draft.kind) AS kind, coalesce(r.title, draft.title) AS title,
    coalesce(r.goal, draft.goal) AS goal, r.attempt_limit, r.attempt_timeout_ms,
    draft.revision AS pending_revision, draft.title AS pending_title, draft.goal AS pending_goal,
    a.id AS latest_attempt_id, a.state AS attempt_state, a.revision AS attempt_revision,
    a.wait_reason, a.waiting_since_ms, a.resume_at_ms, a.active_ms,
    -- The task state machine (docs/USECASES.md): live attempt, then approval, then failure.
    CASE WHEN t.lifecycle NOT IN ('new', 'ready') THEN t.lifecycle
         WHEN a.state IN ('running', 'paused', 'waiting') THEN a.state
         WHEN r.revision IS NULL OR draft.revision IS NOT NULL THEN 'new'
         WHEN a.state = 'failed' AND t.reset_pending = 0 THEN 'failed'
         ELSE 'ready' END AS status,
    (SELECT count(*) FROM attempts spent WHERE spent.task_id = t.id AND spent.number > t.counted_from
        AND (spent.state = 'succeeded' OR (spent.state = 'failed'
            AND spent.failure_category IN ('check', 'review', 'validation', 'agent', 'unknown')))) AS attempts_used
FROM tasks t
LEFT JOIN task_revisions r ON r.task_id = t.id AND r.revision =
    (SELECT max(revision) FROM task_revisions WHERE task_id = t.id AND lifecycle = 'ready')
LEFT JOIN task_revisions draft ON draft.task_id = t.id AND draft.revision =
    (SELECT max(revision) FROM task_revisions WHERE task_id = t.id AND lifecycle = 'draft'
        AND revision > coalesce(r.revision, 0))
LEFT JOIN attempts a ON a.task_id = t.id AND a.number = (SELECT max(number) FROM attempts WHERE task_id = t.id);
CREATE VIEW queue_tasks AS SELECT * FROM task_details WHERE closed_at_ms IS NULL;
CREATE VIEW history_tasks AS SELECT * FROM task_details WHERE closed_at_ms IS NOT NULL;
-- Running work is pinned by its attempt, not a second mutable task-state field.
CREATE VIEW current_tasks AS
SELECT * FROM queue_tasks q WHERE q.id = (
    SELECT candidate.id FROM queue_tasks candidate WHERE candidate.project_id = q.project_id
    ORDER BY CASE WHEN candidate.attempt_state = 'running' THEN 0 ELSE 1 END, candidate.order_key LIMIT 1
);
