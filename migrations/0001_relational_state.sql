-- Unreleased V1 schema. Edit this definition in place until the first release.
-- No historical data conversions or compatibility-only fields.

CREATE TABLE projects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    name TEXT NOT NULL UNIQUE CHECK (length(trim(name)) > 0),
    root_path BLOB NOT NULL UNIQUE CHECK (length(root_path) > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0)
) STRICT;

-- Defaults are copied into revision_models, not followed through a mutable reference.
CREATE TABLE project_models (
    project_id INTEGER NOT NULL REFERENCES projects(id),
    role TEXT NOT NULL CHECK (role IN ('planning', 'execution', 'review', 'validation', 'remediation')),
    provider TEXT NOT NULL CHECK (length(trim(provider)) > 0),
    model TEXT NOT NULL CHECK (length(trim(model)) > 0),
    effort TEXT CHECK (length(trim(effort)) > 0),
    PRIMARY KEY (project_id, role)
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
    origin TEXT NOT NULL CHECK (origin IN ('human', 'planner', 'remediator', 'import')),
    row_version INTEGER NOT NULL DEFAULT 1 CHECK (row_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    closed_at_ms INTEGER CHECK (closed_at_ms >= created_at_ms),
    resolution_reason TEXT,
    resolved_by_kind TEXT CHECK (resolved_by_kind IN ('human', 'ai', 'system')),
    resolved_by TEXT,
    UNIQUE (id, project_id),
    UNIQUE (project_id, order_key),
    CHECK ((lifecycle IN ('new', 'ready', 'blocked') AND closed_at_ms IS NULL)
        OR (lifecycle IN ('succeeded', 'succeeded_override', 'failed_skipped', 'superseded', 'cancelled', 'deleted') AND closed_at_ms IS NOT NULL)),
    CHECK (lifecycle NOT IN ('succeeded_override', 'failed_skipped') OR
        (resolution_reason IS NOT NULL AND length(trim(resolution_reason)) > 0
         AND resolved_by_kind IS NOT NULL AND resolved_by IS NOT NULL AND length(trim(resolved_by)) > 0)),
    CHECK ((resolved_by_kind IS NULL AND resolved_by IS NULL) OR
        (resolved_by_kind IS NOT NULL AND resolved_by IS NOT NULL AND length(trim(resolved_by)) > 0))
) STRICT;

-- No selected-revision pointer/circular FK. Latest ready revision is derived; attempts pin inputs.
CREATE TABLE task_revisions (
    task_id INTEGER NOT NULL REFERENCES tasks(id),
    revision INTEGER NOT NULL CHECK (revision > 0),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('draft', 'ready', 'rejected', 'retired')),
    kind TEXT NOT NULL CHECK (kind IN ('ai', 'human')),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    goal TEXT NOT NULL CHECK (length(trim(goal)) > 0),
    change_reason TEXT,
    created_by_kind TEXT NOT NULL CHECK (created_by_kind IN ('human', 'ai', 'system')),
    created_by TEXT NOT NULL CHECK (length(trim(created_by)) > 0),
    attempt_limit INTEGER CHECK (attempt_limit > 0),
    attempt_timeout_ms INTEGER CHECK (attempt_timeout_ms > 0),
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
    wait_reason TEXT CHECK (wait_reason IN ('question', 'rate_limit', 'provider_unavailable')),
    waiting_since_ms INTEGER CHECK (waiting_since_ms >= started_at_ms),
    resume_at_ms INTEGER CHECK (resume_at_ms >= waiting_since_ms),
    outcome TEXT,
    failure_category TEXT CHECK (failure_category IN ('rate_limit', 'timeout', 'transport', 'environment', 'protocol', 'check', 'review', 'validation', 'agent', 'interrupted', 'unknown')),
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
CREATE UNIQUE INDEX ux_live_attempt_task ON attempts(task_id) WHERE state IN ('running', 'paused', 'waiting');
CREATE UNIQUE INDEX ux_running_attempt_project ON attempts(project_id) WHERE state = 'running';

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
    state TEXT NOT NULL CHECK (state IN ('running', 'succeeded', 'failed', 'interrupted', 'skipped')),
    session_id INTEGER,
    provider TEXT,
    model TEXT,
    effort TEXT CHECK (length(trim(effort)) > 0),
    invocation TEXT CHECK (invocation IN ('start', 'resume', 'nudge')),
    project_check_id INTEGER,
    command TEXT,
    working_directory TEXT,
    timeout_ms INTEGER CHECK (timeout_ms > 0),
    exit_code INTEGER,
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
    CHECK ((role IN ('execution', 'review', 'validation', 'remediation') AND session_id IS NOT NULL
                AND provider IS NOT NULL AND length(trim(provider)) > 0
                AND model IS NOT NULL AND length(trim(model)) > 0
                AND invocation IS NOT NULL
                AND project_check_id IS NULL AND command IS NULL AND working_directory IS NULL AND timeout_ms IS NULL AND exit_code IS NULL)
        OR (role = 'check' AND session_id IS NULL AND provider IS NULL AND model IS NULL AND effort IS NULL AND invocation IS NULL
                AND command IS NOT NULL AND length(trim(command)) > 0 AND working_directory IS NOT NULL AND timeout_ms IS NOT NULL))
) STRICT;

CREATE TABLE criterion_results (
    step_id INTEGER NOT NULL,
    task_id INTEGER NOT NULL,
    revision INTEGER NOT NULL,
    criterion_ordinal INTEGER NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('running', 'passed', 'failed', 'skipped')),
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

CREATE TABLE artifacts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id INTEGER NOT NULL REFERENCES attempts(id),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    media_type TEXT NOT NULL CHECK (length(trim(media_type)) > 0),
    relative_path TEXT NOT NULL UNIQUE CHECK (length(relative_path) > 0),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms >= 0),
    pruned_at_ms INTEGER CHECK (pruned_at_ms >= created_at_ms)
) STRICT;

CREATE INDEX ix_tasks_history ON tasks(project_id, closed_at_ms DESC, id DESC);
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

CREATE VIEW task_details AS
SELECT t.*, r.revision, r.lifecycle AS revision_lifecycle,
    coalesce(r.kind, draft.kind) AS kind, coalesce(r.title, draft.title) AS title,
    coalesce(r.goal, draft.goal) AS goal, r.attempt_limit, r.attempt_timeout_ms,
    draft.revision AS pending_revision, draft.title AS pending_title, draft.goal AS pending_goal,
    a.id AS latest_attempt_id, a.state AS attempt_state, a.revision AS attempt_revision,
    a.wait_reason, a.waiting_since_ms, a.resume_at_ms, a.active_ms,
    CASE WHEN a.state IN ('running', 'paused', 'waiting') THEN a.state
         WHEN t.lifecycle <> 'ready' THEN t.lifecycle
         WHEN a.state IS NOT NULL THEN a.state ELSE 'ready' END AS status,
    (SELECT count(*) FROM attempts spent WHERE spent.task_id = t.id
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
