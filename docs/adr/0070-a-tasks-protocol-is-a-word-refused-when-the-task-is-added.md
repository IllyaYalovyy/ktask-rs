# 0070. A task's protocol is a word, refused when the task is added

- **Status:** accepted
- **Date:** 2026-09-21

## Context

ADR-0068 gave a work protocol its shape: `direct()` and `tdd()` build typed
phase sequences in code, because §9 forbids a protocol an attempt could
assemble. ADR-0068 also recorded what that left open — "Selection per task is
T075" — and T075 is that selection. §9 wants three things of it: protocols "are
chosen per task", a project "defaults to" one, and "The current protocol and
phase are first-class state, visible in the TUI queue and inspector." The
plumbing for two of those already exists: `docs/DESIGN.md`'s `tasks` table has a
`protocol TEXT` column commented "NULL means the configured default",
`Config::default_protocol` is a documented key whose default is `direct`, and
`EventKind::AttemptStarted` already journals a `protocol` field.

What is missing is the word. A task block is markdown, so a protocol arrives as
text — and text is where a mistake hides: `spec-first` is a real §9 name that
this build cannot run, `Direct` is a near miss, and an empty `**Protocol:**`
section says nothing at all. The failure mode each of them shares is the
expensive one: a supervisor that could not honour the word would discover that
twenty minutes into an attempt it has already paid for, and the cheaper-looking
repair — run it `direct` anyway — spends an attempt on a protocol nobody asked
for. Six things here are decisions rather than steps.

## Decision

1. **An unrunnable name is refused when the task is added, and the refusal is
   the whole import.** `task::validate` asks the protocol question of every row
   and `build_task` asks it of every block, so `plan lint`, `add` and the door
   to the queue (`Journal::put_tasks`, which validates each row before it
   inserts) all give the same answer, and no row an attempt could start exists
   without a name this build runs. A word that is refused at the door costs the
   operator one line of markdown; the same word refused at phase selection costs
   the attempt it was written into, and the state that attempt left behind.
2. **The names, the constructors and the refusal are spelled once.**
   `protocol::PROTOCOLS` is a two-entry table of `(word, constructor)`, and
   `by_name`, `names`, `alternatives` and `refusal` are the four ways in. A
   refusal cannot then promise a protocol the build does not have, and adding
   §9's `spec-first` when v0.2 lands is one row: the word becomes acceptable to
   a task, to `default_protocol` and to the two sentences that offer the
   alternatives, in the same edit.
3. **Matching is exact.** `Direct`, ` TDD ` inside a word, `spec-first` and the
   empty string all name nothing. Section trimming has already removed the
   whitespace a parser owns; what is left is the operator's text, and a name
   that had to be folded or repaired to match is a name written wrong. Running
   a task under a protocol its author did not write is the mistake this
   forecloses, and it is the same refusal ADR-0058 makes of a `provider` word
   that names no adapter.
4. **The resolution order is the task's word, then the project's, then
   `direct()`.** A task that names `tdd` is worked `tdd` under a project that
   defaults to `direct`, and a task that names nothing takes the project's
   setting. `direct` is the last rung rather than another setting, because §9
   calls it the v0.1 default and a queue with nothing written anywhere still has
   to be worked. A *blank* — an unset `default_protocol`, a field with only
   whitespace in it — is the absence that lets the rung below answer, not a
   choice of `direct`; the difference matters only for a row assembled in
   memory, since a blank section is refused at the door by rule 1.
5. **An unrunnable `default_protocol` is refused, not worked as `direct`.**
   `config.rs` types a value and checks nothing about it, so
   `for_task` is where a misspelt default meets the name set — as
   `Error::Config` keyed `default_protocol`, so the message names the setting to
   fix rather than the task that tripped over it. Silently falling back would
   make a typo mean "run every task in this project `direct`", which for a
   project that opted into red/green discipline is the opposite of what was
   asked for, and would be invisible in the journal.
6. **The word is stored in the column the schema already gives it, and read
   back from there.** This is the one place a task's optional section is *not*
   re-derived from the body: `gate` has no column, so ADR-0019 re-reads it out
   of the text, while `docs/DESIGN.md` gives `protocol` a column whose `NULL`
   has a documented meaning. Overwriting that `NULL` with a re-parse would
   destroy the distinction the comment cares about — no default chosen versus a
   default that applies — so `put_tasks` writes `task.protocol` and
   `Journal::tasks` reads it. That means one line each in `journal.rs`, outside
   the two files T075 names: the alternative was a `protocol` field that a queue
   read always returned as `None`, which is a choice stored and then lost on the
   way to the display that was the outcome.

## Alternatives considered

- **Refusing an unrunnable name at selection only**, in `for_task`. It is
  simpler and it is still true — `for_task` does refuse, and keeps refusing as
  the last line of defence for a row written by something that did not ask. It
  lost because the done-when is "rejected when the task is added, not when it
  runs": by selection time the queue holds a task no build can work, and every
  display of it is a lie until an attempt spends itself discovering the fact.
- **Folding case and trimming to be forgiving.** Cost one typo to notice, and
  the repair is a word the operator did not write. ADR-0058 refuses an unknown
  provider word the same way, and a protocol is the same kind of instruction.
- **Re-deriving the protocol from the body on read,** as `gate` is. It would
  have kept this task inside its two files. It lost on the meaning of `NULL`:
  the schema comment makes the absence load-bearing, so a read that re-parses
  either invents a word for a task that named none or reports `None` for a task
  that named one — and the second is how a stored choice disappears.
- **A typed `Task::protocol`** holding an enum rather than the word as written.
  Every other `Task` field is text kept as written, because a `Task` is the
  queue's projection of a markdown block; a typed field would have to decide
  what to do with a name it cannot represent, and the answer to that question is
  the refusal this task already has.
- **Storing a `Protocol` value instead of a word.** ADR-0068 declined serde for
  `Protocol` because nothing stores it and its name is a `&'static str`. The
  queue row holds the word and `AttemptStarted` journals the word; both are
  mapped back through `by_name`, which is the only place the two constructors
  are reached by text.
- **Validating `default_protocol` in `config.rs` at load.** It is the earliest
  defensible moment, and it is the right place for a later task that gives
  configuration a value-level validation pass. It is not this task's file,
  and — more than that — a check there would not cover a `Config` built in
  memory, so `for_task` has to hold the rule anyway. One rule, held where the
  names are.

## Consequences

- An unknown protocol name is a failed import naming the word, the two words
  that would have worked, and the block or row that held it: `add` refuses
  without touching the queue, and a `put_tasks` refusal writes no rows.
- `status` can display the choice from one column read rather than a body parse
  (`docs/CONTRACT.md`'s queue line, T111 prints it; the TUI's queue and
  inspector panes that §9 asks for are T139's). Nothing renders it yet, because
  `crates/ktask-cli/src/main.rs` is still a stub with no commands.
- `for_task` has no call site until the runner starts an attempt (T088, and the
  phase step at T091). "Record the chosen protocol in `AttemptStarted`" is
  therefore held by a test over the journal record's own shape — the word
  `for_task` returns is the word the record carries, and the word in the record
  rebuilds the phases the attempt ran — rather than by a run that writes one.
  An attempt that did not record its protocol cannot be replayed, and
  `PhaseEntered` naming a phase no protocol declared is unfalsifiable without
  the protocol beside it; that is what those two assertions are worth.
- `spec-first` is refused by name today, which is what §9's v0.2 label means
  operationally. When it lands, the row is added to `PROTOCOLS`; the refusal
  sentence and the offered alternatives follow automatically, and the test that
  pins the two names is the one that fails first if they do not.
- `journal.rs` now participates in this rule (`put_tasks` writes the column,
  `tasks` reads it), so a later change to the queue's columns has to keep the
  `NULL`-means-default distinction that rule 6 depends on. The proptest over
  store-and-read covers that column, including its absence.
