# 0059. A failure is classified by an ordered chain that reads only its own evidence

- **Status:** accepted
- **Date:** 2026-09-19

## Context

VISION.md §7 fixes nine failure classes and the response each one earns, and
T064 is the task that turns an observed failure into one of those names. The
enum already existed (ADR-0010, ADR-0011); what did not exist was the reading
that reaches it, and four things made that reading a decision rather than a
match statement.

- **The same text is evidence for two classes at once.** A session that exits
  non-zero has refused, and its prose can name a limit, a disconnected stream
  *and* a decision it is waiting on in one paragraph. An ordered chain resolves
  that, so the order is the policy — and VISION.md §7's recovery rules are what
  fix it: a limit is waited out to the minute, a configuration mistake pauses
  for a human immediately, and only an agent failure earns a remediation. Read
  one arm off, a run either spends a bounded remediation on what was a wait, or
  loops a configuration mistake until the circuit breaker trips.
- **The gates and the session disagree by construction.** A session that
  printed `KTASK_RESULT: DONE` and exited 0 can still face a red verify gate; a
  session that died mid-sentence can face a gate that never ran. So *who is
  allowed to answer* differs per arm, and a reading that let any text reach any
  class would be argued about by whichever of them spoke most recently.
- **The signature has one error slot.** T064 fixes
  `classify(outcome, gates, git_error)`, and ADR-0054 and ADR-0057 both recorded
  the consequence: a provider that is killed by a signal answers with
  `Error::Provider` and no `Outcome` at all, so a caller that has both a refusal
  and a session report is a caller under the `tdd` protocol's several sessions,
  and a caller that has one is missing the other.
- **An unrecognised fault still has to land somewhere.** The last arm is
  `AgentFailure`, and VISION.md §7's response to it is a remediation session:
  expensive, bounded, and the wrong answer for a provider that refused in words
  no table knows.

## Decision

**One pure function, ten arms, applied in the order T064 names.** Provider
configuration, provider limit, provider transient, git conflict, policy,
verification, needs input, environment, an unrecognised provider fault, and only
then agent failure. Each arm is one named predicate over one piece of evidence,
so the chain reads as the policy it is.

**A session's prose is provider evidence only when it did not claim success.**
`provider_text` reads `Outcome`'s streams when `exit_code != 0` and ignores
them when it is zero. A session that exited 0 has asserted it is finished, and
VISION.md §3's invariant 4 makes the gates the answer to that assertion, so a
line mentioning `429` in a successful run cannot impersonate a provider fault.
A refusal handed in through the error slot is read whatever the session exited
with, because there the refusal *is* the fact.

**A refusal contributes only the provider's own words.** A provider refusal
contributes its `detail` and not the sentence this project wraps around it,
because `provider `claude` failed: …` is ours: a table that matched it would
recognise a refusal from the wrapper rather than from anything the provider said,
and the variant check would be left as the only thing deciding the class while no
test could tell that check from a phrase. A configuration refusal contributes no
text at all, because its variant decides in the first arm — words that cannot
change an answer are not evidence, they are a second path to one class, and a
classifier with two paths per class cannot be tested arm by arm.

**Needs input is the exception, and it is anchored.** `KTASK_RESULT: NEEDS_INPUT`
is read from the session text whatever the exit code, because it is a claim
about what the agent is waiting for rather than a claim that it succeeded — and
it is matched only at the start of a line. A session quoting its own prompt, or
describing this very rule, is prose about the protocol rather than a use of it;
an unanchored match would pause the queue on every echo of the template.

**Gate text reaches only the two arms a gate is evidence for.** Policy reads
what a refusing gate printed (`forbidden`, `outside the write scope`, a bypassed
gate), and environment reads a gate that never reached a verdict: 126, 127, or
a signal nothing here sent. A timeout *is* a verdict, because the budget that
was exceeded is the one this project configured. Verification therefore means
"a check ran and refused", which is why a 126 is not one: nothing was compiled
and no test ran, so no check failed.

**A `git` that ran and refused is a conflict; a `git` that could not be started
is the machine.** `git.rs` deliberately funnels every git refusal into
`Error::Git` (ADR-0040) because §7 keys `git_conflict` on a git error, so the
variant carries both halves of that sentence and the arm separates them on the
one phrase `git.rs` writes for a missing binary.

**The error slot is matched on its variant, never on the caller's intent.**
`Error::Config` is a configuration failure, `Error::Policy` a policy failure,
`Error::Git` a conflict, `Error::Provider` a provider fault. An error the
taxonomy names no class for — `Io`, `Database`, `Serde`, `Corrupt`, `NotFound`,
`InvalidTransition` — is an environment failure, because the fallback would
otherwise let an agent take the blame for its supervisor.

**The fallback is explicit and netted.** Before it, an `Error::Provider` is a
provider transient whatever its words, and session prose that names a provider
noun beside a fault word is one too. The net is narrow on purpose: a table that
matched any complaint would absorb every agent failure into a retry, which is
the same budget spent the other way round.

**The phrases are tables compiled once, per process.** Each is a `static` behind
a `OnceLock` (the shape `redact.rs` already uses), and a phrase that does not
compile is skipped rather than panicking — with a test insisting every table
compiled whole, so a skipped phrase is a failing build rather than a class that
quietly stopped being reachable. `regex` is already in the dependency set
`docs/DESIGN.md` fixes, so this adds no dependency and no lock-file change.

## Alternatives considered

- **One pass over a score, or a specificity ranking, instead of a fixed order.**
  It loses because the order *is* the reviewed policy: VISION.md §7 states a
  response per class, and a ranking would let an unreviewed interaction between
  two phrases pick which response a run takes.
- **Classifying from the exit code.** `Outcome::exit_code` is documented as
  evidence a session ran and nothing more; VISION.md §3's invariant 4 exists
  precisely because an agent's own verdict is not the supervisor's.
- **Reading gate output for provider classes.** A gate is a command this project
  wrote; its output is evidence about the repository. Letting it argue about
  providers would let a `curl` in a gate blame a provider.
- **Mapping `Error::Git`'s "could not be started" to `GitConflict` too.** It is
  a git error, and that is the whole objection: no repository decision was made,
  and `GitConflict` has a mechanical answer that a host without `git` will never
  get.
- **Deferring `limit_message` to T065, which owns usage-limit detection.** It
  cannot be deferred: `classify` is required to call it, and a limit has to be
  recognised before the transient arm can read a `429` as a general fault. What
  T065 owns and this task deliberately left small is the wording of the two
  real CLIs and the `limit_patterns` configuration key that will feed
  `patterns`, which `Config` does not have yet.

## Consequences

- A failure is decided by evidence that is allowed to speak, which makes the
  classes testable against realistic inputs rather than against a table of
  phrases: the tests are the review of this ordering.
- The gap ADR-0054 and ADR-0057 recorded stays open and is now narrower: the
  error slot carries a provider refusal *if the caller has one to hand*, so a
  session killed by a signal is classified correctly only when its caller passes
  the refusal it received. A caller with an `Outcome` alone gets a class from the
  session's prose, which is why that prose is read at all.
- Prose tables are a maintenance surface. Every phrase here came from a real
  refusal — ours, or a CLI's documented one — and a new CLI wording is a
  configuration entry (`limit_patterns`) rather than a release, for limits, and a
  pull request otherwise.
- Coverage of the arms is line coverage of one function; what the classes are
  *worth* is proved by the mutation record in T064's report, where each arm
  removed or reordered is caught by the test that names the order.
