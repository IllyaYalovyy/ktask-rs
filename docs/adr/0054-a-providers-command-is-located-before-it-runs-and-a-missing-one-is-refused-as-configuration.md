# A provider's command is located before it runs, and a missing one is refused as configuration

- Status: accepted
- Date: 2026-09-19
- Task: T059

## Context

VISION.md §12 puts a Claude CLI behind an adapter, and T059 is the first of the
two launch adapters that reaches a real binary: `dummy` replays a scripted
scenario and starts nothing, so everything said so far about how a provider is
started came from `provider::process`, which is deliberately indifferent to which
CLI it runs. An adapter is the answer to three questions nothing above it can
answer.

- **What words start the process?** The task fixes them: `--print`,
  `--permission-mode bypassPermissions`, and `--model <id>` when a model was
  configured, with the prompt handed over on standard input.
- **Where is the program?** `Config` carries `provider: String` and
  `model: Option<String>` and no per-provider command key yet — that lands with
  T063 — so the adapter is constructed with the command word: a bare name to
  search `PATH` for, or a path to be used where it stands.
- **What is the answer when the program is not there?** The done-when is
  explicit: "a missing executable is ProviderConfiguration".
  `FailureClass::ProviderConfiguration`'s own definition is "Authentication, an
  invalid model, a missing executable. No retry can fix this, so nothing is
  retried", and VISION.md §7 pauses that class for a human rather than spending
  another attempt on it.

Three constraints make the shape of the answer non-obvious.

- The words have to be reviewable without spending a session. The real CLI costs
  money and talks to a network, so a test that finds a wrong flag by running the
  CLI is a test nobody will run twice, and it cannot run in CI at all.
- `crate::Error` has no provider-configuration *variant*, and `docs/DESIGN.md`
  fixes the payloads it declares; `Error::Provider { provider, detail }` is what
  `Provider::invoke`'s own `# Errors` section names for a CLI that "could not be
  started".
- A `Command` is resolved twice by default: once when the program word is read,
  and once by the kernel after the child's `chdir`. `run_streaming` sets that
  `current_dir` to the task's worktree, and a worktree is a directory an agent
  writes into.

## Decision

**Build the argument vector in a pure function, and refuse an id that cannot be
sent as a value.** `arguments(Option<&str>) -> Result<Vec<String>>` answers "what
will run" from a model id alone, with no `Command` and no process in sight: three
words always, five when a model was configured, in the order a reviewer reads a
failure log against. The same function is what makes an unusable model id a
refusal instead of a flag: a value beginning with `-` is a flag to the CLI's own
parser, so the session would run on some other model while the attempt record
named the configured one — the mismatch VISION.md §12 says is rejected. An empty
id is refused on the same grounds: it names nothing. `git::publish` met the same
trap from the other side and wrote down what a value wearing an option's clothes
does once it reaches a command.

**Locate the command before the spawn, and answer in the adapter's own words.**
`locate` resolves the command word — a bare name along `PATH`, a word holding a
`/` at the location it names — and accepts only an existing regular file with an
execute bit. Absent, a directory, and non-executable all produce
`Error::Provider` naming the adapter, the configured command, where it was looked
for, and the words "no retry can start one". Checking first is what lets the
answer be about the configuration; letting the spawn answer returns the OS's
wording for `ENOENT` inside a message about a command line, which is prose a
classifier has to read.

**Skip an empty `PATH` entry.** POSIX reads one as the current directory. Every
directory a run works in is written into by an agent, so a task worktree holding a
file named `claude` would be reported to an operator as *where their configured
CLI lives*. This is a deliberate divergence from `execvp`, taken in one function
and documented there.

**Make the located path absolute, and leave symlinks alone.** The path that
passed the check is the path handed to the child, so the program word cannot be
resolved once by the search and again, after the child's `chdir`, somewhere else.
Symlinks are not resolved because the target is a different file from the one the
operator named: what was checked must be what runs.

**Give a session's failure back the adapter's name.** `run_streaming` attributes a
session's failure to the program word it was handed, which after `locate` is an
absolute path. That is the right answer about a process and the wrong one about a
provider — `Provider::name` and `Error::Provider`'s payload both say a failure is
attributed to `claude` — so the adapter replaces the name and hands the `detail`
on unwritten. A reason rewritten by an adapter is a reason no later reader can
trust, so only the name moves.

## Alternatives considered

- **`Error::Config { key, detail }` for a missing executable.** It names the right
  class of problem, but there is no `key` to name: the command arrives through
  `Claude::new`, and `Provider::invoke` documents `Error::Provider` for exactly a
  CLI that could not be started. A variant chosen to look like a class, rather
  than the class the message states, is a decision made twice.
- **Letting the spawn answer.** Cheaper by twelve lines, and the answer is
  `No such file or directory (os error 2)` beside a joined command line —
  accurate, unattributable, and indistinguishable from a directory that vanished
  mid-run. The whole point of a class that never retries is that something
  recognised it before the retry budget was spent.
- **A `which`-style dependency for resolution.** It adds a crate, a
  `Cargo.lock` change, and a platform abstraction nothing here uses, to answer a
  two-branch search this repository has to state its own policy about — the
  empty-entry rule above is not what that crate does.
- **Resolving the model id into the command line as one string.** Rejected with
  the pure function that made it impossible: an argv assembled by concatenation is
  an argv whose quoting has to be reviewed by running it.
- **Reporting `structured_output: true` because the CLI can do it.** Capability
  detection answers what *this* adapter asks for. No output format is requested
  here, so nothing structured arrives to be read, and a `true` would tell a caller
  a figure is available while `Outcome::usage` stays `None` — ADR-0049's unknown,
  never a zero.

## Consequences

- A misconfigured CLI is caught on the first attempt that reaches it and pauses
  the run for a human instead of burning the retry budget. The message names the
  command and the search that failed, which is also what `ktask-rs doctor`'s
  preflight (T062) will report.
- `Error::Provider`'s `detail` is now load-bearing for a failure *class*, not only
  for a log line. T064's `classify(outcome, gates, git_error)` cannot see a
  provider error at all, so the mapping from these refusals to
  `FailureClass::ProviderConfiguration` is reported as a gap rather than guessed
  at here; a phrase matched out of prose is the alternative, and both this file
  and ADR-0053 exist to avoid it.
- `Claude::new(command, idle, hard)` is the seam T063's provider factory must
  fill: `Config` has no per-provider command key today, so the factory supplies
  the command word plus `idle_timeout_secs` and `attempt_timeout_secs`. An adapter
  that read configuration mid-session could not have its session reproduced from
  the arguments it was called with, which is why the clocks are constructor
  arguments and not lookups.
- Any second adapter over `run_streaming` inherits the argv[0] attribution and has
  to restore its own name too. Better is for `run_streaming` to be handed the
  provider name; that is a change to T057's delivery, not to T059's, and T060
  should make one of the two.
- Empty `PATH` entries are ignored everywhere this adapter looks. If a deployment
  genuinely relies on the current directory being searched, it has to say so in
  the command word as a path — which is the safer place for that decision anyway.
