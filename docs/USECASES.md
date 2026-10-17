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

- Open the app → inspect current task and status; an empty queue shows no current task and an Add action.
- Monitor current → follow output, checks, cost, and events; scroll back and return to the latest output.
- Run current → see it running; pause → see it paused with progress retained and no later task started; resume → continue the same task.
- Retry failed current → start a new attempt on that task, preserving previous attempts and their evidence.
- Inspect failure → ask the capable model to remediate → it raises model/effort in a new revision or inserts ordinary repair tasks before failed current → execution continues; ask for human approval only when required.
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
- Answer current AI questions → submit valid answers → record them and continue; incomplete answers leave the task waiting.
- View project statistics → see task counts, completed work, failures, elapsed execution time, and cost.
- Inspect History → find a task → read its result, changes, cost, and attempts without altering execution.
- Export tasks → choose scope and destination → see success or an actionable error; confirm before overwriting a file.
- Import tasks → preview and validate → choose insertion position (default: end of Queue) → confirm; cancel or invalid input leaves Queue unchanged.
- Break down pending work or failed current → the capable model inserts ordinary subtasks in execution order → mark the original superseded and retain its attempts in History; require approval only when policy calls for it.
- Switch screens, open help, or resize → return with selection, filters, and scroll position retained.
- Change Configuration → apply valid settings or cancel unchanged; invalid settings show an error without replacing saved values.
- Open About → read application identity and version → return to the previous screen unchanged.
- Use CLI → perform the same nonvisual operations and inspect their results; screen navigation, layout, and mouse preferences are TUI-only.
