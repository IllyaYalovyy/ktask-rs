# 0067. A self-healing boundary is a table of paths, refused the same way every attempt

- **Status:** accepted
- **Date:** 2026-09-20

## Context

VISION.md §3's invariant 5 — "Self-healing cannot weaken checks, change policy,
install host software, or conceal failures" — and §2's non-goal "No
self-modification of policy" are one rule read from the two ends of a run. §7
restates it again: "Self-healing repairs project-controlled defects only. It
never touches host configuration, never edits gate definitions, never invents
product policy." None of that was enforced anywhere. The gate configuration, the
lint configuration, the scripts that *are* the gates and `.ktask/` itself were
ordinary files an ordinary attempt could edit, which puts `scripts/quality.sh` —
the definition of done — one edit away from being whatever the attempt needed it
to be. A prompt that asks the agent not to do it is a request; the case that
matters is the one where the agent did it anyway.

T072 fixes the shape: `check_no_policy_edit(diff_paths: &[PathBuf]) -> Result<()>`
refusing a diff that touches gate configuration, `deny.toml`, `clippy.toml`,
`rustfmt.toml`, `scripts/` or `.ktask/`, called on every attempt rather than only
on a remediation, "against the real diff". Five decisions are left to this
module, and each is a decision rather than an obvious step: what "gate
configuration" names concretely; how wide each guard stands; which file the
boundary must leave alone even though a gate reads it; what to do with a path the
check cannot read; and what a refusal is once one has been reached.

## Decision

1. **The protected set is a table of named entries, in code.** Two directories —
   `scripts/` (the gate commands themselves: VISION.md §8 makes them
   runner-executed rather than agent-authored) and `.ktask/` (the project's own
   configuration and per-run state, including the verification profile the runner
   builds its gates from) — and six file names: `clippy.toml`, `rustfmt.toml`,
   `.rustfmt.toml`, `deny.toml`, `_typos.toml`, `typos.toml`. Those are the files
   the nine gates of `docs/QUALITY.md` read for their rules; the two spellings
   each of `rustfmt` and `typos` accepts are both listed, because guarding one
   and not the other guards nothing. A table rather than a matched sentence
   because the rule has no gradation — an entry is guarded or it is not — and one
   test walks the table, so an entry nobody named cannot be dropped in passing.
2. **A directory is guarded at the repository root; a file by its name at any
   depth.** `scripts/` and `.ktask/` are top-level or they are something else:
   `crates/ktask-core/src/scripts/` is a module, and a boundary that refused it
   would be a boundary a task learned to work around by naming its directories
   carefully. A configuration file is the opposite case. `clippy` and `rustfmt`
   read the nearest configuration walking *up* from the file they are checking,
   so a `clippy.toml` inside one crate is what that crate's lint gate reads — and
   a guard fixed to the repository root would leave the deeper one, the one a
   task would actually write, untouched.
3. **`Cargo.toml` is not protected, and that is recorded as a hole rather than
   passed over.** It is where `[workspace.lints]` lives (`docs/QUALITY.md`), so it
   is in a real sense gate configuration. It is also the file every dependency
   change must edit, which AGENTS.md then requires be committed with the lockfile
   in the same commit; guarding it would refuse ordinary work, and a boundary that
   refuses ordinary work gets worked around. Path granularity cannot separate the
   lint table from the dependency table, so the manifest stays open and the half
   of the rule that lives in it is reported as unfinished rather than quietly
   dropped.
4. **A path this check cannot place inside the repository is refused.** An
   absolute path, one that climbs above the root the protected set is rooted at,
   and one that names no component (`""`, `.` — the repository, which contains
   `scripts/`) are each refused, under a sentence of their own. Everything else
   is normalized lexically first — `.` dropped, `..` cancelling the component
   before it — so `scripts/inner/../quality.sh` is the gate script. Passing what
   cannot be read is the exact shape of the hole this module exists to close.
5. **One predicate, for every attempt, journalled as the task's failure.** The
   signature takes no attempt number, which is what makes it unable to care about
   one: the remediation is the attempt most motivated to edit the gate that
   refused it, but the first attempt that edits the lint configuration has broken
   the identical rule, and T072's own done-when asks both to "fail the same way".
   The refusal is `Error::Policy` naming every offending path, which
   `crate::classify` already classes `policy_failure` under VISION.md §7's "forbidden
   file, dirty tree, attempted gate bypass", and `policy_edit_event` turns it into
   the `TaskFailed` record that ends the task from `Running` and from
   `Remediating` alike. No new catalog entry is admitted (ADR-0011) because none
   is needed: `state::apply` already fails a task on that event (ADR-0022), and
   `trip_event` of ADR-0062 set the precedent for a refusal that arrives as data.

## Alternatives considered

- **Trust the prompt, or the agent's report of what it touched.** VISION.md §3's
  invariant 4 forbids taking an agent's word for what it did; a boundary built on
  it is the boundary this task exists to replace.
- **Make it a gate inside `scripts/quality.sh`.** Self-referential in the bad way:
  the script is itself a protected path, so the check that catches an edit to it
  would live inside it. It also runs at verification time, after the phase that
  did the editing, whereas the rule is about refusing the attempt.
- **Match diff contents rather than diff paths.** A hunk-level rule needs to know
  which edits *weaken* a configuration, which is a judgement about TOML semantics
  per tool, and every judgement is a place to argue that this particular edit was
  harmless. One character changed in `clippy.toml` moves a threshold, so there is
  no reading of the question on which a partial edit is safe.
- **Glob patterns, or a configurable protected set in the project's
  configuration.** VISION.md §3: "v1 is strict-only: no relaxation knobs". A
  boundary an attempt can configure is a boundary an attempt can remove, and the
  configuration file it would configure it through is `.ktask/config.toml`.
- **Resolve paths through the filesystem before comparing.** Canonicalization
  answers symlink questions this module has no business answering — it would need
  the worktree, and the check would stop being a pure predicate over what `git`
  reported. `git` reports repository-relative names for real changes; resolving
  `.` and `..` lexically covers the spellings that reach it.

## Consequences

- An attempt that edits the rules it is judged by is refused rather than graded,
  and the journal says which paths it touched, so a run that stopped this way
  reads back as a policy failure rather than as a red gate with an unexplained
  history.
- Nothing consumes the check yet, exactly as nothing consumed `should_continue`
  when ADR-0066 landed. The call site is the runner's phase step, which T091
  builds beside `protocol::check_scope` and `git::changed_paths` — the same
  evidence, so the two checks cannot disagree about what changed.
- The check is about paths, so it is cheap, pure and testable, and it cannot be
  talked to by an agent's prose. What it cannot see is what it does not judge: a
  symlink inside a worktree that points at a protected path resolves to whatever
  `git` says changed, and path comparison is case-sensitive, so a filesystem that
  folds case is the caller's problem — one the git layer already owns, since a
  task worktree is built and refused by `crate::git`.
- `Cargo.toml` remains editable by any attempt, and with it
  `[workspace.lints]`. Closing that needs a different mechanism than path
  matching — a gate that diffs the lint table against the committed one — and is
  reported as unfinished work rather than done by this ADR.
- If a later task makes the gate set data-driven with a configuration file named
  by the project rather than by `docs/QUALITY.md`, decision 1 has to grow: the
  table is compiled in on purpose, and a gate defined by a path nobody listed here
  is a gate nothing guards.
