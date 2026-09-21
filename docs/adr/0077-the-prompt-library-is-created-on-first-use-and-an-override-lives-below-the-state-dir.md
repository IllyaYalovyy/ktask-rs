# 0077. The prompt library is created on first use, and an override lives below the state directory

- **Status:** accepted
- **Date:** 2026-09-21

## Context

ADR-0075 made `assemble` a pure projection that reads no clock, no environment and
no path. That purity has a cost: the two standing halves of a prompt — the context
document and the template — are read by something else, and T082 is that something.
It asks for three functions: `prompt_library()` for where they live,
`ensure_defaults()` for the machine that has none, and `load_template(project)` for
the choice between a project's own words and the global ones.

**A missing template is a queue that will not start.** VISION.md §6 makes the
context document from the private prompt library the standing half of every prompt,
and a template the part the task is filled into. On a machine that has never been
configured, the first task of the first project would otherwise fail on an absent
file before an agent had been asked for anything — a failure that reads as the tool
being broken rather than as the operator having one edit left to make. The task's
done-when is explicit: a first run creates the defaults rather than failing.

**Where an override lives decides the invariant.** §11 puts prompts and templates
in a global, private library and says per-project overrides "also live outside the
repo"; §3's invariant 6 is the rule that makes that matter. `Project` carries
`root`, `id` and `state_dir`, the first of which *is* the repository. Until a path
is chosen, "outside" is not testable, and an invariant that is not testable is one
an agent breaks by accident.

**A rule that is only written down is not a rule.** The likeliest accident is a
symbolic link: `ln -s ~/work/proj/.ktask/prompt.md ~/.local/state/ktask-rs/<id>/prompts/task.md`
is one command, after which a template is read from inside a working copy while the
privacy scan at push time sees nothing, because nothing was written. Testing where a
link points is not decidable from here — targets move, links chain, and a target
that is outside now can be inside by the time it is opened.

**Two runs, one machine, and somebody's configuration.** `ensure_defaults` on a
library that already holds an operator's own template must not write on it: the
reason a prompt library exists is to hold the prompt somebody wrote. And while §3's
invariant 5 keeps one task active at a time in the runner, `doctor`, the TUI and a
CLI status call can each ask for a prompt independently of that lock.

**Permissions are asked for, not assumed.** §11 asks for restrictive permissions
rather than for a considerate umask, and `crate::project` already makes a state
directory `0700` by setting the mode rather than by trusting the environment. A
prompt library holds the prompts of every project one machine supervises.

## Decision

**The library is `$XDG_CONFIG_HOME/ktask-rs/prompts`, a sibling of the
configuration document.** `paths` now builds both below one `config_root_with`, so
the `$HOME/.config` fallback and the empty-value-is-unset rule are written once
rather than once per file. Like `state_root` and `config_file`, `prompt_library`
resolves a location and touches nothing: `doctor` and the configuration screen say
where the library *would* be before anything decides to write in it.

**`ensure_defaults` creates what is missing and no more.** Each of `task.md` and
`context.md` is written only when its path is absent: the path is looked at first,
so an existing document keeps its bytes *and* the mode somebody gave it and is not
even reopened, and creation is `create_new` with mode `0600`, so two writers agree
without either taking a lock and the loser of the race finds its own bytes already
in place — safe only because both would have written the same default. The library
directory is `0700`, set rather than only requested at creation (the same reason
`project` sets it), and re-asserted on every pass, because a grant that was never
asked for cannot be removed by asking for it. Each written document is `sync_all`-ed
before the call returns: a file created but left in the page cache comes back empty,
and the never-overwrite rule above would then keep an empty template forever.

**The default template names `{{TASK}}`; the default context document names
nothing.** A template without the placeholder hands a provider a prompt that asks
for nothing. A context document with a hole in it is a template that got swapped
in, so a test asserts the two defaults are different documents of which exactly one
carries the placeholder. The template's text states the two rules that hold on every
task this tool runs — scope, and that completion is mechanical — because on a fresh
machine it is the first prompt sent and the only one its author can be sure was
read.

**`load_template` prefers `<state_dir>/prompts/task.md`.** The override carries the
library's own directory name on purpose, so the whole workflow is spelled one way:
copy `prompts/task.md` out of the library, edit the copy, put it in the project's
`prompts/`. `Project::root` is never consulted, and a test proves it by filling a
scratch working copy with a `task.md`, a `prompts/task.md` and a `.ktask/prompt.md`
and asserting the bytes came back from the library while the repository stayed
untouched.

**The library is ensured before it is read.** `load_template` runs the body of
`ensure_defaults` rather than opening a file and handling its absence, so a first run
returns a template instead of a missing-file error, and the two documents an operator
was meant to find are the two that are there. Both entry points take the environment
through an accessor, per `docs/DESIGN.md` Conventions, so no test writes into a real
`$XDG_CONFIG_HOME`.

**A symbolic link is refused rather than followed, in both directions** — at the
library directory, a library document, the override directory and the override file.
`Error::Policy` names the path and says to copy it. Writing through a link would file
a run's default wherever it points; reading through one is how a private-library
template turns out to be a file inside a supervised repository. Refusing the link
removes the affordance instead of policing one of its outcomes.

**Not having an override is the ordinary answer, not an error.** A project with no
state directory yet gets the library default, and reading a prompt does not create
that directory: a run is not entitled to register a project merely by reading a
prompt. Anything that *is* there in the wrong shape — a directory where a document
belongs, a file where a directory belongs — is refused by name, because an operator
who wrote an override and pointed it at the wrong thing deserves telling rather than
being silently sent the default.

**The text comes back as the file holds it**, untrimmed and undecorated. Whether it
wants a task in it is `assemble`'s question, asked identically of an operator's own
template and of the default (ADR-0075 appends the task under a heading when the
placeholder is missing). A document that is not UTF-8 is `Error::Corrupt` naming the
path, since a prompt with mangled bytes in it is not a prompt.

## Alternatives considered

- **Keep template and context in the repository, as this project's own
  `.ktask/prompt.md` does.** §11 and §3's invariant 6 place both the library default
  and the per-project override outside, and grant exactly one exception to that
  rule: ADRs. This repository's committed `.ktask/` is the supervisor dogfooding
  itself and is reported as an observation, not made precedent for what
  `load_template` reads — and decisively, an agent working a task can edit the
  template that governs it, which ends the argument on its own.
- **`std::env::set_var` in the test that wants the public wrapper.** Three lines
  against twenty, and refused by the convention that keeps the suite deterministic:
  the variable would outlive the test for every other test in the process.
- **`canonicalize` the override and refuse it when it resolves inside
  `project.root`.** The obvious mechanical form of "never from the repository", and
  refused on three counts: it polices only the one repository in scope, so a link
  into a *second* supervised one passes; the answer can change between the check and
  the read; and it leaves links allowed, which is the affordance the whole class of
  accident needs.
- **Refuse an empty or placeholder-less template at read time.** A default cannot be
  empty — this module writes it and syncs it — and a read-time policy depends on who
  reads, where the write-time rule already guarantees the property for everyone.
- **Add `load_context(project)` for symmetry.** Nothing calls it yet: the caller that
  owns a state directory reads `prompt_library()/context.md` itself. Adding it now
  is API nobody holds.
- **Read the library inside `assemble`.** Fewer arguments, and it deletes the purity
  ADR-0075 exists to protect along with the ability to reproduce the exact words an
  attempt was given.
- **Write the defaults only from an `init` command.** That moves the failure from the
  first task to the first task after `init` was forgotten, and the done-when says a
  first run creates them.

## Consequences

- A fresh machine sends a real, complete prompt on its first task, and its operator
  starts from two files worth editing rather than from an error message.
- Reading a template can create directories. That is deliberate — it is what makes
  the read path total — and it is cheap (two lookups and a permission set once the
  library exists), so the library is never in the state "installed, directory
  absent".
- An operator who wants to link their own editor-managed template into the library
  is refused, with a message that says to copy. If that becomes a real complaint the
  answer is an explicit allowlist of link targets, not the removal of this rule.
- The context document still has no loader of its own, and nothing resolves
  `prompt_library()` for the runner yet: the task that starts a provider owns both —
  it passes the context document to `assemble` and resolves ADR-0075's
  `<state_dir>` marker.
- `prompt_library` is public and creates nothing, so `doctor` can name the path
  before any write happens, and the `prompts` name is one constant rather than a
  string in two modules.
