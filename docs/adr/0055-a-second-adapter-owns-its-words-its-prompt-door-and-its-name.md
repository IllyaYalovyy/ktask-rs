# A second adapter owns its words, its prompt door, and its name

- Status: accepted
- Date: 2026-09-19
- Task: T060

## Context

VISION.md §12 names two launch adapters, and T059 delivered one of them. T060
delivers the other: a `Codex` adapter sitting on the same
`provider::process::run_streaming` that `Claude` sits on, so everything a
session *does* — streaming output, the two clocks, the process group, the
capture bound, the shape of an `Outcome` — is already written and already
tested. What is left is the definition of an adapter: which words start the
process, and which door the prompt goes through. The task fixes the write set to
one new file and the done-when to "the argument vector is unit tested without
spawning; behavior differences from Claude live only in this file".

Four things made this a decision rather than a copy of T059.

- **The words are not ours to invent.** They were read off the CLI installed on
  this machine (`codex-cli 0.154.0`, `codex exec --help`): `[PROMPT]` is read
  from standard input "if not provided as an argument (or if `-` is used)";
  `--dangerously-bypass-approvals-and-sandbox` will "skip all confirmation
  prompts and execute commands without sandboxing"; `-C, --cd <DIR>` tells the
  agent "to use the specified directory as its working root";
  `--skip-git-repo-check` allows running "outside a Git repository"; and
  `-m, --model <MODEL>` names the model. The help also lists `--json` and
  `--output-schema`, which matters below.
- **ADR-0054 left this task a question.** Its consequences say: "Any second
  adapter over `run_streaming` inherits the argv[0] attribution and has to
  restore its own name too. Better is for `run_streaming` to be handed the
  provider name; that is a change to T057's delivery, not to T059's, and T060
  should make one of the two."
- **`-C <dir>` puts the working directory in the child's own command line**,
  which Claude's argv never carried — and `run_streaming` has already started the
  child with `current_dir` set to that same directory. The value is therefore
  read by a process that has already been moved.
- **A session that runs a real CLI costs money and talks to a network**, so the
  argument vector is what a test checks, exactly as ADR-0054 decided for Claude.

## Decision

**Send the fixed words in the order the CLI reads them, with the prompt's operand
last.** `exec`, `--dangerously-bypass-approvals-and-sandbox`,
`--skip-git-repo-check`, `-C <dir>`, then `--model <id>` when a model was
configured, then `-`: six words always, eight with a model, built by a pure
`arguments(model, working_dir)` that no test spawns anything to read. The
trailing `-` goes last because an operand is what ends option parsing — placed
before a flag it becomes that flag's value, and a CLI that reads a flag's value
as its prompt runs a session on a prompt nobody wrote. `exec` goes first because
it is a subcommand, not an option: bare `codex` opens the CLI's own interactive
TUI, which is a hang manufactured by our own argv.

The model rule ADR-0054 wrote is restated here rather than shared: an id that is
empty names nothing and an id beginning with `-` is a flag to the CLI's parser,
and both are refused before a session starts, because VISION.md §12 rejects a
configured-vs-reported model mismatch rather than tolerating it. Restating it in
the adapter is deliberate — a shared helper that refuses a value is a shared
helper that knows one CLI's flag.

**Hand the directory over absolute.** `run_streaming` starts the child in
`Invocation::working_dir`, and the CLI resolves the `-C` value itself, so a
relative value would name a place that depends on which side of that `chdir` it
was read from — the directory the check saw and the directory the session works
in would not be the same directory. Made absolute, the value means one place on
either side, which is the same reason `locate` makes the program word absolute.
The installed CLI confirms the value is the CLI's to act on:
`codex exec --dangerously-bypass-approvals-and-sandbox --skip-git-repo-check -C
/nonexistent-dir-ktask-t060 -` answered `Error: No such file or directory (os
error 2)` before any model was asked anything.

**Restore the adapter's name in the adapter.** `run_streaming` attributes a
session's failure to the program word it was handed, which after `locate` is an
absolute path; `Codex::invoke` maps that error back to `codex`, keeping the
`detail` exactly as the session gave it. This is ADR-0054's cheaper of the two
options, and it is taken with the alternative stated rather than left as an
accident: handing the provider name to `run_streaming` is the better end state,
but it changes a green file this task does not own — the signature is
`run_streaming_as`'s too, and every test in T057's 2 000-line file calls through
it — to relocate six lines of naming.

**Promise model selection and nothing else.** `structured_output` and
`usage_telemetry` are `false`, and `Outcome::usage` stays `None`. The CLI can do
both — `--json`, `--output-schema`, and its own token accounting are right there
in the help — and capability detection answers what *this* adapter asks for, not
what the binary is capable of. A `true` would tell a caller a figure is
available while the record holds `None`, which is the substitution ADR-0049
refuses. Each flag flips with the argv and the parsing that earns it, in this
file.

**Keep the command-location rules in this file, duplicated, and name the
duplication.** `locate`, `candidates`, `is_executable`, `absolute` and the two
`Error::Provider` shapers now stand in both adapters, with the same rules and the
same tests (ADR-0054's `PATH` search, its skipped empty entry, its absolute-and
-unresolved result). Hoisting them is the better shape and the wrong commit:
T059's file is green and its rules are its own tested delivery, this task's write
set is one file, and VISION.md §16 ranks scope creep first.

## Alternatives considered

- **Handing the provider name to `run_streaming`.** ADR-0054's preference, and
  the right end state. It lost on the size of the diff it forces through a file
  this task does not own, not on its merits. Revisit when a third adapter exists
  or when T063's factory makes the same call for a third CLI.
- **Hoisting `locate` into `provider/mod.rs`.** Same objection, plus one of
  principle: `mod.rs` is what every adapter is reached *through*, and a `PATH`
  search for a binary is an adapter's concern about a process, not part of the
  interface above it.
- **Passing the prompt as the trailing positional word instead of `-`.** One
  `exec`'s argument-size limit and anyone running `ps` would decide how much of a
  prompt survives and who reads it; VISION.md §11 keeps prompts out of what a run
  logs.
- **Sending `-C` relative, since the child already stands there.** Rejected with
  the test that pins it: the CLI reads the value itself, so a relative word is
  resolved from the directory it was meant to name.
- **Assembling the same effect from `-s danger-full-access` plus an approval
  flag.** That is a claim about which of this CLI's flags routes approval prompts
  — a version-coupled guess — where the CLI documents one word for exactly the
  wanted effect, and the task names that word.
- **Reporting `structured_output: true` because the CLI supports `--json`.**
  Rejected for the reason ADR-0054 gave: a capability nobody asked for is not a
  capability detected.

## Consequences

- Codex is reachable as `dyn Provider` and nothing selects it yet. The mapping
  from `Config::provider` to an adapter is T063's factory, and this file's
  refusal for a missing command is what T062's `doctor` preflight will report.
- Two files now own the same command-location rules, so a rule changed in one is
  a rule the other does not follow until they are hoisted. The tests in both
  files pin the same strings, so drift shows as a failing test rather than as a
  silent difference between two CLIs.
- An attempt run through Codex has no cost and no token figure, and says so. A
  run of such attempts is a run whose cost is unknown, not a cheap one.
- The argv is checked against the CLI's documented surface (0.154.0), never
  against a session. A CLI upgrade that renames a word is caught by a real
  preflight (T062) and by an attempt that fails — not by a unit test that would
  have had to spend money to notice.
- A third adapter from VISION.md §12's backlog (`kiro`, `opencode`, `goose`)
  pays this duplication a third time. That is the moment to hoist the location
  rules and to hand `run_streaming` the provider name, with two adapters'
  evidence rather than one.
