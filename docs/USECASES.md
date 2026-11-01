# User flows

## Screens

- Current Task — opens at startup; task details, execution controls, output, checks, and questions.
- Queue — current and pending tasks, in execution order; search, filter, and manage work.
- History — all other tasks, with results and previous attempts.
- Configuration — application settings, including mouse support.
- About — application identity and version.

## Rules

- Current is running work, otherwise the first unfinished task in execution order; the first unresolved failure blocks later work. Inserting before non-running current makes the inserted task current, including when it needs approval.
- Only current can run, resume, or retry. Selecting another task only opens its details; failure blocks later work.
- Queue excludes historical tasks. Search and filters preserve execution order; display sorting is deferred.
- Done, Cancelled and Superseded are final. Copy to queue brings any of them back as a new pending task; the original stays in History unchanged.
- All user flows support mouse and keyboard. Mouse is enabled by default and can be disabled in Configuration.

## Task states

| State | Actions |
|---|---|
| Needs approval | Approve → Pending; Reject → Pending (previous revision) or Cancelled (none); Edit, Move, Mark done |
| Pending | Run, Edit, Move, Cancel, Supersede, Mark done |
| Running (including a provider-limit wait) | Pause, Stop, Mark done; automatically → Paused (runner died), Needs input, Failed, Superseded (judge breakdown), Done |
| Paused | Resume, Edit, Stop, Mark done |
| Needs input | Answer, Edit, Stop, Mark done |
| Failed | Retry, Edit, Stop, Mark done |
| Done, Cancelled, Superseded | Copy to queue |

- Stop returns any live task to Pending: the worker stops, the attempt ends, and uncommitted changes are saved as a git stash named `ktask: task #N attempt #M`. Run from Pending is a full restart: checks first, round 0, full limits.
- Retry keeps the code, continues from the checks, and restarts the limits. Resume continues the same attempt; after an Edit it continues in a new attempt with the new revision.
- Mark done is the owner's verdict from any unfinished state; a live attempt stops and stashes first. Only Pending can be cancelled or superseded; Supersede names at least one replacing task.
- Remediation is automatic: the supervisor's judge repairs, revises, escalates, inserts prerequisites or breaks the task down. When it cannot, the task is Failed.
- The TUI and the CLI offer exactly these actions; a refused action names the state and what it offers.

## Flows

- Define a high-level goal, constraints, and observable success criteria → a capable execution model investigates, designs, implements, and verifies within the task → stronger models review and remediate; separate planning or breakdown is optional.

- Open the app → inspect current task and status; an empty queue shows no current task and an Add action.
- Monitor current → read the situation line, the step results, and the attempt report; follow the raw log while it runs; scroll back and return to the latest output.
- Current needs input → the question is shown on screen and r Answer comes first; the answer is saved and the task continues. s Stop returns it to Pending instead.
- Current failed and not worth more attempts → D Mark done records the owner's verdict, or s Stop returns it to Pending to edit; attempts and evidence stay.
- Run current → continue the queue by default, with Run one available explicitly; see the mode, goal, current phase and next task; pause from any screen, or `ktask pause` → the runner stops gracefully and exits, with progress retained and no later task started; r Resume → continue the same attempt.
- Stop a running, paused, needs-input or failed task from any screen (s, [s Stop], or `ktask stop ID`) → the worker stops and is reaped, the attempt ends with its output, usage and evidence, uncommitted changes are stashed, and the task is Pending and editable. A running task completes the Stop once its runner exits; the screen never blocks.
- Edit models of a pending task → preview the current project defaults for execution, review, validation and remediation, then choose Apply current project defaults.
- From Queue, press r or use its Run control to start the current task, regardless of which row is selected; the selected row remains a browsing selection.
- r Retry failed current → a new attempt continues from the checks on the existing code with restarted limits, preserving previous attempts and their evidence.
- The runner dies while a task runs → the next TUI refresh or `ktask run` settles the orphaned attempt to Paused with its work kept; r Resume continues it.
- Failure inside a run → the stronger remediation model judges the evidence and repairs within the task by default → revise, escalate, insert prerequisites, or break the task down only when the evidence warrants it; ask the owner only for a consequential product decision and require approval only by policy.
- Review a new task → approve with one action to make it ready, or reject it (a task with no ready revision is cancelled); show who approved its revision (human or AI).
- Mark done any unfinished task (D, [D Mark done], or `ktask done ID`) → record actor/reason and move it to History without rewriting its attempts.
- Configure a task → choose execution, review, validation, and remediation model/effort → save a new revision; previous attempts retain their original settings.
- Switch to another task → preserve useful changes, stop/reap processes, and clean owned temporary state before execution; a failed handoff blocks the start. Retrying the same task may keep its workspace and reuse its compatible chat session.
- Complete current → move it to History and show the next pending task as current; when none remain, show queue complete.
- Search or filter Queue → select a matching task → read its details; no matches offers clearing the search/filter.
- Add a task → save → see it appended and selected; cancel leaves Queue unchanged.
- Edit a pending, paused, needs-input or failed task → save updated details or cancel unchanged; a running task and History are read-only.
- Cancel a pending task (x) → confirm with an optional reason → it moves to History as cancelled; Copy to queue brings it back.
- Supersede a pending task (R) → name the replacing task IDs → it moves to History as superseded, linked to them.
- Reorder or insert pending work → choose an execution position → see the new order; show neighboring task IDs even when filtered. Insertion before current is allowed unless it is running; never insert into History.
- Mark done a current Human task → record completion and advance; until then, execution waits.
- Answer current AI questions → submit valid answers → record them and continue automatically; incomplete answers leave the task needing input.
- View project statistics → see task counts, completed work, failures, elapsed execution time, and cost.
- Inspect History → find a task → open Reviews or Failures directly for the selected task, or open its attempt details and select Summary, Results, Output, or Transcripts. Read each recorded review and failure step without scrolling through output; switch attempts and steps with mouse or keyboard without altering execution.
- Copy to queue a done, cancelled or superseded task from History (C) → choose a position (default: end of Queue) → a new pending task with the same definition is selected in Queue, linked to the original; the original's revisions, attempts and evidence stay in History.
- Export tasks → choose scope and destination → see success or an actionable error; confirm before overwriting a file.
- Import tasks → preview and validate → choose insertion position (default: end of Queue) → confirm; cancel or invalid input leaves Queue unchanged.
- Break down pending work → the capable model inserts ordinary subtasks in execution order → mark the original superseded and retain its attempts in History; require approval only when policy calls for it.
- Switch screens, open help, or resize → return with selection, filters, and scroll position retained.
- Change Configuration → apply valid settings or cancel unchanged; invalid settings show an error without replacing saved values.
- Open Configuration → Git delivery → choose Local, Direct push, or Pull request; set the remote and base branch using mouse or keyboard. The same versioned settings are available through `delivery` in CLI. Running attempts retain their pinned destination.
- Complete verified work in Direct mode → fetch and rebase when the base advanced → rerun checks, review, and validation → push only the verified commit to the configured base branch. A push race repeats this bounded cycle. A content conflict enters the ordinary judge/fix cycle without losing the attempt or its work.
- Complete verified work in Pull request mode → keep queued tasks on one owned project branch → push with an exact lease for that branch → create or update its PR through `gh`. Empty changes do not create a PR. A changed remote branch owned by someone else stops delivery with the work retained.
- Open Configuration → Storage → inspect retained bytes and the exact eligible attempts → set retention days or apply the preview. Cleanup preserves Queue evidence, unresolved failures and History records; pruned output and artifacts are labeled in History. Workers and checks save flat regular files in `KTASK_ARTIFACT_DIR`; the guardian archives them per attempt before scratch cleanup (8 MiB per file, 64 MiB per attempt). Attempt details and CLI `show` list artifacts; CLI `artifact ID` opens text files. Later judges receive their read-only paths, checksums and source trees. The same preview, policy and apply commands are available in CLI. Interactive remediation pins its evidence through provider cleanup, including concurrent task resolution; interrupted ownership is recovered only after owner and guardian locks are idle and guardian cleanup is confirmed.
- Open About → read application identity and version → return to the previous screen unchanged.
- Use CLI → perform the same nonvisual operations and inspect their results; screen navigation, layout, and mouse preferences are TUI-only.

- Start a task → the project checks run on its clean starting commit first, with no model and no tokens → a red start stops with the failing tests (fix them, then r Retry); a green start begins implementation.
- Implement → check → review → fix → validate is the normal lifecycle inside one active attempt. Sol implements; stronger Astra models review, plan each review/validation repair as the read-only failure judge, and validate. Execution preflight rejects reuse of the implementation model for those roles. A failing check goes straight back to Sol with its failing tests and errors; critical review/validation findings start with the read-only plan, which may also escalate the execution model. Sol applies each fix in its own resumed implementation session, Status shows why the round runs, and checks and independent review/validation run again. Minor findings never start a round. Workers run focused tests only; the supervisor's full check run is the gate. Pause and resume preserve phase and repair round. Review findings show “fixes requested”, never a failed task. Up to ten repair rounds are allowed; after them, independent validation distinguishes concrete critical blockers from recorded minor findings. Critical blockers wait for a decision; minor notes can remain only when all mandatory criteria and checks pass.
