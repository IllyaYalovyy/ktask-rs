# 0046. A publication is proven by the tip a fetch brought back, not by the push that succeeded

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T050 turns steps 5–7 of VISION.md §10 into
`publish(worktree, remote, branch, candidate) -> Result<()>`: push the
candidate, fetch again, and require the fetched remote mainline tip to equal the
candidate SHA. §10's step 7 makes that comparison the gate a task waits behind
before `published_verified`, so the question this ADR answers is not "how do we
push" but "what is allowed to count as evidence that the push worked".
ADR-0040 put the transport underneath, ADR-0041 the repository queries, and
ADR-0045 the commit that produced the candidate.

**A `git push` that exits zero does not mean the remote holds the commit.** It
means the remote accepted the ref update at the moment it answered. A
supervisor that trusted the exit status would journal `PublishVerified { commit,
remote_sha }` — the event catalog in `docs/DESIGN.md` names both SHAs — from a
single command's silence, which is the pattern VISION.md's "nothing is done on
an agent's say-so" forbids when the agent is a remote instead.

**Measured (git 2.55.0): the push records its own success locally.** After
`git push origin -- <sha>:refs/heads/main`, the working repository's own
`refs/remotes/origin/main` names `<sha>` immediately, with no fetch. The same
happens when `remote.origin.pushurl` points somewhere else than `remote.origin.url`:
the push to the mirror prints its own success and writes it into the tracking
ref, while `origin`'s `refs/heads/main` stays where it was. So a check written
against the tracking ref alone passes in exactly the state it exists to catch,
and a test that pushes and fetches the same repository cannot tell the two
commands apart — the push has already written the answer the fetch would bring
back. This is why the fixture points push and fetch apart.

**Measured: `git rev-parse main` answers about the wrong branch.** In a clone
standing on `main` whose `refs/remotes/origin/main` holds a different commit,
`git rev-parse main` printed the *local* commit. A comparison written that way
grades the candidate against the commit the run started from and reports success
for a push no remote ever took.

**Measured: a candidate that reads like an option is taken as one.**
`git push origin -f` — `-f` in the refspec position, no separator — was parsed as
`--force` and force-pushed the branch the checkout stood on, moving the remote.
Behind a `--`, the same string is refused as `src refspec -f does not match any`.
The candidate arrives from a journal line or a caller's variable; it is data, and
this module's own rule (ADR-0040) is that a git call takes one argv.

**The failure has to be classifiable by a run that is not here.** VISION.md §7
files a git refusal under `git_conflict`, which a run may remediate. "The push
was refused" and "the remote holds something else after a push that worked" pull
in opposite directions — the first is retryable, the second is evidence a
publication is being claimed that did not happen — and both were about to arrive
as the same `Error::Git`.

## Decision

`publish` is three commands a human would run, plus a comparison:
`git push <remote> -- <candidate>:refs/heads/<branch>`, then `fetch`, then
`git rev-parse --verify refs/remotes/<remote>/<branch>` compared to `candidate`.

- **The refspec names the candidate, not the branch.** `<candidate>` is the only
  commit this call can publish; the checkout's own branch is never the candidate,
  because task work happens in a detached worktree (ADR-0043). The compared SHA
  is then the same value that was offered, so no two spellings of "what did we
  publish" can disagree.
- **`--` separates options from the refspec.** No argument this module builds is
  allowed to reach git wearing an option's clothes.
- **The comparison reads a ref that a fetch just wrote.** Fetch first, then read
  `refs/remotes/<remote>/<branch>` fully qualified, through `--verify`. The
  qualification is what makes `rev-parse` answer about the remote instead of
  about the local branch, and the `refs/remotes/` prefix means no `remote` or
  `branch` value can turn the argument into an option either.
- **A mismatch is `Error::Git` carrying the read-back argv and both SHAs.** The
  argument vector identifies which half failed — `push` for a refusal,
  `rev-parse` for a disagreement — and the message states candidate and fetched
  tip in one sentence, because that pair is the fact worth journaling and git
  printed neither half of it.
- **No force, no retry, no repair.** Nothing here moves a ref to make its own
  claim true. A branch the remote will not fast-forward stays refused.

## Alternatives considered

- **Trusting the push's exit status** — the exact claim §10 step 6 refuses; it
  also cannot see a `pushurl` divergence, a hook that rewrote the ref, or a peer
  that moved the branch microseconds later.
- **Reading `refs/remotes/<remote>/<branch>` without fetching** — measured above:
  the push itself writes that ref, so the check is circular. The ref is a cache
  of the last fetch by definition, and §10 asks for a fetch *after* the push.
- **`git ls-remote <remote> refs/heads/<branch>`** — fresher still, and it lost:
  the fetch is not only a comparison source, it is also what brings the remote's
  objects into the local repository. A comparison that cannot notice a missing
  fetch cannot notice a stale one either, and §10's fetch would have become a
  call whose absence changes nothing.
- **`git rev-parse <remote>/<branch>`** — the abbreviated form resolves through
  `refs/remotes/` today, but `rev-parse` accepts a local branch name for the same
  string whenever one exists (measured above), so the abbreviated spelling has a
  way to answer the wrong question. The qualified name has no such ambiguity.
- **`git diff --quiet <candidate> <remote>/<branch>`** — compares trees, not
  commits: an empty commit or a rebased commit with the same tree would pass, and
  §10 asks for SHA equality.
- **A new `Error::PublishMismatch { candidate, remote_sha }` variant** — the
  honest taxonomy answer, and the one worth revisiting. `docs/DESIGN.md` closes
  the enum at eleven variants ("Later tasks require these fields by name") and
  VISION.md §3 gives that decision to the human, so it was not taken here; the
  event catalog already has `PublishVerified { commit, remote_sha }` to hold the
  pair once a run has it.
- **`--force` or `--force-with-lease`** — a supervisor that overwrites a branch
  it did not fetch destroys the recovery property §10 exists to provide, and
  VISION.md's publication mode is a fast-forward mainline push.
- **Taking the pushed SHA from `git push --porcelain` output** — one more format
  to parse, and it reports what git intended to set, not what the remote holds
  now.

## Consequences

- `published_verified` can be reached on a comparison a reader can reproduce by
  hand, which is the property VISION.md claims over "the tool said so".
- The two failures are separable without a new error variant, but only by
  inspecting `args`; if a runner needs to branch on them, that is the moment the
  new variant earns its place, as a superseding ADR.
- A remote configured with a fetch refspec that does not cover `branch` fails at
  the read-back with git's own `--verify` refusal — one SHA in the story, so it
  is not dressed up as a mismatch.
- The fetch makes publication three git processes and one network round trip per
  attempt, with no time budget (ADR-0040's standing gap): a fetch that hangs
  hangs the run.
- §10 step 5's repository lock is deliberately *not* here. A lock makes a
  mismatch unlikely; this makes one seen. Adding the lock to this function would
  let a serialized run believe its own push again, which is the assumption this
  ADR exists to refuse. The lock belongs to whoever owns integration (a later
  task), and nothing here needs it to be correct.
- Coverage of the mismatch path needs a remote whose push and fetch halves
  disagree; the fixture builds that with a second bare repository and
  `remote.origin.pushurl`. A future real-remote test gets it for free.
