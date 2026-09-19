# 0043. A task worktree is a named sibling checkout built from one resolved SHA

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T047 adds the three calls VISION.md §10's steps 2 and 7 rest on:
`create_worktree(root, name, base_sha) -> PathBuf`, `remove_worktree` and
`list_worktrees`. ADR-0040 fixes the transport under them and ADR-0041 the
read-only queries, so the argv, the trim and `Error::Git` are settled. Six things
were not, and each one is a decision rather than a line of code.

**Nothing in the documents says where a task worktree goes.** VISION.md §10 says
"isolated worktrees … the user's normal checkout is never touched" and §11 keeps
prompts, context, logs, reports and task state out of the repository — a
worktree is in neither list. `docs/CONTRACT.md` adds nothing, and
`docs/DESIGN.md` says only that the supervisor, not the agent, creates it. The
signature the task fixes hands a *name* and returns a *path*, so the mapping from
one to the other belongs to this module and to nowhere else.

**Where it goes is measurable, and one option is quietly fatal.** Measured while
writing this: `git worktree add --detach ./inside <sha>` succeeds inside a
repository, and `git status --porcelain --branch` then answers `?? inside/`.
An untracked directory is dirty (ADR-0041 keeps untracked dirty on purpose), and
VISION.md §10 makes a dirty tree at verification time a `policy_failure` — so a
worktree kept inside the repository fails the task that made it and every task
after it. Any location outside the repository answers nothing at all.

**Reuse is the point of a name, and reuse collides with "create from the given
SHA".** VISION.md §7 requires the worktree to survive into remediation, so a
second `create_worktree` for the same task must hand back the same checkout. But
measured: `git worktree add` refuses a path that exists (`fatal: '…' already
exists`), so git cannot be asked to be idempotent — reuse is this module's
decision. And the second call carries a SHA too, which raises the question the
prompt's "create from the given SHA, never from the current checkout" leaves
open: does a reuse have to be *at* that SHA?

**Detached is the state a SHA gives you, and it has to be visible.** A worktree
that shares a branch with the main checkout cannot be created from a fetched SHA
at all (git refuses a branch checked out elsewhere), and ADR-0041 already made a
detached `HEAD` a `None` rather than an error.

**Removal can destroy work, and it can also fail for reasons only git knows.**
Measured: `git worktree remove` refuses a checkout holding modified or untracked
files (`contains modified or untracked files, use --force to delete it`) and a
locked one (`cannot remove a locked working tree`, which survives even `--force`
and needs `-f -f`); it refuses the main checkout (`is a main working tree`); and
a registration whose directory has already been deleted is removed by the plain
command, exit 0, which is what an interrupted run needs. `git worktree list
--porcelain` still *lists* that last entry, with `prunable gitdir file points to
non-existent location` — the leftover is detectable before anything is created,
rather than met as an `already exists` halfway through.

**The list is also where the operator's own checkout appears,** first, with
`branch refs/heads/…`.

## Decision

Three thin calls over the one `git` helper, plus four rules.

- **The managed directory is the repository's own top level with
  `.ktask-worktrees` appended** — `…/proj` keeps task checkouts in
  `…/proj.ktask-worktrees/<name>`: outside the tree `is_clean` reads, named
  after the repository so two repositories under one parent never share a
  directory, and derived from `git rev-parse --show-toplevel` rather than from
  the path the caller happened to pass. A name is validated as exactly one
  ordinary path component before anything is created; `""`, `.`, `..`, `a/b`,
  `/etc` and `../outside` are refused as `Error::Policy` naming the directory
  they would have written.
- **A worktree is created from a resolved commit, detached.** The SHA is
  resolved first (`rev-parse --verify --end-of-options <arg>^{commit}`) and the
  40-hex answer is what `worktree add --detach` is handed, so a caller's text
  reaches git only as an operand of a resolution call, and the creation command
  holds a value git itself produced. `--detach` because a checked-out SHA has no
  branch, which `current_branch` already reports as `None`.
- **Reuse is granted by ancestry, not by equality.** A name already registered
  comes back unchanged — no checkout, no reset, no clean — but only when its own
  history contains the requested SHA, decided with `git merge-base <sha> <head>`
  compared against `<sha>`. That admits a worktree whose attempt has committed on
  top of the base, and refuses one that stops short of it, which is the case where
  quietly handing the checkout back would verify a commit that does not contain
  what the run thinks it started from. Resolving first means the same commit
  spelled two ways is judged one commit.
- **Removal never forces, and reports git's reason for every refusal.**
  `remove_worktree` is `git worktree remove` and nothing else. A leftover whose
  directory is gone is reclaimed; a clean one is deleted with its directory; a
  dirty one, a locked one and the main checkout are refused with git's own words
  carried back.
- **`list_worktrees` returns `git worktree list --porcelain`'s records**,
  main checkout included: path, head, the full `branch` ref or `None`, plus
  `prunable` and `locked`, each holding git's reason. `detached` (the absence of
  a `branch` line) and `bare` are not modelled, and an unrecognised line is
  skipped because none of them could change what the five fields hold.

## Alternatives considered

- **`$XDG_STATE_HOME/ktask-rs/<project-id>/worktrees/<name>`** — privacy-
  consistent and the place a reader would expect, but `state_root()` reads the
  process environment and `project_id` needs a registered project: `docs/DESIGN.md`
  forbids `set_var`, so no test could point it at a scratch directory, and the git
  layer would grow a dependency on the project layer. The runner that already owns
  both can pass a different `root`… which it cannot, since the location comes from
  the repository. Rejected as untestable at this signature, not as wrong-headed:
  if worktrees must live under the state directory, the parameter to add is a
  location, and that is a task for the human to call.
- **Under the repository's `.git` directory** — never dirty, never tracked, and
  per-repository, but it puts a full second checkout where `git gc`, backups and
  `privacy audit` walk, and `.git` is a *file* inside a linked worktree, so the
  answer would differ depending on where the question was asked.
- **Inside the working tree (`.ktask-worktrees/` beside `.ktask/`)** — measured
  above: the repository answers `?? …/` afterwards, which VISION.md §10 turns into
  a policy failure. Rejected on measurement.
- **A path argument instead of a name** — the most flexible and the least
  surprising to a caller, but the task fixes the signature, and a name is what
  makes reuse and reclaim possible: the same task has to find the same directory
  without remembering where it put it.
- **`git worktree add --force` over an existing path** — one line, and it throws
  away the previous attempt's checkout, which VISION.md §7 names as evidence.
- **Reuse conditioned on `head == base_sha`** — simple and exact, and it refuses
  every remediation, because the first attempt's commit moved `HEAD` off the base.
  Rejected: it makes the preservation VISION.md §7 requires impossible to express.
- **`remove_worktree` with `--force`, or a `force: bool`** — reclaims a dirty
  leftover, and silently decides that uncommitted work is disposable. VISION.md §3
  has no relaxation knobs and §7 treats the worktree as evidence; a bool at this
  layer arrives at the caller with nothing to justify it. Left undone on purpose,
  and named in the consequences.
- **Pruning stale registrations inside `create_worktree`** — the leftover holds
  nothing, so pruning it would lose nothing, and the call would be self-healing.
  Rejected for the same reason the reuse rule is: a run that discovers a leftover
  should record that it found one, so the reclaim is a step with a journal line
  rather than something that happened in a library call.
- **`git symbolic-ref` / parsing `git worktree list` without `--porcelain`** —
  the human format's columns are not a stable interface; `--porcelain` is the
  documented machine-readable one and is already line-oriented for this parser.
- **Returning only the task worktrees from `list_worktrees`** — convenient, and it
  would hide the entry an operator needs when asking "what is holding this
  repository", contrary to ADR-0041's rule that a query answers what it was asked.

## Consequences

- A run's checkout lives beside the repository it came from, which is where a
  human looks and where `git worktree list` points. It is also *unmanaged* disk:
  removal is the only thing that reclaims it, so a project that runs a hundred
  tasks accumulates worktrees until the runner cleans them. Retention is a runner
  decision, deliberately not made here.
- Because the location is derived from `root`'s top level, a project must be
  asked through the same path every run, and `root` must be the supervised
  repository rather than a task worktree: asked from inside a worktree,
  `--show-toplevel` names *that* worktree, and its checkouts would nest. Passing
  the registered project root is what keeps one name one directory.
- A name is one path component, so the naming scheme a runner uses (`task-7`, an
  attempt id, a slug) has to fit that. It also means the derived path is always
  absolute: an argument beginning with `-` cannot be produced by it, and
  `--end-of-options` covers the one argument that is the caller's own text.
- A leftover can be reclaimed exactly when reclaiming destroys nothing — its
  directory already gone, or still there and clean. A leftover holding
  uncommitted work survives every call this module makes, by design: discarding it
  needs a decision with a name and a time attached, and that belongs to the
  interface that asks a human (and to the journal line that records the answer).
  Until something provides it, `remove_worktree`'s refusal is the last word, and
  the disk it occupies is a finding for whoever notices.
- `list_worktrees` hands a caller everything git knows, so a UI can show the
  reason a reclaim will fail (`locked`, `prunable`) before it tries. The five
  modelled fields are git's text: `head` is forty hex, except for an unborn
  checkout, which git prints as forty zeroes and this passes through.
- Two calls now sit between a run and the repository beyond the queries
  (`merge-base`, and the resolution `rev-parse`), each cheap and local, each
  traceable to the rule it serves — no cache, consistent with ADR-0041.
