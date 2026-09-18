# 0040. A git call is one argv, and every way it fails is Error::Git

- **Status:** accepted
- **Date:** 2026-09-18

## Context

`docs/DESIGN.md:53` decides that git is the `git` command — no libgit2, no gix —
because it is what a human would run and its output is inspectable. It does not
decide the transport underneath that, and T044 has to: where the process runs,
what it is allowed to read, what happens to bytes that are not text, and which
`Error` variant a failure arrives as. Everything T045–T052 and T074 add
(`head_sha`, worktrees, `require_clean`, `commit_all`, `publish`, `rebase_onto_remote`)
is a thin wrapper over the one call this decides, so the decision is inherited
about twenty times and cannot be revisited per command.

Two constraints make it a decision rather than an obvious call.

The first is *whose* work this supervisor runs. VISION.md §10 gives git to the
supervisor precisely so an agent cannot publish its own work: the agent edits a
tree, the supervisor commits it. Every argument the supervisor passes to git can
therefore contain text an agent wrote — a path it created, a subject it chose, a
branch it named. If the transport were a command string handed to `sh -c`, a
file named `x; rm -rf ...` would be a code-execution channel from the edited
tree into the process that signs the commits.

The second is the failure taxonomy in VISION.md §7, which classifies a
`git_conflict` from a git error. `docs/DESIGN.md:163` gives `Error::Git` the
argument vector for exactly this reason: a rejected push has to be reportable as
the command that was rejected.

Measured while writing this, because the alternatives differ only in what they
make observable:

- `git` writes its usage to **stdout**, not stderr, and exits 1. So an
  unsuccessful run's stdout is not automatically the explanation — the two pipes
  are not interchangeable.
- A repository can hand back bytes that are not UTF-8 (`cat-file` on a blob with
  raw bytes, `ls-files -z` on a latin-1 path), and `git commit` silently
  re-encodes a non-UTF-8 *message*, so the bytes on stdout are not always the
  bytes that went in.
- `Command::output()` reads both pipes to EOF and then waits, so a child that
  wants to read the terminal is the only way this call can hang: nothing here
  writes to it.

## Decision

`ktask_core::git::git(root, args) -> Result<String>`, and one helper behind
every later wrapper.

- **argv, never a shell.** `Command::args` puts each element in front of `exec`
  as one `argv` entry. No function in `git.rs` builds a command string, so
  "no shell involved" is a property of the module, not of a caller's care. The
  tests prove it the only way that counts: an argument containing `;` runs no
  second command, and an argument containing `>` creates no file.
- **`current_dir`, not `-C root`.** The directory is a property of the process,
  not one of git's arguments. Keeping it out of argv means the vector a caller
  built is exactly the vector git sees, and the `args` field of the error is a
  faithful record of the call rather than a record with two words in it that the
  caller never wrote.
- **Stdout is trimmed.** `git` ends each line with a newline; a SHA carrying it
  compares unequal to the same SHA read from a ref file. One trim, here, so no
  caller repeats it or forgets it.
- **Undecodable bytes are marked, not refused.** Both pipes are decoded with
  `String::from_utf8_lossy`. A repository may hold such bytes; a supervisor that
  stopped reading over one would let a stray byte stop a run.
- **Stdin is closed.** A command nobody is typing into must not be left reading
  a terminal it was never handed.
- **Every failure is `Error::Git`.** Non-zero exit carries git's own stderr; a
  command that could not be started at all (missing directory, missing binary)
  carries the OS reason in the same field. Neither escapes as `Error::Io`,
  because the classifier keys on git errors and a variant it does not know is a
  run that stopped for a reason nothing can name.

## Alternatives considered

- **`sh -c "<command>"`** — refused by the constraint above; it is the one
  transport that turns data in the edited tree into instructions.
- **`git -C root`** — same observable behaviour for a working repository, and it
  gives git's own words for a missing directory for free. It lost because those
  two words land in `args`, so the error would name arguments the caller never
  passed, and because a caller that ever passed its own `-C` would silently
  disagree with the `root` this function was told about.
- **`Error::Io` for a command that never started** — the shortest diff, and it
  hides a git failure in a variant the taxonomy has no rule for.
- **Strict UTF-8 with an error on failure** — turns repository content into a
  run-ending fact, and reports a successful `git` command as a git failure.
- **`Command::status()` with inherited output** — no output to return, journal,
  or show an operator, which is most of the reason the transport exists.
- **Redacting stderr here** — error.rs asks a caller to hand redacted text, but
  ADR-0032 put redaction at the door that makes bytes durable, and
  `crate::run_gate` follows that; a producer that redacted alone would apply a
  pattern set it cannot see (the configured half lives in `Config`). Left to the
  write path, and flagged as a finding for whoever owns what reaches a terminal.

## Consequences

- T045–T052 and T074 each become one call plus a parse of its output; there is
  one place that decides what "ran", "printed" and "failed" mean.
- Two gaps are named rather than papered over, both inherited from this one
  call. There is **no time budget**: a git command that blocks on a credential
  prompt blocks its caller, because the configured budget in this project lives
  on a `Gate` and git has none. And there is **no cap on retained output**, so a
  whole diff returned by a helper is a whole diff in memory. Either belongs to
  the task that owns the budget, not to a wrapper that would have to invent one.
- `Error::Git`'s `stderr` holds git's text as git wrote it. It reaches the
  journal only through the redacting write door, but it reaches a terminal
  directly; a remote URL carrying a credential is the case to watch.
- The `stdin` line cannot be killed by a headless test — `cargo`'s own stdin is
  already at end-of-file, so an implementation that inherited it behaves the
  same here. It is kept because the property it asserts is real on an operator's
  terminal, where it is not equivalent at all.
