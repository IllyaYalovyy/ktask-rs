# 0045. A commit stages tracked changes and refuses an empty one

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T049 turns step 3 of VISION.md §10 — "all changes are committed inside the task
worktree" — into `commit_all(worktree, message) -> Result<String>`. ADR-0040 put
the transport under it, ADR-0041 the repository queries, and ADR-0044 the
`require_clean` refusal that verification will call before this step is ever
reached. Five things were still open, and four of them are decisions this module
is not entitled to make silently.

**"Stage the changes" has two readings, and only one of them is safe.** `git add`
with no flag stages untracked files as well as edits. VISION.md §3's eighth
invariant gives decisions to the human rather than the agent, and VISION.md §11
spends a whole section on why a repository must not accumulate AI artifacts:
a supervisor that staged everything would commit the scratch file, the replayed
provider log and the build output into the candidate that it then pushes to
mainline. The opposite failure is real too — dropping a path an agent
deliberately `git add`-ed would lose work that was already decided on.

**Measured: git refuses an empty commit loudly, and says nothing where this
module reads.** `git commit -m …` in a clean tree exits 1 and writes its status
report to standard *output* — 195 bytes on git 2.55.0 — with an **empty standard
error**. `git commit --dry-run` is the same (exit 1, 203 bytes, stderr empty).
Under ADR-0040 every non-zero exit becomes `Error::Git { args, stderr }`, so
git's own refusal would arrive as an error quoting the command it refused to run
and an empty explanation, and the caller could not tell "the agent produced no
work" from "the index was locked". The alternative to refusing is
`--allow-empty`, and an empty commit is still a commit: it has a SHA, it passes
the gates that read the tree, and it moves the remote's mainline.

**Measured: `git add --update` without a pathspec is not "the tree".** With no
pathspec it exits 0 and stages only the current directory — git's own shorthand
for `-u .`. Run from a subdirectory, `--update -- :/` staged a change at the
tree's top level while `--update -- .` staged nothing. The difference appears
only as a candidate holding part of the work, which is then verified green and
pushed.

**Measured: an unborn repository answers a different question.** In a repository
with no commit, `git add --update -- :/` exits 128 with "did not match any file(s)
known to git". Taken naively, "nothing git knows about" and "nothing to commit"
collapse into the same refusal, and the first is a broken tree while the second
is an agent that did nothing.

**The SHA has to come from one place.** §10 step 6 compares the local candidate
SHA against the SHA fetched back from the remote, and `Error` journals what this
call returns. `git commit` prints a summary, `git rev-parse HEAD` prints an
object id, and `%H` prints a third spelling; two doors answering "what did you
just commit" is the failure this function exists to prevent.

## Decision

`commit_all` is three commands a human would run, plus a read-back:
`git add --update -- :/`, `git status --porcelain --branch` (through
`status_porcelain`), `git commit -m <message>`, then `head_sha`.

- **Tracked changes only.** `--update` is the whole of the rule: edits,
  deletions and mode changes of paths git already knows go in; a path no commit
  and no index entry ever held does not. Whether such a path belongs to the task
  is a decision, and it belongs to whoever wrote it, not to the process that
  publishes the result.
- **The pathspec is `:/` — git's name for the tree's top level.** The candidate
  is the whole tree, not the subtree the call happened to run in. It is git's
  magic pathspec and not a path this module built, so in a task worktree it names
  that worktree's own top level and cannot reach the checkout the worktree was
  created from.
- **The index is added to, never reset.** Whatever is already staged rides along
  into the commit, and nothing is undone on the way out — a refusal leaves the
  work staged, because the staged work is the evidence a later attempt reads.
- **"Nothing staged" is refused here, as `Error::Policy`.** The check is the
  staged column of the records `require_clean` already knows how to read
  (ADR-0044's `Kind::Staged`), so the two doors cannot disagree about what
  *staged* means. The detail names the tree, and names any untracked path that is
  why the tree looked empty, because that is the fact a reader can act on;
  `paths` carries the tree that was refused to commit into, the way
  `create_worktree` carries the directory it refused to touch.
- **The SHA comes from `head_sha`.** One door answers "what did you commit", and
  the value the caller journals is the value the repository holds.
- **Identity, message and every other refusal belong to git.** No committer name
  or timestamp is invented (ADR-0040): an unconfigured machine gets
  `Error::Git` quoting `commit -m …`, which is `doctor`'s business to prevent. A
  missing message, a locked index, a refusing hook and a non-repository are all
  `Error::Git` with git's own words.

## Alternatives considered

- **`git add --all`** — stages untracked files. Rejected for the reason VISION.md
  §11 gives: the supervisor would decide, by accident, that whatever the agent
  left in the tree is part of the task.
- **`git commit -a`** — same decision, made inside the command that writes it,
  with no way to name what it swept in.
- **`git commit --allow-empty`** — the shape the task's own done-when rule
  forbids, and worse than it looks: an empty commit verifies, publishes and
  reports a candidate containing no work as delivered.
- **Letting `git commit` answer for an empty tree** — measured above: exit 1 with
  an empty stderr, so `Error::Git` would carry no reason, and reading git's
  stdout to classify the refusal would make this module a parser of a
  human-readable report.
- **`git diff --cached --quiet` as the emptiness check** — correct, but a fourth
  process and a second source of truth beside the status records `require_clean`
  already reads.
- **No pathspec, or `-- .`** — measured above: a call made from a subdirectory
  commits part of the work and reports success.
- **A new `Error::NothingStaged` variant** — the honest taxonomy answer, since
  `PolicyFailure` is also the class a run reaches for "the agent produced
  nothing". `docs/DESIGN.md` closes the enum at eleven variants and says "Later
  tasks require these fields by name", and VISION.md §3 gives that decision to
  the human, so it was not taken here. `Error::NotFound` was also rejected: it
  reads as a lookup that missed, not a rule that fired.
- **`git write-tree` + `git commit-tree`, reading the SHA from stdout** — one
  fewer process and no dependency on `user.name`, at the cost of a plumbing
  pipeline no one can run by hand, which is the reason ADR-0040 chose the
  command line at all.
- **Staging with `git add --update` and taking the SHA from git's summary line**
  — two spellings of one answer, where §10 step 6 compares them.

## Consequences

- A task's candidate is exactly the tracked work plus whatever was deliberately
  staged, so publication (a later task) can compare local and remote SHAs
  knowing the candidate's contents were decided by the task, not by a flag.
- An agent that creates a new source file and nothing else produces a refusal
  that names the new file. The behaviour is deliberate and the message says so;
  the fix lives with whoever writes the prompt, and a future `doctor` or prompt
  check is the place to make it rarer.
- A commit needs git identity. On a machine with no `user.name`, this is
  `Error::Git` — reported, not silently worked around. `doctor` (VISION.md §12)
  is where the preflight belongs.
- Four git processes per commit, no time budget and no output cap, both gaps
  ADR-0040 already named. A commit that hangs hangs its caller until gates or a
  supervisor-level budget says otherwise.
- If a project genuinely needs a new file committed, something must stage it
  first; nothing here will do it on the agent's behalf. If that turns out to be
  wrong in practice, the change is one flag plus a policy decision recorded as a
  superseding ADR — not a silent `--all`.
