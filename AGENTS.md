# Working agreement

- Normal TUI tasks are now persistent. The owner authorized SQLite wiring through shared
  application commands. Keep deterministic execution/AI fixtures isolated behind `--demo`;
  never execute them against a real project or open its database in demo mode.
- Follow the owner's `docs/USECASES.md`: Current Task opens at startup and shows the
  first unfinished task in execution order, retaining it while running. An unresolved failure
  blocks later work; remediation may be inserted before it when it is not running.
  Queue, History, Configuration, and About are separate screens. Only current can run,
  resume, or retry; Queue contains current/pending work in execution order, without sorting.
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
