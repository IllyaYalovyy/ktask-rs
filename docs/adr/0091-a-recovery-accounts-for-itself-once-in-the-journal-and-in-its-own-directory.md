# 0091. A recovery accounts for itself once, in the journal and in its own directory

- **Status:** accepted
- **Date:** 2026-09-23

## Context

VISION.md §7 ends with the shortest requirement in the document — "every
recovery produces a self-healing report: classification, attempted repairs,
final result" — and T095 is the task that makes it true. It also lands
`SelfHealingReport`, the catalog entry `docs/DESIGN.md` has listed from the
start and ADR-0011 kept out of `EventKind` until the task that emits it and
gives it an `apply` arm existed. Four questions had to be settled before §7's
sentence could be implemented, and a fifth turned out to be a defect in the
obvious way of writing it.

**Where does an account live?** Two readers ask in different ways. Anything that
reads a run back — a failures screen, the next remediation's bundle, a replay
rebuilding the projected state — asks the journal, and the task's done-when
names it: "retrievable through the journal". A human auditing a recovery opens a
directory. ADR-0065 already made an attempt's directory the place a person
reads, and an account that existed only as a JSON payload in SQLite would be
audited by whoever could remember the decoding.

**Why write an `apply` arm for an entry that moves nothing?** ADR-0022 made the
eight per-state matches exhaustive so that a catalog entry is a compile error in
every state until each one answers it. ADR-0086 met the same question at
`AttemptFinished` and answered it: evidence is accepted, says what happened, and
leaves the machine where it was.

**Which states may answer it?** The row names an attempt, so a state holding
none cannot attribute it. `Running` holds an attempt number too, and a report
could name it — which is the difference between this entry and
[`EventKind::AttemptFinished`], and the reason the answer is not "every state
holding an attempt".

**What enforces "exactly one report per remediation attempt"?** Two stores, and
therefore two candidates for the check: the directory that holds the artifact,
and the journal that holds the row.

**The defect the first version had.** It appended the row with
[`Journal::append`], and everything that was checked passed: the row was in the
journal with the three parts, the artifact was in the attempt's directory at
0600, the refusals refused. What no test asked is whether anybody was told.

## Decision

**One account, two stores, one call.** [`file_report`] writes both halves and
hands back the artifact's path; no caller is handed one half, because a caller
allowed one half produces an account half of which exists. The row is what
§7's "retrievable through the journal" names; the artifact is what §7's audit
actually is — a person reading three lines.

**The row comes through the recorder, not through a bare append.**
[`Recorder::record`] is the one door an event comes through (ADR-0080): it
appends, then publishes the stored envelope. The run log is that bus's own
reader (ADR-0081) and a frontend follows the same bus, so an account that was
only appended is in the database and missing from both — and the `Info` level
and the `class=… repairs=… outcome=…` line `log.rs` gives this entry are
unreachable in the only run that writes one.
`an_account_is_published_to_the_screen_watching_the_remediation` holds the
difference: a [`crate::Subscription`] taken before the filing sees exactly one
record, attributed to the remediated task. It did not compile against the
version that appended, which is this task's first red.

**The duplicate check asks the journal, and asks the journal the recorder
records through.** Whether a remediation has accounted for itself is a question
about an append-only file (ADR-0017), not about a directory that a retention
pass can sweep and a power cut can leave half-written: trusting the directory
would let the loss of one file license a second row, which is the exact thing
the done-when forbids. Asking it through a second connection to the same file
would leave two answers available to disagree, so [`Recorder::journal`] hands
out the recorder's own — shared, and therefore unable to write, since
[`Journal::append`] takes `&mut self`.

**`Remediating` answers it, under attempt equality, and no other state does.**
`Running` is refused even when the report names the attempt `Running` holds: a
first attempt that repaired nothing has no recovery to account for, so an
account of a repair is not something an ordinary attempt may file. A
`Remediating` over attempt 2 refuses an account of attempt 1 for the same reason
it refuses `PhaseEntered` about attempt 1. `LEGAL` gains the one row the sweep
can show — 68 rows against the 300 pairs (ADR-0025) — and
`a_recovery_account_moves_nothing_and_names_the_remediation_it_accounts_for`
holds the moves, the refusal equality makes, and the refusals of every state
that owns no remediation.

**The struct carries `task`; the catalog payload does not.** `docs/DESIGN.md`
gives the entry four fields because [`Journal::append`] carries the task as the
row's own column, and a payload repeating the envelope is a second place for the
two to disagree. The artifact is not so lucky: a file outlives the row it was
written beside and gets read on its own, so its first line is
`# task 77 remediation 2`.

**The artifact is `self-healing.md`, markdown, one fact to a line.** Not
`report.md`, which is an attempt's account of its *work* (ADR-0065) and sits in
the same directory — a remediation is an attempt, so the repair of attempt 1 *is*
attempt 2 and both accounts live side by side. Not JSON: `record.json` is a
machine's file, and a second machine-readable copy of three fields the journal
row already holds is a second source of truth with no reader that needs it.

**A blank outcome is refused; an empty repair list is filed.** The three parts
are three, and the outcome is the one the file is opened for, so a whitespace
outcome is [`Error::Corrupt`]. An empty `repairs` is the finding "nothing was
tried" — the breaker tripped, or a bound stopped the run before it could — and
the account that says `repairs: none` is the evidence that the recovery was
bounded rather than skipped. Both free-text fields go through `sentence`, which
is [`crate::redact::redact`] and a fold to one line, so a credential a gate
echoed cannot reach either store, and a repair described over four lines is
still one repair.

**Every refusal happens before anything is written: refusals, then the row, then
the file.** A crash between the last two leaves a recovery that is journalled
and has no artifact, which a reader can see and a repair can name. The reverse
would leave an artifact no row accounts for, and a reader who trusted the
directory would conclude a recovery had been audited when it had only been
written.

## Alternatives considered

- **Append the row, let the run publish it somewhere else.** It is what the
  first draft did, and it passed every check that existed. It lost because
  ADR-0080 exists precisely to make the two halves unskippable, and a recovery
  report that reaches a query but not the TUI is a report the primary interface
  does not have.
- **Ask the directory whether an account exists.** One fewer journal read, and
  the invariant becomes a file's memory: a swept or torn artifact licenses a
  second row for one remediation, and the reader of the journal is left to
  resolve which of two contradictory accounts is true.
- **Split into `report_event()` plus an artifact writer,** the shape
  [`policy_edit_event`] and [`trip_event`] have in this same module. It lost
  because those two return an event whose *emission* is the caller's whole act;
  here the one-per-remediation refusal needs the read and the row in one place,
  and two calls hand the done-when's invariant to the caller to remember.
- **[`Journal::open_for`] inside the filing for the duplicate read.** It is what
  [`crate::Journal`] readers elsewhere do, and it lost to the accessor: a second
  connection means a second answer to "has this remediation spoken", and one of
  the two can be behind.
- **Overwrite the first account instead of refusing.** One row either way, and
  the loss is the answer: a second, contradicting account silently replaces the
  account of what was actually tried.
- **Give `Running` a row too, since it holds an attempt number.** One fewer
  refusal, and the loss is the check that only a remediation can account for a
  repair; the sweep would then show a legal move the machine has no business
  allowing.
- **Read the class back off the failure row instead of carrying it.** The
  journal does hold a `TaskFailed`; an account that does not name the failure it
  set out to answer cannot be told apart from an account of a different failure
  that landed on the same task.
- **`Error::Policy` for the blank outcome.** The filing is not refused for
  breaking a rule about the run; it is an account that cannot be read, which is
  what `Corrupt` is for elsewhere in this crate.

## Consequences

- The step that ends a remediation has an order to keep: `apply` first, then
  [`file_report`]. `Recorder::record` does not ask the state machine whether a
  row is legal, and a row `apply` refuses does not merely fail to move — a
  journal holding a refused record is not projected at all, so filing an account
  the machine would refuse costs the task its whole projection. `file_report`
  says so in its own documentation rather than checking, because checking needs
  the projected state and this module does not own it.
- Filing an account into an attempt directory that does not yet hold a
  `record.json` makes [`crate::read_evidence`] report the whole task's evidence
  as corrupt, because a directory holding no record is that reader's spelling of
  an interrupted write. The account creates the directory. A runner that files
  the account before the remediation's own record is written therefore turns a
  successful recovery into unreadable evidence; the ordering is T096's to keep
  and the reader is not this task's to change.
- An operator watching a remediation now sees its account scroll past: the row
  reaches the log through the same door the TUI reads. Before this ADR the log
  arms for the entry existed and had no writer.
- The gap the order cannot close is a failed artifact write after a committed
  row: a journalled recovery with no file. Nothing tests it, because nothing in
  this suite fails a `write` cheaply, and it is reported rather than hidden.
- `Recorder::journal` is a door another module-local reader will eventually want.
  It is shared rather than mutable, so what it can do to the journal is nothing;
  if a future task ever needs a `&mut Journal` through a `Recorder`, that is a
  new decision about ADR-0080's one door, not an accessor to widen.
- The account's `class` is not checked against the failure the journal holds. A
  producer can file an account of the wrong failure, and the reader that cares
  compares the two rows; nothing here does, and nothing here claims to.
