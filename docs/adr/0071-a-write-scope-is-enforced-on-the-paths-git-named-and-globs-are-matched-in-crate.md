# 0071. A write scope is enforced on the paths git named, and the globs are matched in-crate

- **Status:** accepted
- **Date:** 2026-09-21

## Context

ADR-0068 made a phase declare a `WriteScope`, and recorded the reservation that
went with it: the scope names the *kind* of access a phase grants, and
"Enforcement is a later task (T076's `check_scope`) and reads the git diff,
never the agent's report of what it touched." T076 is that enforcement. Three
forces shape it.

The first is §3's invariant 4 — nothing is done on an agent's say-so. A phase
that reports which files it edited is a phase grading its own homework, so the
input to the check is `git::changed_paths` (ADR-0069), which asks git two
questions and returns repository-relative paths.

The second is §9's per-language configuration. `WriteScope::TestsOnly` means
"whatever `test_globs` names", and those globs are `Config` keys with defaults
`docs/DESIGN.md` fixes (`**/tests/**`, `**/*_test.rs`, `src/**/tests.rs`). One
protocol has to mean the same thing in a Rust tree and a TypeScript tree, so the
globs arrive at the check rather than living beside the protocol.

The third is that nothing in this workspace matches a glob today. There is no
`glob` or `globset` in the dependency set `docs/DESIGN.md` fixes, and
`AGENTS.md` and that document both require an ADR to add one. So the check needs
a matcher, and choosing where it lives is the decision.

## Decision

1. **`check_scope` is given the paths, and reads nothing.** Its signature is
   `(WriteScope, &[PathBuf], &[String]) -> Result<()>`: a scope, the diff git
   named, and the project's globs. No `git`, no filesystem, no clock — the
   module's rule that a protocol which could act is a protocol an agent could
   talk to survives the enforcement half, and the two cases §9 turns on (a
   production file touched in red; a verify phase that touched nothing) are
   testable without a repository to dirty.

2. **The matcher is this crate's, not a dependency.** `*` matches inside one
   path segment, `?` matches one character, `**` matches as a whole segment only
   — no directory, or that directory and every directory below it — and every
   other character matches itself. Braces and bracketed classes are literal.
   That is the subset the shipped defaults are written in and the subset a
   language profile is written in; it is about sixty lines, and it keeps the
   dependency set the one document fixes.

3. **Matching is on a path's components, never on its string.** A pattern is
   split on `/` and matched segment by segment, so `*` cannot cross a directory
   boundary by accident. A path that cannot be placed inside the worktree —
   absolute, or climbing above the root with `..` — is refused under its own
   sentence rather than matched: `../elsewhere/tests/auth_test.rs` matches the
   default `**/*_test.rs` as a string, and a matcher that read it as written
   would hand a red phase another checkout's tests. This is ADR-0067's rule that
   a boundary must not pass what it could not read, applied to the write scope.

4. **`All` is the worktree, not the universe.** A path that cannot be placed
   inside the root is refused under `All` too, so the three scopes differ only
   about *where inside the worktree* a write may land.

5. **An empty `test_globs` makes `TestsOnly` grant nothing.** No fallback list
   is compiled into the check: `Config::default()` is the one source of the
   defaults (ADR-0004), and a project that wrote `test_globs = []` said where its
   tests live by saying nothing. The alternative is a red phase writing wherever
   a guess pointed.

6. **A pattern is anchored where it is written.** `src/**/tests.rs` begins with a
   name, so it reaches a root-level `src/` and nothing else; a workspace that
   keeps its module tests under `crates/*/src/` adds its own glob. Un-anchoring
   patterns that contain no `/` would be one less rule to remember and would
   make `*` mean `**/` — a scope wider than what was typed.

7. **Nothing calls `check_scope` yet**, as with ADR-0066 through ADR-0070: the
   runner that walks a protocol's phases is T091, and a scope with no call site
   is a smaller claim than a runner that half-reads one.

## Alternatives considered

- **Take `globset`.** It is the better matcher — character classes, alternation,
  and a maintained implementation. It lost because the dependency set is fixed by
  `docs/DESIGN.md`, `cargo deny` and `cargo machete` watch what is in it, and the
  shipped globs need three constructs. If a richer language is ever wanted, the
  tests pin the subset first, so the swap has a floor to be measured against.
- **Match the path string with `Path::ends_with` or a hand-built regex.** Cheaper
  still, and wrong in the same way: `ends_with("tests/mod.rs")` accepts
  `not-my-tests/mod.rs`, and `regex` turns a glob into a pattern an operator
  cannot read off the configuration.
- **Ask the provider what it edited.** The provider's own report is what §3
  exists to distrust; it is kept as evidence, not as a permission.
- **Refuse a `..` path rather than resolving it.** Then
  `crates/ktask-core/src/../tests/protocol.rs` — a path a caller can build
  honestly — is refused while the identical path written flat is permitted, and a
  check whose answer depends on how a path was spelled can be satisfied by
  respelling it.

## Consequences

- A red phase that edits production code ends the attempt as a
  `FailureClass::PolicyFailure`, which `classify` already selects for a policy
  error and which earns no retry: an agent cannot repair a scope violation by
  editing more files.
- A project's tests are wherever its own `test_globs` say, and a project that
  wants a test layout the shipped defaults miss adds a glob rather than waiting
  for a release. `docs/DESIGN.md` keeps the defaults; this check keeps no list of
  its own.
- The supported glob subset is documented on `check_scope` and pinned by tests.
  A configuration written against a richer glob language fails closed — braces
  match literally, so an operator's `*.{rs,test}` refuses rather than widening the
  scope — which is the direction a mistake is allowed to go in.
- `check_no_policy_edit` (ADR-0067) and `check_scope` are two checks on one diff
  and neither subsumes the other: one refuses the rules the run is judged by in
  every scope, the other refuses paths the phase was not granted. A runner that
  calls only one has a hole shaped like the other.
- The path-lexeme walk `check_scope` needs is the second copy in the crate —
  `remediate.rs` has an equivalent private one. Unifying them into `paths.rs` is
  worth doing the next time either is touched, and is not done here because both
  files are outside this task's named file.
