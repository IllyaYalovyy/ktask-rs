# ktask RS

A TUI-first workspace for supervising AI coding tasks.

Define the outcome, constraints, and observable success criteria. A capable execution model
handles investigation, design, implementation, and verification within the task. Configure
stronger models for independent review and remediation. Planning stays brief; separate specs
and task breakdown are optional. Missing implementation details are for the executor to resolve,
with human questions reserved for consequential product decisions.

Requires Linux, Rust 1.97+, Git and a terminal. AI execution requires at least one authenticated
agent CLI: Codex (tested with 0.163.0), Claude Code (`claude`, tested with 2.1.305) or OpenCode
(`opencode`, tested with 1.19.3). Each role may use a different provider. Start with `cargo run --locked`; run checks with `make check`.

| Instance | Start | App directory under each XDG root |
| --- | --- | --- |
| Development | `cargo run --locked` | `ktask-dev/` |
| Installed | `ktask` (install explicitly with `make install`) | `ktask/` |

Two tiny executable entry points select the instance; both use the same application code.
Build profile, working directory, renaming the binary, and inherited environment cannot switch it.
Ordinary builds never update the installed executable. `--paths` shows the executable and
config, state, cache, and runtime locations; the header identifies the instance.

Scratch files belong to one run and are removed on exit. After a crash or forced kill,
the next launch reclaims abandoned scratch directories; ownership locks protect live runs.
Tests use private homes and app roots under `$TMPDIR/ktask-test-scratch/` (system temporary
directory when unset), cleaned on success or panic. The next test reclaims crash debris.

The TUI opens **Current Task**, with separate **Queue**, **History**, **Configuration**,
and **About** screens. Normal startup opens real SQLite tasks for the current directory.
A new project starts empty; changes save immediately, including after a forced exit.
The project name defaults to the directory name. For a new directory with a name already in
use, launch with `--project-name UNIQUE_NAME`. Each instance retains its own database.

Click tabs, rows, buttons, and form fields; use the wheel to scroll. `1`–`5` or `Tab`
switches screens; `j/k`, arrows, `PgUp/PgDn`, and `Home/End` navigate. `?` shows shortcuts.

**Current Task** leads with the situation: one line says what is happening and which action it
calls for, and the action bar lists that action first. An open question from the worker gets its
own panel. The output pane shows the attempt report once work settles and the raw log while it
runs; `o` switches between them. The status panel lists budget, model, and each supervisor step
with its result. Activity shows events for the current task; `l` opens all events. `x` abandons
current when it is failed, waiting or paused: the task moves to History as cancelled with its
attempts and evidence; a waiting or paused attempt can instead be dropped alone, without spending
budget, so the task stays for a fresh run. Queue's `d` does the same for that task.

Workers do not get to ask for permission. Review and validation run in a read-only sandbox by
design and are told so; every role is told that permissions, sandbox limits and tooling are never
questions for the owner. A worker that still returns `blocked` is nudged once, in the same
session, to decide on its own. Only a repeated product question reaches you; a reviewer that
refuses to decide twice stops the task as a protocol failure, which spends no budget.

The judges hold a fixed bar. Review judges the diff against Clean Code: intention-revealing
names, small single-purpose functions, no duplication of what the project already has, no dead
or commented-out code, comments only where code cannot speak, consistent structure, tests kept
fast, independent and intact, no scope creep, and the code at least as clean as before.
Validation judges the evidence: every configured check ran and exited zero, nothing was skipped,
ignored, removed or weakened, and each criterion has proof. Both read; neither runs anything.

A failed attempt is judged before the next one starts. The failure judge reads the evidence in
a read-only turn of the remediation model and returns one decision: a repair plan for the next
worker, a rewritten task when the definition itself is wrong, prerequisite tasks inserted before
it, an escalated execution model, or a product question for the owner. Anything but a repair ends
that attempt as interrupted, spending no budget, and the queue continues on the new plan.
Forms support paste, cursor editing, `Ctrl-A` to clear, `Ctrl-S` to apply, and `Esc` to cancel.
Insertion/move dialogs offer Earlier/Later buttons (`Alt-Up/Down`) and show neighboring IDs.
Mouse can be disabled in Configuration; keyboard controls always remain available.

Real operations: add/edit, per-role provider/model/effort and limits, draft approval/rejection,
fractional ordering, delete/undo, Human acknowledgment, failure resolution, history/events,
and disk import/export. SQL and validation live in shared transactional application commands,
not the TUI. Concurrent stale edits fail without overwriting data; other windows refresh.
New AI tasks have no invented model defaults. **Models** accepts `provider/model`, optional
effort, and limits: `codex/gpt-6-sol`, `claude/sonnet` (or just `claude`, which means Sonnet),
`opencode/anthropic/claude-sonnet-5-5` (OpenCode keeps its own `vendor/model`). Saving makes a new approved revision; previous attempts keep their inputs.
Configuration provides project model defaults, check management, and saved preferences.
Configuration → Storage previews retained output and check transcript bytes, sets the age
of completed work eligible for cleanup, and applies cleanup after review. History marks
pruned content; live work and unresolved failures retain their evidence.
Mouse, run-one/continue-queue, and planner approval preferences persist per project and instance.
Break down uses the configured planning model and previews ordinary subtasks before applying them.

Agent execution, shell checks, independent AI review/validation, and bounded remediation are real.
AI task breakdown uses the planning model. Normal mode never simulates
success or creates fake attempts. Use `cargo run --locked -- --demo` to explore
the isolated simulator: 120 queued/current and 120 historical tasks, no SQLite access.

Implemented in the simulator: execution/pause/retry, ordinary remediation tasks, revision approval,
per-role models/effort and limits, failure resolutions, timed waits, workspace handoffs, Human
acknowledgment, AI questions, queue management, search/filter, history/attempts, statistics,
settings, and import/export previews. Continue queue is the default: the supervisor keeps taking
the next task and stops only for a question, an approval, Human work, or an exhausted budget.
Insertion before current is allowed unless it is running.

Try **Remediate → Apply** on the initial failure: escalate its execution model or insert a repair
task before it. Configuration controls whether planner changes need approval. **Models** edits
all four role settings and limits; **Resolve** records override/skip/supersede with a reason.
Attempts retain their original inputs and sessions. **Events** opens the full activity feed.
Guided, Empty, Waits, and Approvals datasets exercise interventions, timed waits, and approval.
Configuration can fail the next simulated cleanup; a blocked start preserves work and creates
no attempt. Long forms support Tab and mouse **Fields −/+** controls at 70×22.

In `--demo`, task/settings changes last only for the session. Import/export uses **in-memory demo
files**, not disk: `/demo/import.tsv` is valid and `/demo/invalid.tsv` exercises errors.
An exported `/demo/*.tsv` file includes model/effort settings and limits and can be imported
again during the same session. These are definition exports, not history backups.
Normal import/export reads and writes **real files**. The `ktask-tasks-v2` escaped TSV format (v1 still imports)
preserves all acceptance criteria, provider/model/effort, and limits. Imports preview and commit
as one batch; exports replace files atomically after confirmation. These are definition exports,
not history backups. Relative file paths use the invocation working directory. Files are limited to 4 MiB
on import. Demo files are not interchangeable with this format.

The durable [SQLite schema and migration layer](docs/STORAGE.md) never saves demo fixtures
or resets an existing application database. Restart after rebuilding.
The unreleased V1 schema includes separate task/revision/attempt lifecycles, fractional queue
ordering, per-role model/optional effort, sessions, timed waits, and attempt/time limits.
Execution status is derived only from attempts; rejected revisions do not replace ready ones.
Input tokens are normalized to uncached input across providers. Schema design edits stay in 0001.
The supervisor records sessions, normalized usage, checks and criterion evidence in these tables.

- [Vision](docs/VISION.md)
- [User flows](docs/USECASES.md)
- `src/app.rs`: interaction state
- `src/application/`: typed shared commands, transactional SQL, snapshots, definition transfer
- `src/cli/`: argument parsing, shared command calls, human/JSON output (no SQL)
- `src/runner/`: supervisor, agent prompts, process guardian, Git checkpoints
- `src/runner/provider/`: Codex, Claude Code and OpenCode launch arguments and event parsing
- `src/persistent.rs`: TUI forms → commands → refreshed read projections
- `src/ui.rs`: terminal layout
- `src/presentation.rs`: terminal read projections, separate from application command inputs
- `src/mock.rs`: deterministic fake tasks and events
- `tests/cases/workflows.rs`: real-binary mouse and keyboard interaction tests
- `tests/cases/lifecycle.rs`: revised approval, models, resolutions, waits, and handoff flows
- `tests/cases/persistence.rs`: durable terminal flows, restart, conflicts, disk transfer, isolation
- `tests/cases/cli.rs`: real-binary CLI contracts, concurrent writers, and live TUI interoperation

Run `make check` for formatting, Clippy, terminal flows, storage/migrations, isolation,
cleanup, and build checks.
The normal tests use deterministic provider processes; the opt-in live test uses authenticated Codex.

## Run real work

Use an ordinary Git baseline commit and a clean checkout for a new task. No hidden worktrees,
automatic resets, or deletion of project files. Successful work is committed locally by ktask;
failed/paused work stays in place for the same task. Inserting remediation tasks automatically
checkpoints that unfinished work under the workspace lease before switching tasks. The commit
is labelled unfinished and does not change the failed attempt or claim success. Other task
switches require a clean baseline. Project checks are trusted shell commands, not an additional sandbox.

For self-hosting, explicitly install `ktask` with `make install` (`scripts/install-user.sh [--root DIR]`),
then use that installed executable to supervise this source tree. The script installs outside build
outputs, refuses to overwrite a supervisor that is currently running, and verifies the installed
instance and path. `make self-host` (`scripts/self-host.sh`) then registers this checkout with the
installed `ktask`, sets the `make check` project check, and sets model defaults for every role;
environment variables documented in the script override the models, efforts, and check timeout.
Rerunning it only updates those values. `cargo build` only replaces development build products, not the
installed supervisor. Tests and `ktask-dev` use separate databases; execution in the **same** Git
repository is mutually exclusive across instances. Use private checkouts for concurrent test runs.

Open **Configuration → Defaults** to set models, **Checks** to add an enabled check,
and **Preferences** to select Continue queue (default) or Run one. Check edits are blocked
during running, paused, or waiting attempts. Cancel and invalid edits leave saved values unchanged;
stale forms must be reopened. Defaults and checks use relational SQLite rows; preferences use
a typed, atomically replaced file under the instance config directory.

Alternatively, configure checks and model defaults through CLI (replace model placeholders):

```sh
ktask check-set build --command 'make check' --timeout-ms 600000
ktask model-default execution --provider codex --model CHEAP_MODEL --effort medium
ktask model-default review --provider codex --model CAPABLE_MODEL --effort high
ktask model-default validation --provider codex --model CAPABLE_MODEL --effort high
ktask model-default remediation --provider codex --model CAPABLE_MODEL --effort high
ktask
```

Claude Code and OpenCode work the same way, for example
`ktask model-default execution --provider claude --effort high` (Sonnet) with
`--provider claude --model opus` for the judging roles, or
`--provider opencode --model anthropic/claude-sonnet-5-5`.

Defaults apply to new tasks only. Use **Models** or `edit --model` for existing tasks.
In Current, **Run** starts a detached supervisor using the saved run mode. Run one stops after
current succeeds; Continue queue advances after verified success. The Run button names its mode;
**m Run mode** opens preferences directly from Current. The global strip shows that preference
and the next task; Current shows the goal, proof required, and the active supervisor phase.
In queue mode, answering a question, approving current, or acknowledging Human work continues
automatically. A saved intervention stays saved if starting fails; the footer explains the blocker.
**Pause** stops/reaps workers before recording paused state; **Run/resume** retains the attempt
and compatible session. Closing the TUI does not stop the supervisor. Failed checks/review/
validation use the remediation model on a new attempt, bounded by the task's attempt limit.
Two optional time limits, both off by default (Models, or `--task-limit` / `--stage-limit` with
`off`, `on` or a duration such as `45m`): the **task limit** (6 h when on) caps active time across
all attempts; the **stage limit** (30 min when on) caps one agent turn and restarts with every
result, so an agent cannot run long without returning anything. Inserted repairs inherit the
failed task's limits. Insertion does not
reset the original coding budget; Models/limits can grant additional attempts while keeping all
failure evidence. Startup failures show the runner's error directly in the TUI and CLI.
A failed attempt is repaired automatically: the next attempt opens with the read-only failure
judge, and the queue keeps going. Execution stops only when no repair can change the result:
the attempt budget or the task's time limit is used up, ten repair rounds left critical findings,
the judge replanned ten times, or the provider/environment failed three attempts in a row
(five failures of any kind when the budget is unlimited). Then Current shows a red
**EXECUTION STOPPED** banner with the reason, why it was not resolved automatically, and what to
do next. An unresolved failure never advances the queue. Human work/approval/questions stop
automatic work.

CLI: `run` follows the saved run mode (Continue queue by default); `run --queue`
forces queue execution, `run --one` stops after current, and `--background` detaches. `resume` and
`retry` are aliases that still target only current. `pause ID` requests shutdown; `answer ID
--text TEXT` saves an answer, then `resume` continues. `abandon ID --reason TEXT` gives up on failed,
paused or waiting current (History, cancelled); `--attempt` drops only the paused or waiting attempt
nobody will resume, without spending budget, so the task can run again. `recover` marks a crashed supervisor's
attempt interrupted only when repository and worker locks are free. Inspect its work first.
If the guardian itself died without confirming cleanup, execution stays blocked; only after
stopping orphaned processes and preserving work, use `recover --confirm-worker-cleanup`.
Execution, review, validation and repair-worker connection failures retry twice within the same attempt, with short
interruptible backoff and preserved work/session; a failed attempt is then repaired
automatically as above. Infrastructure failures do not spend the coding budget. Rate and usage limits never fail a task:
the attempt/session waits until the provider's own reset time when it names one ("try again at
Oct 26th, 2026 11:32 PM", "try again in 20s"), else backs off 1, 2, 4, then every 8 minutes.
Current Task shows "Provider limit until <time>"; queue mode resumes automatically. Provider
errors before the next successful turn count as the same outage.
Pause stops automatic waiting. Resume (`r`, or `ktask resume`) never waits: it asks the provider
again at once, for example after buying credits; a limit that still holds records a fresh wait.

Every provider gets explicit per-role models, schema-checked structured results, session reuse,
uncached input tokens with disjoint cache counts, and the same failure semantics: limits wait,
infrastructure failures retry and never spend the coding budget. Execution and remediation may
write; review, validation and every failure judge are read-only. Authentication always uses the
provider's existing home (`~/.codex`, `~/.claude`, `~/.local/share/opencode`).
- Codex: `codex exec --json`, sandbox `workspace-write` or `read-only`, `--output-schema`; user
  configuration/rules are not inherited and no approval/sandbox bypass flags are used.
- Claude Code: `claude -p --output-format stream-json --json-schema`; MCP servers and user settings
  are not loaded. Workers use `bypassPermissions` (Claude Code has no workspace sandbox, so they can
  write outside the project like any shell); readers use `dontAsk` with only Read/Grep/Glob and
  read-only `git`/`ls`/`cat`/`rg` commands. Usage limits wait for the reported reset time.
- OpenCode: `opencode run --format json --pure`; the schema travels in the prompt because
  OpenCode has no schema flag. Workers pass `--auto`; readers get an `OPENCODE_PERMISSION` policy
  that denies edits and all but read-only commands, and `run` rejects anything else. Per-step
  usage and cost are summed; `retry-after` sets the limit wait.

Provider cost is recorded when reported (Claude Code and OpenCode), otherwise unknown. The guardian supplies owned `TMPDIR`, kills/reaps descendants, and cleans scratch
before another worker starts. Repository files and provider session history are not scratch.
Codex execution excludes general `/tmp` writes while allowing the owned `TMPDIR`.
Supervisor check transcripts are durable artifacts under the instance state directory, with
relational artifact records. Read-only reviewers receive their paths and inspect actual output,
not just an executor's summary. `make check` retains passing-test output, including rendered PTY
screens. Include `--show-output` or `--nocapture` in custom PTY checks to preserve those screens.

Opt-in live verification (incurs model usage; disposable project and app state):
`KTASK_LIVE_MODEL=YOUR_MODEL cargo test --test tui runner::live_codex -- --ignored --nocapture`.

## Task CLI

Without a command, the app opens the TUI. Task commands run without a terminal; `--help` and `<command> --help`
describe all flags. Use `ktask` after explicitly installing production, or the development binary:

```sh
cargo run --locked -- add --title "Fix pagination" --goal "Show every task" --criterion "Final row is visible" --approval draft
cargo run --locked -- list --json
cargo run --locked -- show 1 --json
cargo run --locked -- approve 1 --if-version 1
```

- Read: `list`, `show ID`, `current`, `events`, `stats`. List scopes: queue/history/all/deleted;
  filtering never sorts execution order. `current` returns null on an empty registered queue.
- Storage: `storage` previews logical output and artifact bytes; `storage --apply` removes
  eligible content. `retention DAYS [--if-version N]` sets the age policy (default 30 days;
  0 allows immediate cleanup of closed work). Both commands support `--json`.
- Change: `add`, `edit ID`, `move ID --before ID`, `approve`, `reject --reason`, `resolve
  --outcome --reason`, `ack`, `delete`, `restore`. Delete is recoverable. Only current Human
  work can be acknowledged; only failed current can be resolved.
- Models/limits: add/edit accept repeated `--model ROLE=PROVIDER/MODEL`, `--effort ROLE=VALUE`
  (`none` clears effort), `--clear-model ROLE`, `--attempt-limit N`, `--timeout-ms N` (`unlimited`
  removes either limit), `--task-limit off|on|6h`, `--stage-limit off|on|30m`. Edit preserves omitted fields; `--criterion` replaces the entire list.
  `--pending` edits the newest draft; `--approval ready|draft` controls the new revision.
- Files: `import tasks.tsv --preview` validates without touching storage. Import commits a batch;
  `--before ID` controls placement. `export tasks.tsv --scope all` refuses replacement unless
  `--overwrite` is explicit. Definitions only, not history backups.
- Selection: `--project DIR` selects a canonical project root without changing instance identity.
  Read commands do not register projects or create absent databases; missing projects return 3.
  Add/import (or the TUI) register a project. `--project-name NAME` resolves a new-root name collision.
- Concurrency: agents should pass `--if-version N` from list/show for every existing-task mutation.
  Without it, the command reads the latest version and still guards its transaction. A conflict
  never silently retries. Successful mutations return committed ID/version receipts; reread for state.
- Attribution: defaults to `human: owner`. AI callers must pass `--actor-kind ai --actor NAME`;
  this is audit metadata, not authentication. Explicit AI actors cannot acknowledge Human work.
- JSON: `--json` emits one `{"schema_version":1,"ok":true,"data":...}` object on stdout;
  errors emit `{"schema_version":1,"ok":false,"error":{"code":"...","message":"..."}}`
  on stderr with empty stdout. Help/version remain plain text. JSON file/path output requires UTF-8.
  Exit codes: 0 success, 1 I/O/database failure, 2 usage, 3 missing task/project, 4 state/version conflict.
- Events use ascending IDs, `--after ID`, and `--limit N` (1–1000); JSON returns `next_after`.
  Show includes all revisions/attempts and the latest 2000 stored output records per attempt.
  Missing cost is null, not zero. Check state before retrying after a lost mutation response.

`--paths --json` reports the absolute executable and isolated data roots. Future agent callbacks
must use the supervisor's absolute executable, never a bare name resolved through `PATH`.
