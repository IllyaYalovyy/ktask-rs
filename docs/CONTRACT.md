# Interface contract

What `ktask-rs` presents to a human and to a script. This document is
normative: a task is complete only when the behavior described here is the
behavior observed. Where it and `VISION.md` disagree, `VISION.md` wins on
intent and this document wins on surface.

## 0. Rules

1. **Equivalence.** Every action available in the TUI is available as a CLI
   command, and every piece of state the TUI displays is obtainable from the
   CLI. Neither interface is a subset of the other in capability.
2. **One core.** Both frontends are thin over `ktask-core` and consume the same
   event stream. No behavior is implemented in a frontend.
3. **Scriptable by default.** Every command that reports state accepts
   `--json`. Human output may change; `--json` output is a compatibility
   surface and changes only by adding fields.
4. **Silence on success.** Commands that act print what changed and nothing
   more. Progress and chatter go to stderr, results to stdout, so
   `ktask-rs status --json | jq` always works.
5. **No hidden state.** A command never depends on a previous command having
   been run in the same shell.

## 1. Exit codes

Semantic and stable. Scripts depend on these, so they are a compatibility surface.

| Code | Meaning |
|---|---|
| 0 | success; for `run`, the queue drained |
| 1 | a task failed after its remediation budget |
| 2 | usage error: bad arguments, malformed task file, no project |
| 3 | provider limit reached; work is paused, not failed |
| 4 | stopped at a human gate |
| 5 | stopped needing input on a decision |
| 130 | interrupted (SIGINT); state is durable and resumable |

Codes 3, 4 and 5 are pauses. They are not failures and must never mark a task
`failed`.

## 2. Global options

Accepted by every command:

```
--project <path>     operate on this project instead of discovering one
--json               machine-readable output on stdout
--no-color           disable styling (also honors NO_COLOR)
--verbose            diagnostic detail on stderr
--quiet              suppress non-essential stderr
```

Project discovery walks up from the working directory to find a registered
project. Failure to find one exits 2 with a message naming `ktask-rs init`.

## 3. Commands

### `ktask-rs doctor`

Checks provider availability, git, toolchain, filesystem permissions and state
directory health. Prints one line per check with pass/fail and a remedy for
each failure. Exit 0 if every check passes, 1 otherwise. `--json` emits an
array of `{check, status, detail, remedy}`.

### `ktask-rs init`

Registers the current repository. Creates state under
`$XDG_STATE_HOME/ktask-rs/<project-id>/`, never inside the repository. Project
identity is the canonical repository path plus remote identity. Idempotent:
re-running prints the existing registration and exits 0.

### `ktask-rs add [--file <path>]`

Opens `$EDITOR` with a task template; `--file` reads instead. The task must
carry `Outcome`, `Done-when`, `Verify` and `Refs` sections. A malformed task is
rejected with exit 2 and a message naming the missing section; it never enters
the queue. Prints the assigned task id.

### `ktask-rs plan lint`

Validates the whole queue without running anything: required sections present,
`Verify` commands parseable, referenced paths existing, no duplicate ids.
Prints one line per problem. Exit 0 if clean, 2 if any task is malformed.

### `ktask-rs status`

The headless dashboard. Prints, per task: id, state, work protocol and current
phase, attempt count, and elapsed time for the active task. Ends with a summary
line of counts by state. Exit 0 always, except 2 when there is no project.

`--json` emits `{project, tasks: [{id, title, state, protocol, phase, attempts,
started_at, ended_at}], summary: {...}}`.

### `ktask-rs run [--task <id>] [--from <id>]`

Drains the queue in order, strictly serially. `--task` runs exactly one task;
`--from` starts at an id. Streams progress to stderr and a per-task result line
to stdout.

Stops at the first terminal failure, human gate, input request or provider
limit, and exits with the corresponding code. Never continues past a failed
task: a successor cannot start until its predecessor reaches
`published_verified`.

### `ktask-rs resume`

Continues from the first task that is not done. Equivalent to `run --from` the
first incomplete id. Exits 2 if the queue is drained.

### `ktask-rs retry --task <id>`

Starts a fresh remediation attempt seeded with the failure bundle:
classification, gate output, diff summary, prior attempt evidence. Never
resumes a provider session. Exit codes as `run`.

### `ktask-rs resolve --task <id> [--note <text>]`

Answers a `waiting_input` question. The answer is recorded as an ADR in
`docs/adr/` and injected into the context of subsequent tasks. Without
`--note`, opens `$EDITOR`. Exits 2 if the task is not waiting for input.

### `ktask-rs ack [--task <id>]`

Passes a human gate. Marks the gate satisfied and leaves the queue paused;
`resume` continues. Exits 2 if no gate is pending.

### `ktask-rs privacy audit [--history]`

Scans tracked files, the outgoing commit range and optionally full git history
for operational artifacts and configured secret patterns. Prints one line per
finding with path and reason. Exit 0 if clean, 1 if anything is found.

### `ktask-rs stats [--task <id>]`

Tokens, cost, durations and success rates per task and per queue, from recorded
attempt evidence. `--json` emits the same data unrounded. Where a provider
reports no usage, the field is `null` and the source is marked, never guessed.

### `ktask-rs tui`

Launches the interface described below. Requires a terminal; exits 2 with a
clear message when stdout is not a TTY.

## 4. The TUI

### Model

Nine screens, reached by number key or by `Tab`/`Shift-Tab`. The TUI attaches
to live state: opening it during a run shows progress immediately, and closing
it never disturbs the run. It holds no state of its own beyond view state —
selection, scroll position, filters.

### Global keys

```
1..9        jump to screen
Tab / S-Tab next / previous screen
? or F1     key map overlay (always reachable, from every screen)
/           search within the current screen
Esc         dismiss overlay, clear search, or step back
j/k, up/dn  move selection
g / G       first / last
Ctrl-C      quit (never kills a running task)
q           quit, or close overlay if one is open
```

Bindings are consistent across screens: a key never means two different things.

### Screens

1. **Queue** — ordered tasks with state, protocol, current phase, attempts.
   Actions: select, pause, interrupt, retry, cancel, jump to inspector.
2. **Live run** — streaming agent output, the command being executed, gate
   results as they land, elapsed time. Follow mode on by default; any scroll
   detaches follow, `f` re-attaches.

   The output pane is the primary thing an operator watches, and it is held to
   a higher standard than the rest of the interface. Provider output is
   untrusted bytes: it is sanitized before rendering, never interpreted as
   terminal control. Specifically, escape sequences and control characters are
   stripped or rendered visibly, carriage returns do not overwrite earlier
   output, invalid UTF-8 is replaced rather than dropped or panicked on, lines
   longer than the pane are wrapped or truncated at a grapheme boundary, and no
   volume of output can push the cursor outside the pane or disturb any other
   region of the screen.
3. **Logs** — raw and structured views, filter by level and phase, search,
   follow mode, jump between errors.
4. **Failures** — classified causes, repeated signatures, circuit-breaker
   state, and the actions available for each.
5. **Task inspector** — objective, acceptance criteria, dependencies,
   completion gates, work protocol with current phase highlighted, per-attempt
   evidence.
6. **Input inbox** — pending questions with context, impact and recommended
   response; resolving here writes the same ADR as `resolve`.
7. **History** — event timeline across every attempt and remediation.
8. **Git** — changed files, diff, commits, publication state, comparison with
   remote mainline.
9. **Configuration and doctor** — effective configuration with the source of
   each value, and doctor results.

### Actions

Every action is also a CLI command: pause, interrupt, resume, retry, resolve,
acknowledge, cancel, attach, open diff, rerun gate, export sanitized
diagnostics.

### Behavior under stress

- Terminals from 80x24 upward render fully. Below that the UI degrades to a
  reduced layout; it never panics, corrupts the display, or loses input.
- Resize is handled live.
- Output of unbounded length is windowed, not buffered without limit.
- Unicode, including wide and combining characters, does not break layout.

## 5. Testability

These are contract requirements, not suggestions.

- Decision logic is pure: `update(state, event) -> state` and
  `render(state) -> buffer`, with terminal I/O confined to a thin shell.
- Every screen and every reachable UI state has a snapshot test.
- Every key binding and every screen transition is exercised by a scripted
  event sequence.
- Resize down to degenerate sizes is tested.
- A property test feeds arbitrary event sequences and asserts the UI never
  panics and never hangs.
- All of it runs headlessly from `scripts/quality.sh`, with no terminal and no
  human.
