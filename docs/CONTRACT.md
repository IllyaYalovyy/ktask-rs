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

### `ktask-rs pause` / `ktask-rs interrupt` / `ktask-rs cancel`

Control of a run in progress, from any terminal. `pause` stops the queue after
the current task reaches a safe boundary. `interrupt` terminates the running
attempt now, leaving durable resumable state and recording it as interrupted.
`cancel --task <id>` marks a task cancelled so the queue may proceed past it.
Each exits 2 when nothing is in a state the command applies to.

### `ktask-rs rerun-gate --task <id> [--gate <kind>]`

Re-runs a gate against the current worktree, discarding any cached result, and
prints the structured outcome. Without `--gate`, runs the whole completion set.
Runs in the task's own worktree, journals each result as a `GateRerun`, and
never changes the task's state. Exits 0 when every gate that ran passed, 1 when
one failed, and 2 when there is nothing to run against: no such task, a
finished task, a supervisor still working it, no worktree, or a gate the
profile does not configure.

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

   ```
   v           structured / raw view
   l           raise the minimum level: debug, info, warn, error, then debug
   p           filter to the next phase seen, then to all
   /           type a search (Enter keeps it, Esc abandons it; Esc later clears it)
   n / N       next / previous match
   e / E       next / previous error
   ```

   Filters and search compose: what a filter hides is neither searched nor
   visited. `G` follows the newest entry; any other movement stops following.
4. **Failures** — classified causes, repeated signatures, circuit-breaker
   state, and the actions available for each.

   ```
   j / k       select an older / a newer failure (newest first)
   g / G       select the newest / the oldest failure
   r           retry the selected failure's task
   c           cancel it
   x           re-run its gates
   ```

   The first row is the circuit breaker. Closed, it says how close the repeats
   have come; tripped, it is a full-width white-on-red bar naming the task and
   the signature, and every failure that reached the limit says `TRIPPED`. A
   failure's detail shows its class, signature and how often it recurred, and
   the actions permitted: a failed task can be retried or cancelled, a task
   being remediated can only be cancelled, `needs_input` is answered in the
   input inbox rather than retried, and only a verification or environment
   failure has gates to re-run. A key for an action that is not permitted does
   nothing and says why.
5. **Task inspector** — objective, acceptance criteria, dependencies,
   completion gates, work protocol with current phase highlighted, per-attempt
   evidence.

   ```
   [ / ]       show the previous / the next attempt
   j / k       inspect the next / the previous task
   g / G       inspect the first / the last task
   ```

   It shows the task selected in the queue (`Enter` opens it here). The
   definition comes first: outcome, done-when, verify and refs. Below it the
   work protocol is a row of phases, each marked done (`✓`), current (`▶`,
   drawn reversed), stopped (`✗`), skipped by a declared TDD exception (`↷`) or
   pending (`·`), followed by what the current phase may write and which gate
   it runs, and the completion gates marked the same way. The phase marks come
   from the journal alone: the current phase is the last one the shown attempt
   entered, and a phase the protocol does not list is still shown. The rest is
   the evidence of one attempt: verdict, each gate's result (with the last line
   a failed gate wrote), timing, exit, model, commits and usage. The newest
   attempt is shown, and followed as new ones start, until `[` steps back;
   `]` back to the newest follows it again. A terminal too short for all of it
   gives each part of the definition and the evidence a line before any gets
   a second.
6. **Input inbox** — pending questions with context, impact and recommended
   response; resolving here writes the same ADR as `resolve`.

   ```
   j / k       select the next / the previous question
   g / G       select the first / the last question
   a / Enter   answer the selected question
   r           answer it, starting from the recommended response
   ```

   A question is listed while its task is paused for input, and leaves the
   list when the journal says it is answered or cancelled. While an answer is
   typed every key is a letter of it, so `q` and the digits do not quit or
   change screen: `Enter` sends it, `Esc` abandons it, `Backspace` removes a
   letter, and `Ctrl-C` still quits. Sending is the `resolve` action; the
   answer is recorded by the same code as `resolve --note`, so the ADR is
   byte-identical to the command's. A blank answer is refused and stays open.
7. **History** — event timeline across every attempt and remediation.

   ```
   j / k       scroll down / up a row
   g / G       the first row / the newest row, following it
   PgUp / PgDn scroll a screenful
   t           switch between the whole queue's timeline and the selected task's
   ```

   Each row is an event with its UTC timestamp, task, kind and description;
   a heading opens each attempt and marks a remediation. The agent's output is
   the logs' business and is not listed. The timeline is read from the journal
   as it is scrolled, never kept whole by the interface, so a long history
   costs a bounded page of memory. The view follows the newest row until it is
   scrolled up.
8. **Git** — changed files, diff, commits, publication state, comparison with
   remote mainline.

   ```
   j / k       select the next / the previous changed file
   g / G       select the first / the last changed file
   PgUp / PgDn scroll the selected file's diff a screenful
   ```

   It shows the task selected on the queue screen: its changed files against
   the commit its attempt began from (added, modified, deleted, renamed, and
   binary marked as such), the diff of the selected file, the commits the task
   made, whether it is published (from the journal, so it is shown after the
   worktree is gone), and how its `HEAD` compares with the remote mainline —
   commits ahead and behind, as of the last fetch; viewing never fetches. The
   files and commits listed are capped and the heading says when they were
   cut. A diff is never held whole: a bounded window of it is read around the
   view and re-read as it scrolls. Only committed work is shown. A task whose
   worktree is gone shows why instead of its changes. A terminal too short for
   every part shows the status lines and the file list.
9. **Configuration and doctor** — effective configuration with the source of
   each value, and doctor results.

### Actions

Every action is also a CLI command: pause, interrupt, resume, retry, resolve,
acknowledge, cancel, rerun gate.

Attaching to a run and opening a diff are **view operations**, not actions:
they change what the interface shows, not what the supervisor does. The
equivalence rule covers actions and state, and the state behind both is
reachable from the CLI — `status` for the live run, `git` for the diff — so
neither needs a command of its own.

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
