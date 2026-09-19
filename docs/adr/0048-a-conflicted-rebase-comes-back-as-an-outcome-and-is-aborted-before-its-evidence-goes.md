# 0048. A conflicted rebase comes back as an outcome, aborted before its evidence goes

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T052 adds `rebase_onto_remote(worktree, remote, branch) -> Result<RebaseOutcome>`,
the mechanical half of VISION.md §7's `git_conflict` recovery: a remote has
refused a candidate that does not build on what it holds now (ADR-0046), and the
drift is repaired before a session is spent on it. §7's rule that "self-healing
repairs project-controlled defects only" sets the boundary — replay commits,
never choose content — and T093 is the caller that branches on the answer: when
the rebase applies it reruns the completion set and retries once, and when it
conflicts it "stops for a human rather than calling an agent".

Four things here were decisions rather than code.

**A conflict is not a failure of the call, and the caller has to tell the two
successes from the failure without a text search.** `Applied` sends a run back
through every gate and then to the remote again; `Conflict` stops the run for a
person. Both are answers about the branch, obtained by asking git a question it
answers reliably. `docs/DESIGN.md` closes `Error` at eleven variants ("Later
tasks require these fields by name"), so a twelfth was not available to take;
and it would have been the wrong shape anyway — `Err` in this module means git
refused to answer, which is a third thing.

**Measured (git 2.55.0): the rebase refusal that means "conflict" is
indistinguishable from the ones that mean "cannot start" by exit status alone.**
Both exit 1 (`error: could not apply …` and `error: cannot rebase: You have
unstaged changes.`), and a ref that never arrived exits 128 (`fatal: invalid
upstream …`). What separates them is what the failed command left behind: a
conflict leaves unmerged index entries *and* its bookkeeping directory
(`.git/rebase-merge`), and a refusal that never began leaves neither. That is the
predicate this function uses, and it is git's own state rather than a reading of
git's prose.

**Measured: `rebase.autoStash` turns an unfinished worktree into a success.** In
a repository with `rebase.autoStash=true` — a setting a developer enables once
and then stops thinking about — a plain `git rebase` over a tree holding an
unstaged edit printed `Created autostash`, replayed the branch, printed `Applied
autostash`, and exited **zero**. A supervisor that repaired a branch that way
would have moved a task's uncommitted work into a stash entry — a ref no gate
reads, no journal names, and no later attempt looks in — and reported a clean
repair. `git rebase --no-autostash` refuses instead (`cannot rebase: You have
unstaged changes`), leaves the edit in the tree, and creates no bookkeeping
directory. VISION.md §10 already refuses a dirty tree at verification time; this
is the same rule where nobody was looking.

**The conflict's evidence is destroyed by the repair of the conflict.** The
unmerged paths are read with `git diff --name-only --diff-filter=U` while git is
stopped on them; measured, the same command after `git rebase --abort` prints
nothing, because the abort resolved the index. So the order inside the function is
forced: read the paths, then abort, then return — and the abort is not optional,
because a checkout left mid-rebase makes every later git command in it a step
inside an unfinished replay, including the `remove_worktree` the next attempt
would try first (ADR-0043 never forces a removal).

## Decision

`rebase_onto_remote` is fetch, rebase, and — only on a conflict — read-then-abort:

```text
git fetch <remote>
git rebase --no-autostash refs/remotes/<remote>/<branch>
  ok    -> Applied { new_sha: head_sha(worktree) }
  error -> paths = git diff --name-only --diff-filter=U
           paths empty -> return git's refusal unchanged
           paths held  -> git rebase --abort; Conflict { paths }
```

- **Fetch first, and rebase onto the fully-qualified remote-tracking ref.**
  `refs/remotes/<remote>/<branch>` is a cache of the last conversation with the
  remote, and ADR-0046 measured that a push writes into it whether or not the
  remote took anything. Rebasing onto the cached value can land on a commit that
  is not the remote's tip and produce a candidate the same remote refuses again —
  the state this call exists to end. The qualified spelling is ADR-0046's
  finding that `rev-parse main` answers about the local branch, applied here; it
  also means no `remote` or `branch` value can reach git wearing an option's
  clothes (ADR-0040).
- **`--no-autostash`, always.** Unfinished work in the tree is refused with git's
  own words. Nothing here moves a task's work aside to make a repair fit.
- **`Applied { new_sha }` comes from `head_sha`.** The caller journals and
  publishes this value, so it is read back from the repository through the same
  door every other SHA in this module comes from (ADR-0041, ADR-0045). A no-op —
  a branch level with its remote, or one that only lags it — reports the commit
  it left in place rather than an error: there was nothing to repair, and a run
  that heard `Conflict` there would stop for a human over a branch that was never
  behind.
- **A conflict is `Ok(Conflict { paths })`.** `Err` stays reserved for git
  refusing to answer at all, and those refusals come back exactly as git wrote
  them, carrying the argument vector that ran.
- **Nothing is pushed, and no gate is claimed.** The replay rewrote the
  candidate, so §8's gates have to run against `new_sha` before it can be offered
  again; §10's publication and its repository lock stay where ADR-0046 and
  ADR-0047 put them.

## Alternatives considered

- **`Error::GitConflict { paths }` as a twelfth variant** — the caller already
  has to distinguish three answers, not two, and `docs/DESIGN.md`'s enum is
  closed. Had the enum been open, this is the honest answer, and superseding this
  ADR is how it would arrive.
- **`Ok(Conflict)` only after a clean tree check of our own** (`require_clean`
  first, `Error::Policy` on dirt) — tempting, because §10 does call a dirty tree
  a policy failure. It was declined: the check would replace git's refusal of the
  rebase with this module's paraphrase, and the caller that owns the policy
  verdict (T093) would lose the fact that it was the rebase that refused. The
  `--no-autostash` flag gets the same guarantee without the paraphrase.
- **`git pull --rebase`** — one command that also decides *which* remote and
  branch to mean from the checkout's configuration, which ADR-0041 refuses for
  every call here, and whose fetch half is invisible from the argv.
- **`git rebase --onto <tip> <old-tip>`** — the three-argument form needs the
  pre-drift tip as a fact, and the caller does not have one worth trusting; the
  merge base git computes is exactly that commit and is computed from the
  repository instead of from an argument.
- **`git merge <tip>` instead of a rebase** — it produces a candidate too, and a
  faster one, but it makes the task's commit a child of two parents, so the
  verification evidence stops describing a linear candidate, and the repaired
  branch carries someone else's history as its own. §10's model is a mainline
  fast-forward.
- **Detecting the conflict from `git status --porcelain`'s unmerged columns** —
  the same fact, one more parser. `status_porcelain` and `dirt_of` classify three
  kinds (ADR-0044) and an unmerged path already lands in two of them; adding a
  fourth kind so that a message can be assembled is a different question from
  "which paths are unmerged", which `--diff-filter=U` answers directly.
- **Leaving the rebase in progress for the caller to abort** — it would preserve
  the paths without reading them, and poison the checkout for every command that
  follows, including the cleanup. VISION.md §7's "preserve the worktree" means do
  not lose the task's work, not leave it in a state only a human can exit.
- **`git rebase --abort` as the detector for "was a rebase running?"** — measured:
  with nothing in progress it exits 128 with `fatal: no rebase in progress`, so it
  works as a probe. It lost on clarity: the tests ask git for the paths of its own
  bookkeeping directories (`rev-parse --git-path`) because that is the fact the
  task names, not git's opinion about a command.

## Consequences

- A `git_conflict` is now repairable without a provider session, and the run that
  repairs it has to re-verify: `Applied { new_sha }` is a candidate no gate has
  yet seen. T093 is where that reruns, and nothing here pretends otherwise.
- The two conflict backends (`rebase-merge`, `rebase-apply`) are both checked by
  the tests' "no rebase in progress" probe, so a change of git's default backend
  cannot silently leave one unexamined.
- `Conflict { paths }` carries git's path text — relative to the tree, C-quoting
  left in — the same rule ADR-0041 keeps for status and `require_clean`. A caller
  that wants absolute paths joins them itself, knowing which tree they came from.
- If the abort itself fails, the paths are lost and the error is the abort's: the
  state a caller must act on is a checkout still inside a rebase, and no path
  list describes that. Untested deliberately — making `git rebase --abort` fail
  needs a locked index or a read-only object store, both of which test the OS
  rather than this decision.
- A remote whose fetch refspec does not cover `branch` cannot be repaired by this
  call: the fetch succeeds, the ref is absent, and git's `invalid upstream`
  refusal comes back. Same shape as ADR-0046's read-back failure, and same advice
  — fix the remote, not the code.
- Rebasing re-creates the task's commits, so a worktree whose repository has no
  committer identity fails at the rebase with git's own message. That is the same
  dependency `commit_all` already has (ADR-0045), not a new one.
- Still open, and not this call's to close: §7's circuit breaker over repeated
  identical conflicts. Two attempts that both report the same paths are the
  signature to trip on, and the runner owns the attempt count.
