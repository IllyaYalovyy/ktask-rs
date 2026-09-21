# 0065. Attempt evidence gets a file home beside the journal

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §6 makes every attempt a thing in its own right, and ADR-0063 gave it a
row: `AttemptRecorded` carries one `AttemptRecord` and `attempt_records` reads a
task's history back out of the journal. T070 fixes the other half — where the
evidence *is* on disk — with a signature that is already written against
elsewhere: `write_evidence(project, record, context) -> Result<()>` and
`read_evidence(project, task) -> Result<Vec<AttemptRecord>>`, under a layout of
`<state_dir>/attempts/<task>/<attempt>/` holding `report.md`, `context.md`,
`gates/<kind>.log` and `record.json`, with directories at mode 0700 and redaction
on the way in. Four forces shape what the task body leaves open.

- **Journal truth fights human-readable.** A rebuild of materialized state drops
  the projection and recomputes it from the events alone (ADR-0024), so a
  projection is not where a report belongs — but neither is a row of JSON: a
  failures screen, a remediation and an operator with a shell all want a gate
  transcript as a file they can open.
- **Interruption is the expected case, not the exception.** VISION.md §6 promises
  that an interruption resolves to a known state, and filing a directory is many
  syscalls, not one. A supervisor can die between the report and the record.
- **A retry must not overwrite** (VISION.md §7 assembles a remediation's bundle
  from the attempt it replaces), so the layout has to make the second attempt's
  home a different path, and this layer has to refuse a second *different* answer
  for one attempt rather than be the one place that lets it through.
- **Privacy is by construction** (VISION.md §11): restrictive permissions, and
  redaction of tokens and configured secret patterns from anything persisted. The
  signature T070 fixes carries no project configuration.

## Decision

**The journal stays the source of truth; the directory is the receipt.**
`write_evidence` files a run's evidence and `read_evidence` reads that directory,
not the database. Both come from one `AttemptRecord`, which is why they can be
checked against each other: the journal holds what was recorded, the directory
holds what the run left behind, and a repair that recomputes the projection
touches neither.

**Two ids name two directory levels, not two filename suffixes.** The layout is
`attempts/<task>/<attempt>/`, each of the last two levels the bare number `TaskId`
and `AttemptId` print as, so a retry adds a directory beside the first attempt's
instead of replacing a file. `evidence_dir` is public because the layout is the
deliverable: a retention sweep, a failures screen and an operator's shell all need
to name the same path without spelling it out a second time.

**`record.json` is written last, and its presence is the completeness claim.**
Context, then the gate logs, then the report, then the record. A directory holding
the record holds the rest; a directory without it is a write that stopped
halfway. `read_evidence` refuses that as `Error::Corrupt`, naming the path a
repair starts from, and `write_evidence` answers it by clearing the directory and
writing it whole again rather than merging into a half-file, so no artifact of an
older shape survives beside the new one. Last-write-as-marker was chosen over a
separate `DONE` sentinel because a sentinel is a second thing that can be
interrupted.

**One attempt files one record.** Re-writing the identical record is a repair: the
same bytes go back and any mode somebody opened up is set again. Writing a
*different* record where one already reads back is refused with `Error::Policy`
before anything is created. The journal refuses the same collapse from the other
side, with the update and delete triggers of ADR-0017.

**Every level this module creates is made owner-only, and the mode is set rather
than only asked for at creation.** A `create_dir` mode is masked by the umask and
means nothing for a directory that already exists, so only an explicit
`set_permissions` takes a grant back — the same reason `project.rs` re-sets a state
directory. Evidence is the least shareable thing a run produces, so files are
opened at 0600 and re-set after the write. A level that is there and is not a
directory — a symlink included — is refused rather than written through, because a
link would put a run's evidence wherever it points. A state directory that is not
there is `Error::NotFound`: evidence has no home before a project is registered.

**Gate output is one file per gate kind, and one file may hold several runs.** A
`tdd` protocol can run its targeted gate red and then green inside one attempt,
and both runs are evidence of that attempt (VISION.md §9), so both go in the one
`gates/<kind>.log` as sections, in the order they ran. A `gates/` directory is made
even for an attempt that ran no gate, because an empty one is the evidence that
none ran.

**Everything is redacted on the way in, from the built-in table only.** The
record, the context, the report and each gate log pass through `redact` and
`redact_json` with `&[]`, because the fixed signature carries no
`secret_patterns` — the gap `remediate::scrub` has, and the gap ADR-0064 records
for the bundle. A project whose keys have a shape of its own must not conclude
they are redacted here.

**A broken layout is refused, not repaired on read.** An entry that is not a
numbered attempt directory, a record that cannot be parsed, and a record naming an
attempt or a task other than the one that filed it all come back as
`Error::Corrupt` — the refusal `attempt_records` makes of a journal row, on the
same grounds: a reader cannot be handed evidence and told which half to
disbelieve.

## Alternatives considered

- **Evidence in a database table.** Rejected outright: ADR-0024 makes anything the
  projection layer cannot recompute a second source of truth, and evidence a
  rebuild can lose is the failure this design exists to prevent. A gate transcript
  in a row also forces every screen to decode JSON before it can show a log line.
- **Flat `<task>-<attempt>.json` files, or the attempt as a filename suffix.**
  Fewer directories, and it turns the done-when — a retry *adds*, never replaces —
  into a rename protocol instead of a property of the path.
- **A `DONE` sentinel, or writing `record.json` first.** Both leave a state that
  looks finished and is not.
- **Merging into a directory an interrupted write left.** Saves one recursive
  delete and can leave a `report.md` describing the older of two records sitting
  beside the new one.
- **Widening the signature to carry `secret_patterns`.** Every task already
  written against `write_evidence` would have to absorb the change, and T070 fixes
  the shape. The gap is reported instead of silently closed.
- **`create_dir_all` with default modes.** It would create a state directory a
  registration owns, and a mode only requested at creation does nothing to a
  directory somebody else left at 0755.

## Consequences

- Two readers of one attempt exist now — `attempt_records` and `read_evidence` —
  and a test holds them to the same record, so a change to the record shape has to
  be made in both places deliberately.
- A torn directory is visible to its writer and to its reader, and both name the
  path it was found at, so recovery needs no journal sequence to interpret it.
- Evidence outlives a rebuild by construction, and a retention sweep has a named
  root to walk: `attempts/<task>/`, one directory per attempt, nothing else
  underneath (VISION.md §11's configurable retention).
- Redaction here is one table, so configured patterns must be honoured where a
  record's inputs are produced; if a project's secrets have a shape of their own,
  closing this signature gap becomes a task of its own.
- Nothing calls `write_evidence` yet: wiring it into the attempt lifecycle belongs
  to the runner tasks, and the verdict-word and gate-summary wording duplicated in
  `remediate.rs` is adjacent work deliberately left alone.
