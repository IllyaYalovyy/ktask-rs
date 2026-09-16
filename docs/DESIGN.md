# Concrete design

Binding decisions. Tasks implement this document; they do not revisit it. If a
task seems to require a choice not made here, that is a gap — report it as
NEEDS_INPUT rather than inventing an answer.

## Dependencies

Fixed. Do not add, remove or substitute a crate without an ADR. This exact set
has been resolved, built and checked against `deny.toml`: it compiles together
and every license is permitted.

```toml
rusqlite    = { version = "0.40", features = ["bundled"] }
serde       = { version = "1", features = ["derive"] }
serde_json  = "1"
toml        = "1"
clap        = { version = "4", features = ["derive", "env"] }
ratatui     = "0.30"
crossterm   = "0.29"
thiserror   = "2"
time        = { version = "0.3", features = ["formatting", "parsing", "macros", "serde"] }
sha2        = "0.10"
unicode-width = "0.2"
unicode-segmentation = "1.13"
regex       = "1.13"
nix         = { version = "0.31", features = ["signal", "fs", "process"] }
signal-hook = "0.4"

[dev-dependencies]
proptest    = "1"
insta       = "1"
tempfile    = "3"
```

`nix` and `signal-hook` exist so that `unsafe_code = "forbid"` can stay
absolute. Killing a process group, handling SIGINT, checking whether a pid is
alive and measuring free disk space all need syscalls and none has a safe std
API; `forbid` cannot be lifted by any `allow`. Rather than carve out an
audited unsafe module, the unsafe lives in these two widely-used crates and
there is none in ktask-rs at all.

Three consequences that shape everything:

- **No async runtime.** Threads and channels only. A supervisor that waits on
  subprocesses does not need one, and it halves the concepts in play.
- **Git is the `git` command**, run as a subprocess. No libgit2, no gix. It is
  what a human would run, and its output is inspectable. Git is the tool's
  responsibility, not the agent's: the supervisor creates the worktree, makes
  the commit and performs the publication.
- **No unsafe code.** `unsafe_code = "forbid"` holds everywhere; syscalls go
  through `nix` and `signal-hook`.

## Crate and module layout

```
crates/ktask-core/src/
  lib.rs          re-exports; no logic
  error.rs        Error, Result
  ids.rs          TaskId, AttemptId, EventSeq
  paths.rs        XDG resolution, project identity
  config.rs       Config, Source, layered loading
  task.rs         Task, TaskStatus, parse, mark
  state.rs        TaskState, Phase, apply()
  event.rs        Event, EventKind, payloads
  journal.rs      Journal: open, append, replay, query
  project.rs      Project: registration, state dir
  gate.rs         Gate, GateResult, run_gate()
  git.rs          thin wrappers over the git command
  provider/mod.rs Provider trait, Capabilities
  provider/dummy.rs
  provider/claude.rs
  provider/codex.rs
  classify.rs     FailureClass, classify()
  remediate.rs    failure bundle, circuit breaker
  protocol.rs     Protocol, ProtocolPhase, write scopes
  runner.rs       the supervisor loop
  events.rs       broadcast channel to frontends
  redact.rs       secret redaction

crates/ktask-cli/src/
  main.rs         clap parsing, dispatch, exit codes
  cmd/<one file per command>
  render.rs       human output; json.rs for --json

crates/ktask-tui/src/
  lib.rs          App, run()
  app.rs          App state, update()
  event.rs        AppEvent
  keys.rs         KeyMap, binding table
  layout.rs       responsive layout
  text.rs         unicode-safe truncation
  screen/<one file per screen>
```

## Core types

Exact definitions. Implement them as written.

```rust
// ids.rs
pub struct TaskId(pub u32);      // 1-based, matches queue order
pub struct AttemptId(pub u32);   // 1-based within a task
pub struct EventSeq(pub u64);    // monotonic, global

// state.rs
pub enum TaskState {
    Queued,
    Preflight,
    Running { attempt: AttemptId, phase: Phase },
    Remediating { attempt: AttemptId, phase: Phase },
    Verifying { attempt: AttemptId },
    Publishing { attempt: AttemptId },
    PublishedVerified { commit: String },
    Done,
    Acknowledged { by: String, at: OffsetDateTime },
    Paused { reason: PauseReason, resume_to: Box<TaskState> },
    Failed { class: FailureClass, detail: String },
    Cancelled,
}

pub enum PauseReason { Limit { until: Option<OffsetDateTime> }, Input, HumanGate, Interrupted, Blocked }

// Phase is defined once, under "Phases and screens" below.

// gate.rs
pub enum GateKind {
    Baseline, Targeted, Verify, Lint, Format, Build, Privacy, Flake, Review,
}

// classify.rs
pub enum FailureClass {
    AgentFailure, VerificationFailure, ProviderLimit, ProviderTransient,
    ProviderConfiguration, GitConflict, EnvironmentFailure, PolicyFailure, NeedsInput,
}
```

`apply` is the only way state changes:

```rust
pub fn apply(state: &TaskState, event: &EventKind) -> Result<TaskState>;
```

It is pure: no I/O, no clock, no randomness. Illegal transitions return
`Error::InvalidTransition { from, event }`.

## Error variants

Exact payloads. Later tasks require these fields by name.

```rust
pub enum Error {
    Io(#[from] std::io::Error),
    Database(#[from] rusqlite::Error),
    Serde(#[from] serde_json::Error),
    Config { key: String, detail: String },
    Git { args: Vec<String>, stderr: String },
    Provider { provider: String, detail: String },
    Gate { kind: GateKind, detail: String },
    Policy { detail: String, paths: Vec<PathBuf> },
    InvalidTransition { from: String, event: String },
    NotFound { what: String },
    Corrupt { detail: String, seq: Option<EventSeq> },
}
```

`Policy` carries every offending path, not a count. `Git` carries the argument
vector so a failure names the command that produced it.

## Configuration defaults

```rust
provider:                 "dummy"
model:                    None
attempt_timeout_secs:     14400   // 4h
gate_timeout_secs:        1800    // cold Rust builds are slow
idle_timeout_secs:        1800    // kill a silent agent, not a quiet build
max_attempts:             2       // one normal, one remediation
max_remediation_attempts: 1
circuit_breaker_threshold: 3      // identical signatures before tripping
mainline_remote:          "origin"
mainline_branch:          "main"
context_budget_bytes:     65536
failure_bundle_bytes:     16384
output_ring_lines:        4096
limit_wait_margin_secs:   60
limit_max_wait_secs:      86400
default_protocol:         "direct"
dummy_scenario_path:      None
test_globs:               ["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]
secret_patterns:          []
flake_runs:               5
retention_days:           90
min_free_disk_bytes:      2147483648   // 2 GiB
```

## Event catalog

`EventKind` is `#[serde(tag = "kind")]`. Every payload is listed; nothing else
may be added without extending `state::apply` in the same task.

| Variant | Payload fields |
|---|---|
| `TaskQueued` | `title: String` |
| `PreflightStarted` | *(none)* |
| `PreflightPassed` | `base_sha: String` |
| `PreflightFailed` | `class: FailureClass, detail: String` |
| `AttemptStarted` | `attempt: AttemptId, protocol: String, pid: u32, base_sha: String` |
| `PhaseEntered` | `attempt: AttemptId, phase: Phase` |
| `AgentOutput` | `attempt: AttemptId, stream: Stream, text: String` |
| `AttemptFinished` | `attempt: AttemptId, exit_code: i32, usage: Option<Usage>, session_id: Option<String>, model_reported: Option<String>` |
| `GateStarted` | `kind: GateKind` |
| `GateFinished` | `result: GateResult` |
| `VerifyPassed` | `attempt: AttemptId, tree_hash: String` |
| `VerifyFailed` | `attempt: AttemptId, class: FailureClass, detail: String` |
| `PublishStarted` | `attempt: AttemptId, candidate_sha: String` |
| `PublishVerified` | `commit: String, remote_sha: String` |
| `TaskDone` | `commit: String` |
| `TaskFailed` | `class: FailureClass, detail: String` |
| `TaskCancelled` | `reason: String` |
| `Paused` | `reason: PauseReason` |
| `Resumed` | *(none)* |
| `Interrupted` | `phase: Phase` |
| `RecoveryDecision` | `decision: Recovery, detail: String` |
| `ProviderDetected` | `provider: String, capabilities: Capabilities, version: String` |
| `TddExceptionUsed` | `exception: TddException, reason: String` |
| `DecisionRaised` | `request: DecisionRequest` |
| `DecisionResolved` | `adr_path: PathBuf, answer: String` |
| `GateAcknowledged` | `by: String, at: OffsetDateTime` |
| `AttemptRecorded` | `record: AttemptRecord` |
| `SelfHealingReport` | `attempt: AttemptId, class: FailureClass, repairs: Vec<String>, outcome: String` |

```rust
pub enum Stream { Stdout, Stderr }
pub enum Recovery { Resume, MarkInterrupted, AlreadyApplied }

pub struct DecisionRequest {
    pub question: String,
    pub options: Vec<String>,
    pub tradeoffs: String,
    pub impact: String,
    pub recommended: Option<String>,
}
```

## Phases and screens

```rust
pub enum Phase {
    Goal, Scope, AcceptanceTests, Implement, Red, Green, Refactor,
    Review, Harden, DoneCheck, Verify, Publish,
}

pub enum Screen {
    Queue = 1, LiveRun = 2, Logs = 3, Failures = 4, Inspector = 5,
    InputInbox = 6, History = 7, Git = 8, Config = 9,
}
```

`Phase` carries every variant any protocol needs, including `spec-first`, so
no later task has to widen it. `Screen`'s discriminants are the number keys.

## Other fixed types

```rust
pub enum RunOutcome {
    Drained,                       // 0
    TaskFailed { task: TaskId },   // 1
    Usage { detail: String },      // 2
    ProviderLimit { until: Option<OffsetDateTime> }, // 3
    HumanGate { task: TaskId },    // 4
    NeedsInput { task: TaskId },   // 5
    Interrupted,                   // 130
}

pub struct App {
    pub screen: Screen,
    pub selected: BTreeMap<Screen, usize>,
    pub scroll: BTreeMap<Screen, usize>,
    pub follow: bool,
    pub search: Option<String>,
    pub overlay: Option<Overlay>,
    pub tasks: Vec<TaskView>,
    pub output: VecDeque<String>,
    pub size: (u16, u16),
}
pub enum Overlay { KeyMap, Confirm { action: Action, prompt: String } }
```

The event bus is a **bounded ring per subscriber**: capacity
`output_ring_lines`, drop-oldest on overflow, recording a `dropped: usize`
count the interface can display. Publishing never blocks, and a subscriber
that stops reading loses old events rather than stalling the run.

## Database schema

One SQLite file per project at `<state_dir>/journal.db`.

```sql
CREATE TABLE IF NOT EXISTS events (
  seq      INTEGER PRIMARY KEY AUTOINCREMENT,
  ts       TEXT    NOT NULL,          -- RFC 3339, UTC
  task_id  INTEGER,                   -- NULL for queue-level events
  kind     TEXT    NOT NULL,          -- EventKind discriminant
  payload  TEXT    NOT NULL           -- JSON
);
CREATE INDEX IF NOT EXISTS idx_events_task ON events(task_id, seq);

CREATE TABLE IF NOT EXISTS task_state (
  task_id    INTEGER PRIMARY KEY,
  state_json TEXT    NOT NULL,
  updated_at TEXT    NOT NULL
);

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
```

`events` is the source of truth and is append-only: no UPDATE, no DELETE, ever.
`task_state` is a projection and may be dropped and rebuilt by replay. Journal
mode is WAL; `synchronous = FULL`, because losing the last event is exactly the
failure the design exists to prevent.

## Paths

- State: `$XDG_STATE_HOME/ktask-rs/<project-id>/`, falling back to
  `$HOME/.local/state` when the variable is unset.
- Config: `$XDG_CONFIG_HOME/ktask-rs/config.toml`, falling back to
  `$HOME/.config`.
- `<project-id>` is the first 16 hex characters of the SHA-256 of the canonical
  repository path, plus `-`, plus the same for the origin URL when one exists.

## Conventions

- Time is `OffsetDateTime` in UTC, serialized as RFC 3339.
- Every public item has a doc comment; the `missing_docs` lint enforces it.
- Tests live in a `#[cfg(test)] mod tests` in the same file, except end-to-end
  tests, which live in `tests/`.
- Test names state the behavior: `rejects_transition_from_done`, not `test_apply`.
- No `unwrap`, `expect` or `panic` outside tests. Errors are values.
- **Tests never write inside the repository.** Every fixture — scratch git
  repositories, bare origins, project directories, state directories — is
  created with `tempfile::TempDir`, which lives under the system temp
  directory, outside this working tree. A test that leaves a file in the
  repository dirties the tree, and a dirty tree at verification time is a
  policy failure, so a stray fixture does not merely litter: it fails the task
  that created it and every task after it.
- **No test sets an environment variable.** `std::env::set_var` is `unsafe` in
  edition 2024 and `unsafe_code = "forbid"` cannot be lifted, so every API that
  reads the environment takes it as a `&dyn Fn(&str) -> Option<String>`
  parameter instead. Tests pass a closure; process-level tests use
  `Command::env`. This is why `paths`, `config` and `project` all thread an
  environment accessor rather than reading the process environment directly.
