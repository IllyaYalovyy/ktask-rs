# ktask RS

A TUI-first workspace for supervising AI coding tasks.

Define the outcome, constraints, and observable success criteria. A capable execution model
handles investigation, design, implementation, and verification within the task. Configure
models for independent review and remediation. Planning stays brief; separate specs
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
with its result. Activity shows events for the current task; `l` opens all events.

Every task is in one state, and the action bar shows exactly what that state offers:

| State | Actions |
| --- | --- |
| Needs approval | `v` Approve, `n` Reject, `e` Edit, `m` Move, `D` Mark done |
| Pending | `r` Run, `e` Edit, `m` Move, `x` Cancel, `R` Supersede, `D` Mark done |
| Running (also while waiting for a provider limit) | `p` Pause, `s` Stop, `D` Mark done |
| Paused | `r` Resume, `e` Edit, `s` Stop, `D` Mark done |
| Needs input | `r` Answer, `e` Edit, `s` Stop, `D` Mark done |
| Failed | `r` Retry, `e` Edit, `s` Stop, `D` Mark done |
| Done, Cancelled, Superseded | `C` Copy to queue (History) |

**Stop** returns a live task to Pending: the worker stops, the attempt ends, and uncommitted
changes are saved as the git stash `ktask: task #N attempt #M`; the next Run starts fresh with
full limits. **Retry** keeps the code, continues from the checks and restarts the limits.
**Mark done** is your verdict from any unfinished state. Remediation is automatic; a task the
supervisor cannot repair is Failed. If the runner dies, the next refresh shows the task Paused
with its work kept; `r` resumes it.

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

Checks are mechanical and cost no tokens. Before a task's first agent turn the supervisor runs
the project checks on its clean starting commit; a red start stops the task at once and shows
the failing tests. A check that fails after work goes straight back to the worker with its
failing test names, panic or assertion messages and compiler errors, read from the transcript
without a model; the same summary is the failure's recorded reason. Review and validation block
only on critical findings, judged by their impact on correctness, behavior, performance or
maintainability; minor findings are recorded and never cost a fix round. `ktask stats` reports
cost to result (time, new and cached tokens, repair rounds) per delivered task by execution
model.

A failed attempt is judged before the next one starts. The failure judge reads the evidence in
a read-only turn of the remediation model and returns one decision: a repair plan, which the
execution model applies in its resumed implementation session, a rewritten task when the definition itself is wrong, prerequisite tasks inserted before
it, or a product question for the owner. Anything but a repair ends
that attempt as interrupted, spending no budget, and the queue continues on the new plan.
Forms support paste, cursor editing, `Ctrl-A` to clear, `Ctrl-S` to apply, and `Esc` to cancel.
Insertion/move dialogs offer Earlier/Later buttons (`Alt-Up/Down`) and show neighboring IDs.
Mouse can be disabled in Configuration; keyboard controls always remain available.

Real operations: add/edit, per-role provider/model/effort and limits, draft approval/rejection,
fractional ordering, stop, mark done, cancel, supersede, copy to queue, history/events,
and disk import/export. SQL and validation live in shared transactional application commands,
not the TUI. Concurrent stale edits fail without overwriting data; other windows refresh.
New AI tasks have no invented model defaults. **Models** accepts `provider/model`, optional
effort, and limits: `codex/gpt-6-sol`, `claude/sonnet` (or just `claude`, which means Sonnet),
`opencode/anthropic/claude-sonnet-5-5` (OpenCode keeps its own `vendor/model`). Saving makes a new approved revision; previous attempts keep their inputs.
Configuration provides project model defaults, check management, Git delivery, and saved preferences.
Git delivery defaults to **Local**, which only commits verified work. **Direct push** fetches
the configured remote and base branch (default `origin/main`), fast-forwards its clean checkout, then
pushes a verified checkpoint to it. **Pull request** creates one project branch from the base,
keeps queued tasks on it, and creates or updates one PR through `gh`. The branch is private to
the configured project and remote; ktask refuses to adopt a branch or remote tip it does not own.
The fetch and push URLs must match. No force push is used for Direct; the PR branch uses an exact
lease against its recorded owned tip when a rebase must rewrite it. `gh` must be installed for PR
delivery. A branch matching the remote base creates no empty PR. Direct delivery requires
the configured base branch to be checked out; it never publishes unrelated local commits.

When the remote base advances, the supervisor rebases its checkpoint and reruns project checks,
independent review and validation before pushing. A content conflict enters the ordinary
read-only judge and repair cycle, retaining the same attempt and ten-round limit. Pause preserves
the rebase and edited files; resume continues them. Network steps have bounded retries and keep
their delivery intent if they stop. Remote synchronization accepts at most six changing targets
per attempt, separately from coding repair rounds. Resume and automatic replans retain that
bound. After exhaustion, wait for remote writers to settle, Stop the task, then Run to retain
the checkpoint and start a fresh bounded synchronization batch. CLI `delivery` reports the versioned configuration; for
example `ktask delivery --mode pr --remote origin --base main --if-version 0`.
Configuration → Storage previews retained output and check transcript bytes, sets the age
of completed work eligible for cleanup, and applies cleanup after review. History marks
pruned content; live work and unresolved failures retain their evidence.
Configuration → Build cache (`b`) shows the same inventory, edits the cache bounds, previews and
runs cleanup, and adopts the project's `target` after confirmation; see
[docs/RESOURCE_LIMITS.md](docs/RESOURCE_LIMITS.md#build-caches-and-diagnostic-trees).
Mouse, run-one/continue-queue, and planner approval preferences persist per project and instance.
Break down uses the configured planning model and previews ordinary subtasks before applying them.

Agent execution, shell checks, independent AI review/validation, and bounded remediation are real.
AI task breakdown uses the planning model. Normal mode never simulates
success or creates fake attempts. Use `cargo run --locked -- --demo` to explore
the isolated simulator: 120 queued/current and 120 historical tasks, no SQLite access.

Implemented in the simulator: execution/pause/retry, stop, mark done, cancel, supersede, copy to
queue, revision approval, per-role models/effort and limits, timed waits, workspace handoffs, AI
questions, queue management, search/filter, history/attempts, statistics,
settings, and import/export previews. Continue queue is the default: the supervisor keeps taking
the next task and stops only for a question, an approval, Human work, or an exhausted budget.
Insertion before current is allowed unless it is running.

Try **r Retry**, **s Stop** and **D Mark done** on the initial failure. Configuration controls
whether planner changes need approval. **Models** edits all four role settings and limits.
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
- `src/app.rs`: view state and input; `src/backend.rs`: the workspace interface screens use
- `src/application/`: typed shared commands, transactional SQL, snapshots, definition transfer
- `src/cli/`: argument parsing, shared command calls, human/JSON output (no SQL)
- `src/runner/`: supervisor, agent prompts, process guardian, Git checkpoints
- `src/runner/provider/`: Codex, Claude Code and OpenCode launch arguments and event parsing
- `src/persistent.rs`: the SQLite workspace: TUI forms → commands → refreshed projections
- `src/demo.rs`, `src/mock.rs`: the `--demo` workspace and its deterministic fake tasks
- `src/ui/`: terminal layout: frame chrome, Current, Queue/History, dialogs, text fitting
- `src/presentation.rs`: terminal read projections and formatters; `task_text` renders tasks
- `tests/cases/workflows.rs`: real-binary mouse and keyboard interaction tests
- `tests/cases/lifecycle.rs`: revised approval, models, resolutions, waits, and handoff flows
- `tests/cases/persistence.rs`: durable terminal flows, restart, conflicts, disk transfer, isolation
- `tests/cases/cli.rs`: real-binary CLI contracts, concurrent writers, and live TUI interoperation

Run `make check` for formatting, Clippy, terminal flows, storage/migrations, isolation,
cleanup, and build checks.
The normal tests use deterministic provider processes; the opt-in live test uses authenticated Codex.

## Use ktask on your project

Install once with `make install`; it puts `ktask` in `~/.cargo/bin`. Execution also needs
bubblewrap (`bwrap`) and a signed-in agent CLI. Save model defaults once, and every project
you set up afterwards starts with them:

```sh
ktask model-default execution --provider claude --model opus --effort high --new-projects
ktask model-default planning --provider claude --model opus --effort high --new-projects
# likewise review, validation and remediation
```

The same user-wide defaults also cover the agent network, baseline checks, check timeout, run
mode, planner approval and delivery mode, so a new project needs only its verification command,
delivery destination and domain instructions:

```sh
ktask defaults --agent-network off --check-before-task on --check-timeout-ms 600000 \
    --run-mode queue --planner-approval automatic --delivery-mode local
ktask defaults            # the saved defaults, and what they would change in this project
ktask defaults --apply    # explicitly apply them to this existing project
```

Named profiles reuse a role/model/effort mapping across projects without repeating it.
Profiles map roles to models for the existing providers; ktask never claims a model exists.

```sh
ktask profile create Anthropic --from-project           # capture this project's roles as they are
ktask profile create Qwen --model execution=opencode/vendor/qwen-model --model review=opencode/vendor/qwen-model
ktask profile select Anthropic                          # this project; or --none
ktask profile default Anthropic                         # new projects start with it
ktask profile effective                                 # each role's model and where it comes from
```

A role's model resolves through the user defaults, the project's profile, the project's own
overrides (`model-default`, cleared by `profile inherit`), then a task's own models. A task
copies the result into its revision, so profile edits and selections affect only tasks defined
afterwards; apply them to a pending task with `edit --apply-project-defaults`. A profile a
project selects cannot be deleted. TUI: Configuration → **f Profiles**.

They are saved once per user and instance (TUI: Configuration → **u User defaults**, which
can preview and apply them to the open project). Only a project registered afterwards copies
the other settings; an existing project keeps them until you apply the defaults. Role models are
inherited, not copied, unless the project overrides them. Tasks and attempts keep their pinned inputs. Applying keeps the project's check commands, remote and base
branch. The check command, directories, remote and branch are never inherited.

Review, validation and remediation (the failure judge that plans each repair) may use any provider and model,
including the execution model's; the execution model applies the repair plans in its
own session, so a fix does not start cold. Workers run focused tests; the supervisor's full check
run is the gate that reviewers read. Then, in each project (a Git repository root with at least one commit):

```sh
cd ~/src/my-project
ktask init     # registers the project, detects its check, lists anything still missing
ktask          # TUI: a Add, then r Run
```

`init` detects the check from a Makefile `check` or `test` target, `Cargo.toml`, a
`package.json` test script, `go.mod`, or Python tests (pytest or unittest, at the root or in
`tests/`); `--check-command CMD` sets it explicitly. The supervisor reads the test summaries
of `cargo test`, pytest, unittest (and Django), Jest, Vitest, Mocha and `node --test`: a
reported failed test fails the check even when the command exits 0, and fewer passing or more
ignored tests than at the previous checkpoint block validation. Other runners, such as
`go test`, are judged by exit code alone. `--agent-network on` lets Codex agents that change the project use the network,
for example to install dependencies; it is off by default. `--check-before-task off` skips the
checks on a task's starting commit (on by default; Configuration → Preferences has the same
switch). `init` exits 4 and lists each fix
when the project still cannot run tasks. The TUI covers the same setup: in a new project
**u Readiness** lists what is missing, with **c Checks** (the first check is prefilled with
the detected command), **d Defaults** (choose "New projects: Start with these" to save them
for later projects), Configuration → **u User defaults** and **e Preferences** → Agent network.
`init` applies the user defaults, then your options, and prints "Project-specific setup still
to do": the verification command, the delivery destination (when the inherited mode pushes)
and domain instructions (AGENTS.md or CLAUDE.md). Each project keeps
its own queue, checks and model defaults in the same instance database, and has its own
supervisor: several projects run at the same time, each with its own TUI or CLI, and pausing
one leaves the others running.

## Task kinds

A task is one of three kinds. **AI** (default) is code work: the project checks run on the clean
starting commit, after every implementation or fix, and gate review and validation. **Human**
work is done by the owner and confirmed with Mark done. **Docs** is for documents, plans and
other non-code files: it runs like an AI task (implement, independent review, fix rounds with
the read-only judge, validation, commit and delivery) but never runs project checks. There is
no green-baseline check, no check step, and the worker is told not to run tests or builds.
Review and validation judge the written result against the goal and criteria (accuracy,
completeness, consistency with the code and existing docs, clarity) instead of the Clean Code
bar; a checked-in change must still leave links, references and tables of contents consistent.
Choose the kind in the task form (Kind: AI, Human, Docs) or with `--kind ai|human|docs` on
`add` and `edit`. `list` and `show` print it, `list --kind docs` and the Queue/History filter
(**f**) select it, other kinds are tagged in rows (`[Docs]`, `[Human]`), and TSV export/import
keeps it. A queue of only Docs tasks does not need a configured project check.

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
Answering a question continues that task at once in either mode. In queue mode, approving
current or marking Human work done also continues the queue. A saved intervention stays saved if starting fails; the footer explains the blocker.
**Pause** stops/reaps workers before recording paused state; **Run/resume** retains the attempt
and compatible session. Closing the TUI does not stop the supervisor (the background runner):
press `p` on any screen, or run `ktask pause`; it returns once the runner has exited. Start it again with `r` or `ktask run`. A failing check
returns to the worker directly; critical review and validation findings go to a read-only judge
that plans the fix. Each fix round shows a one-line **Why:** in
Status and Activity, and regenerates proof; the ten-round limit applies, then the failure judge
decides. A terminal failed attempt opens a separately recorded repair attempt.
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
`retry` are aliases that still target only current. `pause` stops the runner and waits for it;
`answer ID --text TEXT` saves an answer and the task continues. `stop ID` returns a live task to
Pending and prints the stash holding its changes; `done ID --reason TEXT` marks any unfinished task
done. `cancel ID`, `supersede ID --by 3,4` and `copy ID [--before ID]` manage pending and finished
work. A supervisor that died leaves its attempt to be settled as paused by the next `run` or TUI
refresh; worker processes it left are stopped first.
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

Reviewers start with an evidence packet, not raw log tails. Its inline result preview is at
most 12 KiB regardless of archive size. Read-only JSONL indexes list every criterion,
latest check, prior step and artifact, with source-tree identity, checksums and missing/pruned
status. Full reports and workflow context remain accessible separately. Review and validation
must inspect each criterion's cited proof, unresolved critical findings and required images;
old assessments and executor claims do not establish a pass.

`ktask evidence FILE --offset 0 --limit 4096` reads a bounded page without opening the database.
Add `--search TEXT` for literal search. Follow `next_offset` until `eof`; each search scans at
most 1 MiB. The maximum page is 8192 bytes. Use the image viewer for image evidence.

Checks can print one `KTASK_CHECK_RESULT ` line per suite followed by JSON, for example:
```text
KTASK_CHECK_RESULT {"name":"integration","status":"passed","passed":42,"failed":0,"skipped":0}
```
Statuses are `passed`, `failed`, `crashed`, `incomplete`, and `unknown`. Optional count fields
and an `evidence` reference describe the suite. Malformed records or any non-passing suite
fail the check even if its process exits zero. A nonzero exit still fails. Native framework
summaries and Minecraft GameTest completion/crash messages produce separate suite records;
Python results cannot stand in for GameTests. Checks without recognized suite reports retain
their exit-code contract and have no invented suite completion proof. Full transcripts remain
archived under the finite safeguards in [resource limits](docs/RESOURCE_LIMITS.md).

Opt-in live verification (incurs model usage; disposable project and app state):
```sh
KTASK_LIVE_MODEL=gpt-6-sol KTASK_LIVE_JUDGE_MODEL=gpt-6-astra \
  cargo test --locked --test tui runner::live_codex -- --ignored --nocapture
```

This runs two consecutive tasks in a second disposable project with the real provider,
checks their commits and distinct model roles, verifies the first project's queue is untouched,
and inspects the completed queue through mouse and keyboard terminal interactions.

## Task CLI

Without a command, the app opens the TUI. Task commands run without a terminal; `--help` and `<command> --help`
describe all flags. Use `ktask` after explicitly installing production, or the development binary:

```sh
cargo run --locked -- add --title "Fix pagination" --goal "Show every task" --criterion "Final row is visible" --approval draft
cargo run --locked -- list --json
cargo run --locked -- show 1 --json
cargo run --locked -- approve 1 --if-version 1
```

- Read: `list`, `show ID`, `current`, `events`, `stats`. List scopes: queue/history/all;
  filtering never sorts execution order. `current` returns null on an empty registered queue.
- Storage: `storage` previews logical output and artifact bytes; `storage --apply` removes
  eligible content. `retention DAYS [--if-version N]` sets the age policy (default 30 days;
  0 allows immediate cleanup of closed work). Both commands support `--json`.
- Build caches: `cache` inventories owned caches, unowned ones (listed, never cleaned),
  provider session storage (reported separately, never cleaned), exclusions and capacity
  pressure. `cache adopt [PATH]` is the explicit opt-in that takes ownership of a Cargo
  `target`; `cache clean [--apply]` previews or retires obsolete generations;
  `cache limits [--max-bytes N --max-files N --keep-generations N --grace-secs N
  --min-free-bytes N --if-version N]` shows or sets the finite bounds. All support `--json`.
- Kinds: `add`/`edit --kind ai|human|docs`; `list --kind docs` filters; text `list` prints
  `ID, version, state, kind, title`, and JSON carries `"kind"`.
- Change: `add`, `edit ID`, `move ID --before ID`, `approve`, `reject --reason`, `stop`, `done`,
  `cancel`, `supersede --by`, `copy [--before ID]`. Each task's JSON lists its `state` and the
  `actions` it offers; a refused action exits 4 naming both.
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
  this is audit metadata, not authentication. Explicit AI actors cannot mark Human work done.
- JSON: `--json` emits one `{"schema_version":1,"ok":true,"data":...}` object on stdout;
  errors emit `{"schema_version":1,"ok":false,"error":{"code":"...","message":"..."}}`
  on stderr with empty stdout. Help/version remain plain text. JSON file/path output requires UTF-8.
  Exit codes: 0 success, 1 I/O/database failure, 2 usage, 3 missing task/project, 4 state/version conflict.
- Events use ascending IDs, `--after ID`, and `--limit N` (1–1000); JSON returns `next_after`.
  Show includes all revisions/attempts and the latest 2000 stored output records per attempt.
  Missing cost is null, not zero. Check state before retrying after a lost mutation response.

`--paths --json` reports the absolute executable and isolated data roots. Future agent callbacks
must use the supervisor's absolute executable, never a bare name resolved through `PATH`.
