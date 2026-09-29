# Planner

How tasks are written. These rules are not negotiable.

## The title is a deliverable

A task's title says what someone can do, or what is true, once the task is done — in the
words of the person who benefits.

- Yes: **A user can add a new task to the queue.**
- Yes: **A user cannot remove a task while it is running.**
- No: "Add a task via CLI". "Task model and `add` command". "TUI form". "Journal refactor".

A title never names an interface, a command, a screen, a module or a technique.

## Every feature is delivered in both interfaces, in one task

A feature a user can use exists in the CLI **and** in the TUI, and one task delivers both,
wired and tested end to end. There is no task for "the CLI part" and none for "the TUI
part". A feature available in only one interface is not done.

The only exception: commands meant for agents, not for users (such as `report`), exist in
the CLI only.

## One task, one deliverable

- One small, concrete, testable piece of functionality.
- Acceptance criteria are observable in the running product, and cover the CLI and the
  TUI.
- Work with no user-visible result (a refactor, a test repair) is titled by what becomes
  true — **The queue's rules live in one place** — and its first acceptance criterion is
  that behaviour does not change.
