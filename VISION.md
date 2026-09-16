# ktask v2 - Vision

> Status: DRAFT for review. This document is the north star for the Rust rewrite of ktask.
> Design authority: the human. Agents implementing this vision do not get to renegotiate its opinions.

## 1. Project Brief (the prompt)

Build **ktask v2**, shipped as the single binary `ktask-rs`, a Rust tool that supervises unattended AI-agent work on a local machine: it drains an ordered queue of tasks, hands each task to a coding-agent CLI (Claude and Codex at launch; further tools are additive), and refuses to call the work done until it has been mechanically verified, cleanly published, and confirmed present on remote mainline. It is a **local delivery supervisor**, not a multi-agent platform. Its market position is deterministic execution, verifiable completion, privacy, and recovery.

ktask v2 is opinionated by design. It is the distillation of long practical experience building applications with AI agents. Everything that experience taught us, which today lives in prompt prose and discipline, becomes mechanism: enforced by a typed state machine, mechanical gates, and a git transaction model that no agent can bypass. The design test for every feature: *could an agent ignore this?* If yes, it is not done.

The product distinction in one sentence: **other tools automate agents; ktask guarantees that ordered work was actually verified, published, and recoverable, without leaking its operational context into the repository.**

## 2. Goals and Non-Goals

### Goals (v1)

- Typed task state machine with atomic, durable, append-only persistence (SQLite).
- Completion that is never based on an agent's exit code or self-report: local verification, clean publication, and fetched remote-mainline equality are all required.
- Structured failure classification with bounded, class-specific self-healing and a circuit breaker.
- Mechanical quality gates (baseline, tests, lint, format, build, privacy) executed by the runner, out-of-band from the agent.
- Git transaction model: isolated task worktrees, serialized publication, verified against the exact candidate commit. Publication is by commit and push to mainline; pull-request handoff is deliberately out of scope.
- Work protocols: opinionated per-task state machines (`direct` and `tdd` at launch), enforced by the runner with per-phase write scopes, gates, and evidence.
- Privacy by construction: all operational state (prompts, context, logs, reports, task state) lives outside the repository by default.
- Stable provider layer with capability detection; `dummy`, Claude and Codex at launch, with adapters shaped so further CLIs are additive.
- Headless CLI backed by a core library; a full operational TUI on top. The TUI is never required for scripting or testing.
- Import of existing v1 `.ktask/` directories (tasks.md, reports, logs).
- Flight recorder: every attempt preserved with executor session, timestamps, model IDs, exit reason, commands, results, git SHAs, tokens, and cost.

### Non-Goals (v1)

- Not a multi-agent orchestration platform, cloud service, or CI/CD system. ktask never configures, runs, or waits on a hosted pipeline; every check is local.
- No parallel task execution (strict serial is the default and the only v1 mode; DAG-based parallelism is backlog).
- No agent-to-agent conversation, no model routing intelligence, no prompt optimization.
- No web UI, no daemon requirement for basic operation.
- Not a content or article pipeline. Non-code workflows (for example the Medium authoring pipeline) may eventually reuse `ktask-core` crates as a separate frontend with their own rules; their work never becomes ktask tasks.
- No automatic provider fallback (switching models mid-remediation changes behavior and context; fallback is opt-in and backlog).
- No self-modification of policy: self-healing cannot weaken checks, change configuration, install host software, or conceal failures.

## 3. Core Invariants (the constitution)

Enforced by the state machine and the git layer, not by prompts:

1. Exactly one task is active at a time; parallel execution does not exist in v1.
2. A successor cannot start until its predecessor reaches `published_verified`.
3. Every state transition is persisted atomically before it takes effect.
4. A task is never done based only on an agent exit code or statement.
5. Self-healing cannot weaken checks, change policy, install host software, or conceal failures.
6. Prompts, context, logs, reports, and task state remain outside the repository.
7. Completion requires local verification, clean publication, and fetched remote-mainline equality.
8. Design decisions belong to the human. An unresolved product or technical decision is a first-class pause state, and its resolution is recorded as a decision record (ADR) available to future tasks.

v1 is strict-only: no relaxation knobs, no dev mode, no per-invariant overrides. An opinion you can silently disable is not an opinion. A non-strict mode may exist one day, but only alongside DAG-based parallel execution (backlog), and it will be loud.

One deliberate exception to invariant 6: ADRs. Recorded design decisions are committed to the repository (`docs/adr/`) because they are project documentation, valuable to humans and to future tasks alike. Everything else operational (prompts, context, plans, logs, reports, state) stays outside.

## 4. Primary Usage (UX contract)

```bash
# One-time, per machine
ktask-rs doctor                  # provider preflight, git, toolchain, permissions

# Per project (no .ktask/ created in the repo)
ktask-rs init                    # registers project; state under $XDG_STATE_HOME/ktask
ktask-rs import ~/Projects/foo/.ktask   # migrate a v1 queue, reports, logs

# Queue management
ktask-rs add                     # $EDITOR with a structured task template; malformed tasks are rejected
ktask-rs plan lint               # validate the whole queue before running anything
ktask-rs status                  # headless dashboard

# Execution
ktask-rs run                     # drain the queue, strict serial
ktask-rs run --task 7            # single task
ktask-rs tui                     # the full operational interface

# Recovery and decisions
ktask-rs retry --task 7          # fresh remediation with the failure bundle
ktask-rs resolve --task 7        # answer a waiting_input question; stored as ADR
ktask-rs ack                     # pass a human gate
ktask-rs privacy audit           # scan repo and optionally git history for leaked artifacts
ktask-rs stats                   # tokens, cost, durations, success rates per task/queue
```

Exit codes remain semantic and scriptable (drained, task failed, provider limit, human gate, needs input, interrupted).

Tasks are authored as structured Markdown blocks: human-friendly to write, but with required sections (Outcome, Done-when, Verify, Refs) that `add` and `plan lint` validate; a malformed task never enters the queue. Plans live in ktask state, not in the repository.

## 5. Architecture Overview

Cargo workspace:

```
crates/
  ktask-core/    # state machine, event journal, gates, git transactions, providers, classifier
  ktask-cli/     # headless commands; thin over core
  ktask-tui/     # ratatui app; thin over core; testable via TestBackend
```

`ktask-core` has no terminal I/O; progress and events flow through typed channels/callbacks. Both frontends consume the same event stream, so everything the TUI shows is scriptable from the CLI.

Persistence: one SQLite database per project under `$XDG_STATE_HOME/ktask/<project-id>/`, containing an **append-only event journal** (the source of truth) and **materialized current state** (a projection, rebuildable from the journal). Project identity is the canonical repository path plus remote identity.

## 6. Task Lifecycle

Pipeline states:

```
queued -> preflight -> running -> verifying -> publishing -> published_verified -> done
                          ^          |
                          +-- remediating (bounded)
```

Durable pause states: `waiting_limit`, `waiting_input`, `human_gate`, `interrupted`, `blocked`.
Terminal states: `done`, `failed`, `cancelled`.

- `preflight` proves the world is sane before spending tokens: clean fetched mainline, green `baseline_command`, provider available, disk space, lock acquired.
- `waiting_input` is the mechanism behind invariant 8: the agent (or a gate) surfaces a structured decision request (question, options, trade-offs, impact); the queue pauses; `ktask-rs resolve` records the answer as an ADR that is injected into the context of subsequent tasks.
- **Crash recovery is deterministic and is a first-class feature, not an edge case.** A machine can lose power mid-gate, mid-commit, or mid-push; the supervisor must come back knowing exactly what happened. On restart it inspects the live process table, the worktree, and the last persisted transition, then either resumes the in-flight phase or marks the attempt `interrupted`. It never guesses, and it never silently re-runs work that may already have taken effect.
- Every state transition is journaled **before** its side effect, so an interruption is always recoverable to a known state rather than an ambiguous one. Recovery from an interruption at any phase boundary — including mid-publication, the dangerous one — is exercised by tests that kill the process at each point, not merely reasoned about.
- Every attempt is preserved separately: executor session ID, timestamps, configured and provider-reported model IDs, exit reason, commands run, gate results, git SHAs, tokens, and cost.
- Task context is assembled by the runner, never hand-injected per task: in v0.1 it is a static context document from the private prompt library plus every ADR recorded so far; typed-source assembly under a size budget (VISION excerpt, relevant ADRs, prior resolutions) arrives in v0.2.

## 7. Failure Taxonomy and Self-Healing

All failures are classified before any recovery is attempted:

| Class | Meaning |
|---|---|
| `agent_failure` | agent could not complete the implementation |
| `verification_failure` | tests, lint, build, or privacy checks failed |
| `provider_limit` | usage limit, with known or unknown reset |
| `provider_transient` | network error, temporary service failure, process crash |
| `provider_configuration` | authentication, invalid model, missing executable |
| `git_conflict` | branch drift, rejected push, conflicting publication |
| `environment_failure` | missing SDK, dependency, or host capability |
| `policy_failure` | forbidden file, dirty tree, attempted gate bypass |
| `needs_input` | unresolved product or technical decision |

Recovery policy is configurable per class, within hard limits:

- Every remediation launches a fresh provider session seeded with a compact failure bundle (classification, gate output, diff summary, prior attempt evidence). Session resume is never relied on: determinism and reproducibility outrank token economy.
- Preserve the worktree and all prior attempt evidence across remediation.
- Detect repeated identical failure signatures and trip a circuit breaker.
- Bound remediation by attempts, elapsed time, and token budget.
- `provider_limit` with a known reset waits until the exact reset time, with margin and jitter; unknown resets use bounded backoff.
- `provider_configuration` and `needs_input` never loop: they pause for the human immediately.
- After any remediation, every completion gate reruns from scratch. No cached evidence survives a file change.
- Every recovery produces a self-healing report: classification, attempted repairs, final result.

Self-healing repairs project-controlled defects only. It never touches host configuration, never edits gate definitions, never invents product policy.

## 8. Mechanical Quality Gates

Gates are runner-executed commands, defined per project in a verification profile:

- `baseline_command`: prove the project was green before the task started.
- `targeted_test_command`: fast edit-loop verification during the run.
- `verify_command`: the mandatory, complete local suite. Not optional, not skippable by config in strict mode.
- `lint_command`, `format_command`, `build_command`.
- `privacy_command`: scan staged files, tracked files, and the full outgoing commit range for forbidden paths and content patterns.
- `flake_command`: repeated or randomized execution of affected tests (optional, recommended).

Each gate has its own timeout, environment, working directory, and retry policy. Green results are cached only against the exact tree hash plus the gate-configuration hash; any file change invalidates all cached evidence. Common test output formats (Cargo, JUnit, pytest, Flutter) are parsed into structured results while raw output is retained.

## 9. Work Protocols (per-task state machines)

The supervisor lifecycle (§6) is about custody of a task and is fixed. What happens *inside* `running` is a second, per-task state machine: the **work protocol** — an opinionated definition of what it means to work on a task. Each protocol is a sequence of typed phases; every phase declares its write scope (which paths the agent may modify), its gate command, its provider/model (phases may use different providers: one implements, another reviews), and the evidence it records. Loops declare bounds. The current protocol and phase are first-class state, visible in the TUI queue and inspector.

Protocols are chosen per task; they are not user-definable in v1:

- **`direct`** (v0.1): single implementation phase, then the mandatory completion gates. v1-equivalent behavior.
- **`tdd`** (v0.1): runner-enforced red/green/refactor, below.
- **`spec-first`** (v0.2): define goal, define scope, write acceptance tests, implement, review, red/green loop with an iteration bound, harden, done-check, DONE.
- **Composition** (backlog): custom protocols assembled from typed phase primitives (agent phase with write scope, mechanical gate, human gate, bounded loop). The constitution is structural: every protocol must terminate in the mandatory verify-publish gates, every loop must be bounded, gates cannot be removed. How you work is configurable; what done means is not. There will never be a free-form workflow DSL.

### The `tdd` protocol

No final test run can prove tests were written first, so the runner enforces order when a task opts in (or the profile defaults to it):

1. **red**: the agent may add or modify tests only; production paths are read-only (enforced via configured test-path globs per language profile).
2. The runner executes `targeted_test_command` and confirms the expected *new* failure.
3. **green**: implementation changes become permitted.
4. The runner confirms the new test passes.
5. **refactor**: cleanup allowed while targeted tests stay green.
6. Full verification and publication follow as usual.

Explicit exception categories (recorded in task history): documentation, pure refactoring, build configuration, and bugs already covered by a failing test. RED and GREEN evidence (command, output, tree hash) is stored with the attempt.

## 10. Git Transaction Model

Isolated worktrees even for strictly serial execution; the user's normal checkout is never touched.

1. Fetch remote mainline.
2. Create the task worktree from the fetched remote SHA.
3. All changes are committed inside the task worktree; a dirty tree at verification time is a `policy_failure`.
4. Final verification runs against the exact candidate commit.
5. Publish per the configured strategy, under a repository lock that serializes all integration and publication operations.
6. Push mainline, fetch again, and require local candidate SHA to equal remote mainline SHA.
7. Only after that comparison does the task reach `published_verified`.

Publication is **commit and push to mainline**, and that is the only mode in
v1. No pull requests, no forge integration, no CI system to wait on: the gates
have already run locally, and a green gate run against the exact candidate
commit is the evidence. Adding a review handoff would mean adding a second
definition of done, which is the one thing this design refuses.

`local-merge`, `review-only` and `pull-request` handoff are backlog.

## 11. Privacy by Construction

No `.ktask/` in project repositories by default.

- State under `$XDG_STATE_HOME/ktask`, configuration under `$XDG_CONFIG_HOME/ktask`.
- Prompts and templates live in a global, private prompt library; per-project overrides also live outside the repo.
- Secondary protections: `.git/info/exclude` entries, optional pre-commit and pre-push guards.
- Before every push, the complete outgoing commit range is inspected for forbidden paths and content patterns; detection covers already-tracked AI artifacts, not only newly staged files.
- Tokens, credentials, and configured secret patterns are redacted from logs.
- Restrictive filesystem permissions; configurable retention for logs and attempt evidence.
- `ktask-rs privacy audit` reports on the repo and, optionally, git history.

## 12. Provider Layer

A stable capability interface, with adapters:

- Claude and Codex at launch. Kiro, OpenCode, Goose and further CLIs are additive: the adapter interface is designed for them, but they are backlog and no launch behavior depends on them.
- A built-in `dummy` provider ships as a first-class adapter: it replays predefined, deterministic responses (success, failure, hang, limit message, input request) on cue. It powers the scenario suite, CI, and offline development of ktask itself.
- Startup capability detection: structured output, model selection, usage telemetry, approval modes. (Session identifiers are still recorded in attempt evidence, but no correctness path depends on session resume.)
- `ktask-rs doctor` performs a minimal real provider preflight.
- Rate limits, authentication errors, input requests, and session identifiers are normalized into the failure taxonomy and pause states.
- Configured vs provider-reported model IDs are both recorded; unexpected mismatches are rejected.
- Provider selection by task type is supported (for example: one provider implements, another reviews).
- Provider fallback is opt-in and off by default.

## 13. TUI

**The TUI is the primary interface**, not a decorative view over the CLI. It is
where an operator lives while work is running: watching a task execute, reading
why one failed, answering a blocking question, inspecting a diff before it is
published. It is held to the same standard as the engine — a terminal UI that
merely prints what the CLI already prints has failed its purpose.

The CLI remains a first-class, complete interface for everything scriptable and
everything quick: adding tasks, checking status, resolving input, driving
unattended runs. Neither is a subset of the other in capability; they differ in
posture. Both are views over the same `ktask-core`, and no behavior may exist in
one that cannot be reached from the other.

Screens:

- **Queue**: ordered tasks, blockers, current phase, attempts.
- **Task inspector**: objective, acceptance criteria, dependencies, completion gates, work protocol and current phase.
- **Live run**: agent output, commands, test results, resource usage.
- **History**: event timeline across every attempt and remediation.
- **Logs**: raw and structured views, search, filters, follow mode, error navigation.
- **Git**: changed files, diff, commits, publication state, remote comparison.
- **Failures**: classified causes, repeated signatures, available actions.
- **Input inbox**: focused questions with context, impact, and recommended response.
- **Configuration and doctor results.**

Essential actions: pause, interrupt, resume, retry, resolve, acknowledge, cancel, attach, open diff, rerun gate, export sanitized diagnostics. Every action is also a CLI command; the TUI is a view over the same core.

Requirements that make it a real TUI rather than a rendering of log output:

- **Live, not polled-looking**: output streams as it is produced, with visible phase and progress; the interface stays responsive while a task runs.
- **Navigable**: keyboard-driven throughout, discoverable (a key map that is always reachable), consistent bindings across screens, and no action that is available only by editing files.
- **Attachable mid-flight**: opening the TUI while a run is in progress shows the live state immediately, and closing it never disturbs the run.
- **Honest under stress**: long output, tiny terminals, resize, and unicode content degrade gracefully and never corrupt the display.
- **Testable headlessly**: see §15. A TUI that can only be checked by eye is not finished.

## 14. Development Plan

Scope note: the TUI is **v1 scope, not a later addition**. It is the primary
interface (§13), so a release without it is not a release. It is listed under
v0.2 below only in the sense of build order — the engine must exist before it
can be rendered — and both phases are required for v1.

**v0.1 (foundation, the minimum honest product):**
state machine + SQLite journal; headless CLI with v1 command parity; structured Markdown task format with `plan lint`; `import` for v1 `.ktask/`; gates (baseline, targeted, verify, lint, format, build, basic privacy scan); git transaction model with commit-and-push publication; work protocols `direct` and `tdd`; failure classifier with bounded fresh-session remediation and circuit breaker; static context + ADR recording and injection; `dummy`, Claude, and Codex adapters; `doctor`; attempt records including tokens and cost; `stats`.

**v0.2 (operations, and equally required for v1):**
The complete TUI — all nine screens of §13 (queue, live run, logs, failures and inspector first; input inbox, history, git, configuration and doctor after); `waiting_limit` exact-reset handling; `flake_command`; full `privacy audit`; typed-source context assembly with size budgets; `spec-first` protocol with per-phase provider selection.

**Backlog (post-v1, explicitly deferred):**
independent read-only review agent before publication; dependency DAGs with strict serial default and opt-in parallelism for explicitly independent tasks; test-impact-based targeted checks; built-in flaky-test investigation with persisted reproduction evidence; shared task templates and verification profiles; GitHub/GitLab issue import and closure; desktop notifications; enforced cost/token/time budgets (recording ships in v0.1, enforcement is backlog); reproducible sanitized execution bundles; retrospective generation proposing prompt/context patches as human-merged PRs; provider fallback; further provider adapters (Kiro, OpenCode, Goose); protocol composition from typed phase primitives; reuse of `ktask-core` (journal, state machines, providers, TUI widgets) by other supervisors, such as an article-authoring frontend.

## 15. Testing Strategy

- The core state machine and classifier are pure logic: exhaustive unit tests, property tests on journal replay (materialized state must always equal the journal projection).
- **Scenario suite**: the built-in `dummy` provider drives full end-to-end runs with deterministic outcomes; assertions on final states, journal contents, exit codes, and git results. v1 ktask behavior on shared scenarios serves as the semantic baseline.
- Git layer tested against disposable local remotes (bare repos), including conflict, rejected-push, and drift scenarios.
- An optional integration tier (off by default, behind a flag) smoke-tests real provider adapters against a cheap model. It exists for adapter development only and is never part of scored or gating test runs.
- TUI tested headlessly via ratatui `TestBackend` snapshots; no screen-watching required anywhere in the suite.
- Toolchain gates on the project itself: fmt, `clippy -D warnings`, `#![forbid(unsafe_code)]` outside audited process/PTY modules, `cargo deny` with a pre-approved dependency allowlist.

## 16. Risks and Mitigations

| Risk | Likelihood | Mitigation |
|---|---|---|
| Scope creep toward a platform | Certain | §2 Non-Goals; backlog discipline; the one-sentence product distinction as the test |
| Provider CLI churn breaks adapters | High | capability detection, `doctor` preflight, thin adapters behind a stable trait |
| TDD phase enforcement misclassifies files | Medium | per-language test-path globs, explicit exception categories, loud recorded overrides |
| Protocol engine drifts toward a workflow DSL | High | fixed built-ins only in v1; composition restricted to typed primitives; completion gates structurally mandatory |
| SQLite state corruption | Low | append-only journal as source of truth; materialized state rebuildable; periodic backup |
| Migration friction from v1 `.ktask/` | Medium | first-class `import`; v1 kept runnable until parity is proven |
| Scope is large enough that a run may not finish | High | tasks are ordered so each phase boundary is a coherent, demonstrable product; an unfinished run is a real result, not a void one |
| TUI becomes a time sink | High | headless CLI first; TUI is a view over the event stream, phased in v0.2 |
