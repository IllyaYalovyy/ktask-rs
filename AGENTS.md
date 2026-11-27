# Working agreement

- High-level tasks are the default. A capable execution model owns investigation, design,
  implementation and verification within the task; independent models review and remediate.
  Keep planning brief. Separate specifications and breakdown are optional, never prerequisites
  merely because implementation details are unspecified. Preserve the full goal and acceptance bar.

- ktask is measured by delivered results: cost to result first, then time to result.
  Run project checks on the clean starting commit first. A red start enters bounded
  automatic repair with the failing evidence and original task goal; it is not a hard stop.
  Repeated unchanged check failures get a remediation plan before stopping for no progress.
- The normal lifecycle is implement → check → review → fix → validate within one attempt. Use
  Sol for implementation and Astra for review, remediation (the failure judge) and validation in
  this project. Sol applies each fix in its resumed implementation session.
  A failing check goes straight back to the worker with its failing tests and errors; no judge.
  Review and validation block only on critical findings (correctness, behavior, performance,
  maintainability); minor findings are recorded and never cost a round. Critical findings go to
  a read-only judge, which may repair, revise, insert or ask the owner. Every
  fix round shows a one-line why. Persist phase and the ten-repair-round limit across
  pause/resume. After ten rounds the failure judge decides. Preserve historical failed records.

- Agent fix (`ktask agent fix`, `runner::fix`) is the one owner-started way an agent repairs a
  project; ktask never starts it and it does not replace automatic remediation. It pauses the
  runner through the state machine before touching anything, runs one per project and never
  beside a report. It may use ktask commands for that project (`cli::repair` is the scope),
  repair the folder and Git without discarding work, and change the ktask user's account. Never
  project delete/move, other projects, shutdown, task done/cancel, unasked approval, shared
  settings, privileges, or product code (queue a task). Its changes are an AI actor's, naming
  the fix; refused steps are listed as left for the owner; only a repeated failure that now
  passes is done.
- Normal TUI tasks are now persistent. The owner authorized SQLite wiring through shared
  application commands. Keep deterministic execution/AI fixtures isolated behind `--demo`;
  never execute them against a real project or open its database in demo mode.
- Follow the owner's `docs/USECASES.md`: Current Task opens at startup and shows the
  first unfinished task in execution order, retaining it while running. An unresolved failure
  blocks later work; remediation may be inserted before it when it is not running.
  Queue, History, Configuration, About, and Projects are separate screens. Projects lists
  every project of the instance and opens one at a time; queues, execution order and runners
  stay per project, and no screen merges tasks across projects. Starting the TUI outside a
  registered project never registers one by itself. Deleting a project removes its rows and
  ktask's own files only after its exact name is typed, never anything in its folder; moving
  one changes only its registered folder. Both require its runner to be stopped.
  Only current can run, resume, or retry; Queue contains current/pending work in execution order, without sorting.
- The task state machine (`application::lifecycle`, docs/USECASES.md) is the one source of
  what a task allows; TUI, CLI and runner all ask it. Stop returns any live task to Pending
  with its changes stashed; remediation is automatic only. Do not add side paths around it.
- Support every user flow with both mouse and keyboard. Enable mouse support by default;
  allow disabling it in Configuration. Test both input paths in the real terminal.
- Separate Queue (browse/manage work) and Current Task (execution, output, events).
  Keep run status visible globally; preserve each view's selection and scroll independently.
  Validate with 200+ tasks across Queue and History; dialogs perform actions.
- For every view and interaction, drive the real binary in a pseudo-terminal and inspect
  its rendered screen. Unit tests alone do not count. If you cannot inspect or test it, STOP
  and report the blocker; do not mark it done or continue dependent work.
  Read-only review/validation agents inspect the supervisor's recorded PTY screens and check
  transcripts instead of rerunning writable tests. Missing evidence still blocks acceptance.
- The owner authorized persistence, CLI and real orchestration. All use shared application
  commands. Codex execution must pass supervisor checks plus independent review/validation
  before success. Never present simulated outcomes as real. CLI JSON and exit codes are contracts.
- The owner has authorized the relational SQLite schema and migration layer. This is
  application code, not disposable scaffolding. TUI mutations use `application::Service`;
  SQL, validation, transactions and version guards stay outside the TUI. Never serialize
  the mock/UI model into a database payload. CLI edits may supply `--if-version`; retain the
  same transaction/version guards as the TUI. Actor attribution is explicit metadata, not
  authentication. Never attribute an explicitly AI-issued approval to a human.
- The schema is unreleased V1: edit migrations/0001_relational_state.sql in place. Do not add
  conversion migrations or legacy fields for earlier design drafts. Never reset existing data.
- Task, revision, and attempt have separate lifecycles. Planner/remediator work uses ordinary
  tasks; approval is required only by policy. Select the latest ready revision and show newer
  drafts separately. Store execution state only on attempts; derive the displayed task status.
  Keep per-role model/optional effort and attempt/time limits on each revision. Rate-limit
  waits resume the same attempt; infrastructure failures do not consume the coding budget.
- Missing local tools and SDK components pause for external setup after a bounded probe and
  explicit recheck. Workers request a dependency through the live guardian callback; they do
  not install toolchains in task TMPDIR. Sandbox permissions are still not owner questions.
- Judges never run anything: review holds the Clean Code bar on the diff, validation checks the
  evidence is complete. A review or validation fix opens with a read-only judge; during normal
  rounds apply its repair, revise or insert decision before a worker, and carry the
  original task’s ten-round count across replans. Owner questions wait until critical findings
  remain after ten rounds. Automatic recovery from actual failures retains all four judge
  decisions. Permission and sandbox limits are never questions for the owner; missing local
  prerequisites use the dependency wait.
- Normalize input_tokens to uncached input for every provider; cache read/write are disjoint.
- Before switching tasks, preserve useful work, stop/reap workers, and clean owned scratch.
  Same-task retries may keep their workspace and reuse a compatible AI session. Never discard
  useful changes to satisfy cleanup or rewrite failed attempts when resolving their task.
- Versioning: the product name is `ktask RS`; the Cargo package version is the release version.
  Every small change increments the patch version and every large change increments the minor
  version, in the same commit; the major version changes only on the owner's direct request.
- Run `make check`. Use compiler/Clippy diagnostics and behavioral tests, not custom
  file/function line limits. Use complexity and duplication measurements to guide review;
  do not invent metric gates or refactor solely to satisfy a number.
- A resource limit exists to stop a runaway producer from filling the host, and only that may
  stop work. Everything after a producer exits (collecting and archiving evidence, bookkeeping)
  is housekeeping: complete it or degrade with a visible note, never pause sound work. Limits
  are stored settings the owner sees and changes (docs/RESOURCE_LIMITS.md); environment
  variables only override them in tests. Never retry an unchanged deterministic breach or
  repeat a recorded result.
- Resource-limit tests use explicit small per-test budgets and fixtures. Never create a
  production-scale capacity fixture inside an enclosing task's `TMPDIR`; its guardian
  accounts for those files too. Assert the specific capacity failure and cleanup.
- Keep prompts, specs, and reports short. State the outcome, constraints, and proof.
- Self-hosting: `ktask` supervises development; `ktask-dev` and tests own separate app roots.
  Never infer the instance from cwd, build profile, filename, or inherited environment.
  Future agent callbacks must use the supervisor's absolute executable path, never bare `ktask`.
- Every task/check subprocess gets an owned scratch directory as `TMPDIR`; stop and
  reap its processes before cleanup. Tests use the managed sandbox helper and private checkouts.
  Reclaim abandoned scratch on startup, never live work. Keep deliberate logs/artifacts in
  state storage, separate from scratch; never preserve an entire scratch tree for diagnostics.
- Keep the installed supervisor outside build outputs when self-hosting. Do not replace it
  during a run. Workers are guarded by a stdin lifeline, a separate workspace lease and Linux
  subreaper cleanup. Recovery must not steal a live worker's workspace. New tasks require a
  clean Git baseline; retries preserve useful changes. Never reset/clean the user's repository.
