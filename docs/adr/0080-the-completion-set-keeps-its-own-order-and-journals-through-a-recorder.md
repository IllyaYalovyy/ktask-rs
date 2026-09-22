# 0080. The completion set keeps its own order and journals through a recorder

- **Status:** accepted
- **Date:** 2026-09-21

## Context

VISION.md §3's first invariant is that nothing is done on an agent's say-so, and
§8 names what says so instead: a verification profile of mechanical gates —
format, lint, build, the complete local suite, a privacy scan. T085 is the task
that runs them as one set, and the whole of a task's claim to be done comes down
to what that one function decides. Four things had to be settled before it could
be written, and none of them is obvious from the plan's one-line sketch:

**The order is a fact two runs have to agree on.** `Profile` is a `Vec` in
whatever order a project's TOML wrote its gates in (ADR-0035), and a queue is
compared line by line against a journal: "stopped at the first failure" means
different gates depending on how a file happened to be edited. VISION.md §3 also
refuses to let anybody shorten the set, so a rule about which gate may be skipped
is a rule about the invariant itself, not a scheduling preference.

**The plan's signature cannot journal anything.** T085 was sketched as taking
`bus: Option<&Bus>`. `Bus` is the live-view door — a bounded ring per subscriber
that a view opens and drops (ADR-0030) — and it has no write side: `Bus::publish`
is what `Recorder::record` calls *after* the journal has accepted a row, and a
published event whose sequence its emitter chose is exactly what ADR-0016
forbids. ADR-0030's own words are that a record is the only door into a live
view. So the parameter the plan named can, at best, publish rows the journal
never held — a live view that recovery cannot replay, which is the failure mode
VISION.md §3's second property exists to prevent.

**One of the two entries the set needs was unwritable until now.**
`docs/DESIGN.md` documented `GateStarted` with a payload field called `kind`,
and `EventKind` is `#[serde(tag = "kind")]`: the tag and the payload claim the
same JSON key, so the derive refuses the variant (ADR-0011 measured it, and
`.ktask/queue/findings-38.md` recorded that the rename belongs to "the commit
that first emits it" — this one). `GateFinished` was absent for the other half
of the reason: it nests a `GateResult`, which no earlier task had written.

**The privacy scan reads a range only the run knows.** §8's scan covers the
"full outgoing commit range", and a gate is a command plus an environment a
project configured before this run existed. The base commit is the one fact
about the run the scan cannot recover afterwards — by the time it runs, the work
it is scanning is the tip.

**Who the rows belong to is not known here.** `Recorder::record` takes
`Option<TaskId>` and the journal stamps it (ADR-0016), and VISION.md:270 wants
gate progress attributable. But the plan's four parameters do not name a task,
and a row naming a task this function was never handed attributes a run to the
wrong place in the queue.

## Decision

**The set owns its order, in one named constant.** `COMPLETION_SET` is
format → lint → build → verify → privacy, and `run_completion_set` iterates it
rather than the profile's `Vec`. Cheap checks first is the order that wastes the
least of a broken tree's time, and it is written once so two runs of one task
produce the same sequence of journal rows.

**A refusal stops the set, and `Verify` is the one gate a refusal cannot skip.**
Every gate carries a timeout budget (ADR-0035), so a gate that already refused
makes the next one's cost pure loss — except the mandatory suite, which runs
whatever refused before it, because a task cannot be called done over a suite
that was never read. `Verify` is also the last gate the set stops at: once the
suite itself has refused, the answer is in hand and the privacy scan behind it
has nothing left to clear. §7's rerun after a remediation is the same call, from
scratch, not a partial one.

**A gate nobody configured is neither run nor reported.** `Profile::validate`
already refuses a profile with no `Verify` gate before a single command spawns,
so the mandatory gate's presence is a configuration error rather than a
runtime surprise; the other four kinds are optional, and inventing a default
command for one would prove a thing nobody asked to prove.

**A refusal is a returned `GateResult`, not an error.** The set returns
`Ok(Vec<GateResult>)` containing every gate it started, failures included,
because `crate::FailureClass` is chosen from the status and exit code inside
those records. `Err` is reserved for the two cases where there is no verdict to
read: `Error::Config` for an unrunnable profile, and `Error::Gate` for a gate
that could not be spawned at all.

**The set journals through a `Recorder`, and the plan's `Bus` is recorded as
impossible.** The signature is
`run_completion_set(profile: &Profile, root: &Path, base_sha: &str, recorder:
Option<&mut Recorder>) -> Result<Vec<GateResult>>`. `Recorder::record` appends
and then publishes, so a caller holding a `Bus` reaches a live view by holding
the `Recorder` that wraps it — nothing is lost against the sketch, and the
journal gets its row first. `None` runs the set and writes nothing, which is
what a caller checking a tree outside any task is asking for. T093's rerun and
T087's preflight pass `&mut self.recorder`.

**Each gate gets a start before it spawns and a finish after it.**
`GateStarted { gate }` is recorded before the command is spawned, and
`GateFinished { result }` — carrying the whole `GateResult`, per ADR-0036's
nested payload — after it returns. That pairing is the recovery affordance: a
journal left by a run that died mid-set shows a start with no finish, which
names the gate to rerun and distinguishes it from one that ran and refused. An
`Error::Gate` propagates from between the two rows on purpose, leaving exactly
that shape behind.

**`GateStarted`'s field is `gate`, and the document was corrected rather than
the collision worked around.** `docs/DESIGN.md`'s table now reads
`gate: GateKind`; `GateFinished` keeps its documented `result: GateResult`
unchanged. Renaming the field is the only shape the derive accepts, and leaving
the document describing a variant that cannot compile is how the next reader
re-litigates it.

**`KTASK_BASE_SHA` goes to the privacy gate alone, and the run's value wins.**
The variable joins the `KTASK_*` family `crate::Config` documents, is laid over
a *clone* of that gate's `Gate::env` (a `Profile` is what a project configured,
which a run reads and does not edit), and overwrites any configured value. A
project's settings must not be able to point the scan at an empty range, which
is how a scan is made to pass. The other four gates get no such variable, so no
other command can act on a range it was never asked to read.

**The rows carry no task, and that is recorded as unfinished.** Every row this
function writes has `task_id: None`. The alternative was a fifth parameter the
plan does not name; a row that named a task this function was never handed is a
lie about attribution, and VISION.md:270's attribution requirement is a caller's
job to satisfy. Whoever threads this into a task run either widens the signature
or wraps it — recorded as a finding by T085 rather than guessed at now.

**The ten new transition rows are state-preserving.** `GateStarted` and
`GateFinished` are accepted in `Preflight` (T087's baseline gate), `Running` and
`Remediating` (§9's phase gates and §7's reruns), `Verifying` (the completion set
itself) and `Publishing` (T093's rebase rerun, which is the same fact that
already gives `VerifyPassed` a `Publishing` row); they are refused in `Queued`,
`PublishedVerified` and `Paused`, where nothing is running to have started a
command. Like `TddExceptionUsed`, neither entry names an attempt, so no state can
check an equality it does not hold. They join the declared table: 66 rows against
the 288 pairs of the sweep (ADR-0025), eight exhaustive matches (ADR-0022).

## Alternatives considered

- **`Option<&Bus>`, as the plan sketched.** It cannot write the journal, and the
  journal is where "done" has to be readable from. ADR-0030's ring is bounded and
  a view opened mid-set misses the rows above it forever. It lost because the
  parameter named in the plan cannot carry what the task's own done-when line
  demands.
- **A `dyn` sink trait over "something that records a gate row."** Cheaper to
  mock, but it would let a caller record a row without appending one, which is
  the split ADR-0016 exists to prevent. A `&mut Recorder` cannot be implemented
  outside the crate, and that is the property the invariant needs.
- **Strict short-circuit — stop at the first failure with no exception.** The
  plan says "stopping at the first failure" and its done-when says "the mandatory
  verify gate always runs" in the same breath; only one of the two can be a rule.
  The invariant in §3 is the reason the second one wins: skipping the suite is
  how an agent's say-so becomes the verdict.
- **Run all five regardless and report every verdict.** More rows to read and no
  decision to make, at the cost of every remaining timeout on a tree already
  known bad. §8's gates are minutes apiece in a real project.
- **`Vec<GateResult>` plus a `bool` for the first refusal, or a `GateOutcome`
  enum.** The caller already derives the class from the records; a second summary
  is a second source of truth about the same run.
- **`GateStarted { kind }` with a serde rename.** A `#[serde(rename)]` would put
  a field in the JSON that the tag owns, silently: the derive's own output
  decides which one survives encoding. Renaming the Rust field costs one document
  row and is honest in both languages.
- **Skip the privacy gate's base sha and let the project configure it.** Then
  `base_sha` is a parameter nothing reads, and the configured value — possibly
  stale, possibly the empty range — decides what got scanned. That is a gate that
  can be made green by editing configuration, which no other gate can.
- **Add a `TaskId` parameter and stamp the rows.** Right for the caller, wrong
  for now: no caller exists yet, and the shape the threading task (T087) needs
  depends on whether the runner owns the id or the recorder does. Inventing it
  here means a task that knows better edits a signature twice.

## Consequences

- `run_completion_set` is exported from `crate::lib` alongside `run_gate` — one
  line outside the task's file list, and required by `unreachable_pub`: an item
  no caller can reach is a lint, not an oversight.
- Gate *output* is still not published. `run_gate`'s `bus` parameter is accepted
  and dropped (ADR-0037), and this function passes `None`, so a live view shows
  that a gate started and finished and cannot show its progress. `.ktask/queue/
  findings-41.md` recommended a batched `GateOutput` entry land here; it needs an
  `apply` arm in all eight per-state helpers (ADR-0022) and a batching rule, and
  `a_listening_bus_changes_nothing_about_what_a_gate_reports` pins the current
  reading, so changing it fails a named test rather than drifting.
- Rows written here carry `task_id: None`. A screen that groups gate progress by
  task cannot group this set's rows until the signature grows; the finding is
  recorded here rather than guessed at by the task that found it.
- Callers that want a rerun after a remediation call this function again from
  scratch (VISION.md §7). There is no "rerun only what failed", and a partial
  rerun would be a new decision about which gates are allowed to age.
- The `Verify` exemption means one journal ordering is legal that a reader might
  not expect: a `GateFinished` for `Format` refusing, followed by a
  `GateStarted` for `Verify`. Recovery replays what is written, so the shape is
  safe; a reader who assumed "a refusal ends the rows" is the one to update.
- `KTASK_BASE_SHA` is now part of what a privacy gate can rely on. A project's
  scan command that ignores it scans something undefined — the contract is in
  this ADR and in `Config`'s variable list, not in a type.
- `Profile::validate` runs first, so a misconfigured profile costs no gate
  timeouts. It also means a profile with no `Verify` gate cannot be run *at all*
  by this function, including for a caller that only wanted a format check.
