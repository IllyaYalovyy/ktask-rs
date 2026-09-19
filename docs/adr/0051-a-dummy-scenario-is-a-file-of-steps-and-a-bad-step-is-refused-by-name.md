# 0051. A dummy scenario is a file of steps, and a bad step is refused by name

- **Status:** accepted
- **Date:** 2026-09-19

## Context

T055 gives the `dummy` provider its script. VISION.md §12 calls it "a built-in
`dummy` provider … [that] replays predefined, deterministic responses (success,
failure, hang, limit message, input request) on cue", and §15 makes it the thing
the scenario suite runs on: "the built-in `dummy` provider drives full
end-to-end runs with deterministic outcomes". The outcome of the task is that
"deterministic provider behavior is declared in a file", and `tests/scenarios/
README.md` already warns that a hidden subset scores the implementation — so the
file format has to be right rather than merely workable for the cases in view.

What is not yet decided, and this task cannot defer, is what the file means. The
task names the fields (`on_task` or `on_attempt`, an `outcome` of five words,
optional `stdout`, `exit_code`, `delay_ms`, and `files` to write) and the two
done-when clauses (a round trip through TOML, and an unknown outcome refused by
naming the step). Everything else about what a step means is a decision here:

- **A step has to be addressable, and only two of its fields say which session
  it answers.** `on_task` is coarse — every session of one task — and
  `on_attempt` is fine, so the two together can say "fail once, succeed on the
  retry" (`AttemptId` is per-task, VISION.md §6). Neither alone covers the cases
  the suite needs; both at once on one step is two rules claiming one step.
- **A scenario whose fields mean nothing is not deterministic.** `exit_code` and
  `delay_ms` are optional in the format, and "optional" is the interesting part:
  a scenario that omits `exit_code` on a `failure` step still has to run a
  session that reports something, because a scenario that ends the attempt with
  status 0 silently changes what the classifier is being asked to classify.
- **A typo in a scenario is a silent finding.** The whole value of the suite is
  that an outcome was *provably* produced. A `timeout_ms` nobody defined that
  reads as "no extra key" and an `outcome = "expode"` that reads as a session
  that did something are two ways a run passes against a script that never ran.
- **A step writes files into somebody's working directory.** VISION.md §10
  isolates a task in its own worktree and §3's invariants are built on
  attribution to that directory, so a scenario reaching above it would be a
  fixture that damages a checkout it was never assigned.

## Decision

The scenario format lives in `provider/dummy.rs`, and its rules are load-time
rules — a scenario that cannot be replayed as written is refused before a session
starts, not discovered mid-run.

**TOML, as a list of steps.** `[[steps]]`, read in the order written, in the
same format `Config` and the verification `Profile` are written in. TOML is
already a dependency, and a scenario is data an operator or a reviewer edits by
hand: the format's job is to be readable in a diff.

**Exactly one cue per step.** `on_task` or `on_attempt`, never both and never
neither. The cue is stored as the file wrote it (`Option<TaskId>` /
`Option<AttemptId>`) and the rule lives in `Scenario::validate`, so a scenario
built in code is held to it as firmly as one read from disk.

**The outcome is kept as the word it was written as, and checked by name.**
`StepOutcome` is the five-word enum with an `ALL` ledger, and `Step::outcome_kind`
reads the field against it. Deserializing into an enum instead would refuse a
sixth word earlier and less usefully: by the time serde has given up, the position
of the step that carried it is gone, and "unknown variant" is not a finding an
operator can act on. `Scenario::validate` keys the refusal `steps[i].outcome` and
quotes both the word it refused and the cue of the step — task or attempt number —
which is the done-when clause.

**An absent `exit_code` is the one the outcome word implies.** `failure` reports
1 and every other word reports 0; a declared value always wins, in both
directions, including the pair (`failure`, `exit_code = 0`) that stages an agent
reporting success while the scenario knows it is not done. That pair is the
format's own test that completion is never read off an exit status (VISION.md §3
invariant 4). This is a default about a script, not a substituted measurement:
ADR-0049 forbids inventing a figure a provider did not report, and a scripted
session has no figure until its step says one.

**`files` is a path-to-contents table, refused above the working directory.** A
`BTreeMap`, so the order files are written in is the order of their paths rather
than the order a hash table happened to iterate them in — the deterministic
replay §15 asks for. A path is accepted when every one of its components is a
name, which is one rule that refuses the absolute path, the `..` that climbs out,
and the empty key, without a list of what else a path might try.

**Anything the format does not define is refused.** `deny_unknown_fields` on both
the document and the step, and a scenario with no steps at all is refused: an
empty script answers no session, and loading it to run nothing is how a suite
goes green by going empty.

Load failures are `Error::Config` (ADR-0001): a file that cannot be read keeps
the OS reason as `Error::Io`, a file that is not a scenario is keyed by its path,
and a step that broke a rule is keyed by the step.

## Alternatives considered

- **JSON, the journal's format.** It is the format durable data already uses, and
  the journal is where a scenario's *results* go. It lost because a scenario is
  written and read by a human, not appended by a run, and because TOML is what
  every other hand-edited document in this project is. The
  `dummy_scenario_path` setting holds a path, so it does not care what suffix the
  file carries; the sample values in `config.rs`'s tests read `.json` and were
  left as they are, being another task's test text.
- **Scenarios written in Rust.** The shape `Scripted` in `provider/mod.rs`'s tests
  already has, and it is right there: that adapter exists to hold the *interface*
  still. It lost on the task's own outcome — behaviour declared in a file — and on
  the hidden-scoring consequence: a scenario you cannot read in a diff is a
  scenario nobody reviews.
- **A serde enum for the outcome word.** Refuses a bad word one function earlier,
  at the cost of the step's identity in the message. Refused for that reason.
- **A default `exit_code` of 0 for every word.** One rule instead of a word and
  its implication, and it makes `failure` steps report success unless every
  author remembers to declare a status. That is the silent case the format exists
  to refuse.
- **`files` as a list of `{ path, contents }` tables.** Explicit, and extensible
  to a mode or a delete. Refused for today: contents keyed by path is what the
  field is, keyed ordering is what determinism needs, and the extra nesting buys
  nothing a scenario has asked for yet.

## Consequences

- The next task gets a scenario it can consume without deciding what an absent
  field meant: `reported_exit_code` and `delay` answer, `load` has already
  refused anything it could not replay, and `files` arrives in path order.
- The five words are frozen by the format. Renaming one breaks every scenario file
  written before it, which is what `StepOutcome::ALL`'s ledger test is for.
- Refusing the unknown is a stance with a cost: a scenario file that "works" in a
  looser reader is rejected here. That cost is the point.
- If a sixth response is ever wanted, the runner needs a rule for it first;
  `StepOutcome::ALL` makes adding the word a deliberate, visible act.
- A step can still declare a file whose *parent* does not exist. Creating it is
  the writer's decision, and this task deliberately leaves it there.
