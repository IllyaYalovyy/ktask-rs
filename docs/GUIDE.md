# Getting started

This guide takes you from nothing installed to a queue that has drained, using
the built-in `dummy` provider so that no AI account, network or money is
involved. Every command below is run, and its output compared with what is
printed here, by `cargo nextest run -p ktask-cli -E 'test(/docs::guide/)'`,
so what you read is what you get. Only three things vary from machine to
machine and are matched loosely: project ids, commit ids and timestamps.

`docs/CONTRACT.md` is the reference for every command, flag and exit code.
This guide is the walk-through.

## 1. Install

`ktask-rs` is one binary. Building it needs `git` and a Rust toolchain of
version 1.97 or newer; running it needs only `git`, because SQLite is
compiled into the binary and nothing else is loaded at run time. From a
checkout of this repository:

```sh
cargo install --path crates/ktask-cli --locked
```

That builds the release profile and installs `ktask-rs` into `~/.cargo/bin`.
The profile (the `[profile.release]` table in `Cargo.toml`) turns on thin
link-time optimisation and strips symbols, so the result is one small,
self-contained file. Check that it is on your `PATH` with
`ktask-rs --version`; `ktask-rs --help` lists every command.

To put the binary on a machine that has no Rust toolchain, build it once and
copy the file. The target machine needs the same operating system and
processor architecture as the one you built on, and `git`:

```sh
cargo build --release --locked
install -m 755 target/release/ktask-rs ~/.local/bin/
```

The test suite builds the release profile and runs `target/release/ktask-rs`
through a complete queue with the `dummy` provider
(`cargo nextest run -p ktask-cli -E 'test(/release::/)'`), so the binary you
copy is the one that has been proven.

## 2. A project to work on

`ktask-rs` supervises a git repository. Two things about that repository
matter before you start:

- It needs a remote called `origin`, because finishing a task means
  publishing its work and confirming it arrived.
- Its working tree must be clean when a task starts. A task that finds
  uncommitted changes stops with a policy failure instead of building on
  them.

Here is a throwaway project with a local bare repository standing in for the
remote. Skip this if you already have a real one, and run the rest inside it.

```console
$ mkdir ktask-demo && cd ktask-demo
$ git init -q --bare origin.git
$ git init -q -b main project && cd project
$ git remote add origin "$(dirname "$PWD")/origin.git"
$ echo "# demo" > README.md
$ git add README.md && git commit -q -m "Start the demo project"
$ git push -q origin main
```

## 3. `init`

`init` registers the current repository:

```console
$ ktask-rs init
project: 3f2a9c1d5b7e4a60
state: /home/you/.local/state/ktask-rs/3f2a9c1d5b7e4a60
```

The project id is derived from the repository's canonical path and its
remote. Everything `ktask-rs` records about the project (the journal of what
happened, its configuration, the agents' reports) lives in the `state`
directory under `$XDG_STATE_HOME`, never inside your repository, so nothing
of `ktask-rs` ends up in your history. Running `init` again prints the same
registration and exits 0.

Commands find the project by walking up from the current directory, so there
is no need to `cd` back to the root, and `--project <path>` overrides the
search.

## 4. `add`

A task is a block of Markdown: a `##` heading for the title, then four
required sections.

- `Outcome`: what will be true when the task is finished.
- `Done-when`: how you would recognize that it is.
- `Verify`: the command that checks it.
- `Refs`: files or documents the agent should read, or `none`.

Keep the plan file outside the repository (the working tree has to stay
clean). A plan file may hold any number of tasks; they run in the order they
appear.

```console
$ cat > ../plan.md <<'EOF'
> ## Add a greeting
>
> **Outcome:** the repository has a greeting file.
>
> **Done-when:** `greeting.txt` exists.
>
> **Verify:** `true`
>
> **Refs:** README.md
>
> ## Say goodbye
>
> **Outcome:** the repository says goodbye.
>
> **Done-when:** goodbye exists.
>
> **Verify:** `true`
>
> **Refs:** none
> EOF
$ ktask-rs add --file ../plan.md
task 1 added: Add a greeting
task 2 added: Say goodbye
```

Without `--file`, `ktask-rs add` opens `$EDITOR` on a template holding those
sections and adds what you save. A task that is missing a section is refused
and never enters the queue. This one has only an outcome:

```console
$ printf '## Half a task\n\n**Outcome:** something.\n' > ../half.md
$ ktask-rs add --file ../half.md
error: add: task "Half a task": policy violation: task is missing required section(s): Done-when, Verify, Refs ([])
$ echo $?
2
```

Exit code 2 always means a usage problem. The exit codes are listed in
`docs/CONTRACT.md` section 1, and scripts can rely on them.

## 5. `plan lint`

`plan lint` checks the whole queue without running anything: every required
section present, every `Verify` command parseable, no duplicate ids. It prints
one line per problem, so a clean queue prints nothing and exits 0:

```console
$ ktask-rs plan lint
$ echo $?
0
```

Run it whenever you have edited the queue and before a long run.

## 6. Choose a provider

The provider is the thing that does the work. Real ones are `claude` and
`codex`; the `dummy` provider replays a scripted scenario, which is what
makes this guide reproducible and is how the project's own end-to-end tests
work. Two settings go in the project's config file, `config.toml` in the
state directory:

- `provider` and, for the dummy, `dummy_scenario_path`.
- `verify_command`: the gate every task must pass before it can be published.
  It is mandatory. `run` refuses to start without one and exits 2.

A scenario is a list of steps, and each invocation of the provider consumes
one. A task takes two: a probe that checks the provider works (before the
task starts), then the attempt itself. The attempt reports back by writing a
report whose first line is `KTASK_RESULT: DONE`, into the state directory
under `attempts/<task>/<attempt>/report.md`. A real agent is told where to
write it; the scenario has to spell it out.

`ktask-rs` reads the state directory from `init`, so keep it in a variable:

```console
$ STATE=$(ktask-rs init | sed -n 's/^state: //p')
$ cat > "$STATE/scenario.toml" <<EOF
> [[steps]]
> outcome = "success"
>
> [[steps]]
> outcome = "success"
> stdout = "wrote greeting.txt\n"
>
> [[steps.files]]
> path = "$STATE/attempts/1/1/report.md"
> content = "KTASK_RESULT: DONE\nSummary: added greeting.txt\n"
>
> [[steps]]
> outcome = "success"
>
> [[steps]]
> outcome = "success"
> stdout = "said goodbye\n"
>
> [[steps.files]]
> path = "$STATE/attempts/2/1/report.md"
> content = "KTASK_RESULT: DONE\nSummary: said goodbye\n"
> EOF
$ cat > "$STATE/config.toml" <<EOF
> provider = "dummy"
> dummy_scenario_path = "$STATE/scenario.toml"
> verify_command = ["true"]
> EOF
```

The dummy agent changes no files, so there is nothing for `true` to reject.
A real agent commits its work to the branch, and an attempt that leaves
uncommitted changes behind fails verification.

## 7. `run`

`run` drains the queue: it takes the tasks in order, one at a time, and does
not start a task until the one before it has been published and confirmed on
the remote. Progress streams to stderr as it happens, and each finished task
gets one result line on stdout, so `ktask-rs run > results.txt` keeps only
the results.

```console
$ ktask-rs run
task 1: preflight: checking the repository
task 1: preflight: passed (base 20c088b)
task 1: attempt 1 started (direct)
task 1: attempt 1: phase Implement
task 1 | wrote greeting.txt
task 1: attempt 1 finished (exit 0)
task 1: attempt 1: phase Verify
task 1: attempt 1: verified
task 1: publishing 20c088b
task 1: published 20c088b (confirmed on the remote)
task 1: done
task 1: done (commit 20c088b)
task 2: preflight: checking the repository
task 2: preflight: passed (base 20c088b)
task 2: attempt 1 started (direct)
task 2: attempt 1: phase Implement
task 2 | said goodbye
task 2: attempt 1 finished (exit 0)
task 2: attempt 1: phase Verify
task 2: attempt 1: verified
task 2: publishing 20c088b
task 2: published 20c088b (confirmed on the remote)
task 2: done
task 2: done (commit 20c088b)
run: queue drained
$ echo $?
0
```

Both streams are shown above, interleaved as a terminal shows them. Exit 0
means the queue drained, and nothing is called done on anyone's say-so: a
task is done only after its report was read, its gate passed and its work was
seen on the remote.

`--task <id>` runs exactly one task and `--from <id>` starts at one.

## 8. `status`

`status` prints one line per task and ends with a count of tasks by state. It
only reads the journal, so it is safe to run at any time, from any terminal,
including while a run is in progress:

```console
$ ktask-rs status
1 Add a greeting state=Done protocol=direct phase=- attempts=1
2 Say goodbye state=Done protocol=direct phase=- attempts=1
summary: Done=2
```

The queue is drained when every task is `Done`. For a script, `--json` gives
the same information as one JSON object (the project id, the tasks with their
state, attempts and timestamps, and the summary):

```console
$ ktask-rs status --json
{"project":"3f2a9c1d5b7e4a60","tasks":[{"id":1,"title":"Add a greeting","state":"Done","protocol":"direct","phase":null,"attempts":1,"started_at":"2026-09-24T14:54:19.040177723Z","ended_at":"2026-09-24T14:54:19.132288182Z"},{"id":2,"title":"Say goodbye","state":"Done","protocol":"direct","phase":null,"attempts":1,"started_at":"2026-09-24T14:54:19.137177543Z","ended_at":"2026-09-24T14:54:19.229799944Z"}],"summary":{"Done":2}}
```

## When the queue does not drain

`run` stops at the first task that cannot go on and says why through its exit
code. It never starts a task past a failed one.

| Exit code | Meaning | What to do |
|---|---|---|
| 1 | a task failed | read the message, then `ktask-rs retry --task <id>` |
| 3 | the provider hit a usage limit | wait, then `ktask-rs resume` |
| 4 | stopped at a human gate | `ktask-rs ack`, then `ktask-rs resume` |
| 5 | a task needs a decision | `ktask-rs resolve --task <id> --note "..."`, then `ktask-rs resume` |
| 130 | you pressed Ctrl-C | `ktask-rs resume`; the state is durable |

Codes 3, 4 and 5 are pauses rather than failures. `ktask-rs resume` carries
on from the first task that is not done.

The interface most people will live in is `ktask-rs tui`, which shows the
queue, the live agent output, logs, failures and the journal's history, and
offers the same actions as the commands above. It needs a terminal. Its keys
are in `docs/CONTRACT.md` section 4, and `?` lists them on screen.

## Next: a real provider

Change `provider` in `config.toml` to a real one, set `verify_command` to your
project's own gate (for a Rust project, `["cargo", "test"]`), and write tasks
whose `Verify` you would trust. `ktask-rs doctor` checks that the provider,
git, the toolchain and the state directory are in working order, prints a
remedy for anything that is not, and exits 1 if any check fails.
