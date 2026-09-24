# Operating ktask-rs: failures and recovery

What each thing that can stop a queue means, what `ktask-rs` does about it
without being asked, and what only you can do. Read
[GUIDE.md](GUIDE.md) first for the commands; `docs/CONTRACT.md` section 1 is
the reference for exit codes and `VISION.md` section 7 is the design behind the
failure classes.

Every claim below about an exit code and a class is checked by
`cargo nextest run -p ktask-cli -E 'test(/docs::operating/)'`, which fails
when a failure class or a pause reason has no entry here, when an entry's exit
code disagrees with the contract, or when a command it tells you to run does
not exist.

## 1. Failure or pause?

A queue stops for one of two reasons, and they are handled differently.

A **failure** means the supervisor tried, and could not finish the task within
its budget. The task is `failed`, `run` exits **1**, and nothing behind it
starts: a successor never runs until its predecessor is `published_verified`.
Every failure carries one of the nine classes in section 2, chosen before any
recovery is attempted.

A **pause** means the work is not wrong, it is waiting. The task keeps its
place and its worktree evidence, is never marked `failed`, and `run` exits
**3**, **4**, **5** or **130** according to why. There are five pause reasons,
in section 3. Two classes, `provider_limit` and `needs_input`, are pauses
rather than failures: they never end a task as `failed`.

| Exit | Meaning | Where it comes from |
|---|---|---|
| 1 | a task failed | seven of the nine classes, section 2 |
| 3 | provider limit; paused | `provider_limit`, pause `waiting_limit` |
| 4 | stopped at a human gate | pause `human_gate` |
| 5 | needs input on a decision | `needs_input`, pause `waiting_input` |
| 130 | interrupted; durable and resumable | pauses `interrupted` and `blocked` |

Exit 2 is not a failure of a task: it is a usage error (bad arguments, a
malformed task, no project) and no task has been touched.

## 2. Failure classes

Each entry gives the exit code `run` (and `retry`) ends with, what the
supervisor does by itself, and what you do. Whatever the class, `retry --task
<id>` is how a failed task goes again: it starts a fresh provider session
seeded with the failure (classification, gate output, diff summary and the
earlier attempts' evidence). A provider session is never resumed, and every
gate reruns from scratch.

Where two classes could both describe a failure, the first in this list
decides, in the priority VISION.md section 7 fixes: `provider_configuration`,
`provider_limit`, `provider_transient`, `git_conflict`, `policy_failure`,
`verification_failure`, `needs_input`, `environment_failure`, and last
`agent_failure`. An attempt that both hit a usage limit and failed a gate is
reported as the limit, because that is the class that should govern recovery.

### `agent_failure`

- **Exit code:** 1
- **Meaning:** the agent could not complete the implementation: it exited
  non-zero, crashed, refused, or finished without a report. Nothing else
  matched, so this is the fallback class.
- **Automatically:** nothing to verify, so nothing to remediate. The task is
  `failed` after the one attempt, the journal keeps its output, and the queue
  stops behind it.
- **You:** read what the agent said (the task's attempt evidence, or the
  `logs` screen of the TUI). If the task was unclear, fix its text and
  `ktask-rs retry --task <id>`; if it is not something an agent can do, do it
  yourself and `ktask-rs cancel --task <id>` so the queue may proceed past it.

### `verification_failure`

- **Exit code:** 1
- **Meaning:** a gate failed: tests, lint, build or the privacy scan.
- **Automatically:** bounded remediation. The supervisor launches a fresh
  session seeded with the failure bundle, then reruns every gate from scratch.
  It allows `max_remediation_attempts` rounds (default 1) and gives up early
  if the same failure signature comes back `circuit_breaker_threshold` times
  (default 3), or the elapsed-time bound is spent. Nothing that fails
  verification is ever published. When the budget is spent the task is
  `failed`.
- **You:** find out why the gate fails. `ktask-rs rerun-gate --task <id>` runs
  it again against the task's worktree and prints the result without changing
  the task. If the gate is right, fix the cause; if the environment or a
  flaky test is at fault, fix that. Then `ktask-rs retry --task <id>`. Never
  edit the gate to make it pass.

### `provider_limit`

- **Exit code:** 3
- **Meaning:** the provider reported a usage or rate limit, with a reset time
  or without one. This is a pause, not a failure; see `waiting_limit` in
  section 3.
- **Automatically:** reads the reset time out of the provider's message. With
  one, the pause records that instant plus `limit_wait_margin_secs` (default
  60); without one, the wait is a bounded backoff, never an unbounded one. One
  session ran, no second one is started, and the task is not `failed`.
- **You:** nothing, except to run `ktask-rs resume` once the reset has
  passed. Resuming earlier starts nothing and exits 3 again.

### `provider_transient`

- **Exit code:** 1
- **Meaning:** the provider failed in a way a retry may fix: a network error, a
  temporary service failure, a provider process that crashed.
- **Automatically:** the attempt in flight is journaled as failed with this
  class and the task is `failed`. The supervisor does not retry it on its own.
- **You:** check that the service is back, then `ktask-rs retry --task <id>`.

### `provider_configuration`

- **Exit code:** 1
- **Meaning:** the provider cannot be used as configured: bad credentials, an
  invalid model, or an executable that is not installed.
- **Automatically:** found during preflight, before any tokens are spent, so no
  attempt starts. It never loops: retrying cannot fix a wrong configuration,
  so the task is `failed` at once and waits for you.
- **You:** run `ktask-rs doctor`, which names the failing check and its
  remedy. Fix the credential, model name or installation, then
  `ktask-rs retry --task <id>`.

### `git_conflict`

- **Exit code:** 1
- **Meaning:** the repository could not be brought or kept in the state
  publishing needs: mainline could not be fetched, the branch drifted, or a
  git operation while committing or publishing was rejected.
- **Automatically:** preflight stops before an attempt when mainline cannot be
  fetched. The task is `failed`; the queue stops behind it. Nothing is
  published on a failure.
- **You:** look at the repository and its remote (`git status`, `git fetch`),
  restore a reachable, up-to-date mainline or resolve the conflicting change
  yourself, then `ktask-rs retry --task <id>`.

### `environment_failure`

- **Exit code:** 1
- **Meaning:** the host lacks something the run needs, independent of the agent
  and the provider: a missing SDK or dependency, or too little free disk
  (`min_free_disk_bytes`).
- **Automatically:** preflight refuses before spending anything. The task is
  `failed` and no attempt starts. Self-healing never touches host
  configuration, so it does not try to repair this itself.
- **You:** run `ktask-rs doctor`, install or free what is missing, then
  `ktask-rs retry --task <id>`.

### `policy_failure`

- **Exit code:** 1
- **Meaning:** the attempt broke a rule the runner enforces: a forbidden file, a
  dirty worktree (an untracked or uncommitted file left behind), or an attempt
  to bypass a gate.
- **Automatically:** treated like a failed gate: one round of remediation in a
  fresh session, gates rerun from scratch, and the same circuit breaker. The
  journal names the offending file. A dirty tree is never published. Then the
  task is `failed`.
- **You:** read the offending path in the journal. If the rule is right, fix
  the task so the agent has no reason to break it; if the file is in fact
  legitimate, that is a decision about policy, and only you may make it. Then
  `ktask-rs retry --task <id>`.

### `needs_input`

- **Exit code:** 5
- **Meaning:** an unresolved product or technical decision that only a human
  can make. This is a pause, not a failure; see `waiting_input` in section 3.
- **Automatically:** never loops. The question, its options, trade-offs and
  impact are journaled, the queue pauses at once after one session, and the
  task is not `failed`.
- **You:** answer it: `ktask-rs resolve --task <id> --note "<answer>"`. That
  records an ADR for the decision. Commit and push it, because preflight
  refuses an untracked file, then `ktask-rs resume`.

## 3. Pause states

A paused task keeps its place in the queue. `ktask-rs status` shows which
pause it is in, and the queue does not move past it.

### `waiting_limit`

- **Exit code:** 3
- **Meaning:** the provider's usage limit is outstanding, with a known reset or
  an unknown one.
- **Automatically:** records the reset time (plus margin) when the provider
  said one, and bounded backoff otherwise. It does not spend a session while
  the limit stands, and it does not loop.
- **You:** wait for the reset, then `ktask-rs resume`. No decision is needed.

### `waiting_input`

- **Exit code:** 5
- **Meaning:** a task raised a structured decision request and is waiting for
  an answer.
- **Automatically:** holds the queue where it is until the journal records an
  answer. Nothing behind the task starts.
- **You:** `ktask-rs resolve --task <id> --note "<answer>"`, or answer it in
  the TUI's input inbox, which records the same ADR. Commit and push the ADR,
  then `ktask-rs resume`. The task then runs a fresh attempt with the ADR in
  its context.

### `human_gate`

- **Exit code:** 4
- **Meaning:** a gate: a queue entry that asks a human for something before
  the work after it may proceed. It is never handed to an agent and makes no
  commit.
- **Automatically:** stops the queue at the gate so the entries after it cannot
  run ahead of your approval.
- **You:** do what the gate asks, then `ktask-rs ack --task <id>`. That marks
  the gate acknowledged and leaves the queue paused; `ktask-rs resume`
  continues it.

### `interrupted`

- **Exit code:** 130
- **Meaning:** the run stopped before the task did: you sent `SIGINT` or ran
  `ktask-rs interrupt`, or the process died, the machine lost power, or it
  was restarted. Whatever the cause, the state is durable and resumable.
- **Automatically:** every transition is journaled before its side effect, so
  on the next start the supervisor inspects the process table, the worktree
  and the last journaled transition, then resumes the phase or marks the
  attempt interrupted. It never guesses and never silently re-runs work that
  may already have happened. A task caught mid-publication is settled by
  fetching and comparing with the remote, never by pushing again blindly.
- **You:** `ktask-rs resume`. If the interruption was a crash you cannot
  explain, `ktask-rs status` and the history screen show what the journal
  recorded before it.

### `blocked`

- **Exit code:** 130
- **Meaning:** the queue was stopped on purpose by `ktask-rs pause`: the task
  that was running finished, and the next one was parked instead of started.
  It is the same durable stop as `interrupted`, reached without killing
  anything.
- **Automatically:** parks the next task where it was. Nothing is lost.
- **You:** `ktask-rs run` or `ktask-rs resume` lifts the pause and continues.

## 4. When you are not sure

- `ktask-rs status` shows every task's state, and `--json` gives the same for
  a script.
- `ktask-rs doctor` checks providers, git, the toolchain and the state
  directory, and prints a remedy for each failure.
- The journal is the source of truth: the `history` screen of `ktask-rs tui`
  lists every event of every attempt, and each recovery leaves a report of the
  classification, the repairs attempted and the final result.
- A task you cannot or will not finish is `ktask-rs cancel --task <id>`, which
  lets the queue proceed past it.
