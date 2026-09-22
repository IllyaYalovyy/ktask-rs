# 0083. A run is built from a project alone, and its attempt opens with a base and a record

- **Status:** accepted
- **Date:** 2026-09-22

## Context

T088 hands over one sentence of shape — a `Runner` holding `project`, `config`,
`profile`, `recorder` and `provider`, a constructor that "requires nothing but a
registered project", and a `begin_attempt` that journals `AttemptStarted` with
"the protocol from `protocol::for_task`, the current pid and the base SHA" and
"persists the record with `attempt::write_evidence` so an attempt's evidence
exists from the moment it starts, not only when it ends". Five sentences written
by five earlier tasks now sit behind it, and each answers one of those words in a
way the next caller can contradict:

- `config::load_for` resolves three layers; `gate::profile_from` refuses a
  project with no `verify_command`; `provider::build` refuses a word it has no
  adapter for and, for `dummy`, a scenario it was never given. All four of those
  are refusals, so a constructor has to decide what a refusal *leaves behind*.
- `Journal::append` is the only writer of a sequence number, and
  `Recorder` is the only caller of it that publishes (ADR-0016). The recorder
  owns its journal; nothing above it can read the rows it has appended.
- `AttemptRecord::base_sha` documents itself as "the commit this attempt started
  from — the one `PreflightPassed` recorded", and T087's `preflight` hands a base
  up in its report rather than journaling the verdict itself (ADR-0082).
- `write_evidence` files `record.json` last and refuses a *second, different*
  record for an attempt that already has one (ADR-0065), because a remediation
  reads a retry's evidence out of the attempt it replaced.
- `EventKind::AttemptStarted` carries a pid "so recovery can tell a dead run from
  a live one instead of asking it" — which makes the pid a fact about the process,
  not about the task.

Four of those sentences collide somewhere. A constructor with five parts to build
has to order them; an attempt has to name a base it did not measure; and evidence
that is written at the start is evidence that cannot be written again.

## Decision

**`Runner::new` takes a `Project` and nothing else, and resolves it in the order
the refusals get cheaper: configuration, profile, adapter, journal.** The three
that can refuse on somebody's *settings* come first, so a project configured with
no complete local suite, or with an adapter this build does not have, is refused
before a database file is created in its state directory. A run that never began
has no transitions to record, and an empty `journal.db` is the artifact that says
somebody else started work here. `Journal::open_for` is last because it is the one
step whose refusal is not about the configuration at all: a project with no state
directory is a project that was never registered, and `journal` keeps that rule
(ADR-0082 states the same one for preflight).

The rings the recorder publishes into are sized from the resolved configuration's
`output_ring_lines` through `Recorder::with_bus`, so a run's screens are sized by
its own settings rather than by the compiled-in default the recorder would
otherwise pick.

**A run holds no view of itself; it hands one out.** `Runner::subscribe` delegates
to the recorder's bus, which is where the plan's `Bus::subscribe` actually gets
called. A sixth field holding a `Subscription` is the obvious alternative and it
is wrong twice over: dropping the value a `#[must_use]` call returned is a warning
in itself, and a subscriber that never reads is a ring that overflows and counts a
loss for every event of the run that owns it. Frontends come and go; the bus
already models that (VISION.md §5).

**`begin_attempt` is `pub`, not the plan's bare `fn`.** Nothing in this crate
calls it yet — the queue walk that would is T089 — and an unread private method is
a `dead_code` failure under `-D warnings`. The rule here is that a gate is never
widened with `#[allow]`, so the choice is between `pub` and inventing a caller to
keep the visibility private. Inventing a caller is inventing behaviour; `pub` on
the method the plan named is not.

**An attempt's base is the base the journal says preflight recorded for that
task, and a task with none is refused.** The last
`EventKind::PreflightPassed` row belonging to the task answers it — "the last"
because a task that failed preflight, was repaired and passed is based on the
second answer, not the first. Nothing else is read: not `git::head_sha`, not the
report preflight returned to whoever called it eight minutes ago. An attempt based
on a tree no check proved green is precisely what the `preflight` state exists to
make impossible, and `base_sha`'s own documentation names the row it means. The
refusal is `Error::NotFound` naming the task, before anything is appended, so a
task that cannot be worked costs no journal.

**The attempt number comes from the journaled rows, not from memory**: one plus
the highest `AttemptStarted` number the task already has. A supervisor that
started again mid-queue then continues a task's numbering rather than filing its
retry as attempt 1 beside the record it is replacing.

**The row is appended before the evidence is filed**, and both happen inside
`begin_attempt`. The reader who arrives after a crash between the two finds a
journaled attempt with no evidence directory, which is the shape recovery already
reads as "this attempt never completed"; never an evidence directory for an
attempt the journal has never heard of.

**The record filed at the start says what the attempt *is*, and nothing about
what it did**: its task, its base, the model its configuration asked for, and
`"started as pid N"` as its exit reason — the one true sentence available to an
attempt that has not stopped, carrying the same pid the row carries so the two
cross-check. `ended`, `session_id`, `model_reported`, `usage`, `gates` and
`candidate_sha` are all absent, and stay absent until there is an answer to give.
The context document beside it is written empty: assembling it is the next task's
step, and the artifact belongs to the directory that says "this attempt existed".

## Alternatives considered

- **`base_sha` from `git::head_sha`.** One line, and it is the line that loses the
  task's point. The checkout's tip is a fact about the worktree now; the base is a
  fact about what was proved before an agent was let near it, and after a
  remediation's commit the two differ. Recording the first as the second makes the
  journal self-contradicting — `PreflightPassed` says one SHA, `AttemptStarted`
  another — and ADR-0016's whole apparatus exists to keep that from being
  readable.
- **Falling back to `head_sha` when no `PreflightPassed` exists.** A fallback is a
  silent weakening of the state before it: the queue would keep moving, and the
  evidence would look complete. `NotFound` makes an operator decide instead.
- **An `AttemptId` counter held in the `Runner`.** It answers the question the
  field would answer only until the process is restarted, which is the one moment
  the number matters (VISION.md §6's crash recovery). The journal is already the
  source of truth for what a task has done (ADR-0024).
- **Filing evidence at the end only, where `write_evidence` was designed to be
  called.** The plan is explicit that an attempt's evidence exists from the moment
  it starts, and it is right: an attempt killed mid-session is the case recovery is
  built for, and it currently has nothing to read. The cost is real and is
  recorded under *Consequences* rather than argued away.
- **A `Journal` field beside the `Recorder`, to read rows back.** Two owners of one
  connection is how a future change appends through the wrong one and loses a
  publish. A second connection opened for the read (the way a repair process and
  T087's own tests read a run that is still open) keeps exactly one writer.
- **`#[derive(Debug)]` on `Runner`.** Impossible: `Box<dyn Provider>` has no
  `Debug` to forward to, and adding one to the adapter contract would change the
  trait every provider implements for a diagnostic. The hand-written
  implementation prints project, configured word, built adapter and gate words,
  and says `..` where it withholds the whole `Config` and a live database
  connection — `finish_non_exhaustive`, which is the intended spelling of "there
  is more here", not a suppression.

## Consequences

- A project must be registered *and* configured before a `Runner` opens: with
  nothing written anywhere, the default `provider = "dummy"` has no scenario and
  `profile_from` has no `verify_command`, so `Runner::new` refuses. That is the
  correct order of operations — registration precedes configuration precedes
  running — and callers must stop reading a bare project as runnable.
- `begin_attempt` is `pub` before a caller exists. When T089 wires the queue walk
  it should stay `pub` (the TUI starts attempts too), and nothing else needs to
  change.
- **An attempt that filed its record at the start cannot file a different one
  later.** `write_evidence` refuses it (`Error::Policy`, ADR-0065), so the task
  that ends an attempt cannot call it again with `ended`, `gates`, `usage` and
  `candidate_sha` filled in. That task needs a closing write that reads the filed
  record and *fills* the absent halves — and `write_evidence`'s "one attempt
  writes one record" rule has to be revised to mean "never contradicts", not
  "never completes". This is a known, deliberate debt of this task, stated here so
  the later task meets a decision rather than a surprise.
- Reading a task's rows needs a second journal connection per attempt. With WAL
  mode and one writer, that costs an open and a read; if it ever shows up in a
  profile, the right fix is a read-only accessor on `Recorder`, not a second
  field on `Runner`.
- The base is the *last* `PreflightPassed` the task has, and this function does
  not ask whether a `PreflightFailed` came after it. That is a lifecycle question
  — a task in `running` has passed preflight, and the state machine is what knows
  — so it belongs to the transition table, not to a function that reads two kinds
  of row.
- `Runner`'s five fields are exactly the plan's five. `profile` and `provider` are
  read by `Debug` and by nothing else yet; their consumers are the gate and
  session tasks, and neither has a placeholder accessor standing in for that work.
