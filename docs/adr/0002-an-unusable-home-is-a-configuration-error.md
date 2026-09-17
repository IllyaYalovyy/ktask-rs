# 0002. An unusable home directory is a configuration error

- **Status:** accepted
- **Date:** 2026-09-16

## Context

`paths::state_root` and `paths::config_file` fail when neither the XDG variable
nor `HOME` yields a base directory, and `docs/DESIGN.md` fixes the `Error`
enum: no variant is named for the environment, so the question is which of the
fixed variants means this.

The same resolution decides a second thing. `docs/DESIGN.md` says the fallback
applies "when the variable is unset", which is silent on `XDG_STATE_HOME=` —
set, empty, and exportable from a dotenv file or a CI step. Whether `""` is a
base directory decides whether supervisor state can land in a relative path.

## Decision

The failure is `Error::Config { key, detail }` with `key` set to `HOME` and the
`detail` naming both variables, so the message reads
``config error `HOME`: `XDG_STATE_HOME` is unset or empty and `HOME` is unset
or empty, so the ktask-rs location cannot be resolved``.

An empty value is treated as an absent one, which is the XDG Base Directory
Specification's own rule ("if $XDG_... is either not set or empty, a default
equal to $HOME/... should be used"). The check is applied to `HOME` as well: an
empty `HOME` is refused rather than used, because it would build
`.local/state/ktask-rs` — a path relative to whatever directory the CLI happened
to run in.

## Alternatives considered

- **`Error::Io`.** Rejected: its contract is that the filesystem refused the
  operation, and nothing was touched. Nothing here opens a file.
- **`Error::NotFound`.** Rejected: its documentation reserves it for the subject
  of the operation — "a task, a project, a run" — and the missing variable is
  not the thing being looked up, it is what the lookup needed.
- **A new `Error::Environment` variant.** Rejected: `docs/DESIGN.md` fixes the
  enum and 160 tasks are written against it.
- **Treat `""` as a set value.** Rejected: it produces a relative path, and a
  relative state directory means a journal inside the repository it supervises
  whenever the CLI ran from one — the exact placement CONTRACT.md forbids.

## Consequences

`paths.rs` reports a config-key failure for what is, literally, an environment
lookup. That is the intended reading: XDG variables *are* configuration, and
`config.rs` treats the environment as one of its layered sources, so a caller
formatting a configuration problem already has the right case.

Neither entry point validates that a set variable is absolute, which the
specification also requires. Resolution stays a pure lookup; a `doctor` check
that reports a relative base directory is where that belongs, because refusing
to start on a malformed variable and telling the operator about it are two
different jobs. If that check is never written, a relative `$XDG_STATE_HOME`
puts state somewhere surprising and stays silent about it.
