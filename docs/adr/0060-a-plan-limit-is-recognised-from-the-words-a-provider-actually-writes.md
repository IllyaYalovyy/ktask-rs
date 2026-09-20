# 0060. A plan limit is recognised from the words a provider actually writes

- **Status:** accepted
- **Date:** 2026-09-19

## Context

ADR-0059 built the ordered chain and deliberately left the limit table generic:
nine paraphrases that recognise a limit as a concept, with the two real CLIs'
wordings deferred to the task that owns usage-limit detection. This is that
task, and three things made the table a decision rather than a list of strings.

- **The response to a limit is the most expensive one to reach by accident.**
  VISION.md §7 waits a `provider_limit` with a known reset out to the exact
  instant, and `docs/CONTRACT.md` §1 gives it exit code 3 because it is a pause
  and not a failure. Read one arm off, the same evidence lands on
  `AgentFailure`, which spends a bounded remediation session on what was a wait
  and reports exit 1 — a scripted run sees a failed task where the queue is
  merely parked.
- **The wording belongs to the API, not the CLI, and it arrives in two shapes.**
  Claude Code's subscription refusal is prose ("you've hit your weekly limit"),
  while the same condition through the API is a JSON body whose only content is
  `"type":"rate_limit_error"`. Codex leads with OpenAI's quota sentence and its
  `insufficient_quota` error type. A table of English paraphrases recognises one
  of the three and misses the other two.
- **Two near-misses look exactly like a limit.** An overloaded server is a
  provider that will answer again if asked, and a request that overflows the
  context window is a limit with no ceiling that ever resets. Both read as
  limits to a table matched on the word.
- **An unexercised default is invisible.** `every_table_compiles` proves a
  phrase parses; a qualifier dropped from an alternation parses perfectly and
  matches nothing, and the class it was written for keeps being chosen for
  whatever else reaches the arm.

## Decision

**Twelve defaults, each one wording a provider writes rather than a paraphrase
of one.** Claude Code and the Anthropic API contribute the windowed nouns
(`session`, `daily`, `weekly`, `spend`, `plan`) beside `limit`, the HTTP 429 and
its reason phrase, `credit balance is too low`, the `rate_limit_error` type, and
`retry-after`. Codex and the OpenAI API contribute the quota sentence that leads
with its verb ("exceeded your current quota"), the `insufficient_quota` type,
and the tokens-per-minute refusal that names the limit first and the number
after it. Machine-readable error types are their own phrases because a JSON body
carries no prose to paraphrase.

**One fixture line per default, positional against the table.** The fixture list
asserts its own length against `LIMIT`'s, and the line at index `i` must be
matched by the phrase at index `i` and returned whole by `limit_message`. Adding
a default without a line beside it fails the build; narrowing a default so it no
longer recognises the release it was written for fails the build. Nothing else
in the suite can tell a phrase that matches everything from a phrase that
matches nothing.

**Each fixture is asserted against `classify` as well.** The same line, on
either stream, with a refused verify gate beside it, must land on
`FailureClass::ProviderLimit`. That is the difference between testing a phrase
and testing a class: the assertion pins the arm's *position* in the chain, so a
limit stays a pause rather than a remediation even when the run also has a red
gate to explain.

**Configured patterns are additive, never a replacement.** `patterns` of
`limit_message` joins the built-in table. An operator whose provider words a
limit in a way no release has learned is covered today by configuration, and one
bad entry in that list cannot delete the safety net the built-ins are.

**The two near-misses stay out, on purpose.** `overloaded` and a dropped stream
are read by the transient arm: there is no ceiling to wait out, and asking again
is the whole recovery. A context-window refusal ("prompt is too long") is named
nowhere, though it is a limit of a kind — it has no reset time, so
`ProviderLimit` would back off from a request that will be refused identically
forever, where the honest class is the one a shorter prompt gets out of.

## Alternatives considered

- **Matching only the machine-readable error types.** It loses twice: Claude
  Code's user-facing subscription refusal is prose with no error body at all,
  and Codex's rate-limit snapshot is a JSON *key* (`rate_limits`) rather than an
  error type.
- **Letting configuration replace the defaults.** One entry would then decide
  whether a limit is recognised at all, and an operator adding a provider-specific
  phrase would silently un-recognise every built-in one.
- **Reading reset wording ("resets at", "will reset") as evidence.** Cheap and
  tempting, and it turns every line about a reset — a `git reset` in gate
  output, a test named `resets_when_...` — into a pause. Every real limit line
  already names the ceiling and what happened to it, which is what the two verb
  phrases read.
- **Fixtures as files under `tests/`.** The task's verify command is
  `test(/classify::/)`, and keeping the line beside the phrase means the wording
  and the regex that owes it are reviewed in one screenful.

## Consequences

- A new CLI wording is a configuration entry rather than a release, and the
  built-in half of the table is reviewable as evidence: each phrase can be
  checked against the provider's own output before it is merged.
- Editing a phrase now costs a fixture edit. That friction is the mechanism, not
  a side effect.
- One gap ADR-0059 recorded stays open and is now the only one: `Config` has no
  `limit_patterns` key, so `classify` passes `&[]` as `patterns`. The wiring is
  the configuration task's, and until it lands an operator's extra phrase has no
  door.
- `limit_message` returns the line; nothing parses a reset out of it yet. That is
  T066, and until it lands every limit is waited out with bounded backoff rather
  than to the minute.
