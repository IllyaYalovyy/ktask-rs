# Database schema V1

Sources: [initial schema](../migrations/0001_relational_state.sql),
[ordering](../src/storage/ordering.rs), [execution records](../src/storage/execution.rs),
[token normalization](../src/storage/usage.rs).

This is the unreleased first schema. Edit 0001 in place; no prior-schema conversions exist.

## Lifecycle and current task

| Record | States | Meaning |
| --- | --- | --- |
| Task | new, ready, blocked, succeeded, succeeded_override, failed_skipped, superseded, cancelled, deleted | Readiness and final resolution, NOT live execution state |
| Revision | draft, ready, rejected, retired | Approval of an exact definition, limits, and model configuration |
| Attempt | running, paused, waiting, succeeded, failed, interrupted, cancelled | Execution state; retry creates another attempt, resume does not |

task_details selects the newest **ready** revision. A newer draft appears separately as
pending_revision/title/goal and does not replace approved inputs. Rejected/retired revisions
are excluded. With no ready revision, the task remains visible but cannot run; its draft can
supply the display title. Attempts always retain their exact revision.

The displayed status combines task readiness/resolution with attempt state; it is a view, not
a second stored lifecycle. Running is pinned by the attempt. Otherwise current is the first
unfinished task in execution order: failure blocks later work, but insertion before non-running
current can make remediation current. New/unapproved current is never silently skipped.

Planner/remediator work uses ordinary tasks, not proposal tables. Approval is a human or AI
decision on the revision. An override/skip/supersede resolves the task without rewriting its
attempts. Escalation creates a new revision with new role settings.

## Waiting and limits

attempts.wait_reason distinguishes question, rate_limit, and provider_unavailable. A wait
records waiting_since_ms and optional resume_at_ms (earliest UTC resume time). A rate limit
normally suspends the same attempt; it is not a failed coding attempt. Question waits require
answers before resumption. Wait events retain the history after the current wait fields clear.

Each revision has attempt_limit and attempt_timeout_ms (positive, NULL = unlimited).
attempt_limit applies across ALL revisions of the task; escalation does not reset usage.
attempts_used counts succeeded attempts and failures in check/review/validation/agent/unknown.
Rate-limit, transport, environment, protocol, timeout, interruption, and cancellation do not
consume the coding-attempt budget. Number is an audit sequence, not budget usage.
Here timeout means an infrastructure/request timeout. Exhausting the active execution limit
is an agent failure and consumes budget; it must not be mislabeled as infrastructure.

attempt_timeout_ms limits active execution time for that attempt's exact revision. Paused and
waiting time never enters active_ms. Start checks the attempt budget; resume checks elapsed
active time and the wait deadline. The supervisor accounts active time with a monotonic clock,
stops work at the limit, and flushes terminal evidence after worker shutdown.

## Storage conventions

One SQLite file per instance state root; projects have independent queues. Project names are
nonblank and UNIQUE using SQLite BINARY comparison (case-sensitive). Production, development,
and tests never share roots. Every table is STRICT. PK = primary key, FK = foreign key,
UQ = uniqueness. Integer IDs are AUTOINCREMENT; schema versions are not. NULL means unknown or
absent, not zero. Timestamps/durations are integer milliseconds; money is integer micro-USD.

Every task, including History, has a non-null `order_key`, unique within its project. Keys are
opaque, canonical lowercase hexadecimal fractional strings with no trailing zero. BINARY
string comparison is execution order; keys are not task numbers or hierarchy. The allocator
handles prepend, append, nested insertion, and balanced batches without shifting other tasks.
It considers historical keys too, because they still occupy the ordering space.

All FKs are immediate NO ACTION for delete/update. There is no selected-revision cycle and
no workflow trigger. Shared commands own transitions and atomic multi-table writes; SQL owns
types, required fields, enums, reference integrity, and uniqueness. Direct SQL is not a command API.

## ERD

Each edge is an actual FK. Labels list composite columns. || = one, o| = zero or one,
o{ = zero or many. Composite uniqueness is retained only where an FK uses that exact scope.

### Definitions

```mermaid
erDiagram
    projects {
        INTEGER id PK
    }
    projects ||--o{ project_models : "project_id"
    projects ||--o{ project_checks : "project_id"
    projects ||--o{ tasks : "project_id"
    tasks ||--o{ task_revisions : "task_id"
    task_revisions ||--o{ revision_models : "task_id, revision"
    task_revisions ||--o{ acceptance_criteria : "task_id, revision"
    tasks ||--o{ task_links : "related_task_id, project_id"
    tasks ||--o{ task_links : "task_id, project_id"
    schema_migrations {
        INTEGER version PK
    }
```

### Execution

```mermaid
erDiagram
    task_revisions ||--o{ attempts : "task_id, revision"
    tasks ||--o{ attempts : "task_id, project_id"
    tasks o|--o{ ai_sessions : "task_id, project_id"
    projects ||--o{ ai_sessions : "project_id"
    project_checks o|--o{ attempt_steps : "project_check_id, project_id"
    ai_sessions o|--o{ attempt_steps : "session_id, project_id, task_id, role, provider"
    attempts ||--o{ attempt_steps : "attempt_id, project_id, task_id, revision"
    acceptance_criteria ||--o{ criterion_results : "task_id, revision, criterion_ordinal"
    attempt_steps ||--o{ criterion_results : "step_id, task_id, revision"
```

### Evidence

```mermaid
erDiagram
    ai_sessions o|--o{ model_calls : "session_id, project_id, provider"
    attempt_steps o|--o{ model_calls : "step_id, project_id, task_id, revision"
    task_revisions o|--o{ model_calls : "task_id, revision"
    tasks o|--o{ model_calls : "task_id, project_id"
    projects ||--o{ model_calls : "project_id"
    attempts ||--o{ questions : "attempt_id"
    attempts o|--o{ events : "attempt_id, task_id"
    tasks o|--o{ events : "task_id, project_id"
    projects ||--o{ events : "project_id"
    attempts ||--o{ attempt_output : "attempt_id"
    attempts ||--o{ artifacts : "attempt_id"
```

## Table definitions

Every column, PK, FK, unique constraint, and explicit index is listed. Integer PKs are non-null.

### acceptance_criteria

Ordered, nonblank acceptance criteria attached to an exact revision; ordinal is positive.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `ordinal` | INTEGER | no | Positive order within its parent. |
| `description` | TEXT | no | Required observable outcome, not a command. |

- PK `(task_id, revision, ordinal)`.
- FK `(task_id, revision)` → `task_revisions(task_id, revision)`.

### ai_sessions

Durable provider chat identity, independent of attempts and worker processes. Compatible retries may reuse a session; different roles have separate sessions. A project planning session need not belong to a task.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | yes | Task scope; NULL for project-wide planning. |
| `role` | TEXT | no | planning, execution, review, validation, remediation. |
| `provider` | TEXT | no | Provider/adapter identifier; never a credential. |
| `provider_scope` | TEXT | no | Nonsecret provider account/workspace namespace. |
| `provider_session_id` | TEXT | yes | Opaque resumable provider session ID; NULL until assigned. |
| `state` | TEXT | no | open, closed, unavailable. |
| `created_at_ms` | INTEGER | no | Creation time. |
| `last_used_at_ms` | INTEGER | no | Last use time, not before creation. |

- PK `(id)`.
- FK `(task_id, project_id)` → `tasks(id, project_id)`.
- FK `(project_id)` → `projects(id)`.
- Index `ix_sessions_task`: `INDEX ix_sessions_task ON ai_sessions(task_id, project_id)`.
- UQ `(id, project_id, task_id, role, provider)`.
- UQ `(id, project_id, provider)`.
- UQ `(provider, provider_scope, provider_session_id)`.

### artifacts

Retained files outside SQLite, owned by an attempt. Paths are unique within the instance artifact root. Commands validate containment and reject traversal/symlink escapes; pruning removes bytes and retains metadata.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `attempt_id` | INTEGER | no | Owning attempt. |
| `name` | TEXT | no | Nonblank name. |
| `media_type` | TEXT | no | Nonblank content type. |
| `relative_path` | TEXT | no | Path under the instance artifact root, never the project root. |
| `size_bytes` | INTEGER | no | Nonnegative retained byte count. |
| `created_at_ms` | INTEGER | no | Creation time. |
| `pruned_at_ms` | INTEGER | yes | Deletion time; NULL while retained. |

- PK `(id)`.
- FK `(attempt_id)` → `attempts(id)`.
- Index `ix_artifacts_attempt`: `INDEX ix_artifacts_attempt ON artifacts(attempt_id, id)`.
- UQ `(relative_path)`.

### attempt_output

Ordered output chunks, not one row per character. Writer assigns a positive monotonic sequence per attempt and batches chunks in short transactions.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `attempt_id` | INTEGER | no | Owning attempt. |
| `sequence` | INTEGER | no | Positive per-attempt output cursor. |
| `stream` | TEXT | no | stdout, stderr, system. |
| `recorded_at_ms` | INTEGER | no | Chunk capture time. |
| `content` | TEXT | no | Text chunk; no structured entity dump. |

- PK `(attempt_id, sequence)`.
- FK `(attempt_id)` → `attempts(id)`.

### attempt_steps

Ordered phases inside an attempt. AI phases record session/provider/model and optional effort; tool checks record command context. Terminal steps require finish time. The two composite parent keys enforce exact revision/project references in criterion_results and model_calls; attempt_id/ordinal enforces phase order.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `attempt_id` | INTEGER | no | Owning attempt. |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `ordinal` | INTEGER | no | Positive order within its parent. |
| `role` | TEXT | no | execution, check, review, validation, remediation. |
| `state` | TEXT | no | running, succeeded, failed, interrupted, skipped. |
| `session_id` | INTEGER | yes | Required for AI phases, absent for checks. |
| `provider` | TEXT | yes | Actual AI provider; absent for checks. |
| `model` | TEXT | yes | Actual AI model; absent for checks. |
| `effort` | TEXT | yes | Provider-supported effort; NULL means no effort setting, not an invented sentinel. |
| `invocation` | TEXT | yes | start, resume, nudge; absent for checks. |
| `project_check_id` | INTEGER | yes | Configured tool check, if applicable; actual command is also snapshotted. |
| `command` | TEXT | yes | Shell command executed by the supervisor. |
| `working_directory` | TEXT | yes | Working directory relative to the project root; validated by runner. |
| `timeout_ms` | INTEGER | yes | Positive execution timeout. |
| `exit_code` | INTEGER | yes | Tool exit status; NULL if no exit was observed. |
| `started_at_ms` | INTEGER | no | Start time. |
| `finished_at_ms` | INTEGER | yes | Finish time; NULL while active. |
| `report` | TEXT | yes | Human-readable summary/evidence; not a serialized entity. |

- PK `(id)`.
- FK `(project_check_id, project_id)` → `project_checks(id, project_id)`.
- FK `(session_id, project_id, task_id, role, provider)` → `ai_sessions(id, project_id, task_id, role, provider)`.
- FK `(attempt_id, project_id, task_id, revision)` → `attempts(id, project_id, task_id, revision)`.
- Index `ix_steps_check`: `INDEX ix_steps_check ON attempt_steps(project_check_id)`.
- Index `ix_steps_session`: `INDEX ix_steps_session ON attempt_steps(session_id)`.
- UQ `(id, task_id, revision)`.
- UQ `(id, project_id, task_id, revision)`.
- UQ `(attempt_id, ordinal)`.

### attempts

One execution against an exact revision; retry creates another row. This is the sole source of running/paused/waiting/failed status. One live attempt per task and one running attempt per project. Terminal attempts require finish time/outcome; failures/interruption also require a category. Waiting requires a reason and start time.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `number` | INTEGER | no | Positive attempt number unique per task. |
| `state` | TEXT | no | running, paused, waiting, succeeded, failed, interrupted, cancelled. |
| `execution_token` | TEXT | no | Unique invocation/fencing token for worker callbacks. |
| `started_at_ms` | INTEGER | no | Start time. |
| `finished_at_ms` | INTEGER | yes | Finish time; NULL while active. |
| `active_ms` | INTEGER | no | Accumulated active duration, nonnegative; default 0. |
| `wait_reason` | TEXT | yes | question, rate_limit, provider_unavailable; required only while waiting. |
| `waiting_since_ms` | INTEGER | yes | Start of the current wait; required only while waiting. |
| `resume_at_ms` | INTEGER | yes | Earliest resume time for a timed wait; NULL means no scheduled time. |
| `outcome` | TEXT | yes | Nonblank terminal attempt outcome. |
| `failure_category` | TEXT | yes | rate_limit, timeout, transport, environment, protocol, check, review, validation, agent, interrupted, unknown. |
| `base_commit` | TEXT | yes | Checkout commit before this attempt, if available. |
| `head_commit` | TEXT | yes | Resulting commit, if available. |
| `report` | TEXT | yes | Human-readable summary/evidence; not a serialized entity. |

- PK `(id)`.
- FK `(task_id, revision)` → `task_revisions(task_id, revision)`.
- FK `(task_id, project_id)` → `tasks(id, project_id)`.
- Index `ix_attempts_revision`: `INDEX ix_attempts_revision ON attempts(task_id, revision)`.
- Index `ix_attempts_project`: `INDEX ix_attempts_project ON attempts(project_id, id)`.
- Index `ux_running_attempt_project`: `UNIQUE INDEX ux_running_attempt_project ON attempts(project_id) WHERE state = 'running'`.
- Index `ux_live_attempt_task`: `UNIQUE INDEX ux_live_attempt_task ON attempts(task_id) WHERE state IN ('running', 'paused', 'waiting')`.
- UQ `(id, project_id, task_id, revision)`.
- UQ `(id, task_id)`.
- UQ `(task_id, number)`.
- UQ `(execution_token)`.

### criterion_results

A step's verdict and evidence for one criterion of the same revision. Terminal verdicts require nonblank evidence.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `step_id` | INTEGER | no | Step producing this verdict. |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `criterion_ordinal` | INTEGER | no | Exact acceptance criterion ordinal. |
| `state` | TEXT | no | running, passed, failed, skipped. |
| `evidence` | TEXT | yes | Observed result; mandatory for terminal verdicts. |

- PK `(step_id, criterion_ordinal)`.
- FK `(task_id, revision, criterion_ordinal)` → `acceptance_criteria(task_id, revision, ordinal)`.
- FK `(step_id, task_id, revision)` → `attempt_steps(id, task_id, revision)`.
- Index `ix_criteria_results_definition`: `INDEX ix_criteria_results_definition ON criterion_results(task_id, revision, criterion_ordinal)`.

### events

Append-only command/runner audit and activity feed. Task and attempt are optional, but an attempt requires its matching task. Commands append events atomically with state changes.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | yes | Optional task context in this project. |
| `attempt_id` | INTEGER | yes | Optional attempt of that task. |
| `occurred_at_ms` | INTEGER | no | Event time. |
| `kind` | TEXT | no | Namespaced event type, e.g. task.override. |
| `level` | TEXT | no | info, warning, error. |
| `message` | TEXT | no | Nonblank readable event detail. |

- PK `(id)`.
- FK `(attempt_id, task_id)` → `attempts(id, task_id)`.
- FK `(task_id, project_id)` → `tasks(id, project_id)`.
- FK `(project_id)` → `projects(id)`.
- Index `ix_events_attempt`: `INDEX ix_events_attempt ON events(attempt_id, id)`.
- Index `ix_events_task`: `INDEX ix_events_task ON events(task_id, id)`.
- Index `ix_events_project`: `INDEX ix_events_project ON events(project_id, id)`.

### model_calls

Billable provider requests, including project planning before a task exists. Input is always UNCACHED input; cache-read and cache-write are disjoint buckets. Running calls have no finish/error; failed/interrupted calls require both.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | yes | Task context, or NULL for project planning. |
| `revision` | INTEGER | yes | Exact task revision; NULL together with task_id. |
| `step_id` | INTEGER | yes | Attempt phase, if this call belongs to execution. |
| `session_id` | INTEGER | yes | Actual session when applicable; NULL for calls without a session. |
| `role` | TEXT | no | planning, execution, review, validation, remediation. |
| `provider` | TEXT | no | Provider/adapter identifier; never a credential. |
| `model` | TEXT | no | Explicit model identifier. |
| `effort` | TEXT | yes | Provider-supported effort; NULL means no effort setting, not an invented sentinel. |
| `request_key` | TEXT | no | Unique idempotency/correlation key. |
| `state` | TEXT | no | running, succeeded, failed, interrupted. |
| `started_at_ms` | INTEGER | no | Start time. |
| `finished_at_ms` | INTEGER | yes | Finish time; NULL while active. |
| `input_tokens` | INTEGER | yes | Uncached input tokens for every provider; NULL means unreported. |
| `cache_read_tokens` | INTEGER | yes | Tokens read from cache, disjoint from uncached input and cache writes. |
| `cache_write_tokens` | INTEGER | yes | Tokens written to cache, disjoint from uncached input and cache reads. |
| `output_tokens` | INTEGER | yes | Provider-reported output count; nullable, nonnegative. |
| `cost_microusd` | INTEGER | yes | Authoritative normalized cost, nonnegative; NULL means unreported. |
| `error` | TEXT | yes | Nonblank failure/interruption explanation; otherwise NULL. |

- PK `(id)`.
- FK `(session_id, project_id, provider)` → `ai_sessions(id, project_id, provider)`.
- FK `(step_id, project_id, task_id, revision)` → `attempt_steps(id, project_id, task_id, revision)`.
- FK `(task_id, revision)` → `task_revisions(task_id, revision)`.
- FK `(task_id, project_id)` → `tasks(id, project_id)`.
- FK `(project_id)` → `projects(id)`.
- Index `ix_calls_session`: `INDEX ix_calls_session ON model_calls(session_id)`.
- Index `ix_calls_step`: `INDEX ix_calls_step ON model_calls(step_id)`.
- Index `ix_calls_task`: `INDEX ix_calls_task ON model_calls(task_id, revision, id)`.
- Index `ix_calls_project`: `INDEX ix_calls_project ON model_calls(project_id, id)`.
- UQ `(request_key)`.

### project_checks

Configured tool commands (tests, Clippy, etc.), distinct from AI review and goal validation. A step snapshots the command actually executed.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `name` | TEXT | no | Nonblank name. |
| `command` | TEXT | no | Shell command executed by the supervisor. |
| `working_directory` | TEXT | no | Working directory relative to the project root; validated by runner. |
| `timeout_ms` | INTEGER | no | Positive execution timeout. |
| `enabled` | INTEGER | no | 0 disabled; 1 enabled. |

- PK `(id)`.
- FK `(project_id)` → `projects(id)`.
- UQ `(id, project_id)`.
- UQ `(project_id, name)`.

### project_models

Default model configuration per project role. Copy into each new revision; later default changes do not change existing tasks.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `project_id` | INTEGER | no | Owning project. |
| `role` | TEXT | no | planning, execution, review, validation, remediation. |
| `provider` | TEXT | no | Provider/adapter identifier; never a credential. |
| `model` | TEXT | no | Explicit model identifier. |
| `effort` | TEXT | yes | Provider-supported effort; NULL means no effort setting, not an invented sentinel. |

- PK `(project_id, role)`.
- FK `(project_id)` → `projects(id)`.

### projects

Registered checkout. Names and canonical native root paths are independently unique.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `name` | TEXT | no | Nonblank name. |
| `root_path` | BLOB | no | Canonical native absolute path bytes (not necessarily UTF-8). |
| `created_at_ms` | INTEGER | no | Creation time. |

- PK `(id)`.
- UQ `(root_path)`.
- UQ `(name)`.

### questions

Human intervention requested during an attempt. Open has no answer/resolution; answered requires answer, actor, and resolution time; cancelled has resolution time only.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `attempt_id` | INTEGER | no | Owning attempt. |
| `prompt` | TEXT | no | Nonblank question. |
| `state` | TEXT | no | open, answered, cancelled. |
| `answer` | TEXT | yes | Nonblank answer for answered state only. |
| `answered_by` | TEXT | yes | Answering actor identifier. |
| `created_at_ms` | INTEGER | no | Creation time. |
| `resolved_at_ms` | INTEGER | yes | Answer/cancellation time, not before creation. |

- PK `(id)`.
- FK `(attempt_id)` → `attempts(id)`.
- Index `ix_questions_attempt`: `INDEX ix_questions_attempt ON questions(attempt_id, id)`.

### revision_models

Effective model/effort per role for one exact revision. AI approval/start requires all four roles. No implicit live inheritance from project defaults.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `role` | TEXT | no | execution, review, validation, remediation. |
| `provider` | TEXT | no | Provider/adapter identifier; never a credential. |
| `model` | TEXT | no | Explicit model identifier. |
| `effort` | TEXT | yes | Provider-supported effort; NULL means no effort setting, not an invented sentinel. |

- PK `(task_id, revision, role)`.
- FK `(task_id, revision)` → `task_revisions(task_id, revision)`.

### schema_migrations

Immutable migration ledger. Exact applied SQL detects changed migration files; version is positive and name unique.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `version` | INTEGER | no | Positive migration number, not AUTOINCREMENT. |
| `name` | TEXT | no | Nonblank name. |
| `sql` | TEXT | no | Exact applied migration SQL. |
| `applied_at_ms` | INTEGER | no | Application timestamp. |

- PK `(version)`.
- UQ `(name)`.

### task_links

Directed relationships between ordinary tasks in the same project. Self-links are forbidden; dependency cycles are rejected by commands.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `project_id` | INTEGER | no | Owning project. |
| `task_id` | INTEGER | no | Owning task. |
| `related_task_id` | INTEGER | no | Target of the directed relationship. |
| `kind` | TEXT | no | repairs, replaces, depends_on, derived_from. |

- PK `(task_id, related_task_id, kind)`.
- FK `(related_task_id, project_id)` → `tasks(id, project_id)`.
- FK `(task_id, project_id)` → `tasks(id, project_id)`.
- Index `ix_task_links_related`: `INDEX ix_task_links_related ON task_links(related_task_id, project_id)`.

### task_revisions

Versioned definition, limits, and approval decision. Select the newest ready revision; expose a newer draft separately. Rejected/retired revisions never mask a usable revision. Commands forbid editing definitions already used by an attempt.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `task_id` | INTEGER | no | Owning task. |
| `revision` | INTEGER | no | Exact task revision number. |
| `lifecycle` | TEXT | no | draft, ready, rejected, retired. |
| `kind` | TEXT | no | ai or human. |
| `title` | TEXT | no | Nonblank task title. |
| `goal` | TEXT | no | Nonblank task goal. |
| `change_reason` | TEXT | yes | Why this revision was created. |
| `created_by_kind` | TEXT | no | human, ai, or system; creator attribution, including drafts. |
| `created_by` | TEXT | no | Nonblank creator identifier. |
| `attempt_limit` | INTEGER | yes | Maximum charged attempts for the task across revisions; positive, NULL = unlimited. |
| `attempt_timeout_ms` | INTEGER | yes | Active-time limit per attempt in milliseconds; positive, NULL = unlimited. Waiting/paused time is excluded. |
| `created_at_ms` | INTEGER | no | Creation time. |
| `decided_at_ms` | INTEGER | yes | Decision time, not before creation. |
| `decided_by_kind` | TEXT | yes | human, ai, system; NULL for draft. |
| `decided_by` | TEXT | yes | Decision actor identifier; NULL for draft. |

- PK `(task_id, revision)`.
- FK `(task_id)` → `tasks(id)`.

### tasks

Stable task identity, readiness, and final resolution. Execution status exists only on attempts; terminal lifecycle and closed timestamp must agree. Overrides/skips require actor and reason.

| Column | Type | Nullable | Definition |
| --- | --- | --- | --- |
| `id` | INTEGER | no | Stable row ID. |
| `project_id` | INTEGER | no | Owning project. |
| `order_key` | TEXT | no | Canonical fractional string, BINARY execution order; never NULL. |
| `lifecycle` | TEXT | no | new, ready, blocked, succeeded, succeeded_override, failed_skipped, superseded, cancelled, deleted. |
| `origin` | TEXT | no | human, planner, remediator, import. |
| `row_version` | INTEGER | no | Positive optimistic concurrency version, default 1. |
| `created_at_ms` | INTEGER | no | Creation time. |
| `updated_at_ms` | INTEGER | no | Last command time; not before creation. |
| `closed_at_ms` | INTEGER | yes | Terminal resolution time; NULL for Queue. |
| `resolution_reason` | TEXT | yes | Operator/agent reason; mandatory for override and skip. |
| `resolved_by_kind` | TEXT | yes | human, ai, system; paired with resolved_by. |
| `resolved_by` | TEXT | yes | Actor identifier; mandatory with actor kind. |

- PK `(id)`.
- FK `(project_id)` → `projects(id)`.
- Index `ix_tasks_history`: `INDEX ix_tasks_history ON tasks(project_id, closed_at_ms DESC, id DESC)`.
- UQ `(project_id, order_key)`.
- UQ `(id, project_id)`.

## Views

| View | Definition |
| --- | --- |
| `task_details` | Task + newest ready revision + newer pending draft + latest attempt; status and attempts_used are derived |
| `queue_tasks` | task_details with NULL closed_at_ms |
| `history_tasks` | task_details with non-NULL closed_at_ms |
| `current_tasks` | One queue row per project: running attempt first, otherwise smallest order_key |

revision is NULL if none is ready. pending_revision/title/goal show unapproved work separately.
attempt_revision identifies the actual running/historical inputs, even if a newer revision is
approved. Queries explicitly ORDER BY. Commands create task/definition and finish attempt/task
atomically; direct SQL is not the command API.

## Access patterns and sample SQL

Parameters are bound, not interpolated. NULL cursor means first page. Every write recipe
requires input/ownership checks and rollback on errors or unexpected affected-row counts.
Single-statement mutations are fragments of shared commands, not an invitation to bypass
auditing or row-version checks. Never hold a transaction during model/worker execution.

### Projects and queue

Canonicalize native path bytes before registration. Project names are unique, not display-only.

```sql
-- name: register-project
INSERT INTO projects(name, root_path, created_at_ms)
VALUES (:name, :root_path, :now_ms) RETURNING id;
```

```sql
-- name: list-projects
SELECT id, name, root_path FROM projects ORDER BY name, id;
```

Queue paging uses the unique project/order index. Substring search scans candidate rows;
SQLite lowercasing is not full Unicode folding. Mutation invalidates an order cursor: refresh
around the selected task ID. Search/filter never changes execution order.

```sql
-- name: queue-page
SELECT id, row_version, order_key, lifecycle, status, revision_lifecycle, pending_revision, kind, title
FROM queue_tasks WHERE project_id = :project_id
  AND (:after_key IS NULL OR order_key > :after_key)
  AND (:kind IS NULL OR kind = :kind)
  AND (:search = '' OR instr(lower(title), lower(:search)) > 0
       OR instr(CAST(id AS TEXT), :search) > 0)
ORDER BY order_key LIMIT :limit;
```

```sql
-- name: current-task
SELECT id, row_version, lifecycle, status, revision, revision_lifecycle, kind, title, goal,
       pending_revision, pending_title, pending_goal, latest_attempt_id, attempt_revision,
       wait_reason, waiting_since_ms, resume_at_ms, attempts_used, attempt_limit, attempt_timeout_ms
FROM current_tasks WHERE project_id = :project_id;
```

```sql
-- name: history-page
SELECT id, title, lifecycle, order_key, closed_at_ms FROM history_tasks
WHERE project_id = :project_id
  AND (:before_ms IS NULL OR (closed_at_ms, id) < (:before_ms, :before_id))
ORDER BY closed_at_ms DESC, id DESC LIMIT :limit;
```

### Create ordinary planned work and approve it

Capture the first returned ID as :task_id. Choose :order_key through keys_before in the same
immediate transaction; do not invent strings or renumber the queue. Insert every required model
role before approval (execution, review, validation, remediation for AI tasks). Trusted planners
can perform creation and approval in one command. Import repeats creation under one transaction.

```sql
-- name: create-task
BEGIN IMMEDIATE;
INSERT INTO tasks(project_id, order_key, lifecycle, origin, created_at_ms, updated_at_ms)
VALUES (:project_id, :order_key, 'new', :origin, :now_ms, :now_ms) RETURNING id;
INSERT INTO task_revisions(task_id, revision, lifecycle, kind, title, goal, created_by_kind, created_by, attempt_limit, attempt_timeout_ms, created_at_ms)
VALUES (:task_id, 1, 'draft', :kind, :title, :goal, :actor_kind, :actor, :attempt_limit, :attempt_timeout_ms, :now_ms);
INSERT INTO acceptance_criteria VALUES (:task_id, 1, 1, :criterion);
INSERT INTO revision_models(task_id, revision, role, provider, model, effort)
SELECT :task_id, 1, role, provider, model, effort FROM project_models
WHERE project_id = :project_id AND role <> 'planning';
INSERT INTO events(project_id, task_id, occurred_at_ms, kind, level, message)
VALUES (:project_id, :task_id, :now_ms, 'task.created', 'info', :message);
COMMIT;
```

Approval validates criteria, model support, required role coverage, latest revision, and expected
task version under the write lock. Require one row from each UPDATE. An AI approver is recorded
explicitly; approving a definition is not reporting a successful execution.

```sql
-- name: approve-revision
BEGIN IMMEDIATE;
UPDATE task_revisions SET lifecycle = 'ready', decided_at_ms = :now_ms,
    decided_by_kind = :actor_kind, decided_by = :actor
WHERE task_id = :task_id AND revision = :revision AND lifecycle = 'draft'
RETURNING revision;
UPDATE tasks SET lifecycle = 'ready', row_version = row_version + 1, updated_at_ms = :now_ms
WHERE id = :task_id AND row_version = :expected_version AND lifecycle = 'new'
RETURNING id;
INSERT INTO events(project_id, task_id, occurred_at_ms, kind, level, message)
VALUES (:project_id, :task_id, :now_ms, 'revision.approved', 'info', :message);
COMMIT;
```

### Model settings, escalation, and attempt history

Never edit a revision already used by an attempt. Escalation copies the definition, criteria,
and role settings into revision N+1, changes the execution model/effort there, records its
approval, and starts another attempt. The displayed failure remains until execution resumes
or an explicit terminal resolution is chosen; tasks.lifecycle does not duplicate that failure. Do not overwrite attempt N or its configuration.

```sql
-- name: revision-models
SELECT role, provider, model, effort FROM revision_models
WHERE task_id = :task_id AND revision = :revision ORDER BY role;
```

```sql
-- name: task-attempts
SELECT a.id, a.number, a.state, a.revision, r.title, r.goal,
       a.failure_category, a.outcome, a.base_commit, a.head_commit, a.report, a.active_ms
FROM attempts a JOIN task_revisions r ON r.task_id = a.task_id AND r.revision = a.revision
WHERE a.task_id = :task_id ORDER BY a.number DESC;
```

```sql
-- name: attempt-steps
SELECT id, ordinal, role, state, session_id, provider, model, effort, invocation,
       command, exit_code, report FROM attempt_steps
WHERE attempt_id = :attempt_id ORDER BY ordinal;
```

### Starting execution and resolving failures

Database::start_attempt records the start under an immediate transaction, checking current task,
version, readiness, role coverage, criteria, and attempt budget. It creates the attempt and event
and increments the task row version; it does not copy execution state into tasks. wait_attempt
and resume_attempt record timed waits/resumption on the same attempt. These are storage commands,
not a worker runner or permission to bypass future workspace safety checks.

An operator resolution requires a non-running current task, valid actor/reason, and no live
worker. Require exactly one updated row. Keep the failed attempt unchanged. Equivalent commands
can choose failed_skipped or superseded; all terminal tasks retain their order keys.

```sql
-- name: override-failure
BEGIN IMMEDIATE;
UPDATE tasks SET lifecycle = 'succeeded_override', closed_at_ms = :now_ms,
    updated_at_ms = :now_ms, row_version = row_version + 1,
    resolution_reason = :reason, resolved_by_kind = :actor_kind, resolved_by = :actor
WHERE id = :task_id AND row_version = :expected_version AND lifecycle = 'ready'
  AND id IN (SELECT id FROM current_tasks WHERE project_id = :project_id AND status = 'failed')
RETURNING id;
INSERT INTO events(project_id, task_id, occurred_at_ms, kind, level, message)
VALUES (:project_id, :task_id, :now_ms, 'task.override', 'warning', :reason);
COMMIT;
```

Ordinary completion also requires actual automated check evidence, independent review, goal
validation, resolved questions, and current supervisor ownership. After those checks, finish
the attempt and task and append the event in ONE transaction. Do not treat an agent's report
as permission to skip these checks. Failure finishes only the attempt; the view reports failure without copying it onto tasks.

```sql
-- name: finish-success
BEGIN IMMEDIATE;
UPDATE attempts SET state = 'succeeded', finished_at_ms = :now_ms, outcome = :outcome
WHERE id = :attempt_id AND task_id = :task_id AND state = 'running'
  AND execution_token = :execution_token RETURNING id;
UPDATE tasks SET lifecycle = 'succeeded', closed_at_ms = :now_ms,
    updated_at_ms = :now_ms, row_version = row_version + 1
WHERE id = :task_id AND project_id = :project_id AND lifecycle = 'ready'
  AND row_version = :expected_version RETURNING id;
INSERT INTO events(project_id, task_id, attempt_id, occurred_at_ms, kind, level, message)
VALUES (:project_id, :task_id, :attempt_id, :now_ms, 'task.completed', 'info', :outcome);
COMMIT;
```

### Tool checks versus AI validation

Project checks are supervisor-run commands (working directory and timeout are explicit).
Each actual invocation records its command snapshot and result in a check step. Prose criteria
are evaluated by validation steps with criterion_results evidence, not process exit codes.
Changing a project command must not alter the command snapshot in an existing step.

```sql
-- name: project-checks
SELECT id, name, command, working_directory, timeout_ms FROM project_checks
WHERE project_id = :project_id AND enabled = 1 ORDER BY id;
```

```sql
-- name: criterion-evidence
SELECT s.id AS step_id, s.model, s.effort, c.ordinal, c.description, result.state, result.evidence
FROM attempt_steps s
JOIN acceptance_criteria c ON c.task_id = s.task_id AND c.revision = s.revision
LEFT JOIN criterion_results result ON result.step_id = s.id AND result.criterion_ordinal = c.ordinal
WHERE s.attempt_id = :attempt_id AND s.role = 'validation'
ORDER BY s.ordinal, c.ordinal;
```

### Session reuse and project-level planning calls

A retry can reuse the same execution session with a new attempt and invocation=resume. A small
protocol nudge can add a step to the same attempt with invocation=nudge. Review/validation use
separate role-owned conversations. Compatibility and provider resume support must be checked;
fallback to a fresh session is explicit and audited. Never silently reuse another task's session.

```sql
-- name: reusable-sessions
SELECT id, provider, provider_scope, provider_session_id, last_used_at_ms
FROM ai_sessions WHERE project_id = :project_id AND task_id = :task_id AND role = :role
  AND state = 'open' AND provider_session_id IS NOT NULL
ORDER BY last_used_at_ms DESC, id DESC;
```

```sql
-- name: session-attempts
SELECT s.attempt_id, s.ordinal, s.role, s.invocation, s.model, s.effort, s.state
FROM attempt_steps s WHERE s.session_id = :session_id ORDER BY s.id;
```

Goal planning has a project but no task yet. Every provider uses three disjoint input buckets:
input_tokens = **uncached**, cache_read_tokens = cache reads, cache_write_tokens = cache writes.
Total input = their sum. An inclusive provider report is converted by subtracting both cache
buckets; an already-exclusive report is stored directly. usage::TokenUsage implements both
conversions and rejects negative/inconsistent counts. NULL means unreported; known absence
is zero. If conversion lacks a required counter, uncached input remains unknown, never guessed.
Provider adapters are not connected yet; they must use this normalization contract.

```sql
-- name: start-planning-call
INSERT INTO model_calls(project_id, role, provider, model, effort, request_key, state, started_at_ms)
VALUES (:project_id, 'planning', :provider, :model, :effort, :request_key, 'running', :now_ms)
RETURNING id;
```

```sql
-- name: finish-model-call
UPDATE model_calls SET state = 'succeeded', finished_at_ms = :now_ms,
    input_tokens = :input_tokens, cache_read_tokens = :cache_read_tokens,
    cache_write_tokens = :cache_write_tokens, output_tokens = :output_tokens,
    cost_microusd = :cost_microusd
WHERE id = :call_id AND state = 'running' RETURNING id;
```

### Events, output, questions, and retained artifacts

Use cursor indexes rather than OFFSET or timestamps. Batch output writes (target roughly
250 ms, with immediate terminal flush); FULL synchronization is not a per-line logging strategy.
Set retention quotas in the runner before enabling unattended execution. Keep summaries,
failure evidence, and preservation checkpoints; do not delete another live run's data.

```sql
-- name: event-tail
SELECT id, task_id, attempt_id, kind, level, message FROM events
WHERE project_id = :project_id AND id > :after_event_id ORDER BY id LIMIT :limit;
```

```sql
-- name: output-tail
SELECT sequence, stream, content FROM attempt_output
WHERE attempt_id = :attempt_id AND sequence > :after_sequence ORDER BY sequence LIMIT :limit;
```

```sql
-- name: open-questions
SELECT id, prompt FROM questions WHERE attempt_id = :attempt_id AND state = 'open' ORDER BY id;
```

```sql
-- name: attempt-artifacts
SELECT id, name, relative_path, size_bytes, pruned_at_ms
FROM artifacts WHERE attempt_id = :attempt_id ORDER BY id;
```

### Statistics and export

Aggregate relations separately to avoid multiplying costs across steps/checks. Project calls
include planning before task creation. Unknown-cost calls remain visible, not silently free.

```sql
-- name: token-totals
SELECT coalesce(sum(input_tokens + cache_read_tokens + cache_write_tokens), 0) AS known_total_input,
       count(*) FILTER (WHERE input_tokens IS NULL OR cache_read_tokens IS NULL
                              OR cache_write_tokens IS NULL) AS unreported_input_calls
FROM model_calls WHERE project_id = :project_id;
```

The known total includes only calls with all input buckets reported. Always display the
unreported-call count alongside it; missing usage is not zero usage.

```sql
-- name: project-statistics
WITH work AS (
  SELECT count(*) AS tasks, count(*) FILTER (WHERE closed_at_ms IS NULL) AS queued,
    count(*) FILTER (WHERE lifecycle = 'succeeded') AS succeeded,
    count(*) FILTER (WHERE lifecycle = 'succeeded_override') AS overridden,
    count(*) FILTER (WHERE lifecycle = 'failed_skipped') AS skipped
  FROM tasks WHERE project_id = :project_id
), runs AS (
  SELECT count(*) FILTER (WHERE state = 'failed') AS failures, coalesce(sum(active_ms), 0) AS active_ms
  FROM attempts WHERE project_id = :project_id
), usage AS (
  SELECT coalesce(sum(cost_microusd), 0) AS known_cost_microusd,
    count(*) FILTER (WHERE cost_microusd IS NULL) AS unreported_calls
  FROM model_calls WHERE project_id = :project_id
)
SELECT * FROM work CROSS JOIN runs CROSS JOIN usage;
```

Export under a read transaction. Group repeated criteria by task ID. This is a definition
export, not a database backup; a full portable task format must also include revision_models.

```sql
-- name: export-definitions
SELECT t.id, t.order_key, t.lifecycle, t.revision, t.kind, t.title, t.goal, c.ordinal, c.description
FROM task_details t LEFT JOIN acceptance_criteria c ON c.task_id = t.id AND c.revision = t.revision
WHERE t.project_id = :project_id
  AND (:scope = 'all' OR (:scope = 'queue' AND t.closed_at_ms IS NULL)
       OR (:scope = 'history' AND t.closed_at_ms IS NOT NULL))
ORDER BY t.order_key, c.ordinal;
```

## Workspace and session requirements

The implemented Linux supervisor uses kernel repository/worker leases, not SQL process guesses.
It requires a clean Git baseline for new work, preserves failed/paused changes for same-task retries,
and commits verified success before advancing. A guardian stops/reaps descendants and removes owned
TMPDIR before releasing the worker lease. It also shuts workers down on supervisor EOF/crash.
Never reset/delete user work to satisfy cleanup. Installed production supervises source builds;
development and tests use independent application roots/private test checkouts.

Codex sessions are reused only for matching project/task/role/provider/scope/model/effort. Attempts
pin revisions; pause/question/rate-limit resumption retains the attempt. Rate limits record a
60-second backoff, not a provider reset estimate; queue mode automatically resumes when eligible.
Waiting time is excluded from active time. Infrastructure failures do not spend the coding budget.
Execution/remediation, supervisor checks, independent review and validation create separate steps.
Structured criterion evidence is required before success; process exit zero alone is insufficient.

## Schema initialization and implementation boundary

0001 is the sole, unreleased V1 definition. There is no historical conversion path, legacy check
role, migration origin, or nullable handoff placeholder. Keep design edits in 0001 until release.
Existing incompatible development databases are rejected, never silently reset or converted.
No database file is deleted by initialization.

Normal connections use FK enforcement, WAL, FULL synchronization, and a bounded busy timeout.
Schema application is atomic and checked with foreign_key_check. The generic migration loader
remains for future released upgrades; its tests use synthetic changes, not fictitious mk3 history.

Implemented: schema initialization, ordering, execution-record guards, token normalization,
and TUI persistence through `application::Service`. Commands atomically create tasks/revisions,
approve/reject drafts, move, soft-delete/restore, acknowledge Human tasks, resolve failures,
and import definitions. Edits require the version captured when the form opened. Stale edits
fail without overwriting newer data; windows refresh on external SQLite commits.

The canonical working directory selects a project; its name defaults to the directory name.
`--project-name NAME` resolves a name collision when registering a new root. Each executable
still owns its separate instance database. Normal startup does not seed tasks or model settings.
The TUI is a read projection, never a database payload. Snapshot reads use one transaction;
attempt inputs and evidence remain immutable when resolving or revising tasks. Human completion
does not fabricate an AI attempt. Deletion is a retained `deleted` row, hidden from Queue/History;
the immediate Undo command restores it with a version check and a running-current order guard.

The task CLI reuses these commands and version guards, with explicit human/AI attribution.
Its read commands select an existing project; add/import can register one. Deleted rows remain
inspectable/restorable by ID within their project. JSON is an API representation, not storage.

`--demo` retains session-only fixtures and never opens SQLite. Real orchestration uses the same
schema without changes. CLI/TUI request controls; only the supervisor finalizes running attempts
after worker cleanup. Check commands and defaults are configured through shared application commands.
Still not connected: other providers and output retention.

Documentation tests check every table/column/PK/FK/index and ERD relationship against SQLite,
prepare all SQL examples, and execute representative workflows. Run make check.
