# 0097. Recovery of an interrupted publication asks the remote, which is the only witness to a landed push

- **Status:** accepted (supersedes ADR-0096 on "recovery fetches nothing")
- **Date:** 2026-09-25

## Context

VISION.md §10 makes a publication two side effects and one proof: step 6 is "Push
mainline, fetch again, and require local candidate SHA to equal remote mainline SHA",
and step 7 is "Only after that comparison does the task reach `published_verified`".
T101 asks what a restart concludes when the journal's last row for a task is
[`EventKind::PublishStarted`] — the instant at which the commit exists locally and the
push may or may not have reached anywhere else.

ADR-0096 answered this question with three inputs, and none of them can answer it. The
journal says a push was begun and nothing after it. The process table says the process
that began it is gone. The checkout says which commit it holds — and this is the crux:
**the checkout's answer is identical whether the push landed or died in the middle.**
A candidate sitting in the task worktree's `HEAD` looks exactly the same in the
repository that published it and in the one that never did, because the push moves a
ref in some other repository entirely.

T100's rule for this case read `HEAD`, found the offered candidate, and answered
[`Recovery::AlreadyApplied`]. That verdict is about the *commit*, and it is right about
the commit; but a publication is not the commit, and `AlreadyApplied` on the commit
alone leaves the task in [`TaskState::Publishing`] with no route to
[`TaskState::PublishedVerified`] — which by §10 step 7 is the only state a finished
publication may hold. So the old rule either under-claimed (a task that had published
was left in the dangerous phase forever) or, if read as "the publication is applied",
claimed a push it had never read.

A fourth input exists, and it is the only one that was there for the push: the remote.
Two properties of it had to be measured rather than assumed.

**The remote-tracking ref is a cache, not an answer.** `refs/remotes/<remote>/<branch>`
is the record of the last conversation with the remote, and [`git::publish`] names it as
the thing a fetch has to move before it can be believed. Two scratch-repository cases
show it is wrong in both directions: a ref left behind by an earlier instant reports a
landed push as unmade, and a ref that holds a commit no push ever carried reports an
unmade push as landed. The second is the expensive one — it is how published work gets
published twice, or un-published work gets reported as done.

**A blind re-push is not a safe default.** `git push <remote> -- <candidate>:refs/heads/<branch>`
is not forced ([`git::publish`]), so re-offering a candidate the remote has since moved
past is refused, and the refusal's repair path is a rebase and a rerun of every gate
(T093) — work spent rewriting and re-grading a commit the remote already holds. §6
prohibits exactly this: "it never silently re-runs work that may already have taken
effect."

## Decision

**The remote is a fourth input, asked only about a publication.** When the phase being
decided is [`TaskState::Publishing`] and the journal holds an
[`EventKind::PublishStarted`] row for that attempt, recovery resolves the mainline's
name from [`config::load_for`] (`mainline_remote`, `mainline_branch`), fetches that
remote, and reads `refs/remotes/<remote>/<branch>` with `rev-parse --verify` — the same
reading [`git::publish`] performs after its own fetch, so recovery and publication
cannot disagree about what "the remote's tip" means. Equal means the push landed;
unequal means it did not. Recovery never pushes.

**Both names are resolved lazily.** A pass that holds no publication never loads the
settings and never reaches the network. A run that died in `Preflight`, in an agent's
session or in the gates is decided from ADR-0096's three inputs, and neither an
unreadable settings document nor an unreachable remote gets a say over it.

**Equal means the push landed, and the proof is journalled.** The verdict is
[`Recovery::AlreadyApplied`], and it is the only verdict in the module that carries a
second event: [`EventKind::PublishVerified`] with both `commit` and `remote_sha` set to
the candidate, appended immediately *before* the [`EventKind::RecoveryDecision`] row.
That event is the only transition §10 step 7 allows `Publishing` to take, and recovery
has just read its proof itself; recording it is what lets the next restart reach
`PublishedVerified` from the events alone, without a second fetch, which is what keeps
the pass idempotent. The order is ADR-0096's: the row that states the finding goes down
before anything that acts on it.

**Unequal means it did not land, and the publication is retried.** The verdict is
[`Recovery::Resume`], and the checkout sub-table decides the shape of that retry: a
checkout already holding the candidate resumes with the commit left alone — only the
push is repeated — a checkout still at the attempt's base resumes having committed
nothing, and a checkout holding a commit no row offered stays ADR-0096's
[`Error::Policy`] refusal.

**A remote that will not answer is a refusal, not a verdict.** [`Error::Git`] from the
fetch or from a branch the remote does not have stops the pass with nothing journalled,
leaving the task in `Publishing`. So does [`Error::Config`] when the settings that name
the remote cannot be read. A push whose fate cannot be read is not a push that failed.

**Liveness still comes first, and no candidate still means no question.** A `pid` the
machine reports live is a push still in the air: [`Recovery::Resume`], remote unasked.
A `Publishing` whose [`EventKind::PublishStarted`] row never arrived has no candidate to
compare, so the remote is not asked about it either.

**The rule ADR-0096 recorded is narrowed, not reversed.** "It never fetches" was true of
every phase T100 had to decide, and it still is. What this ADR adds is the one place
where reading the remote is the only evidence available — and the fetch is two
commands that move no ref anywhere: recovery proves a publication, it does not make one.
ADRs are append-only, so ADR-0096 stands as written and this one supersedes that clause.

## Alternatives considered

- **Decide from `HEAD` alone, as T100 did.** Rejected: the checkout is blind to the
  push, and `AlreadyApplied` on the commit strands published work in `Publishing` with
  no route to the state §10 requires.
- **Read the cached `refs/remotes/<remote>/<branch>` without fetching.** Rejected: it is
  the last conversation, not the current one, and it is wrong in both directions — the
  stale ref strands work that is already published, the optimistic one re-publishes work
  that is not.
- **Re-push and let git's own answer decide.** Rejected: §6's prohibition on re-running
  work that may have taken effect; a push is a side effect, and the rebase-and-rerun path
  a refused re-push takes spends a session and a gate run rewriting published work.
- **Answer the landed case with the decision row alone, no `PublishVerified`.** Rejected:
  `detail` is prose for a human, and only the event moves the state; a restart that has
  to re-fetch to reach the same conclusion is a restart that can reach a different one.
- **Fetch for every phase, for uniformity.** Rejected: it makes preflight, gates and
  agent sessions dependent on a settings document and a network for no evidence they
  need.
- **Add a fourth [`Recovery`] variant for "the push landed".** Rejected for ADR-0096's
  reason, which still holds: `Recovery` is durable data with exactly three variants. The
  landed push is `AlreadyApplied` plus the event that proves it.

## Consequences

- The dangerous case has both branches as tests against a scratch origin: one with the
  push applied, one without. Recovery's verdict about a landed push is checked against
  what the bare origin itself holds, not against what the fetching repository remembers.
- Two further tests hold the cached ref wrong on purpose — stale, and ahead of the truth
  by a commit nothing pushed — so the fetch cannot be dropped without a test failing.
- Recovery now touches the remote, so a publication becomes unrecoverable while the
  remote is unreachable, and the whole pass refuses. That is deliberate and matches how
  ADR-0096 treats a root that holds no repository: a question that cannot be asked is
  not a question answered no.
- `config::load_for` folds environment overrides in over the project document, so
  recovery's mainline is the same pair of names the publication itself would have used.
  An operator who published to `upstream/trunk` is reconciled against `upstream/trunk`.
- `fetch` plus `rev-parse --verify` on a qualified `refs/remotes/…` name is now spelled
  in a fourth place ([`git::publish`], [`git::rebase_onto_remote`], `runner` and here). They
  agree; the next task that touches one of them should fold them into one `git` helper.
- Published work is no longer parked when its checkout has gone. A vanished tree plus a
  remote holding the candidate resolves to `PublishedVerified`, because marking it
  interrupted would be an invitation to publish the same work again.
- `reconcile` remains unwired into `run`/`resume` and the TUI's start-up path, which is
  the task that owns those commands (ADR-0096's consequence still stands).
