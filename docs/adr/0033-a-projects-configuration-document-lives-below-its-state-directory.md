# 0033. A project's configuration document lives below its state directory

- **Status:** accepted
- **Date:** 2026-09-18

## Context

`config::load` (ADR-0005) merges four layers and takes both document paths as
arguments, which is what makes every layer testable. Nothing resolved them, so
nothing in the running tool could actually read a configuration: the layers
existed in the type and nowhere else.

Three things had to be decided to change that, and none of them is obvious from
`docs/DESIGN.md`.

**Where a project's document is.** The Paths section names one file —
`$XDG_CONFIG_HOME/ktask-rs/config.toml` — and the loader's layer list has always
had a project document in it, unnamed. The candidate locations are the working
copy, the machine's configuration directory, and the project's state directory,
and they are not equivalent: one of them is forbidden outright.

**Who names it.** `crate::journal::journal_path` names `<state_dir>/journal.db`,
so a filename already has a precedent of being owned by the module that reads
the file, while `crate::paths` holds the filename of the global document. Two
owners of "what a project's directory contains" is one too many.

**How the environment reaches the global path.** The global path is a function of
`XDG_CONFIG_HOME`/`HOME`, and `docs/DESIGN.md` Conventions forbids a test setting
an environment variable, so an entry point that resolves that path cannot itself
be the one a test calls.

## Decision

**`<state_dir>/config.toml`.** `crate::project::project_config_path` returns it,
and `crate::config::load_for` resolves the three layers above the defaults from
it: the machine's document from `crate::paths`, the project's own from this
function, and the process environment. `load` is unchanged and still takes its
paths as arguments.

**`project.rs` names the location, `paths.rs` names the filename.**
`CONFIG_NAME` became `pub(crate)` and both documents use it, so the two files an
operator can edit are spelled identically; `project.rs` joined it to the state
directory it already owns. `config_file_with` became `pub(crate)` for the same
reason `state_root_with` already is — one recipe for where a file goes, one
place to inject an environment while looking for it.

**The public entry point is the ambient one.** `load_for(project)` reads the
process environment through `paths::process_env`; a module-private
`load_for_with(env, project)` does the work, and the tests call it. This is the
shape `register`/`register_with` and `state_root`/`state_root_with` already have.

## Alternatives considered

- **`<repo>/.ktask/config.toml`.** Rejected outright: VISION.md §11 puts no
  operational file of the supervisor's inside the repository it supervises, and
  the verification gates treat a dirty working tree as a policy failure — a file
  this tool reads on every run would be written into the tree it must leave
  clean.
- **Beside the global document, one file per project id.** Rejected: the reason a
  project layer exists at all is that a repository's settings are about the work,
  and a document filed under a machine's home directory is the machine's. It also
  invents a second root the state directory already is.
- **`load_for(project, env)` as the public signature.** Rejected: every other
  entry point in `paths` and `project` is ambient with a `*_with` seam beside it,
  and a caller that wanted to inject an environment would be handed a second
  `process_env` to write. The task's surface is `load_for(project: &Project)`.
- **`project_config_path` in `config.rs`, beside `journal_path`'s analogue.**
  Rejected: `project.rs` is the module whose doc comment says it is about where a
  project's state lives, and `config.rs` reaching into a project's layout would
  make two modules answer one question.
- **Resolving the paths inside `load`.** Rejected: `load` would need the process
  environment to read a document, which is exactly what makes ADR-0005's layer
  tests possible.

## Consequences

- A project's settings are derived state now, exactly like its journal: they live
  under the identity `crate::project_id` computed, so a repository moved to a new
  canonical path gets a new state directory and leaves the old one behind. That
  is not new here — it is how the journal already behaves — but configuration
  inherits it too, and a future `ktask-rs init` migration has to move both.
- Nothing in this task writes the document. There is no command that produces a
  project configuration yet; `load_for` only reads one if an operator wrote it by
  hand, and refuses by name if it is there and unreadable or malformed.
- The CLI still has no flag layer to hand `load` (ADR-0005 left `Source::Flag`
  unproduced). Wiring `--project`, `--json` and the flag layer onto `load_for` is
  the next task's surface, not this module's.
- `docs/DESIGN.md` Paths section still lists only the global file. The location
  this ADR chooses is recorded here rather than by editing the design document
  from an implementation task.
