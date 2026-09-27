# Vision

`ktask-rs` orchestrates AI-driven software tasks and executes them reliably on one
machine. It replaces `ktask`, a Python script, and must give more control, better
visibility and less fragility.

## Principles

- **Orchestration and reliable execution are the product.** Everything else is
  configuration.
- **Nothing is built in that a project might not want.** Reviews, checks, commits, pushes,
  PRs, approvals — each is a step a project adds when it wants it. A bare task is: hand it
  to an agent, record what happened. (Reusable process templates: next version.)
- **No CI/CD.** Nothing waits on a hosted pipeline. Checks, when configured, run locally.
- **It must always be obvious what is happening and what has happened.**
- **Every process element must earn its place.** Friction without value is a defect.

## Queue

- An ordered list of tasks. Each task is one small, concrete, testable piece of
  functionality.
- Tasks can be added anywhere, inserted between two others, and reordered. IDs never
  change when the queue does.
- **Breakdown.** If a task is too big, the working agent says so, and a more capable model
  (configurable) splits it into smaller tasks inserted in its place.
- Tasks run one at a time, in order.

## Pipeline

Each task runs through a pipeline of steps configured for the project, overridable per
task. Step kinds are mechanisms: an agent step with a role (plan, implement, review, test,
resolve), a command step, a human approval step.

- **Multi-model.** Every agent step names its provider and model, with a fallback.
  Different roles and different tasks can use different models.
- **Review → fix loop.** When configured: a reviewer produces findings; findings are
  critically filtered — unrelated, unimportant and repeated ones are dropped — before a
  fixer sees them; the loop runs until clean or a cut-off, and each round must reduce what
  remains or the loop stops.
- **Self-healing and escalation.** When a step fails, configurable hooks decide what
  happens next: retry, give the same model a diagnosis from a more capable one, hand the
  task to a more capable model, or stop for a human.
- **Verification by the tool.** Whatever checks a project configures, the tool runs them.
  An agent's claim of success is never evidence.

## Git

The tool does the git work that needs no judgement, so no tokens are spent on it: before a
task starts it makes sure the work begins from an up-to-date code base; it gives the agent
an isolated working copy; it records what changed, for status, diffs and audit. It does not
decide what gets committed, pushed or proposed — commits, pushes and PRs are configured
steps. Each mechanism can be turned off per project.

## Providers

A provider is an agent CLI described by configuration: the command and arguments, how the
prompt and model are passed, how output is streamed and parsed, and which messages mean a
limit, an error or a question. **Adding a new CLI is a configuration change, not a code
change.**

Claude Code and Codex ship configured at launch, plus a scripted `dummy` for tests.
Integration with them is deep and tested: streamed output, session and usage data, limits,
errors, model reported vs requested. Where a CLI offers structured output, a provider
configuration can use a built-in parser for it; the parsers are few and generic, not one
per CLI.

## Status and audit

- **Live status** for every task: current step, attempt *X of N* and why the last attempt
  ended, time spent (per step and total), model in use.
- **Every step is recorded**: provider, model, duration, tokens, cost, outcome, and how
  much help it needed — retry, hint, stronger model, human.
- **The tool shows whether the process works.** Per model and per step: first-attempt
  success, escalation rate, cost per delivered task, review rounds. If every task needs the
  expensive model to finish, the tool says so.
- **Cost and limits.** Usage is tracked against each provider's plan limit. Near a limit
  the tool pauses until the reset or falls back to another provider, as configured. A
  limit, network failure, full disk or hung process never costs a task an attempt.

## Escalation to a human

A stop tells the operator, on one screen: **what** happened (which step, which check,
which error), **why** (cause, and whose it is — this task, an earlier one, or the
environment), **when**, and **exactly what is expected** — one decision with options and a
recommendation, or one command. Never a symptom like "report not found".

## Outside the repository

All tool state and configuration — queue, journal, logs, prompts, reports, project
settings — lives outside the project's repository (`$XDG_STATE_HOME`, `$XDG_CONFIG_HOME`).
The tool never creates files in the working tree. The repository only receives the work.

## Interfaces

- **TUI** — where the operator lives: queue, live output, status, failures with their
  what/why/ask, pending decisions, history and audit. Keyboard-driven, attachable mid-run.
- **CLI** — complete for scripting and for agents: every TUI action and every view has a
  CLI command with `--json` output. Agents read and change tool state only through it.

The TUI exists because a terminal application can be driven and read by tests with no
human looking at it. **Every screen and every flow is exercised end to end, by tests,
against the real binary.** That is why the product is a TUI.

## Not

A hosted service, a CI system, a multi-agent platform, or a workflow programming language.
No parallel task execution.
