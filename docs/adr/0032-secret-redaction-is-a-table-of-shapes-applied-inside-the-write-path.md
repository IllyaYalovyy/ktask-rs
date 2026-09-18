# 0032. Secret redaction is a table of shapes, applied inside the write path

- **Status:** accepted
- **Date:** 2026-09-18

## Context

VISION.md §11 asks that tokens, credentials and configured patterns be redacted
from what a run leaves behind. T035 states the outcome harder — secrets cannot
reach disk — which turns it from a formatting nicety into a property of the two
durable files a run writes. Four constraints made it a decision rather than a
function:

- **The payload is where an agent's own words land.** `AgentOutput.text` and
  `PreflightFailed.detail` carry whatever a subprocess printed, including the
  `Authorization:` header it was told to send and the remote URL it was cloned
  from. Redaction cannot be a courtesy a caller remembers: the one call site
  that forgot would be the leak.
- **The journal already promises byte-identity.** `journal.rs` documents that the
  payload column is what the encoder produced, and
  `every_catalog_entry_appends_under_its_own_name_and_carries_its_own_object`
  asserts `row.payload == serde_json::to_string(kind)`. Whatever redaction is, a
  record holding no secret has to come back out of it unchanged.
- **`redact(text, extra) -> String` has no error channel**, and the task names
  that signature. A configured pattern that is not a regular expression cannot be
  reported by the call that would have used it.
- **`secret_patterns` already exists** as a configuration key (`config.rs`, with
  `KTASK_SECRET_PATTERNS`) that nothing honoured, and `regex = "1.13"` is already
  in the fixed dependency set of `docs/DESIGN.md` — unused by any crate until
  now. `docs/PROCESS.md` makes adopting a dependency a design decision.

One property of the `regex` crate shaped the rules: deliberately, it has no
look-around and no backreferences (its guarantee is linear time). Anything that
looks like "a value, but not if it starts with `[`" has to be written as a class
rather than as an assertion.

## Decision

1. **`ktask-core` depends on `regex`**, `workspace = true`, the `1.13` pin
   `docs/DESIGN.md` names, with `Cargo.lock` in the same commit.
2. **Redaction is a table of shapes**, not an entropy detector: nine built-in
   rules (private-key block, truncated key header, provider prefixes, JWT,
   authorization header, bare scheme, URL password, URL username, secret-named
   assignment) applied in a stated order, then every configured pattern. The mask
   is the whole value — no length, no last four characters. Over-redaction is the
   accepted failure direction.
3. **It is applied inside the write path**, at step 1 of
   `Journal::append_encoded`: encode, redact, stamp, then open the transaction. A
   redaction that cannot answer with a record writes no row and spends no
   sequence number, exactly as an encoder's refusal already did.
4. **Patterns are scoped to the contents of a JSON record's string literals.**
   The structure is copied verbatim, each literal is decoded, redacted and
   re-encoded only if redaction changed it, and the finished record is parsed once
   before it is handed back. Structure is therefore out of any pattern's reach.
5. **The mask is a fixed point of the table.** The two rules that keep a label
   will not match a value whose first character is the mask's `[`, and match a
   bracketed value whole instead; a second pass over redacted text rewrites
   nothing.
6. **A configured pattern is refused where it is attached**, not where it would
   have been used: `Journal::with_secret_patterns` calls `redact::check_patterns`
   and answers `Error::Config` naming `secret_patterns`.
7. **A masked value keeps its label.** `Authorization: Bearer [redacted]` says
   which credential leaked; `[redacted]` alone says only that something is hidden.

## Alternatives considered

- **Parse to `serde_json::Value`, redact every string, re-serialize.** Loses
  byte-identity: `Value` without `preserve_order` re-serialises maps in key order,
  which is precisely what the catalog test compares. It also makes redaction a
  walk over decoded values rather than a table over text, so a shape spanning two
  fields could not be matched at all.
- **Key-name-aware redaction** (mask any value whose key reads like a secret).
  Deliberately not done. A payload is a tagged `EventKind` this crate controls, no
  field of it is named like a secret, and `secret_patterns` is the operator's
  answer to a shape the table cannot see. Recorded as the boundary, below.
- **An entropy scanner.** It would mask the base shas, commit shas and gate
  counters that are the reason a journal is worth reading. A run whose evidence is
  masked into noise proves nothing, which is the failure VISION.md §2 exists to
  prevent.
- **`fancy_regex`/`onig`** for look-around, to make a mask unmatchable directly.
  Not in the fixed dependency set; decision 5 achieves the same property with a
  character class.
- **Skipping an uncompilable configured pattern**, since `redact` cannot report
  one. That is a leak that looks like coverage. The refusal moved to where the
  patterns are handed over instead.
- **Redacting in the recorder or the frontends before appending.** Every write
  path would need its own copy, including the ones not written yet. The journal is
  one door, so the check is made once, at the door.

## Consequences

- Every `EventKind` is covered by the journal's append, including kinds that do
  not exist yet, and no caller can leave an unredacted row behind by forgetting.
- A record that holds no secret reaches the file byte-for-byte as it was encoded,
  so the byte-identity test still measures the encoder rather than the redactor.
- A secret in a shape the table does not know still leaks until someone adds a
  pattern. `secret_patterns` exists so that fix needs a config edit and not a
  release, and `check_patterns` means the fix cannot be silently mistyped.
- **Not redacted, deliberately:** the free-text writers that are not the journal —
  `Journal::put_tasks` (`tasks.body`) and `put_state` (`state_json`). They carry
  imported plan text and projected state rather than agent output today; whoever
  puts agent-authored text into one of them inherits this ADR.
- The log file is the other half of "cannot reach disk" and is not written yet
  (T086 owns `<state_dir>/logs/run-<date>.jsonl`). It must pass every record
  through `redact::redact`; until it does, the outcome holds for the journal only.
  The two round-trip tests in `redact.rs` are written against a log file for
  exactly that reason.
- Every planted credential in the tests is *generated*. The `fixtures` module
  joins an issuer's real prefix to a body of the issuer's real length and
  alphabet, drawn from a fixed seed, so no commit of this repository holds a
  credential — whole, or in two adjacent pieces that a scanner rejoins. Push
  protection cannot tell a planted value from an issued one and refuses the
  commit either way, which is this module's own behaviour pointed back at its
  test suite; splitting a value in half is not enough, because the halves sit
  next to each other in the file and are rejoined. The prefixes stay written
  because a prefix is what a rule matches and is not a credential. The seeds are
  fixed, so every run plants the same value and the assertions are the ones the
  table started with; a generated body that stopped matching its rule would fail
  `every_built_in_secret_shape_is_redacted_to_the_mask` rather than pass quietly,
  which is the guard that makes generation safe here.
- If a future field must survive redaction exactly — a sha a runner compares
  against — it must not be shaped like a secret. Over-redaction is accepted, so
  the safe move is a field whose shape is hex and short, and a state-carrying
  value that was redacted must be applied as the journal wrote it rather than as
  it was meant.
