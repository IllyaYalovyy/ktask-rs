# 0042. A disposable repository pins what a commit hashes and owns its own directory

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T046 gives the git tests one fixture instead of the four helpers `git.rs` had
been using to assemble a repository by hand. `docs/TESTING.md` allows
disposable local bare repositories for the git layer and nothing else, and the
publication work ahead needs states only real repositories hold: a push another
repository made impossible, two refs that have grown apart, a fetch with
something to do about it. Building that is a decision rather than boilerplate,
for four reasons.

**A commit's hash is a function of the machine.** A commit object records its
author, its committer and two instants, and hashes all four. With the identity
and the clocks left to the machine, two fixtures built seconds apart in the same
temporary root produce different seed commits — and this machine's global
identity is `Claude`, so a test that inherited it would pass here and mean
nothing anywhere else. That matters because VISION.md §10 ends publication by
comparing a SHA read back out of a repository against the one the run made: a
hash a test cannot write down is a hash it cannot compare.

**A developer's machine reaches into a test that cannot push back.** A globally
enabled `commit.gpgsign` turns every fixture commit into a failure or a wait for
a passphrase nobody is typing; a globally configured `core.hooksPath` runs
whoever set this machine up inside the test process, on every commit and every
push. Neither shows up in the assertion that eventually fails.

**A fixture written into the working tree is a policy violation, not a mess.**
VISION.md §10 makes a dirty tree at verification time a policy failure that
fails the task that made it and every task after it. A scratch repository left
anywhere under the checkout is exactly how a later task gets failed.

**Two fixtures have to stay two.** These tests run in the same binary, in
parallel, over five hundred of them. A shared scratch path, a shared object
store, or a branch name two tests both want turns two unrelated tests into one
test that fails depending on the other.

## Decision

`crates/ktask-core/src/testing.rs` holds the one fixture, `scratch_repo() ->
Result<ScratchRepo>`, and it is a real `git` doing the work:

- Everything is built inside `tempfile::TempDir::new()` — the system temporary
  directory, never inside this repository, asserted by a test that canonicalizes
  both paths. Dropping `ScratchRepo` deletes the working repository, the origin
  and every second repository the helpers created, which a test checks by name
  and then by absence. There is no cleanup step to forget, and a panic leaks
  nothing.
- Both repositories are created with `-b main` asked by name at `init`, the bare
  one included. Measured on git 2.55: `git init --bare` without it leaves the
  origin's `HEAD` at `refs/heads/master`, and a clone of an origin whose `HEAD`
  names a branch that was never pushed has nothing to check out.
- Every call carries `-c user.name=ktask-test` and
  `-c user.email=ktask-test@example.invalid` on its own command line and runs
  with `GIT_AUTHOR_DATE` and `GIT_COMMITTER_DATE` both pinned to
  `2000-01-01T00:00:00+00:00`. The seed commit is therefore one object,
  `acddeb15fa749af9157d88349966f4f4cda4b354`, and the suite asserts it as a
  constant.
- Every repository gets two local configuration entries, `commit.gpgsign=false`
  and a `core.hooksPath` that points at a path which is never created, so a
  global signing key and a global hook path both find nothing.
- The helpers are `commit`, `branch`, `push`, `unfetched_repo` and `diverge`.
  `diverge` moves the origin from a *clone* of it, so a refusal a later test
  asserts about is a refusal `git` gave to a real push. Each divergence's
  message carries the divergence's number: without it the second call's peer
  clone starts from an origin that already holds the same bytes at the same
  path, and `git commit` answers "nothing to commit" on standard *output*, which
  reaches the caller as an `Error::Git` with an empty `stderr`.
- The module is `#[cfg(any(test, feature = "testing"))]` and the crate declares
  `testing = ["dep:tempfile"]`.

## Alternatives considered

- **The per-module helpers this replaced.** They worked, and each new test crate
  would have written them again — with no way to state the one thing the fixture
  is for, that a commit made here and a commit made by a publication test are
  the same object.
- **`tempfile` as a `[dev-dependencies]` entry only**, which is what the task
  body named. A module gated behind a *feature* cannot see a dev-dependency, so
  `--features testing` would not compile: `tempfile` is an optional normal
  dependency that the feature activates, and the dev-dependency stays for the
  suites that call `tempdir()` directly. Verified with
  `cargo tree -p ktask-core -e normal`: a plain build, and so every shipped
  binary, does not compile `tempfile` at all.
- **`std::env::set_var` for the two clocks.** Process-global, so the first test
  dates every later test in the same binary; `docs/TESTING.md` forbids it and
  ADR-0040 already chose per-call arguments over process state. `git_env` —
  `git` with a handful of variables handed to one child — is the seam both this
  fixture and that rule need.
- **Hand-written object files and ref files, or a seeded `--template`.** Faster
  by milliseconds, and it tests the fixture: the six wrappers' answers have to
  come from a repository `git` itself wrote, or a green suite says nothing about
  a real one.
- **A `ktask-testing` crate of its own.** The workspace has three crates and one
  fixture; a feature costs a manifest entry, a crate costs a version, a publish
  surface and a dependency edge. Revisit when a fixture needs to be a *program*
  — a fake provider or a stub editor cannot live behind a library feature.

## Consequences

- Publication tests can name a commit instead of describing one, and a fixture
  can create the failures publication has to survive — a refused push, a
  divergence, a remote-tracking ref that does not exist yet — without a network
  or a second machine.
- The fixture is now load-bearing for determinism, so determinism is asserted:
  changing the seed's message, its file name, the pinned instant or the identity
  changes every hash and fails the named test rather than going unnoticed. The
  price is that a deliberate change needs `SEED_SHA` re-derived by hand.
- Anything outside this crate that wants the fixture enables `ktask-core/testing`
  and gets `ktask_core::testing`. Nothing does yet; that wiring belongs to the
  first task that needs it.
- The fixture is measured like production code — 98% of its lines, and
  `scripts/review-tests.sh` runs over its lines too, because a fixture whose
  pinning silently broke buys exactly the false confidence it was written to
  prevent.
- Two states are deliberately absent: a remote that accepts a force-push, and a
  repository with working hooks. A task that needs either is asking a question
  about force or hook semantics and should decide that in its own ADR rather
  than by extending a helper on the side.
