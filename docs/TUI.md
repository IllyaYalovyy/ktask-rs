# ktask-rs TUI: UX specification

Status: agreed plan, 2026-10-08. Replaces the screen-per-command TUI. Development tasks are
written from this document; it lives in the repository as `docs/TUI.md`. Section 10 records
the decisions taken.

## The decision in one paragraph

The TUI is one **workspace** that is always on screen: a one-line **header** with the state of
every project, numbered **panes** side by side, and a one-line **footer** with the keys that work
right now. The panes are **1 Projects, 2 Queue, 3 Task, 4 Output, 5 Activity**. As in lazygit,
selecting something in a pane fills the panes to its right: the selected project fills the queue,
the selected task fills Task, the selected attempt and step fill Output. A wide terminal shows all
five panes at once. A narrow one shows the Queue beside an inspector whose tabs are Task, Output
and Activity. The keys and their meaning are the same at every size. Pop-ups are used only to *do*
one thing (answer, confirm, pick, import, change settings) and then they close. You never need a
pop-up just to *see* something.

---

## 1. Principles

1. **The answer to "does anything need me, and what do I do" is on screen without pressing a
   key.** This is the most common reason to open the TUI, and the CLI can only answer it by
   running `status`, then `list`, then `output`.
2. **Selection drives detail.** Moving the cursor is how you look at things. Panes to the right
   always show the item selected on the left, so looking at a task costs one keypress, not one
   command.
3. **Text is never cut unless the full text is one key away.** Prose wraps. Only a row in a list
   may end in `…`, and only when the full text is in a pane next to it. Any pane can be zoomed to
   the whole screen. This fixes the owner's main complaint.
4. **Every state can be read as text.** A symbol always has a word next to it in the Task pane or
   the header, and colour is never the only signal. NO_COLOR and 16-colour terminals need this,
   and so do pty tests, which read text.
5. **No modes by default, and pop-ups close when the job is done.** Watching should never trap
   you in a mode. A pop-up does one thing, says which key confirms, and goes away.
6. **One action has one key, the key means the same thing everywhere, and the footer shows it.**
   You learn the keys by using the TUI, not by reading `?`.
7. **Live and quiet.** The screen redraws when the journal changes. Only things that are actually
   moving animate. The terminal bell rings only when you are needed. Runs last hours and run
   overnight, so noise trains you to ignore the screen.

---

## 2. Information architecture

### 2.1 Screen anatomy

```
row 1        HEADER  current project + run state │ counts │ cost │ usage │ other projects
rows 2..H-1  BODY    panes (layout depends on size, §2.3)
row H        FOOTER  keys for the focused pane, or a toast, or a prompt
```

**Header.** A single line, always visible, built from segments separated by ` │ `. When the line
is too narrow, whole segments are dropped from the lowest priority up. A segment is never cut in
the middle. Everything in the header is also shown in a pane.

| Priority | Segment | Examples |
|---|---|---|
| 1 (never dropped) | project + run state | `kt ▶ t12 implementation 14:07` · `kt ◷ t12 waiting: claude 5h limit until 01:00` · `kt ↻ t12 retry 2/3 in 1m40s` · `kt ■ t14 needs you` · `kt ○ idle · R to run` · `kt ■ stopped by you` · `kt ✓ queue finished` · `kt ▶ t12 implementation 34:10 · ████████ quiet 11m` |
| 2 | other projects that need you | `blog ? +1` (one other project needs you, one more is registered) |
| 3 | counts by status | `✓11 ✗1 ○2 ◆1` |
| 4 | cost | `$11.40` (this run); at L size `run $11.40 · today $11.40` |
| 5 | usage | `claude 7d 74%`; at L size every known window, e.g. `claude 5h 9% · 7d 74%` |
| 6 | other projects, idle | `+2 proj`; at L size `blog ○ idle │ infra ○ idle` |
| 7 | brand | `ktask` (L only) |

**Footer.** At most one line. It shows, in order of precedence:
1. A prompt from the focused pop-up, for example `y remove · n keep`.
2. An error that stays until the next key, in the failure colour, starting `✗`.
3. A toast that disappears after 4 s or at the next key, for example `✓ t14 sent back to
   pending · R to run`.
4. Otherwise the key hints for the focused pane: the name of the pane, then its most useful keys
   in a fixed order, then `: actions  ? keys`. Keys that do not apply to the selected item are
   left out. For example, `r retry` is not shown for a running task.

**Terminal title** (OSC 2, already implemented): `ktask kt: ▶ t12 implementation` or
`ktask kt: ■ needs you`.

### 2.2 Panes

Every pane has a number, a title in its top border, and a status in its bottom-right border
(position, filter, count, or follow state). The focused pane is drawn with a heavy border
(`┏━┓┃┗┛`) and the others with a light border (`┌─┐│└┘`). This makes focus visible without colour
and checkable by tests.

**1 Projects.** One row per registered project, showing: the symbol of its most urgent state, its
name, a short state (`t12 implementation 14:07`, `t03 asks you a question`, `idle · 4 pending`),
and the cost today. The order is: projects that need you first, then running projects, then idle
ones, each group alphabetical. Moving the selection switches the project immediately. The pane is
visible at L size. At S and M sizes it is the `P` pop-up.

**2 Queue.** The tasks of the selected project in queue order. Cancelled tasks are hidden by
default (§3.6 lists the filters). Row format:

```
 ›▶ t12 Add back-off to the retry r… a2 $2.40      (M/L: attempts and cost columns)
      sy✓ he✓ im▶ ch○ rv○ ts○ cm○ pu–              (M/L: the running task gets a 2nd line)
  ◆ t15 Rotate the deploy key     human
```

Column 1 holds the selection marker `›`. It is shown in the focused pane and dimmed in an
unfocused one, and selection is also drawn as a full-width background bar (§5.1). Column 2 holds the status
symbol (§5.2). Titles are cut with `…` only in this pane, because the full title is always the
first line of the Task pane. The bottom border shows `15 tasks`, `filter "back" 2 of 15`, or
`needs you 1 of 15`.

**3 Task.** Everything about the selected task, wrapped to the pane width, never cut. It has no
cursor and scrolls like a pager. The sections, from top to bottom, are shown only when they have
content:

1. Title (full, wrapped). At L size, if the task is in another project, the project name comes
   first.
2. State line: `RUNNING · agent · attempt 2 of 3 · 10th of 15`. The status word is always in
   capitals.
3. Provider line: `claude sonnet-4.5 (project default)`, or `(set on this task)`.
4. **Needs-you box** (§5.7), when the task needs the operator.
5. **Question** (for a blocked task), in full.
6. **Resolver report**, when the latest attempt went to the resolver: model, answer
   (`answered STOP`, `RETRY`, `SKIP`, `SUPERSEDE → t18 t19`), cost, then WHAT / WHY / ASK as
   labelled blocks. The labels sit in a 6-column gutter and the text wraps under itself.
7. **Attempts**, newest first. Each attempt has a one-line pipeline (§5.3) with its time, cost and
   the router's decision, then the failure reason in full, wrapped. The *selected* attempt (§3.4)
   also shows its steps as a vertical list: symbol, step name, time, tokens, cost, and for a
   running step its activity bar (§5.4). At S size only, a running attempt also gets a **Live** section with
   the last 4 output lines, so you can watch a run at 80×24 without changing tabs.
8. **Body**, **Criteria** (bullets), **Links**.
9. **Notes**: answers given, done reasons, acknowledgement messages, with their times.

The bottom border shows `1–26 of 44 ↓` when there is more to scroll.

**4 Output.** The selected step of the selected attempt of the selected task, in one of three
views:
- **transcript** (`t`): agent messages in full, wrapped, prefixed `agent`. Each tool call is one
  line, `▸ Tool  argument summary  result summary`, and `Enter` expands it to show the full input
  and output (`▾`, with output lines prefixed `│`). Command output from check and health-check
  steps is shown as plain lines.
- **raw** (`T`): the provider stream exactly as received, the same as `output --raw`.
- **diff** (`d`): what the attempt changed (§4.6).

The header block of the pane gives attempt, step, provider, model, the full session id, tokens,
cost, time and state (`following`, `ended: blocked`, `exit 101`). While a step is live and the
view is at the bottom, the pane **follows**. Scrolling up stops following, and `G` or `F`
resumes it. When the selected task has no attempts, Output shows the **run plan**: the next tasks
`R` would run, where the run would stop (human task), and the pipeline with each step's command or
model and whether it is switched off. Planning is checked here.

**5 Activity.** A timeline of journal events for the selected project, newest at the bottom. It
contains task started, attempt failed and the router decision, waiting, resumed, done (with
attempts, time and cost), resolver answers, run stopped and why, operator actions (retry, answer,
move, import, settings changed), usage warnings, and refusals. A divider line marks
`── new since you looked: 22:14 yesterday (9h ago) ──`, and the last line after it is a summary:
`summary: 2 done · 1 needs you · 11 attempts · $11.40`. `Enter` on an event jumps to its task,
attempt and step. `f` toggles between this project and all projects.

### 2.3 Layouts and breakpoints

The size class is chosen from the inner terminal size on every resize:

| Class | Condition | Body layout |
|---|---|---|
| **XS** | below 80 cols or below 24 rows | Only the focused pane, with a tab line `1 2 [3] 4 5` as its title. Supported, not designed for. |
| **S** | 80–119 cols, or fewer than 32 rows | Left: **Queue**, 32 cols at 80, growing to 40% of the width. Right: **inspector** with tabs `[3 Task] 4 Output 5 Activity`. Projects is the `P` pop-up. |
| **M** | 120–159 cols and 32 rows or more, or 160 cols or more with fewer than 40 rows | Left 36%: Queue on top; **Activity** below it (12 rows) when there are 36 rows or more, otherwise Activity is a tab of the right column. Right 64%: **Task** on top (55%) over **Output**. Projects is the `P` pop-up, or a strip above the Queue (one row per project, up to 4) when more than one project is registered and there are 36 rows or more. |
| **L** | 160 cols or more and 40 rows or more | Three columns of 28% / 36% / 36%. Left: **Projects** (one row per project + 2, up to 8 rows) over **Queue**. Middle: **Task** (65%) over **Activity**. Right: **Output** at full height. |

Rules that hold at every size:
- The number keys `1`–`5` always reach the pane with that number. If the pane is hidden, the key
  makes it the active inspector tab, or opens the `P` pop-up for 1.
- A resize keeps the focus, the selection and each pane's scroll position. If the focused pane
  becomes a tab, that tab becomes active.
- At 300×80 the L proportions still apply. Prose in Task and Output wraps at 100 columns at most
  and the rest of the pane stays empty, because long lines are hard to read. Code, diff and raw
  output use the full width.
- If the start-up rule (§3.7) finds events since you last looked, the S inspector opens on the
  Activity tab.

### 2.4 Mock-ups at 80×24

**S1. Run in progress.** The Queue is focused. The inspector shows the Task tab, with the Live
section because Output is not visible.

```
 kt ▶ t12 implementation 14:07 │ ✓9 ○4 ◆1 │ $6.20 │ claude 7d 61% │ +2 proj
┏━ 2 Queue ━━━━━━━━━━━━━━━━━━━━┓┌─ [3 Task] 4 Output 5 Activity ───────────────┐
┃  ✓ t08 Parse import TOML     ┃│ t12 Add back-off to the retry router         │
┃  ✓ t09 Show attempt diff in …┃│ RUNNING · agent · attempt 2 of 3             │
┃  ✓ t10 Duration values in se…┃│ claude sonnet-4.5 (project default)          │
┃  ✓ t11 Rename journal events ┃│                                              │
┃ ›▶ t12 Add back-off to the r…┃│ Attempt 2  sy✓ he✓ im▶ ch○ rv○ ts○ cm○ pu○   │
┃  ○ t13 Usage warning in run …┃│   ✓ sync                2s                   │
┃  ○ t14 Migrate journal to v3…┃│   ✓ health-check       38s                   │
┃  ◆ t15 Rotate the deploy key ┃│   ▶ implementation  14:07  41.2k tok  $1.10  │
┃  ○ t16 Document the router r…┃│     activity ▍──────────                     │
┃  ○ t17 Provider fallback ord…┃│   ○ check   ○ review  ○ testing              │
┃                              ┃│   ○ commit  ○ push                           │
┃                              ┃│ Live                                         │
┃                              ┃│   ▸ Bash cargo test -p ktask-core route ✓    │
┃                              ┃│   agent  Router tests pass. Running the      │
┃                              ┃│          whole workspace next.               │
┃                              ┃│   ▸ Bash cargo test --workspace  ▶ 41s       │
┃                              ┃│                                              │
┃                              ┃│ Attempt 1  sy✓ he✓ im✓ ch✗      → retry 1/3  │
┃                              ┃│   check failed: 3 tests failed in            │
┃                              ┃│   route::backoff (exit 101). Transient?      │
┗━━━━━━━━━━━━━━━━━━━ 15 tasks ━┛└─────────────────────────────── 1–20 of 41 ↓ ─┘
 ⏎ open  t transcript  d diff  [ ] attempt  X stop  n new  : actions  ? keys
```

**S2. Run stopped, a failure needs the operator.** You pressed `!`, so Task is focused and
scrolled to the top. The Needs-you box and the start of the resolver report are on screen, and
`j` or `z` reads the rest. Done tasks above t11 are scrolled out of the Queue.

```
 kt ■ t14 needs you │ ✓11 ✗1 ○2 ◆1 │ $11.40 │ claude 7d 74% │ +2 proj
┌─ 2 Queue ────────────────────┐┏━ [3 Task] 4 Output 5 Activity ━━━━━━━━━━━━━━━┓
│  ✓ t11 Rename journal events │┃ t14 Migrate journal to v3 format             ┃
│  ✓ t12 Add back-off to the r…│┃ FAILED · agent · 3 of 3 attempts used        ┃
│  ✓ t13 Usage warning in run …│┃╭ Needs you ─────────────────────────────────╮┃
│ ›✗ t14 Migrate journal to v3…│┃│ Run stopped at t14: attempts used up and   │┃
│  ◆ t15 Rotate the deploy key │┃│ the resolver answered stop. See ASK below. │┃
│  ○ t16 Document the router r…│┃│ r retry  e edit  D mark done  x remove     │┃
│  ○ t17 Provider fallback ord…│┃╰────────────────────────────────────────────╯┃
│                              │┃ Attempt 3  sy✓ he✓ im✓ ch✗        → decide   ┃
│                              │┃ Resolver · claude opus-4.1 · answered stop   ┃
│                              │┃   WHAT  Attempt 3 failed at check again: 14  ┃
│                              │┃         tests in journal::read fail because  ┃
│                              │┃         the reader still expects v2 field    ┃
│                              │┃         names ("step", not "step_name").     ┃
│                              │┃   WHY   The task does not say whether v2     ┃
│                              │┃         journals already on disk must stay   ┃
│                              │┃         readable. Each attempt chose a       ┃
│                              │┃         different answer and the tests now   ┃
│                              │┃         contradict each other.               ┃
│                              │┃   ASK   Decide whether v2 journals must stay ┃
│                              │┃                                              ┃
└─────────────────── 15 tasks ─┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 1–20 of 52 ↓ ━┛
 r retry  e edit  D done  x remove  ⏎ open  [ ] attempt  z zoom  ? keys
```

**S3. Idle, with a planned queue.** It is morning, nothing is running and t13 is selected. The
Queue is scrolled to the first pending task.

```
 kt ○ idle · R to run │ ✓10 ○4 ◆1 │ today $0.00 │ claude 7d 12% │ +2 proj
┏━ 2 Queue ━━━━━━━━━━━━━━━━━━━━┓┌─ [3 Task] 4 Output 5 Activity ───────────────┐
┃  ✓ t03 Journal change notif… ┃│ t13 Usage warning in run report              │
┃  ✓ t04 Attach to a running … ┃│ PENDING · agent · runs 1st                   │
┃  ✓ t05 Codex transport retr… ┃│ claude sonnet-4.5 (project default)          │
┃  ✓ t06 Resolver model setting┃│ no attempts yet                              │
┃  ✓ t07 Provider check command┃│                                              │
┃  ✓ t08 Parse import TOML     ┃│ Body                                         │
┃  ✓ t09 Show attempt diff in …┃│   When a provider reports usage above 80%    │
┃  ✓ t10 Duration values in se…┃│   of any window, the run report must say     │
┃  ✓ t11 Rename journal events ┃│   so on its own line, with the window, the   │
┃  ✓ t12 Add back-off to the r…┃│   percentage and the reset time. The TUI     │
┃ ›○ t13 Usage warning in run …┃│   header shows the same numbers.             │
┃  ○ t14 Migrate journal to v3…┃│ Criteria                                     │
┃  ◆ t15 Rotate the deploy key ┃│   • `run` prints "usage: claude 7d 93%       │
┃  ○ t16 Document the router r…┃│     (resets Fri 09:00)"                      │
┃  ○ t17 Provider fallback ord…┃│   • `status --json` carries the same         │
┃                              ┃│     numbers under "usage"                    │
┃                              ┃│ Links                                        │
┃                              ┃│   docs/router.md                             │
┃                              ┃│                                              │
┃                              ┃│                                              │
┗━━━━━━━━━━━━━━━━━━━ 15 tasks ━┛└──────────────────────────────────────────────┘
 R run  n new  o/O insert  e edit  J/K move  i import  x remove  s settings  ?
```

**S4. Several projects.** `P` opens the project switcher over the workspace. The header has
already told you that `blog` needs you.

```
 kt ▶ t12 implementation 14:07 │ ✓9 ○4 ◆1 │ $6.20 │ claude 7d 61% │ blog ? +1
┏━ 2 Queue ━━━━━━━━━━━━━━━━━━━━┓┌─ [3 Task] 4 Output 5 Activity ───────────────┐
┃  ✓ t08 Parse import TOML     ┃│ t12 Add back-off to the retry router         │
┃  ✓ t09 Show attempt diff in …┃│ RUNNING · agent · attempt 2 of 3             │
┃  ✓ t10 Duration values in se…┃│ claude sonnet-4.5 (project default)          │
┃  ✓ t11 Rename ┏━ Projects 3 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓               │
┃ ›▶ t12 Add bac┃ /                                            ┃ ts○ cm○ pu○   │
┃  ○ t13 Usage w┃ ›▶ kt     t12 implementation 14:07   $6.20   ┃               │
┃  ○ t14 Migrate┃  ? blog   t03 asks you a question    $0.80   ┃               │
┃  ◆ t15 Rotate ┃  ○ infra  idle · 4 pending           $0.00   ┃2k tok  $1.10  │
┃  ○ t16 Documen┃                                              ┃               │
┃  ○ t17 Provide┃ ⏎ switch  ! next needing you  esc close      ┃g              │
┃               ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛               │
┃                              ┃│ Live                                         │
┃                              ┃│   ▸ Bash cargo test -p ktask-core route ✓    │
┃                              ┃│   agent  Router tests pass. Running the      │
┃                              ┃│          whole workspace next.               │
┃                              ┃│   ▸ Bash cargo test --workspace  ▶ 41s       │
┃                              ┃│                                              │
┃                              ┃│ Attempt 1  sy✓ he✓ im✓ ch✗      → retry 1/3  │
┃                              ┃│   check failed: 3 tests failed in            │
┃                              ┃│   route::backoff (exit 101). Transient?      │
┗━━━━━━━━━━━━━━━━━━━ 15 tasks ━┛└─────────────────────────────── 1–20 of 41 ↓ ─┘
 ⏎ open  t transcript  d diff  [ ] attempt  X stop  n new  : actions  ? keys
```

**S5. Settings overlay** (`s`). It covers the body. The header stays visible so you can still see
the run.

```
 kt ▶ t12 implementation 14:07 │ ✓9 ○4 ◆1 │ $6.20 │ claude 7d 61% │ +2 proj
┏━ Settings · kt  [Settings] Providers ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃ Pipeline  sync › health › impl › check › review · testing › commit · push    ┃
┃ Steps                                                                        ┃
┃  [x] sync              git pull --rebase on the tracked branch               ┃
┃  [x] health-check      cargo build                                           ┃
┃  [x] check             cargo test --workspace                                ┃
┃  [x] review                                                                  ┃
┃ ›[ ] testing                                                                 ┃
┃  [x] commit                                                                  ┃
┃  [ ] push                                                                    ┃
┃ Agents                                                                       ┃
┃      provider          claude                     ▾   project                ┃
┃      model             sonnet-4.5                 ▾   project                ┃
┃      resolver-provider claude                     ▾   global                 ┃
┃      resolver-model    opus-4.1                   ▾   global                 ┃
┃ Limits                                                                       ┃
┃      max-attempts      3                          −+  project                ┃
┃      attempt-timeout   4h                             default                ┃
┃      silent-after      10m                            default                ┃
┃ Commands                                                                     ┃
┃      health-check      cargo build                    project                ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 1–20 of 26 ↓ ━┛
 space toggle  ⏎ edit  u reset to default  tab providers  / find  esc close  ?
```

**S6. Stop-run pop-up** (`X`).

```
 kt ▶ t12 implementation 14:07 │ ✓9 ○4 ◆1 │ $6.20 │ claude 7d 61% │ +2 proj
┏━ 2 Queue ━━━━━━━━━━━━━━━━━━━━┓┌─ [3 Task] 4 Output 5 Activity ───────────────┐
┃  ✓ t08 Parse import TOML     ┃│ t12 Add back-off to the retry router         │
┃  ✓ t09 Show attempt diff in …┃│ RUNNING · agent · attempt 2 of 3             │
┃  ✓ t10 Duration values in se…┃│ claude sonnet-4.5 (project default)          │
┃  ✓ t11 Rename journal events ┃│                                              │
┃ ›▶ t12 Add b┏━ Stop the run on kt? ━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓s○ cm○ pu○   │
┃  ○ t13 Usage┃ A run is working on t12 (implementation, 14:07). ┃             │
┃  ○ t14 Migra┃                                                  ┃             │
┃  ◆ t15 Rotat┃ a  stop after t12 finishes        (recommended)  ┃ tok  $1.10  │
┃  ○ t16 Docum┃ s  stop after the current step    implementation ┃             │
┃  ○ t17 Provi┃ k  stop now: kill the agent, attempt 2 is lost   ┃             │
┃             ┃                                                  ┃             │
┃             ┃ The TUI keeps running; R starts the run again.   ┃             │
┃             ┃ esc  keep running                                ┃e route ✓    │
┃             ┃                                                  ┃ing the      │
┃             ┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛             │
┃                              ┃│   ▸ Bash cargo test --workspace  ▶ 41s       │
┃                              ┃│                                              │
┃                              ┃│ Attempt 1  sy✓ he✓ im✓ ch✗      → retry 1/3  │
┃                              ┃│   check failed: 3 tests failed in            │
┃                              ┃│   route::backoff (exit 101). Transient?      │
┗━━━━━━━━━━━━━━━━━━━ 15 tasks ━┛└─────────────────────────────── 1–20 of 41 ↓ ─┘
 ⏎ open  t transcript  d diff  [ ] attempt  X stop  n new  : actions  ? keys
```

**S7. Answer pop-up** (`a`), shown alone. It is centred over the workspace, 56 columns wide or
the width minus 8, whichever is smaller.

```
┏━ Answer t03 ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
┃ blog t03 asks:                                       ┃
┃   The old /rss.xml URL has external subscribers      ┃
┃   (Feedly shows 214). Should /rss.xml redirect       ┃
┃   permanently (301) to /atom.xml, or keep serving    ┃
┃   RSS alongside Atom? A redirect changes the format  ┃
┃   subscribers receive; keeping both doubles the      ┃
┃   feed code.                                         ┃
┃ Your answer                                          ┃
┃ ┌──────────────────────────────────────────────────┐ ┃
┃ │Redirect 301 to /atom.xml; drop the RSS template.█│ ┃
┃ └──────────────────────────────────────────────────┘ ┃
┃ ⏎ send   ctrl-o write in $EDITOR   esc cancel        ┃
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛
```

### 2.5 Mock-ups at 160×45

**L1. Run in progress.** You are watching the live Output.

```
 ktask │ kt ▶ running t12 · implementation 14:07 · attempt 2 of 3 │ ✓9 ▶1 ○4 ◆1 │ run $6.20 · today $6.20 │ claude 5h 22% · 7d 61% │ blog ○ idle │ infra ○ idle
┌─ 1 Projects ─────────────────────────────┐┌─ 3 Task ───────────────────────────────────────────────┐┏━ 4 Output · transcript ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓
│ ›▶ kt     t12 implementation 14:07  $6.20││ t12 Add back-off to the retry router                   │┃ Attempt 2 · implementation · claude sonnet-4.5         ┃
│  ○ blog   idle · 3 pending          $0.00││ RUNNING · agent · attempt 2 of 3 · 10th of 15          │┃ session 6b0e2f4c-1d7a-4c38-9a51-2f0c7e1d9b43           ┃
│  ○ infra  idle · 4 pending          $0.00││ claude sonnet-4.5 (project default)                    │┃ 41.2k tokens · $1.10 · 14:07 · following               ┃
└──────────────────────────────────────────┘│                                                        │┃ ───────────────────────────────────────────────────────┃
┌─ 2 Queue ────────────────────────────────┐│ Attempt 2  sy✓ he✓ im▶ ch○ rv○ ts○ cm○ pu–  14:47 $1.10│┃ agent  I'll start by reading the router's retry path.  ┃
│  ✓ t03 Journal change notifier   a1 $0.90││   ✓ sync               2s                              │┃ ▸ Read   crates/ktask-core/src/route/mod.rs  412 lines ┃
│  ✓ t04 Attach to a running run   a1 $1.20││   ✓ health-check      38s  cargo build                 │┃ ▸ Grep   "fn retry_delay" in crates/  3 matches        ┃
│  ✓ t05 Codex transport retries   a2 $2.75││   ▶ implementation 14:07  41.2k tok  $1.10  ▍───────── │┃ agent  The delay is always zero, so a transient failure┃
│  ✓ t06 Resolver model setting    a1 $0.60││   ○ check              cargo test --workspace          │┃        is retried immediately. I'll add an exponential ┃
│  ✓ t07 Provider check command    a1 $0.85││   ○ review             claude sonnet-4.5               │┃        back-off of 30s, 2m and 8m, capped at the       ┃
│  ✓ t08 Parse import TOML         a1 $1.40││   ○ testing            claude sonnet-4.5               │┃        attempt timeout, and record the planned wait in ┃
│  ✓ t09 Show attempt diff in out… a2 $3.10││   ○ commit                                             │┃        the journal so `status` can show it.            ┃
│  ✓ t10 Duration values in setti… a1 $1.95││   – push               off in settings                 │┃ ▸ Edit   crates/ktask-core/src/route/mod.rs  +38 −6    ┃
│  ✓ t11 Rename journal events     a1 $1.65││ Attempt 1  sy✓ he✓ im✓ ch✗  16:40 $1.30     → retry 1/3│┃ ▸ Edit   crates/ktask-core/src/route/rules.rs  +12 −2  ┃
│ ›▶ t12 Add back-off to the retr… a2 $2.40││   check failed: 3 tests failed in route::backoff       │┃ ▸ Write  crates/ktask-core/src/route/backoff.rs  +64   ┃
│      sy✓ he✓ im▶ ch○ rv○ ts○ cm○ pu–     ││   (exit 101). Router: transient test failure, retry    │┃ ▸ Bash   cargo test -p ktask-core route  ✓ 2.1s        ┃
│  ○ t13 Usage warning in run rep…         ││   1 of 3 after 30s.                                    │┃ agent  Router tests pass. Running the whole workspace  ┃
│  ○ t14 Migrate journal to v3 fo…         ││                                                        │┃        next.                                           ┃
│  ◆ t15 Rotate the deploy key     human   ││ Body                                                   │┃ ▾ Bash   cargo test --workspace  ▶ 41s                 ┃
│  ○ t16 Document the router rules         ││   The router retries at once after a transient error,  │┃   │    Compiling ktask-core v0.9.0                     ┃
│  ○ t17 Provider fallback order           ││   which hammers the provider during an outage. Add an  │┃   │    Compiling ktask-adapters v0.9.0                 ┃
│                                          ││   exponential back-off (30s, 2m, 8m) capped by the     │┃   │    Compiling ktask-cli v0.9.0                      ┃
│                                          ││   attempt timeout, and show the wait in `status`.      │┃   │     Finished `test` profile in 38.20s              ┃
│                                          ││ Criteria                                               │┃   │      Running unittests src/lib.rs                  ┃
│                                          ││   • `status` shows "retry 2/3 in 1m40s" while waiting  │┃   │ running 212 tests                                  ┃
│                                          ││   • the back-off steps are a project setting           │┃   │ test route::backoff::caps_at_timeout ... ok        ┃
│                                          │└───────────────────────────────────────── 1–26 of 31 ↓ ─┘┃   │ test route::backoff::doubles_each_retry ... ok     ┃
│                                          │┌─ 5 Activity ───────────────────────────────────────────┐┃   │ test route::rules::wait_on_rate_limit ... ok       ┃
│                                          ││ 08:30 ✓ t11 done · 1 attempt · 22m · $1.65             │┃   │ test journal::read::v2_fields ... ok               ┃
│                                          ││ 08:31 ▶ t12 attempt 1 started                          │┃   ▼ following · 41s                                    ┃
│                                          ││ 08:47 ✗ t12 attempt 1 failed at check (exit 101)       │┃                                                        ┃
│                                          ││ 08:47 ↻ t12 router: retry 1/3 in 30s (transient)       │┃                                                        ┃
│                                          ││ 08:48 ▶ t12 attempt 2 started                          │┃                                                        ┃
│                                          ││ 08:49 ✓ t12 sync, health-check passed                  │┃                                                        ┃
│                                          ││ 08:49 ▶ t12 implementation · claude sonnet-4.5         │┃                                                        ┃
│                                          ││ 09:01 ! claude usage 7d 61% (warn at 80%)              │┃                                                        ┃
│                                          ││                                                        │┃                                                        ┃
│                                          ││                                                        │┃                                                        ┃
│                                          ││                                                        │┃                                                        ┃
│                                          ││                                                        │┃                                                        ┃
│                                          ││                                                        │┃                                                        ┃
└─────────────────────────────── 15 tasks ─┘└──────────────────────────────────────────────── today ─┘┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ following ▼ ━┛
 4 Output  j/k scroll  ⏎ expand call  F follow  / search  t transcript  d diff  T raw  [ ] attempt  { } step  z zoom  y copy  X stop run  : actions  ? keys
```

**L2. Run stopped, a failure needs the operator, after a night away.** Activity shows what
happened since you last looked. Output shows the failing check of attempt 3.

```
 ktask │ kt ■ stopped at t14 · needs you │ ✓11 ✗1 ○2 ◆1 │ run $11.40 · today $11.40 │ claude 5h 9% · 7d 74% │ blog ○ idle │ infra ○ idle
┌─ 1 Projects ─────────────────────────────┐┏━ 3 Task ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌─ 4 Output · check · attempt 3 of 3 ────────────────────┐
│ ›■ kt     t14 needs you            $11.40│┃ t14 Migrate journal to v3 format                       ┃│ Attempt 3 · check · cargo test --workspace             │
│  ○ blog   idle · 3 pending          $0.00│┃ FAILED · agent · 3 of 3 attempts used · 12th of 15     ┃│ exit 101 · 1m12s · output 340 lines                    │
│  ○ infra  idle · 4 pending          $0.00│┃╭ Needs you ───────────────────────────────────────────╮┃│ ───────────────────────────────────────────────────────│
└──────────────────────────────────────────┘┃│ The run stopped at t14: all 3 attempts failed at     │┃│ running 214 tests                                      │
┌─ 2 Queue ────────────────────────────────┐┃│ check and the resolver answered stop. Its ASK says   │┃│ test journal::read::v3_round_trip ... ok               │
│  ✓ t03 Journal change notifier   a1 $0.90│┃│ what to decide.                                      │┃│ test journal::read::v2_fields ... FAILED               │
│  ✓ t04 Attach to a running run   a1 $1.20│┃│ r retry   e edit body   D mark done   x remove       │┃│ test journal::read::v2_attempt_lines ... FAILED        │
│  ✓ t05 Codex transport retries   a2 $2.75│┃╰──────────────────────────────────────────────────────╯┃│ test journal::read::mixed_versions ... FAILED          │
│  ✓ t06 Resolver model setting    a1 $0.60│┃ Resolver · claude opus-4.1 · answered STOP · $0.40     ┃│ test journal::write::emits_v3 ... ok                   │
│  ✓ t07 Provider check command    a1 $0.85│┃   WHAT  Attempt 3 failed at check again: 14 tests in   ┃│ failures:                                              │
│  ✓ t08 Parse import TOML         a1 $1.40│┃         journal::read fail because the reader still    ┃│                                                        │
│  ✓ t09 Show attempt diff in out… a2 $3.10│┃         expects v2 field names ("step", not            ┃│ ---- journal::read::v2_fields stdout ----              │
│  ✓ t10 Duration values in setti… a1 $1.95│┃         "step_name"). The implementation changed only  ┃│ thread 'journal::read::v2_fields' panicked at          │
│  ✓ t11 Rename journal events     a1 $1.65│┃         the writer.                                    ┃│ ↪ crates/ktask-core/src/journal/read.rs:88:9:          │
│  ✓ t12 Add back-off to the retr… a2 $2.40│┃   WHY   The task does not say whether v2 journals      ┃│ assertion `left == right` failed: field "step" is      │
│  ✓ t13 Usage warning in run rep… a1 $1.85│┃         already on disk must stay readable. Each       ┃│ ↪ missing from a v2 line                               │
│ ›✗ t14 Migrate journal to v3 fo… a3 $5.15│┃         attempt chose a different answer (rewrite on   ┃│   left: None                                           │
│      sy✓ he✓ im✓ ch✗ rv○ ts○ cm○ pu–     │┃         read, refuse, read both) and the tests now     ┃│  right: Some("implementation")                         │
│  ◆ t15 Rotate the deploy key     human   │┃         contradict each other.                         ┃│                                                        │
│  ○ t16 Document the router rules         │┃   ASK   Decide whether v2 journals must stay readable. ┃│ ---- journal::read::v2_attempt_lines stdout ----       │
│  ○ t17 Provider fallback order           │┃         Then edit the task to say so and retry, or     ┃│ thread 'journal::read::v2_attempt_lines' panicked at   │
│                                          │┃         replace it with two tasks: "reader accepts v2  ┃│ ↪ crates/ktask-core/src/journal/read.rs:141:5:         │
│                                          │┃         and v3", then "writer emits v3".               ┃│ unknown journal version 2: expected 3                  │
│                                          │┃ Attempt 3  sy✓ he✓ im✓ ch✗  21:12 $1.90      → decide  ┃│                                                        │
│                                          │┃   check failed: 14 tests failed in journal::read       ┃│ failures:                                              │
│                                          │┃                                                        ┃│     journal::read::mixed_versions                      │
│                                          │┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 1–26 of 44 ↓ ━┛│     journal::read::v2_attempt_lines                    │
│                                          │┌─ 5 Activity ───────────────────────────────────────────┐│     journal::read::v2_fields                           │
│                                          ││ ── new since you looked: 22:14 yesterday (9h ago) ──   ││     journal::read::v2_lines_keep_order                 │
│                                          ││ 22:31 ✓ t12 done · 2 attempts · 47m · $2.40            ││                                                        │
│                                          ││ 22:32 ▶ t13 started                                    ││ test result: FAILED. 200 passed; 14 failed; 0 ignored  │
│                                          ││ 23:05 ✓ t13 done · 1 attempt · 33m · $1.85             ││                                                        │
│                                          ││ 23:06 ▶ t14 started                                    ││                                                        │
│                                          ││ 23:41 ✗ t14 attempt 1 failed at check → retry 1/3      ││                                                        │
│                                          ││ 00:20 ✗ t14 attempt 2 failed at check → retry 2/3      ││                                                        │
│                                          ││ 00:31 ◷ t14 waiting: claude 5h limit, resets 01:00     ││                                                        │
│                                          ││ 01:00 ▶ t14 attempt 3 started                          ││                                                        │
│                                          ││ 01:21 ✗ t14 attempt 3 failed at check → decide         ││                                                        │
│                                          ││ 01:28 ■ t14 resolver: stop · run stopped, needs you    ││                                                        │
│                                          ││ summary: 2 done · 1 needs you · 11 attempts · $11.40   ││                                                        │
│                                          ││                                                        ││                                                        │
└─────────────────────────────── 15 tasks ─┘└─────────────────────────────────────────────── 11 new ─┘└────────────────────────────────────────── 1–41 of 340 ─┘
 3 Task  j/k scroll  r retry  e edit  D mark done  x remove  [ ] attempt  { } step  t transcript  d diff  z zoom  y copy  R run  : actions  ? keys
```

**L3. Idle, with a planned queue.** Output shows the run plan because t13 has no attempts.

```
 ktask │ kt ○ idle · R to run │ ✓10 ○4 ◆1 │ today $0.00 · yesterday $18.80 │ claude 5h 0% · 7d 12% │ blog ○ idle │ infra ○ idle
┌─ 1 Projects ─────────────────────────────┐┌─ 3 Task ───────────────────────────────────────────────┐┌─ 4 Output · run plan ──────────────────────────────────┐
│ ›○ kt     idle · 4 pending          $0.00││ t13 Usage warning in run report                        ││ t13 has no attempts yet. This is what `R` will run:    │
│  ○ blog   idle · 3 pending          $0.00││ PENDING · agent · runs 1st · no attempts yet           ││                                                        │
│  ○ infra  idle · 4 pending          $0.00││ claude sonnet-4.5 (project default)                    ││ Run plan                                               │
└──────────────────────────────────────────┘│                                                        ││   1  t13 Usage warning in run report                   │
┏━ 2 Queue ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓│ Body                                                   ││   2  t14 Migrate journal to v3 format                  │
┃  ✓ t03 Journal change notifier   a1 $0.90┃│   When a provider reports usage above 80% of any       ││   3  t15 Rotate the deploy key        human: run stops │
┃  ✓ t04 Attach to a running run   a1 $1.20┃│   window, the run report must say so on its own line,  ││ Pipeline for each task                                 │
┃  ✓ t05 Codex transport retries   a2 $2.75┃│   with the window, the percentage and the reset time.  ││   sync           git pull --rebase on main             │
┃  ✓ t06 Resolver model setting    a1 $0.60┃│   The TUI header shows the same numbers.               ││   health-check   cargo build                           │
┃  ✓ t07 Provider check command    a1 $0.85┃│ Criteria                                               ││   implementation claude sonnet-4.5                     │
┃  ✓ t08 Parse import TOML         a1 $1.40┃│   • `run` prints "usage: claude 7d 93% (resets Fri     ││   check          cargo test --workspace                │
┃  ✓ t09 Show attempt diff in out… a2 $3.10┃│     09:00)"                                            ││   review         claude sonnet-4.5                     │
┃  ✓ t10 Duration values in setti… a1 $1.95┃│   • `status --json` carries the same numbers under     ││   testing        claude sonnet-4.5                     │
┃  ✓ t11 Rename journal events     a1 $1.65┃│     "usage"                                            ││   commit                                               │
┃  ✓ t12 Add back-off to the retr… a2 $2.40┃│ Links                                                  ││   push           off                                   │
┃ ›○ t13 Usage warning in run rep…         ┃│   docs/router.md                                       ││ Limits                                                 │
┃  ○ t14 Migrate journal to v3 fo…         ┃│                                                        ││   max 3 attempts · 4h per attempt · silent after 10m   │
┃  ◆ t15 Rotate the deploy key     human   ┃│                                                        ││   resolver: claude opus-4.1                            │
┃  ○ t16 Document the router rules         ┃│                                                        ││                                                        │
┃  ○ t17 Provider fallback order           ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃└────────────────────────────────────────────────────────┘│                                                        │
┃                                          ┃┌─ 5 Activity ───────────────────────────────────────────┐│                                                        │
┃                                          ┃│ yesterday                                              ││                                                        │
┃                                          ┃│ 18:02 ✓ t11 done · 1 attempt · 22m · $1.65             ││                                                        │
┃                                          ┃│ 18:49 ✓ t12 done · 2 attempts · 47m · $2.40            ││                                                        │
┃                                          ┃│ 18:50 ■ run stopped by you after t12 (X)               ││                                                        │
┃                                          ┃│ 19:10 + t16, t17 imported from plan.toml               ││                                                        │
┃                                          ┃│ 19:12 ⇅ t15 moved after t14                            ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┃                                          ┃│                                                        ││                                                        │
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━ 15 tasks ━┛└────────────────────────────────────────────────────────┘└────────────────────────────────────────────────────────┘
 2 Queue  j/k move  ⏎ open  n new  o/O insert below/above  e edit  J/K reorder  i import  x remove  R run  s settings  / filter  f view  : actions  ? keys
```

**L4. Several projects.** Projects is focused and `blog` is selected, so Queue, Task, Output and
Activity show blog.

```
 ktask │ blog ? t03 needs your answer │ ✓2 ?1 ○2 │ today $0.80 │ all projects: today $7.00 │ claude 5h 22% · 7d 61% │ kt ▶ t12 │ infra ○ idle
┏━ 1 Projects ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┓┌─ 3 Task ───────────────────────────────────────────────┐┌─ 4 Output · implementation · attempt 1 ────────────────┐
┃  ▶ kt     t12 implementation 14:07  $6.20┃│ blog · t03 Move the feed to /atom.xml                  ││ Attempt 1 · implementation · claude sonnet-4.5         │
┃ ›? blog   t03 asks you a question   $0.80┃│ BLOCKED · agent · attempt 1 asked a question           ││ session 0c9e7a51-3b2d-4f60-8e1a-5d4c2b9f7e10           │
┃  ○ infra  idle · 4 pending          $0.00┃│╭ Needs you ───────────────────────────────────────────╮││ 18.4k tokens · $0.80 · 9:40 · ended: blocked           │
┗━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━┛││ The agent stopped to ask you this. The run on blog   │││ ───────────────────────────────────────────────────────│
┌─ 2 Queue ────────────────────────────────┐││ stopped here. Answer and the task goes back to       │││ agent  The feed is generated by layouts/rss.xml. Before│
│  ✓ t01 Move posts to content/   a1 $0.35 │││ pending; R runs it again.                            │││        I change the URL I need to know whether the old │
│  ✓ t02 Drop the old theme       a1 $0.45 │││ a answer   e edit body   D mark done   x remove      │││        one must keep working.                          │
│ ›? t03 Move the feed to /atom.x… a1 $0.80││╰──────────────────────────────────────────────────────╯││ ▸ Bash   grep -rn "rss.xml" layouts config.toml  4 hits│
│  ○ t04 Add tag pages                     ││ Question                                               ││ ▸ Read   config.toml  62 lines                         │
│  ○ t05 Lazy-load images                  ││   The old /rss.xml URL has external subscribers        ││ ▸ Bash   ktask-rs report blocked --question (412 ch)   │
│                                          ││   (Feedly shows 214). Should /rss.xml redirect         ││ ── ended: blocked, question recorded ──                │
│                                          ││   permanently (301) to /atom.xml, or keep serving RSS  ││                                                        │
│                                          ││   alongside Atom? A redirect changes the format        ││                                                        │
│                                          ││   subscribers receive; keeping both doubles the feed   ││                                                        │
│                                          ││   code.                                                ││                                                        │
│                                          ││ Attempt 1  sy✓ he✓ im?  9:40 $0.80       → blocked     ││                                                        │
│                                          ││   implementation stopped with a question.              ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││ Body                                                   ││                                                        │
│                                          ││   Replace the RSS feed with an Atom feed at /atom.xml. ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          │└────────────────────────────────────────────────────────┘│                                                        │
│                                          │┌─ 5 Activity ───────────────────────────────────────────┐│                                                        │
│                                          ││ ── new since you looked: 08:15 (1h ago) ─────────────  ││                                                        │
│                                          ││ 08:40 ▶ blog t03 started                               ││                                                        │
│                                          ││ 08:50 ? blog t03 asked a question · run stopped        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
│                                          ││                                                        ││                                                        │
└──────────────────────────────── 5 tasks ─┘└────────────────────────────────────────────────────────┘└──────────────────────────────────────────────── ended ─┘
 1 Projects  j/k switch project  ⏎ open queue  ! next needing you  n register a directory  x forget  : actions  ? keys
```

---

## 3. Navigation and focus

### 3.1 Model in one sentence

Focus moves between panes that are always there (number keys, `Tab`, or `l`/`h` for deeper and
back), selection in a pane drives the panes to its right, `z` zooms any pane to read it, and a
pop-up opens only to perform one action.

### 3.2 Focus

- Exactly one pane has focus, shown by the heavy border and named first in the footer.
- `1`–`5` focus a pane directly. `Tab` and `Shift-Tab` cycle in reading order (1→2→3→4→5).
- **Deeper / back** follows the ranger, yazi and lazygit habit: `l`, `→` and `Enter` go one step
  deeper along Projects → Queue → Task → Output, and `h`, `←` and `Esc` go one step back. `Enter`
  in Output expands a tool call instead, and `Enter` in Activity jumps to the event's task.
- `Esc` never quits. Its order is: close the pop-up → clear the search or filter → leave zoom →
  focus back one pane. Pressing it repeatedly always ends on the Queue.
- `q` quits from anywhere outside a text field, with no confirmation, because quitting never
  stops a run (the footer says so on the first quit while a run is live: `run keeps going; ktask-rs
  tui to come back`). `Ctrl-C` does the same.

### 3.3 Zoom (reader)

`z` toggles the focused pane between its normal place and the whole body area. The header and
footer stay. Zoom is the reader for long text: a full resolver report, a long body, a transcript
or a diff. The pane keeps all of its keys and adds `/`, `n`, `N` search where the pane did not
already have it. `z` or `Esc` returns to the same scroll position.

### 3.4 Attempt and step selection

The selected attempt and step belong to the selected task, and Task and Output share them. When
you select a task, they are set as follows:
- The attempt is the latest one.
- The step is the running step. If no step is running, it is the step that failed. If none
  failed, it is the last agent step.

The keys `[` / `]` (previous/next attempt) and `{` / `}` (previous/next step) work while Queue,
Task or Output is focused, so you can change what Output shows without leaving the Queue. In the
diff view, `{` / `}` move between files instead of steps: they mean "previous/next section"
everywhere. The selected attempt is marked `›` in the Task pane's attempt list. The resolver run
is a step called `resolve` of the attempt it judged.

### 3.5 Pop-ups and overlays

| Kind | Used for | Behaviour |
|---|---|---|
| **Overlay** (covers the body) | Settings/Providers, Help | Keeps the header, has its own footer hints, `Esc` closes it |
| **Pop-up** (centred box) | Answer, mark done, acknowledge, remove, stop run, import, project switcher, pickers, palette | One action. Title names the object (`Answer t03`). The footer or last line lists the confirming keys. `Esc` cancels with no effect |
| **$EDITOR** (TUI suspended) | Add task, edit task, long answers (`Ctrl-O` from any text field), editing provider definitions | Terminal restored after; result validated; errors offer re-edit with your text kept |

At most two layers are open at once (for example, the model picker over Settings). Live updates
continue under every pop-up, and a pop-up whose object changes state (the task you are answering
gets answered from the CLI) closes with a toast that says why.

### 3.6 Filter and search

- **Queue filter `/`**: type to filter rows live by id, title and body (case-insensitive;
  smartcase when the text has a capital letter). `Enter` keeps the filter and returns to the
  list, and `Esc` clears it. The bottom border shows `filter "back" 2 of 15`.
- **Queue view `f`** cycles through `all but cancelled` (default) → `needs you` → `not finished`
  (pending, running, failed, blocked) → `everything, including cancelled`. The bottom border
  names the view.
- **Search `/` in Task, Output, Activity and zoom**: highlights matches. `n` and `N` move to the
  next and previous match, and the bottom border shows `match 3 of 12`.
- **Jumps**: `!` goes to the next item needing you, across all projects, in Projects-pane order:
  it selects the project and task and focuses Task. `.` goes to the running task of the current
  project. `gg` / `G` go to the top / bottom.

### 3.7 Start-up

`ktask-rs tui [--project NAME]` opens the named project. Without `--project`, it opens the
project of the current directory if that is registered. Otherwise it opens the project used last
time. If no project is registered, it opens the Projects pop-up with `n register this directory`.
Focus starts on the Queue, and the selection starts on the first task needing you, else the
running task, else the first pending task, else the last task.

### 3.8 Command palette

`:` opens a palette that lists **every action available in the current context**, with its key,
for example `retry t14  r`. It has a fuzzy filter, and `Enter` runs the action. The palette is how
you discover keys and how you reach rare actions that have no key: register or forget a project,
check all providers, export the queue to a file, mark all activity as seen, toggle mouse capture.
It is *not* a CLI prompt and takes no typed arguments, because the selection supplies the
arguments. The CLI already serves the typed form.

### 3.9 Multi-project

The TUI watches the journals of **all** registered projects, each through its own change
notification. The header shows the other projects (priority 2 and 6). `P` opens the switcher at
any size. In the L Projects pane, `j`/`k` switch the project immediately. Each project remembers
its own selection, focus, scroll positions and filter for the length of the session. Actions
always apply to the project that is shown.

### 3.10 Mouse

Mouse capture is on by default, and the palette action "toggle mouse" turns it off so the
terminal can select text. Clicking focuses a pane and selects a row, double-clicking acts as
`Enter`, the wheel scrolls the pane under the pointer, and clicking a tab or a key hint in the
footer activates it. Every mouse action also has a key.

### 3.11 Key map

Keys follow vim and lazygit habits. A key not listed for a pane does nothing there, except that
global keys work everywhere outside text fields. Lower-case keys inspect or act on the selection.
Upper-case keys change the queue or the run (`R`, `X`, `D`, `A`, `J`, `K`) or show a stronger
variant (`T` raw, `G` bottom).

**Global** (no pop-up open, not typing)

| Key | Action |
|---|---|
| `1` `2` `3` `4` `5` | Focus Projects / Queue / Task / Output / Activity |
| `Tab` / `Shift-Tab` | Next / previous pane |
| `l` `→` `Enter` / `h` `←` `Esc` | Deeper / back (§3.2) |
| `z` | Zoom the focused pane on and off |
| `P` | Project switcher pop-up |
| `!` | Next thing needing you (all projects) |
| `.` | Running task of this project |
| `R` | Start a run of this project (the same as `ktask-rs run`) |
| `X` | Stop-run pop-up (§4.10) |
| `s` | Settings overlay (`Tab` inside it for Providers) |
| `:` | Action palette |
| `?` | Help overlay: every key for the focused pane, then the global keys, searchable |
| `Ctrl-L` | Redraw |
| `q` `Ctrl-C` | Quit the TUI (the run continues) |

**Task-scoped** (Queue, Task or Output focused, a task selected; shown only when applicable)

| Key | Action | Applies to |
|---|---|---|
| `[` / `]` | Previous / next attempt | tasks with attempts |
| `{` / `}` | Previous / next step (diff: file) | tasks with attempts |
| `t` / `T` / `d` | Output: transcript / raw / diff, and focus Output | tasks with attempts |
| `r` | Retry (no confirmation; the toast offers `R`) | failed, failed-unknown, skipped |
| `a` | Answer pop-up | blocked |
| `A` | Acknowledge pop-up (optional message) | human task that is pending |
| `D` | Mark done pop-up (reason) | any task that is not done, running or cancelled |
| `x` | Remove (cancel) with confirmation | any task that is not running |
| `e` | Edit in $EDITOR | pending, failed, blocked, skipped |
| `y` / `Y` | Copy (OSC 52): Queue `y` id, `Y` id + title; Task `y` the whole pane as plain text; Output `y` the block under the cursor, `Y` the whole step | |

**2 Queue**

| Key | Action |
|---|---|
| `j` `k` `↓` `↑` | Move the selection |
| `gg` `G` `Ctrl-D` `Ctrl-U` `PgDn` `PgUp` | Top, bottom, half page, page |
| `n` | New task at the end of the queue ($EDITOR) |
| `o` / `O` | New task below / above the selection ($EDITOR) |
| `J` / `K` | Move the selected task down / up one place (written at once) |
| `i` | Import pop-up |
| `/` | Filter |
| `f` | Cycle the view (§3.6) |

**3 Task**: `j` `k` scroll a line, `gg` `G` `Ctrl-D` `Ctrl-U` `PgDn` `PgUp`, `/` `n` `N`
search, `n` (new task at the end) only when no search is active, `o` / `O` as in Queue. The
task-scoped keys also apply.

**4 Output**

| Key | Action |
|---|---|
| `j` `k` | Move the line cursor (the view scrolls with it) |
| `gg` `G` `Ctrl-D` `Ctrl-U` `PgDn` `PgUp` | As above. `G` on a live step resumes following |
| `Enter` / `E` | Expand or collapse the tool call under the cursor / all of them |
| `F` | Toggle following |
| `w` | Toggle wrapping for code, raw and diff lines (prose always wraps) |
| `<` / `>` | Scroll horizontally 8 cols when wrapping is off |
| `/` `n` `N` | Search |
| `t` `T` `d` `[` `]` `{` `}` | As task-scoped |

**5 Activity**: `j` `k` `gg` `G`, `Enter` jumps to the event, `f` toggles this project / all
projects, `m` marks everything as seen (the divider moves to now), `/` `n` `N`.

**1 Projects** (pane or pop-up): `j` `k` select (in the pane this switches at once; in the
pop-up `Enter` switches), `/` filter (the pop-up opens with the filter active), `!` next needing
you, `n` register a directory (path prompt, defaulting to the current directory, with `Tab`
completion), `x` forget, with a confirmation that names the project and says that its files are
not touched.

**Text fields** (in every pop-up): `Enter` submits, `Esc` cancels, `Ctrl-O` continues in $EDITOR
with the text so far, `Ctrl-A` `Ctrl-E` `Home` `End` move to start/end, `Ctrl-W` deletes a word,
`Ctrl-U` deletes to the start, `Ctrl-K` deletes to the end, `Alt-B` `Alt-F` move by word, and
`Tab` completes paths and names where that makes sense.

**Confirm pop-up**: `y` confirms, `n` or `Esc` cancels. `Enter` takes the highlighted choice,
which is the safe one for destructive actions (remove, forget, stop now).

**Settings overlay**: §6. **Help, palette**: `j` `k` move, type to filter, `Enter` runs (palette),
`Esc` closes.

**Deviations, with reasons**
- `h` and `l` move focus between panes instead of scrolling horizontally. This is the lazygit and
  yazi habit, and only Output needs horizontal scroll, which it gets with `<` / `>`.
- `J` / `K` move tasks, where lazygit uses `Ctrl-J` / `Ctrl-K`. Many terminals send `Ctrl-J` as
  Enter, which would make the key unreliable.
- `n` is "new" in Queue and "next match" while a search is active. This is lazygit's own split.
  The footer always shows which one applies.
- `x` removes and `D` marks done. `d` is the diff view, so neither of them uses `d`, and so that a
  mistyped `d` never deletes anything.
- `a` answers and `A` acknowledges. The two keys are next to each other, but they apply to
  disjoint states (blocked vs pending human), so a mis-press only produces "not applicable to a
  blocked task".
- `Ctrl-O` opens $EDITOR from a text field, because `Ctrl-E` is end-of-line for Emacs users and
  `Ctrl-X Ctrl-E` (bash) is two keys.

---

## 4. Interaction flows

Every key below is the key from §3.11. "Toast" is a footer message that is also written to
Activity.

### 4.1 Watching a live run

1. `ktask-rs tui`. The header shows `kt ▶ t12 implementation 14:07` and the Queue selects t12.
2. At S size, the Task tab shows the selected attempt's step list and the Live tail. At M and L
   sizes, Output is already showing the transcript and following it.
3. `4` focuses Output (at S this switches the tab). Read, and press `Enter` on a `▸ Bash cargo
   test` line to see its full output. Scrolling up stops following, and `G` resumes it.
4. Progress has three signals that need no key: the step chips advance, the time and tokens count
   up, and the **activity bar** (§5.4) empties each time output arrives. If nothing arrives it fills,
   turns amber, and at `silent-after` it is full and red; the header shows it too.
5. When the attempt moves to the next step, Output follows the new step only if you were following
   (at the bottom). Otherwise it stays where you are, and the title shows `· implementation ended
   → check`.
6. A wait or retry shows as `◷` or `↻` in the Queue, header and Task with the reset time or the
   countdown, for example `waiting: claude 5h limit, resets 01:00 (in 22m) · nothing to do`.

### 4.2 Coming back after hours

1. `ktask-rs tui`. The selection is on the first task needing you, if any.
2. The header gives the overall picture: `■ t14 needs you`, the counts and the cost.
3. Activity (visible at M/L, the first tab at S) starts with `── new since you looked: 22:14
   yesterday (9h ago) ──`, lists every event since then, and ends with
   `summary: 2 done · 1 needs you · 11 attempts · $11.40`.
4. `j`/`k` in Activity and `Enter` on an event jump to that task, attempt and step, with Output
   showing it.
5. The "last looked" time for each project is saved when the TUI exits and when you press `m`.
   The divider does not move while you are reading.

### 4.3 Diagnosing a failure and continuing

1. `!` selects the task and focuses Task. The Needs-you box says why the run stopped and lists
   only the keys that apply.
2. Read the failure reason under the failed attempt. It is wrapped and complete. `d` shows what the
   attempt changed, `t` shows the transcript of the failing step (for check, the test output), and
   `[` / `]` compare with earlier attempts.
3. Continue with one of these:
   - **Retry** `r`. The task returns to pending and the toast is `✓ t14 sent back to pending · R
     to run`. Retry does not start a run, which matches the CLI. Press `R`.
   - **Fix the task first** `e`. Edit the body in $EDITOR and save. The toast is `t14 updated`.
     Then `r`, `R`.
   - **Answer** `a` (blocked). The pop-up shows the whole question, scrollable with `Ctrl-D` /
     `Ctrl-U` when it is long. Type the answer, or press `Ctrl-O` to write it in $EDITOR, then
     `Enter`. The task returns to pending and the toast offers `R`.
   - **Mark done** `D`. The pop-up asks `Why is t14 done? (recorded with the task)`, then
     `Enter`.
   - **Acknowledge a human task** `A`. The pop-up shows the task body (what you were supposed to do)
     and an optional message line, then `Enter`. The task is done, and the toast offers `R` to
     continue the run.
   - **Remove** `x`, then `y`.
4. **Environment fault.** The Needs-you box is attached to the task the run stopped before. It
   contains `Fix:`, the operator fix from the router, in full, and the keys `R run again` and `s
   settings` (most fixes are a wrong command or branch).

### 4.4 Reading a resolver report

1. Select the task. The latest resolver report is in the Task pane under the Needs-you box, as
   WHAT / WHY / ASK in full.
2. `z` reads it at full screen, and `y` copies the whole Task pane as plain text, for example to
   paste the ASK into a task body.
3. Reports from earlier attempts: `[` selects an earlier attempt, and its report is shown under
   that attempt's line.
4. What the resolver itself did: `}` until the step is `resolve`, then `t`.
5. Supersede: the report shows `SUPERSEDE → t18 t19`, and the new tasks appear in the Queue in the
   place of the old one with `»` on the superseded row.

### 4.5 Reading a transcript

`t` (or `4`), then `j`/`k`, `Ctrl-D`/`Ctrl-U`, `Enter` to expand a tool call, `E` to expand all,
`/` to search, `[ ] { }` to change attempt or step, `z` for full screen, `T` for the raw stream,
and `w` to unwrap long lines with `<` `>` to scroll them.

### 4.6 Reading a diff

1. `d` from Queue, Task or Output. Output switches to the diff view of the selected attempt.
2. The top shows `Attempt 2 changed 3 files  +114 −8`, then one line per file with its `+`/`−`
   counts. Below come the unified hunks, with the file headers in bold, additions in the success
   colour and deletions in the failure colour (prefix `+`/`-` remains in NO_COLOR).
3. `}` / `{` jump between files. `Enter` on a file line folds or unfolds it.
4. The diff is "what this attempt left in the tree": committed changes from the attempt's start
   revision to its end revision, plus the saved leftovers if the attempt was reverted. The title
   says which: `committed as 3f2a1c9` or `reverted; saved leftovers`. If nothing changed, the pane
   says `Attempt 2 changed no files.` This depends on decision D6.

### 4.7 Adding, editing, inserting, reordering, importing

- **Add**: `n` (end of queue), `o` (below the selection), `O` (above). The TUI suspends and opens
  `$VISUAL`, then `$EDITOR`, then `vi`, on a TOML template in the same format as `import`. It
  contains the fields `title`, `kind`, `body` (multi-line string), `criteria`, `links`,
  `provider` and `model`, with comments explaining each one. Save and quit to add the task. It is
  selected and the toast is `+ t18 added after t14`. If the file is saved with an empty title,
  nothing is added and the toast is `nothing added`. If it is invalid, a pop-up shows the error
  with `e edit again (your text is kept)` / `Esc discard`.
- **Edit**: `e` opens the same template filled with the task. A running or finished task cannot be
  edited, and the footer says why.
- **Reorder**: `J` / `K` move the selected task one place and write the change at once. The
  selection follows the task. A task cannot move above the running task or into finished history,
  and the toast says `t16 can't move above the running task`.
- **Import**: `i` opens the Import pop-up. The **Path** field has `Tab` completion. As soon as the
  path names a readable file, a **preview** lists the tasks it contains (`3 tasks: t?? Reader
  accepts v2 and v3 …`) or the validation error in full. The **Position** field cycles with `←`
  / `→` through `end of queue` / `after t14` / `before t14`, where t14 is the selection. `Enter`
  imports, the new tasks are selected, and the toast is `+ 3 tasks imported after t14`.

### 4.8 Changing settings and providers

`s` opens the overlay on the Settings tab. Select a row, then press `Space` (toggle) or `Enter`
(edit). Each change is written at once. §6 has the details. `Tab` switches to Providers. `Esc`
closes the overlay.

### 4.9 Starting a run

`R`. If a run is already active, the footer error is `✗ a run is already in progress (started
22:13 from the CLI)`. Any other refusal is shown in the same words as `ktask-rs run` would use.
On success, the header changes to `▶` and the selection jumps to the task being run. Quitting the
TUI does not stop a run it started.

### 4.10 Stopping a run

`X` opens the pop-up in S6 with three choices:
- `a` stop after the current task (the safe default),
- `s` stop after the current step,
- `k` stop now, which kills the agent and loses the attempt. `k` asks `y` again.

While the stop is pending, the header shows `■ stopping after t12`. When the run has stopped, it
shows `■ stopped by you`, and Activity records who stopped it and when. Pressing `X` again while a
stop is pending offers `cancel the stop`. This flow requires a CLI `stop` (decision D2).

### 4.11 Switching projects

- At S and M sizes, `P` opens the switcher. Type to filter, then `Enter`.
- At L size, `1` focuses Projects, `j`/`k` switch the project, and `l` returns to the Queue.
- `!` crosses projects on its own.
- The header always shows which other project needs you (`blog ?`).

---

## 5. Visual language

### 5.1 The look

Preview in your own terminal: `cat ktask-notes/look/tui-look.ans` (drawn by `look/render.sh`;
the 120×32 M layout, a run in progress). This section replaces the earlier "16 colours only"
rule (D4 is reversed): the TUI has its own designed palette, and degrades cleanly.

**Palette (dark).** The terminal's own background is kept; panes never paint a background, so
the TUI sits in the user's terminal rather than over it. Colours are roles, never literals in
code:

| Role | Truecolor | Used for |
|---|---|---|
| text | `#d8dee9` | body text |
| muted | `#6b7385` | ids, times, hints, tool-call lines, pending `○` |
| line | `#3b4252` | unfocused borders, tracks of bars, separators |
| accent | `#7aa2f7` | focused border, pane title, selection marker, footer pane name |
| live | `#7dcfff` | running: `◉`, spinner, RUNNING pill, activity bar while calm |
| ok | `#9ece6a` | done `✓`, passed steps `●`, `+` lines |
| fail | `#f7768e` | failed `✗`, errors, `−` lines |
| attention | `#e0af68` | waits, retries, usage ≥ 80 %, activity bar half to full |
| human | `#bb9af7` | human tasks `◆`, questions, "needs you" |
| selection | `#282d3a` background | the selected row, full pane width |

A light palette with the same roles is chosen when the terminal's background is light (OSC 11
query at start, 100 ms timeout, dark when unanswered). `~/.config/ktask-rs/tui.toml` may say
`theme = "dark" | "light" | "auto"`.

**Depth.** Truecolor when `COLORTERM` is `truecolor` or `24bit`; else the nearest of the
xterm-256 colours; else the 16 ANSI colours by name (accent blue, live cyan, ok green, fail red,
attention yellow, human magenta, muted bright black, selection reverse video); `NO_COLOR` or
`TERM=dumb`: no colour, with the text fallbacks below. All four are pty-tested.

**Shapes.**
- Unfocused panes: rounded light borders `╭─╮ ╰─╯` in `line`. The focused pane: heavy borders
  `┏━┓ ┗━┛` in `accent`. Focus is visible by shape alone, so tests and NO_COLOR see it.
- Pane titles sit in the top border with one space each side; the status sits in the bottom
  border, right-aligned, in `muted`. One space of inner padding left and right in every pane.
- The selected row has the `selection` background across the pane's full inner width and the
  `›` marker; no reverse video except in 16-colour mode.
- State pills: the run state in the header and the status word in Task are bold dark text on
  the role colour (` RUNNING `, ` NEEDS YOU `, ` DONE `). Under NO_COLOR they are `[RUNNING]`.
- Header segments are separated by a muted `│`; no powerline glyphs (they need special fonts).

**Pipeline, compact form** (replaces the two-letter chips `sy✓ he✓ im▶ …` everywhere,
including the mock-ups above): one dot per step in pipeline order — `●` passed (ok), `◉` running
(live), `✗` failed (fail), `◐` waiting (attention), `○` not reached (line), `·` switched off
(line) — followed by the current or failing step's name: `●●◉○○○○○ implementation`. 8 cells
plus the name. The expanded list in Task gives every step's name, time, tokens and cost.

**Meters.** Activity bar (§5.4), usage meter in the header (`claude 7d ██████▊── 74%`, eighth
blocks on a `line` track, attention at 80 %, fail at 95 %), and a cost-per-hour sparkline
(`▁▂▅▇█▃`) at the bottom of Activity. No other graphics.

**Motion.** Only what is live moves: the braille spinner `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` on the running row and the
running tool call, the activity bar, the clock. 10 frames a second for the spinner, once a
second for clocks; nothing animates when no run is live.

**Type.** Bold for task titles, section labels (Body, Criteria, WHAT/WHY/ASK, Attempt n) and
pills; `muted` for metadata; no italics, no dim attribute, no underline except links.

**NO_COLOR and ASCII.** Symbols and words carry every state: `✓ ✗ ◉ ○ ◆ ◐` plus the status word;
pills become `[WORD]`; the selection is `›` plus bold. `TERM=linux` or a non-UTF-8 locale uses
ASCII: `+ x * o # ~`, borders `+-|`, bars `#` on `-`.

### 5.2 Status symbols

Each symbol is one cell wide and has no emoji presentation. The ASCII set is used when the locale
is not UTF-8 or `KTASK_ASCII=1` is set.

| State | Symbol | ASCII | Word (Task pane, header) | Colour |
|---|---|---|---|---|
| pending | `○` | `.` | PENDING | muted |
| running | `▶` | `>` | RUNNING | live |
| waiting (limit) | `◷` | `~` | WAITING | attention |
| retry back-off | `↻` | `@` | RETRYING | attention |
| done | `✓` | `+` | DONE | ok |
| failed | `✗` | `x` | FAILED | fail |
| failed-unknown | `✗` | `x` | FAILED (no report: crashed or killed at time limit) | fail |
| blocked | `?` | `?` | BLOCKED | attention |
| human, pending | `◆` | `H` | HUMAN | attention when it is next to run, else muted |
| skipped | `↷` | `s` | SKIPPED | muted |
| superseded | `»` | `S` | SUPERSEDED → t18 t19 | muted |
| cancelled | `–` | `-` | CANCELLED | muted |
| project needs you | symbol of the cause | | | attention |
| run stopped | `■` | `#` | stopped / needs you | attention or muted |

The Queue has no spinner. The running row is marked by `▶` and its time counting up. A
once-a-second tick redraws the clock cells only while some watched run is live. It is a local
timer, not journal polling, and tests freeze it with the existing test clock.

### 5.3 Pipeline per attempt

**Compact form** (attempt lines, the Queue's second line for the running task):
`sy✓ he✓ im▶ ch○ rv○ ts○ cm○ pu–`, 31 cells. The two-letter codes are fixed: `sy` sync, `he`
health-check, `im` implementation, `ch` check, `rv` review, `ts` testing, `cm` commit, `pu`
push, and `rs` resolve when the resolver ran. Step states: `✓` passed, `✗` failed, `▶` running,
`○` not reached, `–` switched off in settings, `?` asked a question, `◷` waiting.

The attempt line is `Attempt 3  <chips>  21:12 $1.90  → <decision>`, where the decision is one
of `→ retry 1/3`, `→ wait until 01:00`, `→ decide`, `→ stop`, `→ done`, `→ blocked`, `→ skip`,
`→ supersede t18 t19`.

**Expanded form** (the selected attempt in Task): one step per line, with symbol, full name, time,
and then tokens, cost and model for agent steps or the command for check and health-check steps.
The running step adds its activity bar (§5.4). It never needs more than 56 columns, so it fits at S size
without wrapping.

**Settings pipeline line**: `sync › health › impl › check › review · testing › commit · push`.
`›` joins steps that are on and `·` stands before a step that is off. The step that is off is
also shown in muted colour.

### 5.4 Activity bar

Replaces every "last output N s ago". One look answers "is it working or stuck", with no number
to read.

- **What it measures: silence.** The bar is empty the moment output arrives and fills, smoothly,
  while nothing does. Full means `silent-after` has passed (default 2 min; the scale is always
  the setting, so a full bar means the same thing in every project). Agent work has no known
  length; silence has a known limit, so the bar shows the one honest quantity.
- **How it looks.** 10 cells wide (8 at S size), on the running step's line, in the header next
  to the run state, and on the running task's Queue row at M and L sizes. The filled part uses
  the eighth-block characters `▏▎▍▌▋▊▉█`, so it grows by an eighth of a cell — about 1.5 s per
  step at the default — and visibly creeps rather than jumps. The empty part is a dim `─` track,
  so the bar's full length is always visible. No brackets, no percentage, no label while healthy.
- **Colour.** Up to half full: the success colour, dimmed — calm, barely noticed. Half to full:
  attention (amber). Full: failure (red), and the word `quiet 11m` appears after it, counting
  the whole silence. Output arriving drains the bar to empty in one redraw and the colour
  returns to calm; no flash.
- **Rhythm.** A busy agent keeps the bar near empty with small flickers; a long `cargo test`
  shows a slow creep that resets when the test prints; a stuck agent crawls to red and stays.
  The pattern is readable from across the room.
- **Fallbacks.** 16 colours: green, yellow, red. NO_COLOR: the fill character changes with the
  state — `█` calm, `▓` attention, `▒` plus the word `quiet` when full — so the state never
  depends on colour alone. ASCII-only terminals: `#` fill, `-` track.
- **Tests.** Drawn from the step's last-output time and the test clock; a pty test freezes the
  clock at 0 %, 40 %, 75 % and 110 % of `silent-after` and asserts the characters and the
  `quiet` word.
- **CLI.** `status` keeps the plain fact (`last output 3 s ago`); the bar is the TUI's way of
  showing it.

### 5.5 Wrapping and scrolling

- Prose (bodies, reasons, reports, agent messages, questions) wraps at word boundaries to the pane
  width, or at 100 columns at most. A word longer than the line is broken hard.
- Code and log lines (tool output, check output, raw, diff) wrap by default, and each continuation
  line starts with `↪`. `w` turns wrapping off, and `<` `>` then scroll horizontally.
- Every scrollable pane shows its position in the bottom border (`1–26 of 44 ↓`, with `↓` or
  `↑↓` when there is more). There is no scrollbar, because the position text can be checked by
  tests.
- **Rule for `…`**: it may appear only (a) at the end of a Queue row title, (b) at the end of a
  collapsed tool-call summary line in Output, which `Enter` expands, and (c) in a Projects row
  state. In each case the full text is one key away or in a visible pane. A test checks that no
  other pane line ends in `…`.

### 5.6 Empty states

| Where | Text |
|---|---|
| Queue, no tasks | `No tasks yet. n new task · i import a file · : all actions` |
| Queue, filter matches nothing | `Nothing matches "foo". Esc clears the filter.` |
| Task, nothing selected | `Select a task on the left.` |
| Output, task without attempts | The run plan (§2.2) |
| Output, step without output | `This step produced no output.` (plus exit code and time) |
| Diff, no change | `Attempt 2 changed no files.` |
| Activity, nothing new | `Nothing happened since you looked (22:14).` |
| No project registered | Projects pop-up: `No project registered. n registers this directory: /home/etf/work/x` |
| Providers, never checked | `not checked yet · c to check` |

### 5.7 Needs-you box, errors, confirmations, toasts

- **Needs-you box**: a rounded border in the attention colour with the title `Needs you`, at the
  top of the Task pane. It has one or two sentences saying why the run stopped or why the task is
  waiting, then the applicable keys on its last line. It appears for blocked tasks, failed and
  failed-unknown tasks the router will not retry, a human task that the run has reached, and the
  task before which a run stopped with an environment fault.
- **Errors** from an action (a refusal from the shared use-case layer) are shown in the footer in
  the fail colour, prefixed `✗`, in the CLI's words, until the next key, and are written to
  Activity. An error longer than the footer opens a pop-up with the full text instead.
- **Confirmations** exist only for actions that lose something: remove, forget a project, stop now
  (kill), and discarding an invalid edit. The pop-up names the object and the consequence, for
  example `Remove t14 "Migrate journal to v3 format"? It is cancelled and stays in history.` The
  other actions do not ask, because they can be reversed or are explicit.
- **Toasts**: one line in the footer for 4 s, prefixed `✓`, `+`, `⇅` or `!`, and also written to
  Activity so a test or a returning user can still see them. They report the TUI's own actions
  (retry, add, move, settings changed) and, from other projects, the moment a project starts
  needing you (`! blog t03 asks a question`).

### 5.8 Bell and attention

The terminal bell (BEL) rings **once per transition** into a state that needs you: a run stops
needing you, a task blocks, a run reaches a human task, or an environment fault occurs. It does
this in every watched project. It rings once more when a run finishes the whole queue. It never
rings for progress, retries or waits. `ktask-rs tui --no-bell` and `KTASK_TUI_BELL=0` turn it
off. The terminal title also changes to `■ needs you`, which most terminals show on the tab.

### 5.9 Numbers and times

Durations: `2s`, `38s`, `14:07` (mm:ss under an hour), `1h04m`. Clock times: `22:14` today,
`Tue 22:14` within the week, `2026-09-30` otherwise. Relative times are given in brackets after
absolute ones: `(9h ago)`, `(in 22m)`. Cost: `$1.10` with two decimals. Tokens: `812`, `41.2k`,
`1.3M`.

---

## 6. Settings and providers

### 6.1 Settings tab

Settings is a list of typed rows in fixed groups. It is never a form of text fields. Each row
shows its name, its current value, an edit affordance and the source of the value (`default`,
`global`, `project`). The groups and rows are:

| Group | Rows (setting name → editor) |
|---|---|
| Pipeline line | Read-only picture of the steps (§5.3) |
| Steps | `step-sync`, `step-health-check`, `step-check`, `step-review`, `step-testing`, `step-commit`, `step-push` → **checkbox** `[x]`/`[ ]`, with the step's command or provider shown next to it. `implementation` is shown as `[■] always on` and cannot be selected. |
| Agents | `provider`, `resolver-provider` → **provider picker**. `model`, `resolver-model` → **model picker** |
| Limits | `max-attempts`, `transport-retries` → **number** (`−`/`+`, or type digits). `attempt-timeout`, `silent-after` → **duration** |
| Commands | `health-check`, `check` → **command line** |
| Repository | `tracked-branch` → **branch picker** (local and remote branches, plus free text). `instructions-dir` → **path** with completion |

Editors:
- **Checkbox**: `Space` toggles it and writes the change at once. The toast is `step-testing off ·
  applies from the next attempt`. While a run is live, the toast always says when the change
  applies.
- **Number**: `+` / `-` (or `l` / `h`) change the value by 1 and write after 600 ms without a
  keypress. `Enter` opens an inline field for typing. Out-of-range values are refused inline:
  `max-attempts must be 1–20`.
- **Duration**: `Enter` opens an inline field that accepts `90s`, `15m`, `4h`, `1h30m` or a plain
  number of seconds. The parsed value is shown live to the right (`= 1h30m`), and an invalid entry
  shows `expected a duration like 90s, 15m, 4h` and cannot be submitted.
- **Command line**: `Enter` opens an inline single-line field with the full current value, which
  scrolls horizontally inside the field. `Ctrl-O` edits it in $EDITOR. The row's full value is
  always visible by wrapping on extra lines when it is long, and is never cut.
- **Provider picker**: a pop-up listing the configured providers with their check status (`✓
  claude  checked 09:02`, `✗ codex  not on PATH`, `○ echo`). `Enter` selects one. Choosing a
  provider that failed its check asks for confirmation.
- **Model picker**: a pop-up with the models known for the chosen provider, the current one marked,
  and a final row `other… (type a model name)`.
- **Reset**: `u` resets the selected row to its inherited value (the source becomes `global` or
  `default`). This needs `settings unset` in the CLI (decision D2).
- `/` finds a row by name. `j` `k` `gg` `G` move.

Settings are per project, for the project shown. The overlay title names it (`Settings · kt`).

### 6.2 Providers tab

On the left is a list of providers: symbol for the check result, name, and kind (built-in /
user-defined). On the right (below the list at S size) are the selected provider's details: the
command it runs, its models, which settings use it, and the output of its last check in full,
with the time. Keys: `c` checks the selected provider, `C` checks all of them (the results arrive
row by row), `e` opens the providers configuration file in $EDITOR and validates it on return,
showing any error with re-edit (decision D7), and `Enter` on a provider shows its full details in
zoom.

---

## 7. What we deliberately do not do

- **A built-in multi-line text editor.** Task bodies and long answers go to $EDITOR. Users of this
  tool already have a good editor, and building one in ratatui is costly and worse.
- **One screen per CLI command.** There is no "list screen" or "status screen". Commands are
  actions on the selection, and their results appear in the panes that already exist.
- **A typed command line (`:retry t14`).** The palette lists actions but takes no arguments. The
  typed form is the CLI, and keeping two grammars in step is pure cost.
- **User theme files.** One designed palette, light and dark, is the product; a theme language is maintenance for nobody.
- **Spinners, progress bars and percentages for agent steps.** Agent work has no known length, so
  a progress bar would be invented. The pipeline chips, the clocks and the activity bar (§5.4, it measures silence, which has a known limit) are honest.
- **Polling.** Journal changes come from the change notification. The only timer is the
  once-a-second clock, and only while a run is live.
- **Desktop notifications.** The bell and terminal title cover the need, and reaching the desktop
  pulls in D-Bus and platform code for a terminal tool.
- **Confirmation for reversible actions.** Retry, answer, move, toggle and import do not ask. Only
  actions that lose something ask.
- **Editing finished history.** Done, superseded and cancelled tasks cannot be edited or moved. The
  journal is a record.
- **Per-task tabs or windows, and split-screen comparison of two attempts.** `[` / `]` switch
  attempts in place. Two-attempt diffs can come later if they are actually needed.

---

## 8. Acceptance scenarios (pty)

Every scenario drives the real binary in a pseudo-terminal with the existing frame-synchronised
harness, with the clock frozen and the `echo` provider or a scripted provider. "Visible" means the
text appears in the frame. The size is 80×24 unless stated.

1. **Side by side.** A project with 5 pending tasks and no run. Keys: none. Visible: the line
   containing `› ○ t1` is in a heavy-bordered pane titled `2 Queue`, and `[3 Task]` with t1's full
   title, `PENDING` and the first line of its body is in the same frame. Footer contains `R run`.
2. **Never cut.** t1 has a 2,000-character body, at 80×24. Keys: `l`, `G`. Visible: the last
   sentence of the body. No line inside the Task pane ends in `…`. Then keys `z`, `gg`: the Task
   pane spans the full width (its border reaches column 80) and the first body line is visible.
3. **Run in progress.** Keys: `R`. Visible: the header contains `▶ t1 implementation`, the Task
   pane contains `RUNNING`, `Attempt 1` and `▶ implementation`, and the Live section shows the
   provider's latest line.
4. **Failure needs you.** The scripted provider fails check three times and the resolver answers
   stop with WHAT/WHY/ASK. Keys: `R`, wait for the frame where the header contains `■ t1 needs
   you`, then `!`. Visible: `Needs you`, `r retry`, `WHAT`, `WHY`. Keys `j` until the ASK text's
   last word is visible, and it must be reached without the pane cutting any line.
5. **Retry and continue.** Continues 4. Keys: `r`. Visible: the footer `t1 sent back to pending`,
   the Queue row symbol `○` for t1, no `Needs you`. Keys: `R`. Visible: the header `▶ t1`.
6. **Answer a question.** The scripted provider reports blocked with a question. Keys: `!`, `a`,
   type `use 301`, `Enter`. Visible: `PENDING` in Task, `answered` in Activity (Activity tab at S:
   press `5`). `ktask-rs status` agrees.
7. **Acknowledge a human task.** The queue is a human task t2 after t1. The run stops at t2. Keys:
   `!`, `A`, `Enter`. Visible: the t2 row symbol `✓` and the toast offers `R`.
8. **Coming back.** The journal holds events after the saved "last looked" time. Start the TUI at
   80×24. Visible: the active tab is `[5 Activity]` with `new since you looked` and a `summary:`
   line. Keys `Enter` on the `✗ t1 attempt 1 failed` line: Task is focused and the `›` marker is on
   attempt 1.
9. **Transcript and attempts.** t1 has 2 attempts. Keys: `t`. Visible: `[4 Output]`, `Attempt 2 ·
   implementation`. Keys: `[`. Visible: `Attempt 1`. Keys: `}`. Visible: `· check`. Keys:
   `Enter` on a `▸ Bash` line. Visible: `▾ Bash` and a `│`-prefixed output line.
10. **Diff.** The attempt changed 2 files. Keys: `d`. Visible: `changed 2 files` and both paths.
    Keys `}`: the second path's header is at the top of the pane.
11. **Add and insert.** `EDITOR` is a script that writes a template with the title `Inserted`. Select
    t2, then keys `O`. Visible: the row `t6 Inserted` directly above t2, and selected. `ktask-rs
    list` shows the same order.
12. **Reorder.** Select t4, keys `K`, `K`. Visible: t4 above t2. The toast shows `⇅`. `ktask-rs
    list` agrees.
13. **Import with preview.** Keys: `i`, type the path to a 3-task TOML file, `→` until `after t2`,
    `Enter`. Visible before `Enter`: `3 tasks`. After: the three titles directly below t2.
14. **Settings by type.** Keys: `s`, `/testing`, `Enter`, `Space`. Visible: `[ ] testing`, and
    `ktask-rs settings show` prints step-testing off. Keys: `/attempt-timeout`, `Enter`, type `abc`.
    Visible: `expected a duration`. Keys: `Ctrl-U`, type `90m`, `Enter`. Visible: `1h30m` and
    `project`.
15. **Stop after task.** A run is in progress. Keys: `X`. Visible: `Stop the run on`, `a  stop after
    t1`. Keys: `a`. Visible: the header `stopping after t1`, and later `stopped by you`, with t2
    still `○`.
16. **Several projects and resize.** Two projects, with blog blocked. Start in kt at 80×24. Visible:
    the header contains `blog ?`. Keys: `!`. Visible: the header starts with `blog`, and Task shows
    the question. Resize to 160×45. Visible: `1 Projects`, `5 Activity` and `4 Output` titles, and
    the `›` marker still on blog's t03.
17. **NO_COLOR.** Run scenario 4 with `NO_COLOR=1`. The raw byte stream contains no SGR colour
    parameters (30–37, 39–47, 49, 90–97, 100–107, 38;, 48;). The focused pane still has a heavy
    border, and the Needs-you box is still present.

Scenarios 16 and 17 are extras over the requested 15 and are cheap with the harness.

---

## 9. Delivery slices

Each slice replaces part of the old TUI completely and leaves no screen half old and half new.
Old screens that have not been replaced yet are opened from the new workspace with their current
keys until the slice that replaces them ships.

1. **Workspace, at S size.** Header, Queue and Task side by side, footer key hints, heavy border
   for focus, `1`–`5` / `Tab` / `h` `l` focus, zoom `z`, wrapping with the `…` rule, the colour
   roles and status symbols with NO_COLOR and ASCII fallbacks. The Task pane shows title, state,
   body, criteria, links and the attempt list with full failure reasons. *The owner can read any
   task in full beside the queue.* Scenarios 1 and 2.
2. **Diagnose and continue.** The pipeline chips and expanded step list, router decisions, the
   resolver WHAT/WHY/ASK, the Needs-you box, `!` and `.`, retry / answer / mark done / acknowledge
   / remove as pop-ups, the header's run state, the bell, and toasts written to Activity. *The
   reason a run stopped and what to do next are on screen.* Scenarios 4, 5, 6 and 7.
3. **Live output.** The Output pane (transcript with collapsible tool calls, raw, follow, search,
   `[ ] { }`), the Live tail at S, the M layout with Output under Task, and silence warnings.
   Retires the old output screen. Scenarios 3 and 9.
4. **Coming back.** The Activity pane with the "new since you looked" divider and summary, `Enter`
   jumps, cost and usage in the header and per attempt, and the L three-column layout. Scenario 8.
5. **Run control.** `R` with refusals in the footer, the `X` stop pop-up (after task, after step,
   now), and the `stopping` / `stopped by you` states. Needs CLI `stop` (D2). Scenario 15.
6. **Planning.** Add / insert / edit through the $EDITOR template, `J`/`K` reorder, import with
   preview and position, the run plan in Output, and Queue filter and views. Needs CLI `edit` and
   `move` (D2). Scenarios 11, 12 and 13.
7. **Settings and providers.** The overlay with typed rows, pickers, inline duration and number
   editors, the pipeline line, reset, and the Providers tab with checks. Retires the old settings
   and providers screens. Scenario 14.
8. **Several projects.** All journals watched, the Projects pane (L) and `P` pop-up, other-project
   attention in the header, cross-project `!` and bell, register and forget. Retires the old
   project switcher. Scenarios 16 and 17.
9. **Diff, palette, mouse.** The diff view of each attempt (needs D6), the `:` palette with every
   action, the `?` overlay generated from the same action table, mouse support, and `y` copy.
   Scenario 10.

Slices 1–4 are the core. After slice 4 the owner can watch, come back, diagnose and continue
entirely in the TUI.

---

## 10. Decisions for the owner

- **D1. $EDITOR for adding and editing tasks**, using the import TOML template, with no in-TUI
  multi-line editor.
- **D2. New CLI commands, to keep every TUI action possible from the CLI**: `ktask-rs edit <id>`
  (opens $EDITOR, or `--from FILE`), `ktask-rs move <id> --before|--after <id>`, `ktask-rs stop
  [--after-task|--after-step|--now]`, and `ktask-rs settings unset <name>`.
- **D3. The TUI watches all registered projects at once**, with the bell and `!` working across
  them. The alternative is to watch only the shown project, which is cheaper but blind to the
  overnight case.
- **D4. (Reversed 2026-10-08.)** A designed palette in truecolor with 256, 16 and no-colour fallbacks, light and dark (§5.1).
- **D5. "Last looked"** is stored per project in TUI state (not the journal). It is updated on exit
  and on `m`.
- **D6. Diff data**: the journal must record each attempt's start and end revision and where the
  saved leftovers are. This resolves the open question in `design-notes.md` in favour of "the TUI
  shows what attempt N left behind".
- **D7. Providers are edited by opening their configuration file in $EDITOR.** The TUI and CLI only
  list and check them.
- **D8. Retry does not start a run**, matching the CLI. The toast offers `R`.

---

**Owner's decision, 2026-10-07.** The review → fix loop (M8) comes first; this TUI is M9.
The section 10 decisions are taken as recommended unless the owner says otherwise: new CLI
commands `edit`, `move`, `stop`, `settings unset`; task add/edit through `$EDITOR` with the
import TOML; journal records each attempt's start and end revision and its saved leftovers;
the TUI watches every registered project; designed palette with fallbacks (§5.1); providers edited in their
file through `$EDITOR`; retry does not start a run. Activity bar per §5.4. Slices become
small tasks, never one task per slice.
