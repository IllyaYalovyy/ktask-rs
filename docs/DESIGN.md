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

[dev-dependencies]
proptest    = "1"
insta       = "1"
tempfile    = "3"
```

Two consequences that shape everything:

- **No async runtime.** Threads and channels only. A supervisor that waits on
  subprocesses does not need one, and it halves the concepts in play.
- **Git is the `git` command**, run as a subprocess. No libgit2, no gix. It
  matches v1, it is what a human would run, and its output is inspectable.

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
    Paused { reason: PauseReason, resume_to: Box<TaskState> },
    Failed { class: FailureClass, detail: String },
    Cancelled,
}

pub enum PauseReason { Limit { until: Option<OffsetDateTime> }, Input, HumanGate, Interrupted, Blocked }

pub enum Phase { Implement, Red, Green, Refactor, Verify, Publish }

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
