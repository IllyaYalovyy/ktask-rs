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
- All user flows support mouse and keyboard. Mouse is enabled by default and can be disabled in Configuration.

## Flows

- Define a high-level goal, constraints, and observable success criteria → a capable execution model investigates, designs, implements, and verifies within the task → stronger models review and remediate; separate planning or breakdown is optional.

- Open the app → inspect current task and status; an empty queue shows no current task and an Add action.
- Monitor current → read the situation line, the step results, and the attempt report; follow the raw log while it runs; scroll back and return to the latest output.
- Current waits on a question → the question is shown on screen with Answer first; Abandon gives up on the task (History, cancelled) or drops only the waiting or paused attempt without spending budget.
- Current failed and not worth more attempts → Abandon on Current, or Delete on it in Queue, moves it to History as cancelled with its attempts and evidence; Resolve records it as done or superseded instead.
- Run current → continue the queue by default, with Run one available explicitly; see the mode, goal, current phase and next task; pause → see it paused with progress retained and no later task started; resume → continue the same task.
- Retry failed current → start a new attempt on that task, preserving previous attempts and their evidence.
- Inspect failure → the stronger remediation model judges the evidence and repairs within the task by default → revise, escalate, or insert prerequisites only when the evidence warrants it; ask the owner only for a consequential product decision and require approval only by policy.
- Review a new task → approve with one action to make it ready, or reject it; show who approved its revision (human or AI).
- Resolve failed current → choose succeeded override, failed skipped, or superseded → record actor/reason and move it to History without rewriting the failed attempt.
- Configure a task → choose execution, review, validation, and remediation model/effort → save a new revision; previous attempts retain their original settings.
- Switch to another task → preserve useful changes, stop/reap processes, and clean owned temporary state before execution; a failed handoff blocks the start. Retrying the same task may keep its workspace and reuse its compatible chat session.
- Complete current → move it to History and show the next pending task as current; when none remain, show queue complete.
- Search or filter Queue → select a matching task → read its details; no matches offers clearing the search/filter.
- Add a task → save → see it appended and selected; cancel leaves Queue unchanged.
- Edit a pending task or failed current → save updated details or cancel unchanged; running/paused tasks and History remain read-only.
- Delete a pending task → confirm removal → see the updated queue; undo restores it, while cancel changes nothing.
- Reorder or insert pending work → choose an execution position → see the new order; show neighboring task IDs even when filtered. Insertion before current is allowed unless it is running; never insert into History.
- Acknowledge a current Human task → record completion and advance; until acknowledged, execution waits.
- Answer current AI questions → submit valid answers → record them and automatically continue in queue mode; incomplete answers leave the task waiting.
- View project statistics → see task counts, completed work, failures, elapsed execution time, and cost.
- Inspect History → find a task → read its result, changes, cost, and attempts without altering execution.
- Export tasks → choose scope and destination → see success or an actionable error; confirm before overwriting a file.
- Import tasks → preview and validate → choose insertion position (default: end of Queue) → confirm; cancel or invalid input leaves Queue unchanged.
- Break down pending work or failed current → the capable model inserts ordinary subtasks in execution order → mark the original superseded and retain its attempts in History; require approval only when policy calls for it.
- Switch screens, open help, or resize → return with selection, filters, and scroll position retained.
- Change Configuration → apply valid settings or cancel unchanged; invalid settings show an error without replacing saved values.
- Open About → read application identity and version → return to the previous screen unchanged.
- Use CLI → perform the same nonvisual operations and inspect their results; screen navigation, layout, and mouse preferences are TUI-only.

- Implement → review → fix → validate is the normal lifecycle inside one active attempt. Sol implements; stronger Astra models review, repair and validate. Execution preflight rejects reuse of the implementation model for those roles. Each repair starts with read-only planning, then checks and independent review/validation run again. Pause and resume preserve phase and repair round. Review findings show “fixes requested”, never a failed task. Up to ten repair rounds are allowed; after them, independent validation distinguishes concrete critical blockers from recorded minor findings. Critical blockers wait for a decision; minor notes can remain only when all mandatory criteria and checks pass.
