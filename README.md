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
To configure ktask, see [Configure ktask](#configure-ktask).
To connect an interactive agent, see [MCP for Codex and Claude Code](#mcp-for-codex-and-claude-code).
To oversee several computers from one, see [Link hosts](#link-hosts).

| Instance | Start | App directory under each XDG root |
| --- | --- | --- |
| Development | `cargo run --locked` | `ktask-dev/` |
| Installed | `ktask` (install explicitly with `make install`) | `ktask/` |

Two tiny executable entry points select the instance; both use the same application code.
Build profile, working directory, renaming the binary, and inherited environment cannot switch it.
Ordinary builds never update the installed executable. `--paths` shows the executable and
config, state, data, cache, and runtime locations; the header identifies the instance.

Reusable tool generations live below the explicit instance's XDG data root (`data/tools`);
task scratch and build caches live under XDG cache and are disposable. `tool register`
records an existing installation and its concrete version, host platform, architecture and
components. External paths remain user owned and are never updated or deleted by ktask.
For an existing Android SDK, register `~/Android/Sdk` as `android-sdk` with each required
semicolon separated component (for example `system-images;android-36;google_apis;x86_64`),
then `tool bind android-sdk ID --if-version 0` in the project. The same registered ID can be
bound by another project in this instance. `tool list` shows readiness and binding versions;
`tool refresh` explicitly acknowledges a changed external tree; `tool unbind` returns a
project to its normal environment. The Configuration screen's
`t Tools` dialog offers the same registration and binding commands by keyboard or mouse.
Projects without bindings retain their existing environment. Bound tools supply PATH and
Android SDK variables to probes, checks and workers. Package download caches are under
`cache/package-downloads`, project runtime data under `cache/project-runtime/ID`, and
lockfile dependent build trees stay in their project. No task installs SDKs in its TMPDIR.
If a bound generation disappears or changes, Current Task enters Needs dependency; refresh
or rebind the generation and use Recheck before resuming.

Managed Android provisioning is optional and **manual mode is the default**. A reviewed
`android-36-google-apis-x86_64` recipe installs pinned `platform-tools` 37.0.1, `emulator`
37.2.13, API 36 revision 2 and the Google APIs x86_64 image revision 7 from the Google stable
SDK repository. Register a compatible existing
SDK containing Android CLI or `sdkmanager` as the external bootstrap. Accept its SDK licenses
interactively outside ktask first, inspect `tool provision-plan android-36-google-apis-x86_64 ID`,
then record consent for that exact source and license set with
`tool provision-consent android-36-google-apis-x86_64 ID`. Consent records the source licenses
and bootstrap executable digest. Set
`tool provision-mode automatic` to permit the fixed recipe; `tool provision-mode manual` disables
it. `tool provision-install android-36-google-apis-x86_64 ID` runs it explicitly, while a matching
SDK dependency wait can invoke it once without another AI turn. `tool provision-status` and
`tool provision-cancel` report progress and request cancellation. The Tools dialog on Current Task
and Configuration exposes the same plan and controls. A successful install binds a managed SDK;
the waiting task is rechecked before its original run session continues. Missing consent,
Java, usable KVM or a compatible bootstrap leaves manual setup and Recheck available.
The reviewed packages require `licenses/android-sdk-license` to contain their accepted license
hash; unrelated or empty license files leave the dependency waiting.
Unbind an explicitly selected SDK before choosing a managed generation; provisioning does not
replace that binding silently.

The installer uses its own bounded staging and `TMPDIR` under XDG data, a separate reusable
`data/provision-download-cache`, then publishes a generation atomically under `data/tools`;
task scratch is never used for an SDK download. The plan shows its separate 12 GiB download
cache, 12 GiB staging, 32 GiB retained and 2 GiB free-space budgets and 30-minute
limit. The package manager uses explicit SDK roots: current Android CLI receives `--sdk=PATH sdk
install`, and installed `sdkmanager` receives `--sdk_root=PATH --install`. Android's
Google repository manager handles archive checksums; the recipe isolates its configuration,
retains its cache for supported resumes, and rejects a manager failure or mismatched package
revision before publication. The source bootstrap executable and license files are hashed at
human consent. Android's
[environment variables](https://developer.android.com/tools/variables) distinguish the shared
SDK from isolated AVD and user state; [sdkmanager](https://developer.android.com/tools/sdkmanager)
is deprecated in favor of [Android CLI](https://developer.android.com/tools/agents/android-cli/commands/sdk).
Installing an emulator does not grant [KVM access](https://developer.android.com/studio/run/emulator-acceleration)
or authorize a personal device.
Android-bound runs get a fresh owned `ANDROID_AVD_HOME`, a private adb server port, a leased
emulator console/adb port pair (`KTASK_EMULATOR_PORT`), and `ANDROID_SERIAL` for that emulator.
For the reviewed image, `KTASK_ANDROID_AVD_TEMPLATE` points to a small reusable configuration
template; it never contains a mutable disk. Create the AVD inside the owned directory with the
exact installed image before starting it, and
pass `-port "$KTASK_EMULATOR_PORT"` to the emulator. The runtime and its mutable disk are reset
when the run ends; the SDK and system image stay shared. Android sandboxes hide attached device
nodes and the personal `~/.android` directory. They also use a private network namespace so
adb cannot discover or explicitly select another project's host emulator. Set project agent
network to Off before an Android-bound run; an incompatible On policy leaves an actionable
dependency wait. The Codex provider retains API connectivity. Its network-restricted project
commands use the `adb` and `emulator` wrappers on PATH; a scoped guardian callback runs those
commands against the bound SDK and owned AVD inside one private Android network namespace.
Direct socket access and overrides targeting another server or device remain denied. This does
not change the network policy of non-Android tasks.

Scratch files belong to one run and are removed on exit. After a crash or forced kill,
the next launch reclaims abandoned scratch directories; ownership locks protect live runs.
Tests use private homes and app roots under `$TMPDIR/ktask-test-scratch/` (system temporary
directory when unset), cleaned on success or panic. The next test reclaims crash debris.

The TUI opens **Current Task**, with separate **Projects**, **Queue**, **History**,
**Configuration** and **About** screens. Normal startup opens real SQLite tasks for the
current directory. **Projects** lists every project of the instance with its state, queue size and
current task; Enter opens one in the same terminal, and each project keeps its own queue,
background runner and place on screen. Started in a directory that is not a project, while
other projects exist, the TUI opens **Projects** and registers nothing until **n Add this
folder**; `--project DIR` and the first project of an instance register as before.
`ktask projects` prints the same list from any directory.
On **Projects**, **m Move** points the selected project at another existing folder after you
moved or cloned its repository (`ktask project-move ID DIR`); tasks, history and settings
follow it and ktask moves no files. **D Delete** (`ktask project-delete ID --confirm NAME`)
is permanent: after you type the project's exact name it deletes every task, attempt, output,
evidence and setting of that project and ktask's own files for it, and touches nothing in the
project's folder. Both are refused while the project's runner is alive; the TUI does not
delete the project it has open.
A new project starts empty; changes save immediately, including after a forced exit.
The project name defaults to the directory name. For a new directory with a name already in
use, launch with `--project-name UNIQUE_NAME`. Each instance retains its own database.

Click tabs, rows, buttons, and form fields; use the wheel to scroll. `0`–`5` or `Tab`
switches screens (`0` is **Projects**); `j/k`, arrows, `PgUp/PgDn`, and `Home/End`
navigate. `?` shows shortcuts.

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
the project checks on its clean starting commit. A red start goes to the worker to repair the
cause and complete the original goal. A check that fails after work also goes to the worker with its
failing test names, panic or assertion messages and compiler errors, read from the transcript
without a model; the same summary is the failure's recorded reason. Three failures of the same
check with matching diagnostics on unchanged source trigger a remediation plan. If its repair leaves the same failure,
automatic recovery stops with the evidence instead of spending more tokens on identical work.
Changed source or a passing check breaks this streak; the ten-round repair limit still applies.
Provider and process-launch failures retain bounded retries and the infrastructure failure limit.
Review and validation block
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
Normal import/export reads and writes **real files**. The `ktask-tasks-v4` escaped TSV format (v3, v2 and v1 still import)
preserves all acceptance criteria, work category, review lenses, provider/model/effort, and limits. Imports preview and commit
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
- `src/projects.rs`, `src/application/overview.rs`: the Projects screen, project switching
  and the cross-project read query behind it and `ktask projects`
- `src/application/registry.rs`: moving a project to another folder and deleting one
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

## Configure ktask

Settings live at three levels. A lower level overrides the one above it:

| Level | Applies to | Set it with |
| --- | --- | --- |
| 1. Global | Role models: every project without its own override. Other defaults: copied into a project when it is registered | `ktask model-default --new-projects`, `ktask defaults`, `ktask profile default`; TUI Configuration → **u User defaults** |
| 2. Project | One Git repository | `ktask init`, `check-set`, `model-default`, `profile select`, `delivery`; TUI Configuration |
| 3. Task | One task | `ktask add` / `ktask edit` flags; TUI task form and **Models** |

Install once with `make install` (puts `ktask` in `~/.cargo/bin`). Execution also needs
bubblewrap (`bwrap`) and a signed-in agent CLI. Run every command inside a project directory or pass
`--project DIR`; `model-default`, `defaults` and `profile` register the current directory as a
project if it is not one, so run them inside your first real project. `ktask` and `ktask-dev`
keep separate settings.

### 1. Global configuration

**Role models.** A task needs `execution`, `review`, `validation` and `remediation` models;
`planning` is used only by `breakdown`, and `operator` only by [agent reports](#agent-reports)
and [agent fixes](#agent-fixes)
(unset, they use the remediation model). `--provider` is `codex`, `claude` or `opencode`; `--model`
is optional for `claude` (Sonnet); `--effort` is optional. Any role may use any provider.

```sh
ktask model-default execution   --provider claude --model sonnet --effort medium --new-projects
ktask model-default review      --provider claude --model opus   --effort high   --new-projects
ktask model-default validation  --provider claude --model opus   --effort high   --new-projects
ktask model-default remediation --provider claude --model opus   --effort high   --new-projects
ktask model-default planning    --provider claude --model opus   --effort high   --new-projects
```

`--new-projects` saves the global default and also sets the current project's own override for
that role. To remove the project's own overrides, so its selected profile or the global models
apply, run `ktask profile inherit --all`. Role models are inherited, not copied: every project
without its own override uses the current global models for tasks added afterwards.

To reuse one set of roles across projects, save it as a named profile and make it the global
default (`profile create --model ROLE=PROVIDER/MODEL` also works; a profile a project selects
cannot be deleted; TUI: Configuration → **f Profiles**):

```sh
ktask profile create Anthropic --from-project   # capture this project's roles as they are
ktask profile default Anthropic                 # new projects start with it (or --none)
```

**Other defaults.** These are copied into a project when it is registered:

```sh
ktask defaults --agent-network off --check-before-task on --check-timeout-ms 600000 \
    --run-mode queue --planner-approval automatic --delivery-mode local   # all options optional
ktask defaults           # show the saved defaults and what they would change in this project
ktask defaults --apply   # apply them to an existing project
```

The project where you ran these commands was registered before the settings were saved, so run
`ktask defaults --apply` there once; it also applies the default profile and keeps the project's
check commands, remote and base branch. Those three are never global.

### 2. Project configuration

In the project's Git repository (at least one commit, with `git config user.name` and
`user.email` set):

```sh
cd ~/src/my-project
ktask init --check-command 'make check'   # register; omit the option to detect the check
ktask                                     # TUI: a Add, then r Run
```

`init` applies the global defaults only when it registers the project. On a registered project it
sets the check and any switches you pass, then lists what is still missing. It
detects the check from a Makefile `check` or `test` target, `Cargo.toml`, a `package.json` test
script, `go.mod`, or Python tests; `--check-name NAME` names it. Override only what differs for this
project:

```sh
ktask check-set lint --command 'make lint' --timeout-ms 600000          # add a check; all must pass
ktask model-default review --provider codex --model gpt-6-sol --effort high   # this project only
ktask profile select Anthropic          # use a profile here (or --none)
ktask delivery --mode pr --remote origin --base main                    # local (default), direct or pr
ktask init --agent-network on           # let agents that change the project use the network
ktask init --check-before-task off      # skip the checks on a task's starting commit
ktask profile effective                 # each role's model and which level supplies it
ktask profile inherit --all             # drop this project's model overrides
```

Project model settings change only tasks added afterwards. Checks cannot change while an
attempt is running. TUI: Configuration → **m Defaults** (models), **c Checks**, **e Preferences** (run
mode, mouse, agent network, baseline checks) and **g Git delivery**. Put project rules for agents
in `AGENTS.md` or `CLAUDE.md` at the repository root.

### 3. Task configuration

A new task copies the project's effective models. Override them, and set limits, on the task itself:

```sh
ktask add --title "Fix pagination" --goal "Show every task" --criterion "Final row is visible" \
    --model review=codex/gpt-6-sol --effort review=high \
    --attempt-limit 5 --task-limit 6h --stage-limit 45m
ktask edit 7 --model execution=claude/opus --effort execution=high --if-version 2
ktask edit 7 --clear-model review                 # remove a role's model and effort
ktask edit 7 --apply-project-defaults             # re-copy the project's current models
```

`--model` is `ROLE=PROVIDER/MODEL` (repeat per role); `--effort ROLE=VALUE` sets effort (`none`
clears it). Limits are `--attempt-limit N|unlimited` (default 3), `--task-limit off|on|DURATION` and
`--stage-limit off|on|DURATION`. Other per-task options are `--kind ai|human|docs`,
`--category NAME` and `--lens NAME`. An edit adds a new revision; a running or finished task cannot
be edited, and `ktask show 7` prints the effective settings. TUI: **e** Edit the task, then **Models**.

### Which value wins

A role's model comes from the first of these that is set: the task, the project's own overrides
(`model-default`), the project's selected profile, the global defaults. Profile and default
changes affect only tasks defined afterwards.

## Use ktask on your project

Set up the project with `ktask init` as described in [Configure ktask](#configure-ktask).
Workers run focused tests; the supervisor's full check run is the gate that reviewers read.

The supervisor reads the test summaries of `cargo test`, pytest, unittest (and Django), Jest,
Vitest, Mocha and `node --test`: a reported failed test fails the check even when the command
exits 0, and fewer passing or more ignored tests than at the previous checkpoint block
validation. Other runners, such as `go test`, are judged by exit code alone. `--agent-network on`
applies to Codex agents; it is off by default. `init` exits 4 and lists each fix when the project
still cannot run tasks and prints "Project-specific setup still to do": the verification
command, the delivery destination (when the inherited mode pushes) and domain instructions.
In the TUI, **u Readiness** lists what is missing, with **c Checks** (the first check is
prefilled with the detected command) and **d Defaults** (choose "New projects: Start with
these" to save them globally). Each project keeps its own queue, checks and model defaults in
the same instance database, and has its own supervisor: several projects run at the same time,
each with its own TUI or CLI, and pausing one leaves the others running.

## Move a project to another computer

A project moves as two files: its tasks (`export`) and its settings (`settings-export`). Code
travels with Git. History, attempts, evidence, usage and cost stay on the old computer.

On the old computer, in the project folder, write both files **outside** the repository (an
untracked file stops the next run):

```sh
ktask settings-export ../app-settings.json
ktask export ../app-tasks.tsv            # --scope all also exports finished definitions
```

On the new computer:

1. **Clone** the repository, in any folder, and copy the two files over:
   `git clone URL ~/code/app && cd ~/code/app`. Set the Git commit identity there.
2. **Import settings.** Preview first, then apply:

   ```sh
   ktask settings-import ../app-settings.json --preview
   ktask settings-import ../app-settings.json
   ```

   Import registers the folder as a project when it is not one yet. The preview shows every
   setting that would be added, changed or left as it is, the folder, the paths rewritten to
   it, absolute paths outside the project for you to confirm, and the programs, provider CLIs
   and tool bindings that are missing here. Install and sign in to those, and register tools
   again with `ktask tool register` / `bind`.
3. **Import tasks**, then run: `ktask import ../app-tasks.tsv && ktask run`. Import settings
   first: tasks name the custom categories and lenses the settings bring.

What the settings file carries:

- **The project:** checks (in order, including disabled ones), role models, the selected
  profile, project categories and lenses, default lenses, delivery, retention and storage
  limits, agent report and fix limits, agent network, baseline checks, provisioning mode and
  preferences (mouse, run mode, planner approval).
- **Shared by every project of the instance** (import marks these `shared`): user defaults and
  user models, all profiles, user categories and lenses, and build cache limits. Importing
  them on a computer with other projects changes what those inherit; check the preview.
- **Not carried:** tasks and their history, the project name (the new folder's name, or
  `--project-name`), installed tools, tool registrations and bindings (only their names and
  versions, to report what is missing), license consents, credentials and Git remotes.

It is one readable, versioned JSON file. Paths inside the project are written as
`${KTASK_PROJECT_ROOT}` and resolved against the importing folder; the recorded source folder
is informational and never used. `settings-export` refuses to replace a file without
`--overwrite` and only reads, so it works while the runner is running. `settings-import`
applies everything or nothing; it refuses a malformed file, a file of a newer format and a
folder that is not the root of a Git repository. It replaces the settings the file names and
keeps all others: it never deletes tasks, history or settings absent from the file, existing
tasks keep their revisions, and importing the same file twice changes nothing. When it would
change what a running attempt uses (checks, delivery, agent network, baseline checks, run
mode, planner approval, retention or provisioning), pause the runner first (`ktask pause`);
changed checks or delivery also need the attempt to be over, as with `check-set` and `delivery`.

In the TUI, Configuration → **E Export settings** and **o Import settings** (keyboard or mouse)
do the same; Import shows the preview before **Apply**. In `--demo` they use in-memory files
(`/demo/settings.json`).

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
keeps it. A queue of only Docs tasks does not need a configured project check. The kind says who
does the work; for AI-run work the [work category](#work-categories) says how: AI defaults to
**code**, Docs to **documents**, and `--category art` (or any category of your own) changes the
templates and whether checks apply.

## Work categories

A work category tells the AI turns how to do one kind of work. It is a name, a choice whether
the project's checks apply, and one template each to **execute**, **review**, **validate** and
**remediate** (the failure judge and the repair turns). ktask fills the task's title, goal,
numbered criteria and the turn's runtime context into the template. Provider profiles stay
separate: a profile says which model runs a role, a category says what that role is asked to do.

Built in: **code** (project checks apply, review holds the Clean Code bar), **documents**
(no checks; judged for accuracy, completeness, consistency and clarity) and **art** (no
checks; judged on the delivered files and the visual evidence available, with no code checks
required). The art templates describe asset work with the tools and files that exist in the
environment; ktask supplies no image-generation tool and no template pretends it does. A task
with no chosen category follows its kind: AI runs **code**, Docs runs **documents**, and Human
work, which the owner does, has none. Existing tasks keep exactly that behavior.

Placeholders, written `{{name}}` (see `ktask category placeholders`): `{{title}}`, `{{goal}}`,
`{{criteria}}` (numbered from 1), `{{category}}` and `{{context}}`. Context is **runtime-only**:
review scope, check results, evidence paths, earlier findings, repair plans and answers exist
only while a turn runs, and a preview shows a labelled stand-in. Values are inserted as typed
and never scanned again, so a goal that mentions `{{context}}` reaches the model literally. A
placeholder only chooses where content goes: a task field or the context that a template leaves
out is appended after it. Write `\{{` for a literal `{{`. Unknown or unclosed placeholders
are reported when the category is saved.

Supervisor rules are not part of a category. Around every template ktask adds its own fixed
rules: read-only sandboxes for judges, evidence handling, no commits or branch changes,
scratch only under `TMPDIR`, no questions about permissions, the follow-up callback, and the
structured response with one evidence entry per criterion. Whether checks apply decides the
check steps and which supervisor text a worker or judge receives, whatever the templates say.

```sh
ktask category list                                      # code, documents, art and your own
ktask category save "Poster art" --scope user --no-checks \
    --description "Print posters and banners" \
    --execution 'Design the poster described below as a vector file. Use only tools that exist here; do not claim to have generated an image you cannot show. Task: {{title}}. Goal: {{goal}}. Criteria: {{criteria}}' \
    --review    'Open the exported poster and judge it against {{criteria}}: layout, legibility, palette, dimensions. Context: {{context}}' \
    --validation 'Validate {{criteria}} from the delivered files and any rendered images in the evidence; fail what you cannot see.' \
    --remediation 'Fix each reported poster defect at its cause without changing approved assets.'
ktask category save "Poster art" --scope project --review @review.txt   # this project's own override
ktask category preview "Poster art" --role review        # the effective prompt, runtime parts labelled
ktask add --title "Spring poster" --goal "A3 poster for the spring fair" \
    --criterion "poster.svg exists at A3" --category "Poster art"
ktask category preview --task 7                          # the task's content in its pinned templates
```

A name resolves through this project's override, then the user scope, then the built-in
categories. `category save` creates a category or changes the fields given (`--if-version N`
rejects a stale edit); a new name needs all four templates and `--checks` or `--no-checks`, and
`--from NAME` copies another category first. A template value `@PATH` reads the file.
`category delete --scope user|project` makes the name inherit again. TUI: Configuration →
**w Work categories** lists, creates, edits, overrides per project, previews and deletes them
(a template is one form line: `\n` is a line break); the task form's **Work category** field
selects one, and the Kind default applies while it reads Default.

A task revision pins the category, its checks choice and its templates when it is defined, so
editing or deleting a category never changes a running or resumed attempt; the next revision
(an edit, or one the judge writes) pins the then-current templates. An attempted task keeps its
category: naming a different one is refused, so switch by copying the task to a new one.

## Review lenses

By default every AI-run task gets one independent review, held to its category's bar. A **review
lens** is an opt-in specialist review that a task can use instead, such as **concurrency**,
**security** or **simplicity** (built in), or one you write. Selecting lenses never creates audit
tasks: the same task, attempt and ten-round repair budget cover them all.

A task with lenses is reviewed once per lens, in the order selected. Each lens gets its own
read-only session on the configured review model, with the task's title, goal and criteria and
the same review scope and evidence (the diff and the check results), and nothing from the other
lenses. The review fixed rules still apply, and the category's general review is not run for the
task, so choose a lens such as simplicity when you want that bar kept. Findings keep the lens
that raised them. Only after **every** selected lens has finished does ktask act: a critical
finding from any lens makes one repair request for that reviewed source state, through the
usual failure judge and repair worker, and counts as one of the attempt's ten repair rounds
however many lenses found something; minor observations are recorded and never trigger a
repair. Lenses add no owner approval step. After a repair the new source state is reviewed by
every lens again. A paused or interrupted attempt resumes without repeating a lens whose review
of the unchanged source state is finished.

The built-in lenses ask only for defects a reviewer can reach: each finding names the file and
line, the concrete trigger (a step-by-step interleaving, an attacker-controlled input, or the
future change that gets harder), the consequence and the smallest proportionate correction;
anything weaker is minor.

```sh
ktask lens list                                       # concurrency, security, simplicity and your own
ktask lens save "Data races" --scope user --description "Shared state in the scheduler" \
    --instructions 'Review only unsynchronized shared state in the change. Task: {{title}} ... {{context}}'
ktask lens save security --scope project --instructions @security.txt   # this project's override
ktask lens preview security --task 7                  # the effective prompt with the task's content
ktask lens defaults --set security simplicity         # new tasks select these, in this order
ktask lens defaults --clear                           # back to the ordinary single review
ktask add --title "Lock the cache" --goal "..." --criterion "..." --lens concurrency --lens security
ktask edit 7 --no-lenses                              # this task keeps the ordinary review
ktask show 7                                          # steps and attempts[].lens_reviews name each lens
```

Instructions use the category placeholders (`ktask category placeholders`); a placeholder only
chooses where content goes and anything left out is appended. A name resolves through the
project's override, then the user scope, then the built-ins. `lens save` takes `--if-version N`
and `lens defaults` `--if-current a,b` to reject a stale edit. A task revision pins the selected
lenses with the instructions they had, so editing a lens or the defaults never changes a running
or resumed attempt. Without `--lens` or `--no-lenses`, a new task takes the project defaults
and an edited task keeps its own. Up to five lenses per task.

TUI: Configuration → **L Review lenses** lists, creates, edits, overrides per project, previews
and deletes lenses and sets the project defaults; the task form's **Review lens 1…n** fields
choose the ordered selection (the first reads Project default, None or a lens; the others add
lenses). Task details list **Review lenses**, and each attempt's report and Reviews view
label every lens verdict and its findings.

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

Defaults apply to new tasks only; see [Configure ktask](#configure-ktask) for existing tasks.
In Current, **Run** starts a detached supervisor using the saved run mode. Run one stops after
current succeeds; Continue queue advances after verified success. The Run button names its mode;
**m Run mode** opens preferences directly from Current. The global strip shows that preference
and the next task; Current shows the goal, proof required, and the active supervisor phase.
Answering a question continues that task at once in either mode. In queue mode, approving
current or marking Human work done also continues the queue. A saved intervention stays saved if starting fails; the footer explains the blocker.
**Pause** stops/reaps workers before recording paused state; **Run/resume** retains the attempt
and compatible session. Closing the TUI does not stop the supervisor (the background runner):
press `p` on any screen, or run `ktask pause`; it returns once the runner has exited. Start it again with `r` or `ktask run`.
From any directory, `ktask pause --all` pauses active runners in both `ktask` and `ktask-dev`.
Before an upgrade, `ktask shutdown` also closes open TUIs.

A failing check returns to the worker directly; critical review and validation findings go to a read-only judge
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
judge, and the queue keeps going. Automatic recovery stops when its limits are reached:
the attempt budget or the task's time limit is used up, ten repair rounds left critical findings,
the judge replanned ten times, a repeated unchanged check failure persists after a remediation
plan and repair, or the provider/environment failed three attempts in a row
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

## Agent reports

When something went wrong, ask ktask to investigate instead of logging in and prompting an AI to
read logs. An agent report is owner-started, separate from the automatic failure judge, and wider
in scope. It changes nothing: no file, task, attempt or setting, and it never disturbs a live
attempt or its workspace, whether the runner is running, paused, failed or absent.

```sh
ktask agent report                                  # explain the project and its current task
ktask agent report --task 7 "Why did the check fail twice?"   # prints the report ID at once
ktask agent show 3                                  # progress while running, then the report
ktask agent stop 3                                  # end it and reap its processes
ktask --json agent list
ktask agent limits --time-limit-ms 600000 --cost-limit-usd 1.50
ktask model-default operator --provider claude --model opus   # default: the remediation model
```

In the TUI press **i** (or click **[i Investigate]**) on Current Task, or on Projects for the
selected project. Type an optional question and task ID and apply: progress appears live. Esc
closes the view and the investigation goes on; **s** stops it. To read past reports choose one in
the Report field; **[** and **]** page through them. Limits are in Configuration → **i Report
limits**. `--demo` shows simulated reports and starts no agent.

- **What it reads.** A snapshot of ktask's records for the project (tasks, revisions, attempts,
  events, evidence, runner log, configuration, checks, delivery, dependency waits), the project
  folder and its Git state, and read-only host probes (tool versions, disk space, processes).
- **Sandbox.** It runs on the project's host in bubblewrap with its own scratch `TMPDIR`; the
  project, its Git metadata, ktask's state and configuration, bound tools and other registered
  projects are mounted read-only, and the provider's read-only tool policy applies. The rest of
  the host is guarded by that policy, as for review and validation.
- **Limits.** One investigation per project; a second is refused naming the running one. Default
  15 minutes and 2 USD. The time limit applies to every provider. The cost limit ends a Claude
  Code (`--max-budget-usd`) or OpenCode report; **Codex reports no cost**, so only the time limit
  bounds a Codex investigation, and the record says so.
- **The report.** What happened; then the cause, with the events, attempts, evidence and log lines
  that show it (a cited event, attempt or step that does not exist is marked); then proposed fix
  steps, each a ktask action, configuration change, repository change, host change or owner-only
  decision. When the cause is a defect in ktask itself the report says so, and steps that cannot
  repair it are marked as workarounds. Nothing is applied.
- **Records.** SQLite keeps question, requester, model, progress, token usage and cost, and
  deletes them with their project. One that failed, timed out, passed its cost limit or was
  stopped is recorded as such with what it had found, never as a finished report.
  `ktask shutdown` stops running investigations. Remote access is not part of this command yet.

## Agent fixes

A report says how to fix something; an agent fix does it, so you do not have to log in. You always
start it: ktask never does, an AI actor (`--actor-kind ai`) is refused, and automatic remediation
of task findings is unchanged.

```sh
ktask agent fix --report 3                    # carry out a finished report's steps; prints the ID
ktask agent fix "The build check names the wrong script" --resume
ktask agent fix --report 3 --dry-run          # print the steps it would take; change nothing
ktask agent show 4                            # live progress, then the record
ktask agent stop 4                            # end it and reap its processes
ktask agent limits --fix-time-limit-ms 1800000 --fix-cost-limit-usd 5
```

In the TUI press **f** (or click **[f Fix]**) on Current Task, on Projects for the selected
project, or on a finished report. Choose the report or type an instruction, choose what happens
when verified, Apply, then confirm in the dialog that names the project and the allowed scope.
The record is followed live; Esc leaves it running, **s** stops it, **[** and **]** page through
past fixes. `--demo` shows a simulated fix and starts no agent.

**What happens.**

1. The project runner is paused through the task state machine and the fix waits for it to exit.
   No runner starts, and no other command stashes or changes the project folder, until the fix ends.
   One fix or report runs per project.
2. What fails now is noted: the project checks are run, unmet requirements of the current task
   and uncommitted files no task owns are recorded, and the agent names further commands that
   must pass afterwards.
3. Work in the tree that no task owns is saved as a git stash named
   `ktask: agent fix #N: work found in the tree`. A paused or failed task's own uncommitted work
   stays in place, because its attempt continues with it; the record names it.
4. The agent works, with the operator model, its own scratch `TMPDIR` and the fix limits (default
   30 minutes and 5 USD; Codex reports no cost, so only time bounds it).
5. The same things are repeated. The fix is **Done** only when something that failed now passes,
   nothing else broke, and it left no stray file. It is **Failed** when nothing got better, when
   something passing now fails, when the agent did not finish, or when work or history was
   discarded. Otherwise it is **Unverified**. With `--resume` only a Done fix continues the
   queue; otherwise the runner stays paused.

**Allowed.** ktask commands for this project (checks, models, delivery, setup, project-scope
categories and lenses, tool bindings, task edits, adding and moving tasks, stop, restart, retry,
and answering a question when the instruction gives the answer); repairing the project folder
and Git state without discarding work (commit or stash); changing the host inside your account,
for example installing a missing tool. Restart and retry never start a worker during the fix:
the task runs when the queue continues. Changes are made by the same application commands, with
the same validation and version guards, and recorded as `agent fix #N`, an AI, never as you.

**Not allowed.** Deleting or moving projects, touching other projects, shutting ktask down,
changing limits of agent runs, settings shared across projects, marking tasks done, cancelling or
superseding them, approving drafts unless the instruction says `approve #ID` (or `approve all
drafts`), elevated privileges, and `reset`, `clean`, `restore`, `rebase`, amend, force-push or
deleting branches or stashes. Code changes to the product are added to the queue as tasks;
project files the agent writes itself are named and keep the fix from Done. Refused steps are
listed in the record as left for the owner. Under planner approval `review-required`, tasks and
revisions the fix creates are drafts.

**How it is bounded, and where it is not.** The agent runs in bubblewrap: ktask's records, caches
and program (both instances), other registered projects and bound tools are read-only, it cannot
see or signal other processes, and it cannot gain privileges. ktask commands reach ktask only
through the fix's own callback, which applies the scope above. A guarding `git` refuses
destructive commands, and refs and stashes are checked afterwards: anything lost is kept under
`refs/ktask/agent-fix-N/` and fails the fix. The rest of your account is writable, so this guards
against mistakes, not against a hostile agent. Like other writable turns it follows the agent
network setting: with it off, Claude Code and OpenCode cannot fix, and Codex keeps its workspace
sandbox, so host changes and Git commits are left to you. Remote access is not part of this
command yet.

**The record.** `agent show` lists each step and its result, the files, settings, commits and
stashes changed, the verification outcome with before and after, and what remains. A stopped,
timed out or over-budget fix keeps the steps already applied and leaves the runner paused.
`ktask shutdown` stops running fixes.

## Link hosts

One computer, the **parent**, can list the projects of other computers, its **children**,
[open them in its TUI](#open-a-childs-project-in-the-tui) and
[control their tasks](#control-a-childs-tasks) from its CLI. A child dials out over SSH
(`ssh PARENT ktask link accept`), keeps that one connection and reconnects by itself, so it
needs no address the parent can reach. Authentication and encryption are your SSH keys and
`known_hosts`; ktask adds no cryptography and opens no TCP port. Nothing is linked until you
set it up, and every computer must run the same ktask version.

**Control is on after `ktask link parent`**: from then on the parent can change and run the
child's tasks. Run `ktask link control off` on the child to make the link read-only.

**1. A key that can only link.** On each child create a key without a passphrase (the
service cannot type one), and give the parent an alias in `~/.ssh/config`:

```sh
ssh-keygen -t ed25519 -N '' -f ~/.ssh/ktask_link
cat >> ~/.ssh/config <<'END'
Host ktask-parent
    HostName parent.example.net
    User me
    IdentityFile ~/.ssh/ktask_link
    IdentitiesOnly yes
END
```

**2. Restrict it on the parent.** Add the child's `~/.ssh/ktask_link.pub` to the parent's
`~/.ssh/authorized_keys` as one line, with the full path of the parent's `ktask`:

```
command="/home/me/.cargo/bin/ktask link accept",restrict ssh-ed25519 AAAA... ktask-link@devbox
```

`command=` makes sshd run `ktask link accept` whatever the child asks for, and `restrict`
refuses a terminal and any port, agent or X11 forwarding. That key therefore gets no shell and
nothing but `link accept`, which only pipes the connection into the parent's hub. Without the
restriction the key is an ordinary login key. Connect once by hand (`ssh ktask-parent`) to
verify and record the parent's host key; the link never accepts an unknown one by itself.

**3. Name the parent and run the link.** On each child:

```sh
ktask link parent ktask-parent        # --name NAME lists it under another name
```

Then run `ktask link run` on the parent and on every child. It stays in the foreground; use
the shipped [systemd user unit](scripts/ktask-link.service) to keep it running:

```sh
cp scripts/ktask-link.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now ktask-link
loginctl enable-linger                # keep it running while you are logged out
journalctl --user -u ktask-link       # what it connected to, or why not
```

The unit sets `KillMode=process`, so stopping or restarting the link leaves runs its parent
started alone. A user service has a short `PATH`; add the folders of your agent CLIs with
`systemctl --user edit ktask-link` (`[Service]`, `Environment=PATH=...`) or runs started from
the parent will not find them. Copy the unit again if you installed an earlier one.

With a parent configured `link run` dials it; without one it serves children, and that is the
parent. It retries after any failure, waiting from 1 up to 30 seconds. Changes reach the
parent within two seconds; both sides send a heartbeat every 5 seconds and drop a connection
that is silent for 20.

**What you see.** The parent's **Projects** screen and `ktask projects` show one list sorted
by name then host: local projects marked `local`, the others with their host name, with state,
queue size and current task. The header counts include connected hosts. A disconnected host's
projects stay listed as **Offline** with the time of last contact until
`ktask link forget HOST`. Project and task IDs are per host:
`ktask --json projects` adds `host` (null for local) and `online` to every project, remote
offline ones also `last_contact_ms`, and `hosts` lists the linked computers.

**Check and undo.** `ktask link status` (`--json`) reports the role, peer, whether it is
connected, the last contact and the last error. `ktask link parent --clear` unlinks a child
and ends its `link run`. A host of another ktask version or link protocol is listed as
**Incompatible**, with both versions named on both sides, until they match. `ktask shutdown`
also stops `link run`; start the service again after an upgrade.

The command the child asks for is `ktask link accept` (`ktask-dev link accept` from the
development instance), so without the `command=` line `ktask` must be on the `PATH` of the
parent's non-interactive SSH shell. `link accept` finds the hub below `XDG_RUNTIME_DIR`, which
SSH logins and user services share on a standard systemd system.

### Open a child's project in the TUI

On the parent's Projects screen, press Enter on a remote project (or click **Enter Open**). It
opens in the same terminal with the same Current, Queue, History, task details, events and
output as a local project, and follows the child live; the header names the project and its
host, such as `billing @ devbox`.

Run, Pause, Stop, Restart, Approve, Reject, Answer, Add, Insert, Edit, Move, Cancel and Mark
done work through the same dialogs. They are sent to the child, whose own commands and state
machine decide; a form stays open until the child answers and shows its refusal. Statistics
and Readiness are the child's. Runs belong to the child and continue when you leave.

**Local-only** on the child: configuration (models, checks, profiles, categories, lenses,
delivery, settings export and import), tools and dependency setup, storage and build caches,
project move and delete, reports and fixes, task models, break down, supersede, copy to queue
and task import and export. Their keys say so.

The Current log shows the newest 256 KiB of a live run; task details page through all of it.
Each project keeps its own view, selection and scroll while you switch. When the child goes
**offline**, its screens stay readable, marked offline with the time of last contact, and its
actions are disabled with that reason until it reconnects. A project that was never opened
opens once its host is connected. A read-only link (`ktask link control off`) opens for
reading only.

The TUI sees a child only through the link: it opens none of its files and no database.

### Control a child's tasks

On the parent, add `--host NAME` and name the project by its ID or name on that host:

```sh
ktask --host devbox --project billing list
ktask --host devbox --project billing add --title "Fix paging" --goal "Show every row" --criterion "Last row is visible" --approval draft
ktask --host devbox --project billing approve 12 --if-version 1
ktask --host devbox --project billing run --background
ktask --host devbox --project billing pause
```

`list`, `current`, `show`, `events`, `add`, `edit`, `move`, `approve`, `reject`, `run` (and
`resume`, `retry`), `pause`, `stop`, `restart`, `done`, `cancel`, `answer` and `stats` run this
way. The
child's own `ktask` runs the command line, so JSON, text, exit codes, `--if-version` guards and
the task state machine are exactly the local ones, and the parent never opens a child's database
or files. Everything else, including `project-delete`, `project-move`, `shutdown`, `pause --all`,
`tool` and provisioning commands, is refused by the parent and again by the child. `--host`
registers nothing: the project must exist on the child.

**Who did it.** Pass `--actor-kind` and `--actor` as for a local command. The child records the
name followed by the parent it is configured with, such as `claude-code via ktask-parent` (still
as AI) or `owner via ktask-parent`, and notes every requested change in the project's events.

**Read-only.** `ktask link control off` on the child keeps `list`, `current`, `show`, `events`,
`stats` and the parent TUI's reads, and refuses every change (exit 4, code `read_only`). `ktask link control on` allows
changes again, `ktask link control` shows the setting, and naming the parent again keeps it. A
child only ever takes commands over its own connection to its configured parent.

**When the link fails.** Nothing is ever retried for you.

| Exit | Code | Meaning |
| --- | --- | --- |
| 3 | `not_found` | No linked host of that name, or no such project or task there. |
| 5 | `host_offline`, `host_incompatible` | Fails at once; the command was not sent. |
| 6 | `outcome_unknown` | The connection was lost before the answer; it may or may not have run. Read the task and events before running it again. |

A command that reached the child runs to its end there even if the connection is lost or you
interrupt the parent's `ktask`. So a foreground `run` waits like a local one, but Ctrl-C on the
parent does not pause it: use `ktask --host NAME --project P pause`. A child runs at most 32
commands for its parent at once, and returns at most 32 MiB each of output and diagnostics.

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
  `ID, version, state, kind, title`, and JSON carries `"kind"` and `"category"`.
- Categories: `category list|show|save|delete|preview|placeholders`, and `add`/`edit --category NAME`
  (see Work categories); `edit --kind` alone returns to that kind's default category.
- Review lenses: `lens list|show|save|delete|defaults|preview`, and `add`/`edit --lens NAME` (repeat,
  in review order) or `--no-lenses` (see Review lenses); JSON carries `"lenses"` and, per attempt,
  `"lens_reviews"` with each lens's summary and findings.
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
- Settings: `settings-export FILE [--overwrite]` and `settings-import FILE [--preview]` move
  everything configured that is not a task (see Move a project to another computer). JSON
  export lists `written`; import and preview report `changes`, `rewritten`, `outside` and
  `missing`.
- Selection: `--project DIR` selects a canonical project root without changing instance identity.
  Read commands do not register projects or create absent databases; missing projects return 3.
  Add, import, settings-import (or the TUI) register a project. `--project-name NAME` resolves a
  new-root name collision.
- Concurrency: agents should pass `--if-version N` from list/show for every existing-task mutation.
  Without it, the command reads the latest version and still guards its transaction. A conflict
  never silently retries. Successful mutations return committed ID/version receipts; reread for state.
- Attribution: defaults to `human: owner`. AI callers must pass `--actor-kind ai --actor NAME`;
  this is audit metadata, not authentication. Explicit AI actors cannot mark Human work done.
- JSON: `--json` emits one `{"schema_version":1,"ok":true,"data":...}` object on stdout;
  errors emit `{"schema_version":1,"ok":false,"error":{"code":"...","message":"..."}}`
  on stderr with empty stdout. Help/version remain plain text. JSON file/path output requires UTF-8.
  Exit codes: 0 success, 1 I/O/database failure, 2 usage, 3 missing task/project, 4 state/version conflict;
  with `--host` also 5 host offline or incompatible and 6 outcome unknown.
- Events use ascending IDs, `--after ID`, and `--limit N` (1–1000); JSON returns `next_after`.
  Show includes all revisions/attempts and the latest 2000 stored output records per attempt.
  Missing cost is null, not zero. Check state before retrying after a lost mutation response.

`--paths --json` reports the absolute executable and isolated data roots. Future agent callbacks
must use the supervisor's absolute executable, never a bare name resolved through `PATH`.

## MCP for Codex and Claude Code

`ktask mcp` is one host-wide stdio server for every project of the instance, so a single
user-level registration in Codex and in Claude Code serves them all. Copy the absolute
executable path from `ktask --paths` (`ktask-dev` is a separate instance with its own projects)
and register it once per client:

```sh
codex mcp add ktask -- /absolute/path/to/ktask --actor codex mcp
claude mcp add --scope user --transport stdio ktask -- /absolute/path/to/ktask --actor claude-code mcp
```

These commands were checked with codex-cli 0.166.0 and Claude Code 2.1.331.

The server opens the project for each call, so projects added, moved or deleted while it runs
are seen as they are then, and a failed call never stops it from serving the others. It starts
from any directory, even outside a project or before the instance has a database, and never
creates storage or registers a project. A project whose folder is missing can still be read and
take drafts.

| Tool | Use |
| --- | --- |
| `projects` | List every project with the fields of `ktask --json projects`: `id`, `name`, `root`, `state`, `runner_alive`, `queued`, `finished` and `current`. |
| `current` | Read the current task, including its state, available actions and latest attempt. |
| `list` | Browse the queue in execution order or select `history` or `all`. Pass `next_offset` as `offset` to continue. |
| `show` | Inspect a task by `id`, including its definition, version, and any newer draft. |
| `events` | Read project events in ascending ID order. Pass `next_after` as `after` to continue. |
| `add_draft` | Append a draft with `title`, `goal`, and `criteria`. The owner can review it in Current Task or Queue. |

**Choosing the project.** `current`, `list`, `show`, `events` and `add_draft` take an optional
`project`: a project ID or the absolute folder it is registered at, both listed by `projects`.
Without it they use the project the server is pinned to, else the registered project containing
the server's working directory. When none applies, or the ID or folder is not registered, the call
fails without side effects and the error points to the `projects` tool. Every result names the
project it acted on (`project.id` and `project.name`). Add `--project /absolute/path/to/project`
before `mcp` only to pin the server to that one registered project; it then refuses calls that
name another.

For example, ask “Which projects does ktask have?”, “What is blocking the current task in
billing?” or “Draft a task in billing to fix pagination, with a criterion that the last row stays
visible.” The server is read-only apart from `add_draft`: approvals, execution controls, edits,
moves and deletion are available only through the CLI and TUI. Drafts are recorded as AI-authored
using the configured `--actor` name, which defaults to `mcp`; `--actor-kind human` is refused.
