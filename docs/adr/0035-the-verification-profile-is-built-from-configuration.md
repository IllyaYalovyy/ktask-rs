# 0035. The verification profile is built from configuration

- **Status:** accepted
- **Date:** 2026-09-18

## Context

T039's outcome is that *the gates the runner executes come from configuration*:
eight `Option<Vec<String>>` settings on `Config` and
`profile_from(&Config) -> Result<Profile>`. VISION.md §8 names the eight keys
and says a profile is "defined per project"; it does not say how four things
this task has to settle are settled.

1. **A `Config` field without a documented key is a contradiction.** `Config`
   rejects unknown keys twice over — `deny_unknown_fields` and the `KEYS` table
   that `load` consults and `Config::provenance` renders — and its module
   document states the rule: one field per key in the *Configuration defaults*
   section of `docs/DESIGN.md`, "and nothing else". Adding eight fields without
   writing the keys there makes that sentence false. VISION.md §8 already names
   all eight, so writing them down records what the vision says rather than
   inventing a setting. 63f3f7f added the review gate's two keys the same way,
   for the same reason.
2. **Which timeout a built gate gets.** VISION.md §8 wants "each gate has its
   own timeout", and `Gate` has a `timeout_secs` field for exactly that, but the
   configuration documents one gate budget (`gate_timeout_secs`). Per-gate
   budgets would be eight settings the task does not name.
3. **A key that is set with nothing in it.** `verify_command = []`, or
   `KTASK_LINT_COMMAND=,`, is a gate somebody configured with no words to run.
4. **The order a built profile runs its gates in.** `Profile::gates` is ordered
   and T038 pinned that order as the execution order; a `Config` cannot express
   an order, because it is a map.

`Profile::validate` already refuses a profile with no `Verify` gate, so the
mandatory-gate rule existed before this task and needed a second door, not a
replacement.

## Decision

**The eight keys are documented settings.** They appear in `docs/DESIGN.md`
*Configuration defaults* (each defaulting to `None`, in VISION.md §8's order),
get one `KEYS` entry each with a `KTASK_*` variable, and therefore load through
the same four layers and report provenance like every other setting. In the
environment a gate command is one word per comma-separated entry, the same
spelling `test_globs` and `secret_patterns` use, so
`KTASK_BUILD_COMMAND=cargo,build` is `build_command = ["cargo", "build"]`.

**`profile_from` gives every gate it builds `config.gate_timeout_secs`.** One
documented budget, every gate. `Gate::timeout_secs` stays the place a per-gate
budget lives — a profile read from a document already carries one per gate — so
a later task that adds per-gate settings changes this mapping and nothing else.

**A configured gate with no command words is refused**, with
`Error::Config` keyed by the setting's own name. A gate with nothing to execute
is not a gate, and reading a key that *was* written as though it had not been is
the silent-ignore failure `config.rs` argues against everywhere else. A variable
that is empty or blank is not this case: `variable` treats it as unset, as it
does for every key here.

**The order is VISION.md §8's** — baseline, targeted, verify, lint, format,
build, privacy, flake — spelled by the order of the table inside `profile_from`,
which is also the order of the eight keys in the design document.

**A missing `verify_command` refuses with `key: "verify_command"`,** not the
`key: "gates"` message `Profile::validate` gives. The actionable fact is the
setting an operator has to write. `validate` keeps its rule for profiles that
arrive from a document, so each door enforces its own entry condition rather
than sharing one neither owns: a configuration cannot produce a kind twice, so
`validate`'s other rule has nothing to add here.

## Alternatives considered

- **Default `verify_command` to something.** Rejected outright: a default verify
  command is the tool inventing a project's suite, which is the same product
  decision as letting verification be skipped. The default is `None` and the
  refusal is loud.
- **Per-gate timeout settings.** Rejected: eight settings neither VISION.md §8
  nor the task names, and `docs/DESIGN.md` fixes one gate budget. The profile
  type is already per-gate, so this is a mapping decision, not a schema one.
- **Build the profile, then call `Profile::validate`.** Rejected: the refusal
  would name the `gates` table instead of the key to write, and — because no
  configuration can produce a duplicated kind — `validate` would be unreachable
  defensive code whose removal no test could notice.
- **Read an empty command as no gate configured.** Rejected: it makes a typo in a
  project's configuration (`verify_command = []`) into "this project verifies
  nothing", which is the state the mandatory gate exists to make unreachable.
- **A `Profile::from_config` associated function.** Rejected for the name the
  task gives: `profile_from` reads as what it is, and the free function keeps
  `Profile`'s own constructors (`from_toml`, `to_toml`) about encoding.

## Consequences

- The runner has one place to ask for the gates: `profile_from`. A gate it does
  not find there is a gate nobody configured, which is a decision it must make
  loudly rather than a default to fall back on.
- A built gate sets no `working_dir` and no `env`, so it runs in the project root
  the run was given with the environment the supervisor passes down. Configuration
  says what runs; the run says where.
- `flake_runs` is not threaded into the built profile. `flake_command` says what
  the flake gate runs; repeating it `flake_runs` times belongs to whoever
  executes the gate, and is recorded as adjacent work rather than done here.
- `Config` now documents 30 keys, so the Configuration screen gains eight rows
  and a project that configures none of them loads exactly as before.
- Revisit this ADR if a task adds per-gate timeouts, environments or directories:
  the mapping in `profile_from` is the single place that changes.
