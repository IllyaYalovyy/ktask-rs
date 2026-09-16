# Process

## Definition of done

A task is done when, and only when: the work described is complete, every gate
in `scripts/quality.sh` passes, the change is committed, and the report says
`KTASK_RESULT: DONE`. An agent asserting completion is not evidence of it;
the gates are.

## Commits

One logical change per commit, green at every commit. Subject line in the
imperative, under 72 characters, then a body explaining *why* when the reason
is not obvious from the diff.

## ADRs

`docs/adr/NNNN-short-title.md`, copied from `0000-template.md`. Write one when
choosing between real alternatives: a dependency, a data format, a testing
approach, a concurrency model. ADRs are the only operational documents that
belong in this repository, and they are append-only — supersede, never edit.

## Scope

Do what the task says. If you discover work that needs doing but was not
asked for, note it in your report rather than doing it.
