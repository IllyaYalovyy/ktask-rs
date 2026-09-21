# 0078. Recorded decisions are collected by filename and reach every later prompt

- **Status:** accepted
- **Date:** 2026-09-21

## Context

VISION.md §3's invariant 8 says an unresolved decision is a first-class pause state
and "its resolution is recorded as a decision record (ADR) available to future
tasks". `docs/PROCESS.md` fixes where a record lives — `docs/adr/NNNN-short-title.md`,
copied from `0000-template.md` — and `docs/CONTRACT.md` §3 fixes who writes one:
`ktask-rs resolve`, which "is recorded as an ADR in `docs/adr/` and injected into the
context of subsequent tasks". T083 is the injection half. ADR-0075 had already split
prompt assembly into a pure projection (`assemble`) and a reading half, and ADR-0077
built the reading half for two of the four inputs, leaving a note that whoever starts
a provider owns the remaining two: the context document, and the decisions.

**The one directory the supervisor may read inside a working copy.** Invariant 6 keeps
prompts, context, plans, logs, reports and state out of the repository, and VISION.md
§3 grants exactly one exception: ADRs, "because they are project documentation,
valuable to humans and to future tasks alike". So `collect_adrs` is the first reader in
`ktask-core` that opens a path below `Project::root`, and it has to be exact about
which path and which files — a rule with one exception is only a rule if the code
knows the boundary of the exception.

**Collection order is a correctness question, not a cosmetic one.** A later record
supersedes an earlier one by number, and `assemble` hands the list to a session in the
order it is given. `read_dir` returns entries in whatever order the filesystem keeps
them; mtimes are reordered by a checkout, a copy, a `git reset` and a worktree
rebuild; and nothing about the text inside a record says when it was decided.

**"Nothing there" and "something there in the wrong shape" are different facts.** A
project on its first task has decided nothing and never ran `resolve`; a repository
whose `docs/adr` is a file, or whose record is a symbolic link into somebody's
scratch directory, is a repository whose operator is about to be misled. ADR-0077
settled this distinction for prompt documents and the same rule applies to records.

**The prompt carries every record in full, forever.** Nothing truncates a decision and
nothing summarises one, so the cost of the fourth part of a prompt grows with the age
of the project. This is the point where that becomes visible rather than theoretical:
this repository holds 77 records at the time of writing.

## Decision

**`collect_adrs(repo_root)` reads `docs/adr/*.md` and returns their text in filename
order, skipping `0000-template.md`.** No git, no recursion, no content sniffing. The
repository root goes in and a `Vec<String>` comes out, ordered oldest-first, so the
caller can hand it straight to `assemble`.

**Filename order is decision order, because the name is the number.** `NNNN-…` is
four digits with leading zeros, so sorting the names sorts the decisions. The padding
is load-bearing and is written down as such: past 9999 records, `10000-…` sorts before
`9999-…`, and a project that gets that far revisits the rule rather than discovering it
in a prompt.

**The template is skipped by exact filename.** `0000-template.md` is a shape, not a
decision anybody made, and a prompt that carried it would spend tokens teaching a
session the format of a decision instead of the decision. Skipping by name is one
comparison and cannot mistake a real record numbered 0000 for the template.

**A record is a `.md` document sitting directly in `docs/adr`; anything else there is
ignored, and a symbolic link is refused.** An editor's `.md.bak`, a `README`, a
`drafts/` directory and a directory whose name ends in `.md` are all skipped. A link —
in the directory or of the directory — is `Error::Policy` naming the path, the same
answer ADR-0077 gives a prompt document reached through a link: a decision read through
one is a decision whose author and number the filename does not describe.

**No `docs/adr` directory is an empty list, not an error; a wrong shape is an error.**
An empty list renders through `assemble` as "None recorded yet.", which is a true thing
to tell a session. A file where the directory belongs, a record that is not UTF-8
(`Error::Corrupt`) and an undirectory-able directory are refused rather than worked
around, and a whole prompt is refused over one unreadable record: a session handed a
prompt with a decision quietly missing from it re-decides something already settled,
and the missing record reads like the supervisor forgot it.

**`build_prompt(project, task, attempt, total)` is where the four inputs meet.** It
ensures the prompt library, reads the library's `context.md`, takes the template from
`load_template` (which still prefers the project's own override and still reads nothing
inside the working copy), takes the decisions from `collect_adrs(&project.root)`, and
calls `assemble`. `assemble` keeps ADR-0075's purity — no clock, no environment, no
path — and the runner gets one call that turns a project and a task into bytes.

## Alternatives considered

- **Ask git which records are committed (`git ls-files docs/adr`).** It looks stricter,
  and it is not the fact invariant 8 asks for. `resolve` writes a record for the task
  that comes *next*; whether the commit that publishes it has landed is the run's
  business, not the reader's. It also couples prompt assembly to the git layer and
  leaves a scratch or fixture directory with no readable decisions at all, which is
  where every one of these rules gets tested.
- **Sort by mtime, or by the order the files were read.** A checkout, a copy, a
  `git reset` and a worktree rebuild all rewrite mtimes, and `read_dir` order is
  nobody's order. A superseded decision delivered last is a session that follows the
  wrong rule with confidence.
- **Select records by a numeric-prefix regex instead of an extension plus an exact
  template name.** It accepts a record nobody named according to `PROCESS.md` and
  silently skips a renamed one; the extension rule and the one exact name between them
  accept everything the process allows and nothing it does not.
- **Skip an unreadable record and build the prompt from the rest.** The tempting
  availability win, and the one failure mode this function exists to prevent: an
  omission that looks like a decision the project never made. Loud beats available here
  for the same reason `load_template` refuses a non-UTF-8 template.
- **Recurse into subdirectories, or accept `.markdown`.** Nothing writes either, and a
  `drafts/` folder is exactly the neighbour that should *not* reach a session. A rule
  about extensions would grow one extension at a time.
- **Add `load_context(project)` so the context document gets an override too.**
  ADR-0077 declined it for want of a caller and §11 names an override for the template
  alone. `build_prompt` reads the library's document; if a project ever needs its own,
  that is a decision with its own record, not a symmetry to invent here.
- **Return `Vec<(String, String)>` of title and body, or a typed `Adr` struct.**
  `assemble` numbers the records itself ("1 of N") and needs no metadata to do it; a
  parsed title would be a second, contradicting numbering that has to be maintained.
- **Let the caller pass the ADR directory instead of the repository root.** More
  flexible, and it makes the one path invariant 6 excepts a caller's choice. Fixing it
  here is what keeps the exception knowable from one place.

## Consequences

- `resolve` has to do exactly one thing to make its answer available: write a properly
  named file. Nothing is journaled for injection to work, and no cache is invalidated,
  because the decisions are re-read per attempt from the directory that holds them.
- The supervisor now reads one path inside a working copy, and it is named in one
  constant. Any later "read something from the repo" request is a change to this rule
  and needs a record of its own, not a new `fs::read` in a new module.
- An agent can plant a record in `docs/adr`, and this is not new permission:
  `docs/adr` is the one directory an agent is told to write, so a planted decision was
  always going to reach later tasks through the repository itself. What the rule buys
  is that it arrives under a filename and a number a human can read.
- Prompts grow with the project. Every record is sent in full on every attempt, so the
  standing cost rises with each decision; retention, summarisation or a relevance
  filter is future work with a real budget question attached, and `assemble` already
  numbers each record ("1 of 77") so a session can tell a truncated read from a short
  project.
- Four-digit padding is assumed. Past 9999 records, filename order stops being
  decision order — see the rule above.
- A task that wants a decision *not* to reach its successors supersedes the record with
  a later one; deleting the earlier file is not how this project changes its mind
  (`PROCESS.md`: records are append-only).
