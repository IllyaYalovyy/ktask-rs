# Testing

## Layers

- **Pure logic** (state machine, classifier, journal projection): exhaustive
  unit tests plus property tests. Materialized state must always equal the
  journal projection — that is the invariant the whole design rests on.
- **Git layer**: tested against disposable local bare repositories, including
  conflict, rejected-push, and drift scenarios. Never against a real remote.
- **End-to-end**: driven by the deterministic `dummy` provider. Assertions on
  final states, journal contents, exit codes, and git results.
- **TUI**: see below. Headless, always.

## TUI testing

The terminal interface is tested as thoroughly as everything else. Structure
it so that this is possible: decision logic pure, terminal I/O in a thin shell.

Required coverage:

- every screen and every reachable UI state;
- every keybinding and every screen-to-screen transition;
- resize behavior, including degenerate sizes, without panic or corruption;
- arbitrary event sequences never panic and never hang.

All of it runs headlessly from `scripts/quality.sh`. If a behavior can only be
checked by eye, restructure until it can be checked by a test.

## Test quality

Suite quality is measured by mutation testing (`cargo mutants`). A surviving
mutant means the tests execute code without constraining it. Prefer few sharp
assertions over many broad ones.
