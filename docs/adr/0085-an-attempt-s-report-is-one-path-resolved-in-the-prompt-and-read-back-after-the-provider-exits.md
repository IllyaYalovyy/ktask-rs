# 0085. An attempt's report has one path, resolved in the prompt and read back after the provider exits

- **Status:** accepted (supersedes ADR-0075 on the `<state_dir>` marker and on what
  `assemble` may take as an input)
- **Date:** 2026-09-22

## Context

T090 asks for a round trip: the agent's report is written where the prompt says and
read back. Four things had to be settled for that to be one fact rather than three
that can drift apart and one that was already known to be wrong.

**The prompt named a path that cannot be opened.** ADR-0075 printed
`<state_dir>/attempts/<task>/<attempt>/agent-report.md` because `assemble` carried
no project, and resolving a state directory from `$XDG_STATE_HOME` inside a pure
function would have made the prompt's bytes depend on something that is not one of
its inputs. That ADR named its own fix — "widen this signature with the attempt's
report path (or the project) and delete the marker" — and accepted the marker as the
chosen failure mode: an agent that tried to write there met the filesystem and said
so in its report. That was affordable while nobody read the report. Once a run reads
it, the marker is a run that always finds nothing, and "nothing" is the one answer
VISION.md §3's invariant 4 forbids guessing about.

**The task body names `report.md`, and that name is taken.** ADR-0065 gave
`report.md` to the record the runner generates from the gates and SHAs it watched,
and [`crate::write_evidence`] writes it into the same attempt directory. One name in
two hands means either the agent's account is overwritten by the generated record or
the generated record — the only account of an attempt that the mechanical verdict
rests on — is overwritten by an agent's prose. Invariant 4 is exactly the rule that
the two accounts of one attempt stay two files, so the path stayed the one ADR-0075
resolved, and the task body's spelling is a finding to report rather than an
instruction to obey.

**A report can be absent for two reasons the run cannot tell apart afterwards** — a
session that declined to write an account of itself, and a session that was told to
write into a directory that was never there. Only one of those is the agent's
failure, and the run can make the second impossible before starting the first.

**Reading it back has to be able to say nothing.** The tempting implementation
returns a [`crate::ReportResult`] and defaults it to `Done` when the file is absent,
which is invariant 4 inverted: the agent's silence read as a claim of completion.

## Decision

**`report::report_path(project, task, attempt)` is the one spelling of the path**:
[`crate::evidence_dir`]'s directory plus `agent-report.md`. The prompt header,
[`crate::Runner::prepare_report`] and [`crate::Runner::read_report`] each reach the
path through it, so no two of them can end up naming different files.

**`assemble` takes the `Project`, and the marker is gone.** The header prints the
resolved path, absolute, because an agent's working directory is its task's worktree
(VISION.md §10) and any shorter spelling is resolved inside the repository — §3's
invariant 6 arrived at by an agent doing exactly what it was told. The added input
is the project rather than a finished path: a caller that computed the path itself
could compute a different one, and the point of the whole task is that there is one.
`assemble` still reads nothing; a `Project` contributes its registered state
directory as text, and the determinism property over arbitrary inputs still holds.

**`Runner::prepare_report` makes the attempt's directory before the provider is
started** and hands back the report file's path. The levels it makes are the
evidence layout's own two levels, at `0700`, and it files nothing: `begin_attempt`
already journalled the attempt and filed its record, and re-writing that record to
make a directory would be a different step pretending to be this one.

**`Runner::read_report` is the read that happens after the provider exits**, and it
answers [`crate::ReportClaim`]: `Claimed` with the path, the claim and the whole
text, or `Missing` with the expected path, a class and one line of detail. **A
missing report is `FailureClass::AgentFailure` and never an assumed success** — §7's
`agent_failure` is "the agent could not complete the implementation", which is what
a session that left no account of itself actually said, and its recovery is a fresh
session asked again. It is not `policy_failure` (nothing forbidden was touched), not
`verification_failure` (no gate refused), and not `needs_input` (nothing is
undecided for a human to settle).

**What a report can do stops where ADR-0076 left it.** A `DONE` read out of text is
a claim beside the gates and loses to them; the one direction invariant 4 leaves open
to an agent's account of its own work is to refuse.

## Alternatives considered

- **`report.md`, as the task body spells it.** Rejected above: it is the generated
  record's name, and a collision between an agent's claim and the supervisor's
  evidence is decided by write order. Reported as a finding.
- **Resolve `<state_dir>` inside `assemble` from the environment.** Keeps the fixed
  signature and makes two calls over equal inputs unequal whenever
  `$XDG_STATE_HOME` moves — the determinism §6 ranks above token economy, traded for
  one fewer parameter.
- **`assemble(…, report: &Path)`.** ADR-0075's own first suggestion, and it works.
  It lost to the project because it puts the resolution in every caller, which is
  precisely where a prompt and a reader can start to disagree.
- **Make the report directory inside `begin_attempt`.** The directory is already
  there by the end of that call, so this would have been true by accident. It lost
  because the provider step needs the guarantee on its own terms, without re-filing
  an attempt record — which `write_evidence` refuses to do anyway once one says
  something different.
- **Let the missing report come back as an [`crate::Error`] and be classified.**
  ADR-0057 is the same argument already settled: the classifier reads an error of the
  run's, not a fact the run has already decided. The class arrives as data here, as
  it does in a preflight refusal.
- **Read an unreadable report as a missing one.** One test's worth of convenience,
  and it turns corrupt data into an absent file, which then gets the wrong class and
  the wrong recovery. `Error::Corrupt` naming the file stays.

## Consequences

- The path in a prompt is a path the session can open, and it is the same string the
  run will read. The round trip is asserted twice: `report::round_trip` reads what a
  path was written to, and `runner::report` asserts the prepared path is the one the
  assembled prompt names.
- An attempt directory holds two reports with similar jobs and different authors:
  `report.md`, generated by the run, and `agent-report.md`, written by the session.
  A later screen that renders an attempt should label them rather than let an
  operator guess which one is evidence.
- A retry gets its own directory, so a previous attempt's report is a file in the
  wrong place rather than a stale byte in a shared one. Two tests run two attempts to
  hold that, because it is the failure mode that reads as a success.
- A session that writes nothing is now a classified failure with the expected path
  beside it, which makes the report a *checkable* obligation. That is a new reason
  for an attempt to fail, and it will be seen most by sessions that were never told
  where to write — which is what the header is now for.
- T091 gets these two calls and supplies the middle: the session, the phases, and the
  `AttemptFinished` record an agent's summary belongs in.
- If the prompt ever names a second artifact per attempt, `report_path` is where each
  gets its spelling; the claim of one path is about the report alone. If a project is
  ever re-registered below a different state directory, the prompt recorded in an
  older attempt names a path that no longer exists, and that is correct: the filed
  prompt is the account of what was said at the time.
