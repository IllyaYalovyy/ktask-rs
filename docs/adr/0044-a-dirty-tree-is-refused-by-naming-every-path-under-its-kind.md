# 0044. A dirty tree is refused by naming every path under its kind

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T048 turns the one clause of VISION.md §10 — "a dirty tree at verification time
is a `policy_failure`" — into a call: `require_clean(worktree) -> Result<()>`.
ADR-0041 already put `is_clean` and `status_porcelain` under it, so the fact is
answered; what was missing was the refusal. `classify.rs` already routes "the
tree was dirty at verification time" to `FailureClass::PolicyFailure`, and
`Error::Policy` already documents that "naming one of several paths would send a
human to look at the wrong file". Six things were still open.

**A boolean stops a run without telling anyone what to fix.** `is_clean` returns
`false`. A `policy_failure` journal line holding "the tree was dirty" cannot be
acted on by the agent that has to commit the work or the human that has to
decide whether an untracked file belongs to the task, and `Error::Policy` carries
a `paths` field precisely so that nobody has to re-run the check to learn the
list.

**Three kinds, or one list.** The rule cares about three states that are not the
same instruction: a staged path is waiting on `git commit`, a modified one on
`git add` first, and an untracked one on a decision about whether it belongs to
the task. Collapsing them into "N dirty paths" loses the only part an operator
acts on; the taxonomy has no "half-committed" class to route it to.

**Measured: git's own columns already say all three.** `status --porcelain
--branch` answers `XY <path>`, X being the index against the commit and Y the
working tree against the index, with `??` for a path neither has held. ADR-0041
made sure both columns survive the transport's trim, which was recorded there as
a correctness fix for the *single unstaged edit* — the common case it protects is
exactly the record that would otherwise read as staged.

**Measured: two statuses name two paths, and some name both kinds.** git prints a
staged rename or copy as `R  from -> to` — one record, two paths, both different
from the commit. `MM` and `RM` set both columns, and an unmerged path prints
`UU`/`AA`: neither side's content is committed and none of it is resolved.

**An ignored path must not trigger it, and an untracked directory may be one
path.** `is_clean` inherits git's defaults (ADR-0041); a refusal that listed
different paths than the predicate counted would be two answers to one question,
and a project that builds into its own tree would never reach verification.

**The paths go to a human, and git's text is not always a filename.** ADR-0041
accepted that a record is "a report about a path, not a path to hand back to
`std::fs`" — this git C-quotes a name holding a space. That was tolerable for a
query; it is load-bearing the moment the value is what someone is told to fix.

## Decision

`require_clean` asks `status_porcelain` the one question it already asks, and
adds nothing to the transport.

- **One git call, never three.** The kinds come from the columns of the records
  `is_clean` already counted, so the refusal and the predicate can never disagree
  about how many paths are dirty: they read the same answer.
- **A column that is neither a space nor `?` means that side differs.** Staged is
  X set, modified is Y set, untracked is `??` and nothing else. A path whose
  record sets both columns is named in both kinds, because committing the index
  now would still leave the second half uncommitted and understating either half
  would misdescribe the tree. `Error::Policy::paths` names it **once** — the same
  filename twice in a row reads as two files needing attention.
- **A rename names both of its paths.** git's `from -> to` is split on git's own
  separator; a name containing that separator arrives quoted, and stays quoted.
- **Unmerged paths get no fourth kind.** `UU` and `AA` set both columns, so they
  are named under both, which is honest: the check's job is to refuse, and a
  `conflict` category is a distinction the failure taxonomy does not branch on
  (ADR-0040 routes a git refusal, and this is not one).
- **The rendered detail carries the kind, `paths` carries the paths.**
  `Error::Policy` has one string and one list; the kinds have nowhere else to
  live, so the detail reads
  `the tree holds uncommitted work at <dir>: staged (…); modified (…); untracked (…)`
  and every offending path is in the list rather than only counted.
- **git's defaults stay git's, and git's text stays git's.** Ignored paths are
  never listed, a wholly untracked directory is one record naming the directory,
  and a quoted name is reported quoted. Paths are relative to the tree that was
  handed over, which is the directory git ran in.
- **A refusal, not a report.** The signature is `Result<()>`. A `Dirt` value
  returned beside `Ok(())` would let a caller that ignored it verify a dirty tree,
  and the value's only use is the message.

## Alternatives considered

- **Three narrower git calls** — `diff --name-only` for modified,
  `diff --cached --name-only` for staged, `ls-files --others --exclude-standard`
  for untracked. Three processes, three answers taken at three instants, in a
  tree an agent may still be writing to; and their counts need not agree with the
  one `is_clean` gave. It also re-implements the `--exclude-standard` half of
  git's ignore rules as this project's responsibility.
- **`--porcelain=v2`** — status in its own field and per-file untracked entries,
  but a second format every later caller learns, for a distinction v1's two
  columns already carry. ADR-0041 declined it for the same reason.
- **`-uall`** — names each file inside an untracked directory instead of the
  directory. It turns git's collapsed answer into this module's listing, and a
  directory that is entirely outside every commit is not misdescribed by its own
  name.
- **`git diff --quiet` and friends** — an exit code and no path, so the caller
  that has to name the files asks a second question anyway.
- **First path only, or a count** — forbidden by the task and by the reason
  `Error::Policy` carries a list at all.
- **Unquoting C-quoted paths here** — makes this module's parser the authority on
  a filename, and a name git quoted for a reason would arrive silently changed.
- **A fourth `Unmerged` kind** — invented from a column pair rather than read
  from one, for a caller that does not yet exist.
- **Putting the check in a verification or policy module** — the classification is
  of git's own record format, and ADR-0041 keeps that format's interpretation in
  the one module that owns it.

## Consequences

- Verification (a later task) calls one function at the phase boundary and
  journals a `policy_failure` whose message already names what to commit; no
  second call re-derives the list, and nothing here unstages, commits or cleans
  on the way past.
- The refusal cannot contradict `is_clean`, and inherits its defaults whole:
  `status.showUntrackedFiles` or a `.gitignore` change in a project's
  configuration changes both at once, which is where ADR-0041 said that belongs.
- A path git quotes reaches the message quoted. A caller that needs to open the
  file asks git a narrower question (`ls-files -z`, `diff --name-only -z`) rather
  than trusting this list as filesystem coordinates — named here, so the first
  person to trip on it finds the decision rather than the surprise.
- The kinds live in prose, not in a machine-readable field. A screen that wants
  to colour staged differently from untracked needs a report type, and that is a
  decision to take in the task that needs it — not one to make now for a caller
  that does not exist.
- The check reads the tree it was handed and no other, so a dirty user checkout
  cannot fail a clean task worktree (VISION.md §10's isolation), and a task
  worktree's dirt cannot fail the checkout.
