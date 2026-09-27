# Vision

`ktask-rs` runs an ordered queue of software tasks through AI coding agents on one
machine, and decides for itself — never on an agent's word — whether each task is done.

It replaces `ktask`, a Python script. It must give more control, better visibility and
less fragility.

## What it is

- **A queue.** Tasks are imported from Markdown once; the queue then lives in the tool's
  state, not in files in the repository. Tasks run one at a time, in order. Tasks can be
  inserted anywhere in the queue.
- **A supervisor of agent CLIs.** Claude Code and Codex at launch, plus a scripted `dummy`
  provider for tests. Adding a provider must not touch anything else.
- **A process engine, not a process.** How a task is worked on — implement, review, test,
  open a PR, wait for approval, escalate to a stronger model — is per-project
  configuration, assembled from mechanisms the tool provides. The tool has no built-in
  opinion about TDD, reviews, PRs or which model does what. When nobody knows the right
  process, changing it must be cheap.
- **Verification by the tool.** Whatever checks a project configures, the tool runs them
  itself. An agent's claim of success is never evidence.
- **Recoverable.** Every event is journaled before its effect. Killing the process at any
  point — mid-agent, mid-check, mid-push — leaves a state the tool can explain and resume.
- **Honest about failure.** Every stop is classified: infrastructure (limits, network,
  disk, hang), agent protocol, failing checks, or a decision only a human can make.
  Infrastructure failures wait and retry; they never cost the task an attempt. Every
  failure report answers three questions: **what** went wrong, **why**, and **what exactly
  the operator must do** — one decision or one command.
- **Measured.** Every attempt records provider, model, duration, tokens, cost, and how
  much help it needed (retry, hint, stronger model, human).
- **Out of the repository.** All operational state — queue, logs, prompts, reports — lives
  outside the project's repository. The repository only ever receives the work.

## Interfaces

- **TUI** — where the operator lives: the queue, live agent output, failures with their
  what/why/ask, pending decisions, history, diffs. Keyboard-driven, attachable mid-run.
- **CLI** — complete for scripting. Every TUI action has a CLI command.

The TUI exists because a terminal application can be driven and read by tests with no
human looking at it. **Every screen and every interaction is exercised end to end, by
tests, against the real binary.** That is not a testing preference; it is why the product
is a TUI.

## Not

- Not a hosted service, a CI system or a multi-agent platform.
- No parallel task execution.
- No workflow programming language. Process is configuration.
