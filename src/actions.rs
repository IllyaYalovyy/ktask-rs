use crate::{
    app::{Action, App, Catalog, View},
    dialog::{Dialog, DialogKind, Field},
    mock,
    presentation::{Attempt, Kind, RevisionState, Status, Task},
};
use std::time::Instant;

impl App {
    pub fn enabled(&self, action: Action) -> bool {
        let current = self.current();
        let selected = self.selected_task();
        match action {
            Action::Run => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.definition().kind == Kind::Ai
                            && match t.status() {
                                Status::Pending | Status::Paused => true,
                                Status::Waiting => self.service.is_some(),
                                // Saving an answer retains the waiting attempt until resumed.
                                Status::Question => self.service.as_ref().is_some_and(|service| {
                                    service
                                        .open_question(t.id as i64)
                                        .is_ok_and(|question| question.is_none())
                                }),
                                _ => false,
                            }
                    })
            }
            Action::Retry | Action::Diagnose => {
                self.view == View::Current && current.is_some_and(|t| t.status().failed())
            }
            Action::Approve | Action::Reject => {
                self.view != View::History
                    && selected.is_some_and(|t| {
                        t.draft().is_some()
                            && (matches!(t.status(), Status::New | Status::Pending)
                                || t.status().failed())
                    })
            }
            Action::Models => {
                self.view != View::History
                    && selected.is_some_and(|t| {
                        matches!(t.status(), Status::New | Status::Pending) || t.status().failed()
                    })
            }
            Action::Resolve => {
                self.view == View::Current && current.is_some_and(|t| t.status().failed())
            }
            Action::Pause => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.status() == Status::Running
                            || (self.service.is_some() && t.status() == Status::Waiting)
                    })
            }
            Action::Answer => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.status() == Status::Question
                            && self.service.as_ref().is_none_or(|service| {
                                service
                                    .open_question(t.id as i64)
                                    .is_ok_and(|question| question.is_some())
                            })
                    })
            }
            Action::Acknowledge => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.definition().kind == Kind::Human && t.status() == Status::Pending
                    })
            }
            // Only a stored attempt that no supervisor owns can be abandoned.
            Action::Abandon => {
                self.view == View::Current
                    && self.service.is_some()
                    && current.is_some_and(|t| {
                        matches!(
                            t.status(),
                            Status::Question | Status::Waiting | Status::Paused
                        )
                    })
            }
            Action::Output => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.attempts
                            .last()
                            .is_some_and(|a| !a.report.is_empty() || a.question.is_some())
                    })
            }
            Action::Edit => {
                self.view != View::History
                    && selected.is_some_and(|t| {
                        matches!(t.status(), Status::New | Status::Pending) || t.status().failed()
                    })
            }
            Action::Breakdown => {
                self.view != View::History
                    && selected.is_some_and(|t| {
                        matches!(t.status(), Status::New | Status::Pending)
                            || (t.status().failed() && self.current().is_some_and(|c| c.id == t.id))
                    })
            }
            Action::Delete | Action::Move => {
                self.view == View::Queue
                    && selected.is_some_and(|t| {
                        t.status() == Status::Pending
                            || (self.service.is_some() && t.status() == Status::New)
                    })
            }
            Action::Insert | Action::Import => self.view == View::Queue,
            Action::Undo => {
                self.view == View::Queue && (self.undo.is_some() || self.durable_undo.is_some())
            }
            Action::Inspect => selected.is_some(),
            Action::Filter | Action::Search | Action::Clear | Action::Export => {
                matches!(self.view, View::Queue | View::History)
            }
            _ => true,
        }
    }

    pub fn act(&mut self, action: Action) {
        if self.search_before.is_some() {
            match action {
                Action::Submit => {
                    self.search_before = None;
                }
                Action::Cancel => {
                    if let Some(catalog) = self.search_before.take() {
                        *self.catalog_mut() = catalog;
                    }
                }
                _ => return,
            }
            self.reconcile();
            return;
        }
        if self.service.is_some() && self.persistent_action(action) {
            return;
        }
        if !self.enabled(action) {
            self.message("Unavailable here · only current can execute; running work and History are read-only");
            return;
        }
        let had_dialog = self.dialog.is_some();
        match action {
            Action::View(view) => self.switch(view),
            Action::Back => self.switch(self.previous),
            Action::Quit => self.quit = true,
            Action::Cancel => {
                self.dialog = None;
            }
            Action::Submit => self.submit_dialog(),
            Action::Position(delta) => self.adjust_position(delta),
            Action::Fields(delta) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.focus = dialog
                        .focus
                        .saturating_add_signed(delta as isize * dialog.field_capacity as isize)
                        .min(dialog.fields.len().saturating_sub(1));
                }
            }
            Action::Approve => self.approve_selected(),
            Action::Reject => self.open_reject(),
            Action::Models => self.open_models(),
            Action::Resolve => self.open_resolution(),
            Action::Field(index) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.focus = index;
                    if let Some(field) = dialog.fields.get_mut(index) {
                        if !field.choices.is_empty() {
                            dialog.error.clear();
                        }
                        field.cycle();
                    }
                }
            }
            Action::Scroll(delta) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.scroll = dialog
                        .scroll
                        .saturating_add_signed(delta as isize)
                        .min(dialog.max_scroll);
                }
            }
            Action::Select(id) => {
                self.catalog_mut().selected = Some(id);
            }
            Action::PreviousAttempt | Action::NextAttempt => {
                self.change_attempt(matches!(action, Action::NextAttempt))
            }
            Action::Search => self.search_before = Some(self.catalog().clone()),
            Action::Clear => {
                self.catalog_mut().query.clear();
                self.catalog_mut().filter = 0;
                self.reconcile();
            }
            Action::Filter => {
                self.catalog_mut().filter = (self.catalog().filter + 1) % 3;
                self.reconcile();
            }
            Action::Follow => self.follow = true,
            Action::Run | Action::Retry => self.start_current(),
            Action::Pause => {
                if let Some(task) = self.queue.first_mut()
                    && let Some(attempt) = task.attempts.last_mut()
                {
                    attempt.state = Status::Paused;
                    attempt.outcome = "Paused".into();
                    attempt
                        .logs
                        .push("Paused by user; progress retained".into());
                }
                self.record("Current paused · later tasks will not start");
            }
            Action::Acknowledge => self.complete_current("Human task acknowledged"),
            Action::Add | Action::Insert | Action::Edit => self.open_task(action),
            Action::Delete => {
                if let Some(task) = self.selected_task() {
                    self.dialog = Some(Dialog::new(
                        DialogKind::Delete(task.id),
                        "Delete pending task?",
                        &format!(
                            "#{} {}\nConfirm removes it from Queue. Undo can restore it.",
                            task.id,
                            task.definition().title
                        ),
                        vec![],
                    ));
                }
            }
            Action::Undo => self.restore_deleted(),
            Action::Move => {
                if let Some(task) = self.selected_task() {
                    self.dialog = Some(Dialog::new(
                        DialogKind::Move(task.id),
                        "Move pending task",
                        "Choose a position in the full execution order.\nBlank means end of Queue.",
                        vec![Field::text("Before task ID", "")],
                    ));
                }
            }
            Action::Diagnose => {
                let id = self.current().expect("enabled current").id;
                self.dialog = Some(Dialog::new(
                    DialogKind::Repair(id),
                    "Remediate failure · capable model (mock)",
                    "Use a stronger execution revision or insert an ordinary repair task.\nApproval follows Configuration policy; no separate proposal queue.",
                    vec![Field::choice(
                        "Strategy",
                        &["Escalate model", "Insert repair task"],
                        "Escalate model",
                    )],
                ));
            }
            Action::Breakdown => {
                if let Some(task) = self.selected_task() {
                    self.dialog = Some(Dialog::new(
                        DialogKind::Breakdown(task.id),
                        "Break down task · capable model (mock)",
                        &format!(
                            "Replace #{} in place with the edited subtasks below.\nOriginal and attempts move to History. Cancel changes nothing.",
                            task.id
                        ),
                        vec![
                            Field::text("Step 1 title", "Implement the focused change"),
                            Field::text("Step 2 title", "Verify the acceptance checks"),
                        ],
                    ));
                }
            }
            Action::Answer => {
                let id = self.current().expect("enabled current").id;
                self.dialog = Some(Dialog::new(
                    DialogKind::Answer(id),
                    "Answer AI question",
                    "Which export format should be used?\nAn answer is required before current can continue.",
                    vec![Field::text("Answer", "")],
                ));
            }
            Action::Stats => self.open_stats(),
            Action::Events => {
                self.dialog = Some(Dialog::new(
                    DialogKind::Events,
                    "Project events",
                    &self.events.join("\n"),
                    vec![],
                ))
            }
            Action::Help => self.open_help(),
            Action::Output => {
                self.show_log = !self.show_log;
                self.follow = true;
            }
            Action::Abandon => {
                self.message("Abandon applies to stored attempts only");
            }
            Action::Inspect => {
                if let Some(task) = self.selected_task() {
                    self.open_inspect(task.id, task.attempts.len().saturating_sub(1));
                }
            }
            Action::Configure => self.open_config(),
            Action::ProjectModels | Action::ProjectChecks => {}
            Action::Import => {
                self.dialog = Some(Dialog::new(
                    DialogKind::Import,
                    "Import · in-memory demo files",
                    "Available: /demo/import.tsv (valid), /demo/invalid.tsv\nOnly simulated files are read; no disk access.",
                    vec![Field::text("Source", "/demo/import.tsv")],
                ))
            }
            Action::Export => {
                self.dialog = Some(Dialog::new(
                    DialogKind::Export,
                    "Export · in-memory demo files",
                    "Writes to the session's virtual file shelf, NOT disk.\nUse the destination in Import to read it back.",
                    vec![
                        Field::text("Destination", "/demo/export.tsv"),
                        Field::choice(
                            "Scope",
                            &["Queue", "History", "All"],
                            if self.view == View::History {
                                "History"
                            } else {
                                "Queue"
                            },
                        ),
                    ],
                ))
            }
        }
        if self.service.is_some() && !had_dialog {
            self.prepare_persistent_dialog();
        }
    }

    pub(crate) fn start_current(&mut self) {
        let Some(task) = self.current() else { return };
        if task.ready().is_none() {
            self.message("Approve a revision before execution");
            return;
        }
        let resume = task.status() == Status::Paused;
        let settings = &task.definition().settings;
        if !resume && settings.attempt_limit > 0 && task.used() >= settings.attempt_limit {
            self.message("Attempt budget exhausted · change limits in Models or resolve current");
            return;
        }
        let id = task.id;
        let cleanup = self.last_workspace != Some(id) || (!resume && self.clean_retry);
        if cleanup {
            self.record(format!(
                "Workspace #{id}: useful work checkpointed (mock); existing attempts retained"
            ));
            if self.cleanup_fail_once {
                self.cleanup_fail_once = false;
                self.record(
                    "Cleanup blocked (mock) · work preserved · retry Run/Retry to check again",
                );
                return;
            }
            self.record(format!("Workspace #{id}: owned scratch cleaned (mock)"));
        } else {
            self.record(format!(
                "Workspace #{id}: retained for same-task retry/resume (mock)"
            ));
        }
        self.last_workspace = Some(id);
        let task = self.queue.first_mut().expect("current");
        if resume {
            if let Some(attempt) = task.attempts.last_mut() {
                attempt.state = Status::Running;
                attempt.outcome = "Running".into();
                attempt.logs.push("Resumed same attempt and session".into());
            }
        } else {
            let session = task.attempts.last().and_then(|a| a.session).unwrap_or(id);
            task.attempts
                .push(Attempt::start(task.definition().clone(), session));
        }
        self.last_tick = Instant::now();
        self.follow = true;
        self.record("Current running · simulation only");
    }

    fn open_task(&mut self, action: Action) {
        let edit = matches!(action, Action::Edit);
        let insert = matches!(action, Action::Insert);
        let task = if edit { self.selected_task() } else { None };
        let mut fields = vec![
            Field::text("Title", task.map_or("", |t| t.definition().title.as_str())),
            Field::text("Goal", task.map_or("", |t| t.definition().goal.as_str())),
            Field::text(
                "Acceptance check",
                task.map_or("", |t| t.definition().check.as_str()),
            ),
            Field::choice(
                "Kind",
                &["AI", "Human"],
                task.map_or("AI", |t| t.definition().kind.label()),
            ),
        ];
        if insert {
            fields.push(Field::text("Before task ID", ""));
        }
        self.dialog = Some(Dialog::new(
            DialogKind::Task {
                id: task.map(|t| t.id),
                insert,
            },
            if edit {
                "Edit task"
            } else if insert {
                "Insert task"
            } else {
                "Add task"
            },
            "Title, goal, and acceptance check are required.\nSave applies changes; cancel keeps the original.",
            fields,
        ));
    }

    pub fn insertion_position(
        &self,
        text: &str,
        excluding: Option<usize>,
    ) -> Result<usize, String> {
        let tasks: Vec<_> = self
            .queue
            .iter()
            .filter(|t| Some(t.id) != excluding)
            .collect();
        let position = if text.is_empty() {
            tasks.len()
        } else {
            let id: usize = text
                .trim_start_matches('#')
                .parse()
                .map_err(|_| "Enter a pending task ID, or leave blank for end of Queue")?;
            tasks
                .iter()
                .position(|t| t.id == id)
                .ok_or("That ID is not an insertion target in Queue")?
        };
        if position == 0 && tasks.first().is_some_and(|t| t.status().pinned()) {
            return Err("Cannot insert before running current".into());
        }
        Ok(position)
    }

    pub fn position_preview(&self, text: &str, excluding: Option<usize>) -> String {
        match self.insertion_position(text, excluding) {
            Ok(pos) => {
                let tasks: Vec<_> = self
                    .queue
                    .iter()
                    .filter(|t| Some(t.id) != excluding)
                    .collect();
                let before = if pos == 0 {
                    "start".into()
                } else {
                    format!("#{}", tasks[pos - 1].id)
                };
                let after = tasks
                    .get(pos)
                    .map_or("end".into(), |t| format!("#{}", t.id));
                format!("Position: after {before} / before {after}")
            }
            Err(error) => error,
        }
    }

    fn restore_deleted(&mut self) {
        if let Some((position, task)) = self.undo.take() {
            let min = usize::from(self.current().is_some_and(|t| t.status().pinned()));
            let position = position.max(min).min(self.queue.len());
            let id = task.id;
            self.queue.insert(position, task);
            self.reveal(id);
            self.record("Deleted task restored");
        }
    }

    fn adjust_position(&mut self, delta: i32) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let Some((field, excluding)) = dialog.position_field() else {
            return;
        };
        let tasks: Vec<_> = self
            .queue
            .iter()
            .filter(|t| Some(t.id) != excluding)
            .collect();
        let current = self
            .insertion_position(dialog.value(field), excluding)
            .unwrap_or(tasks.len());
        let min = usize::from(tasks.first().is_some_and(|t| t.status().pinned()));
        let next = current
            .saturating_add_signed(delta as isize)
            .clamp(min, tasks.len());
        let value = tasks.get(next).map_or(String::new(), |t| t.id.to_string());
        if let Some(dialog) = &mut self.dialog {
            dialog.fields[field].value = value;
            dialog.fields[field].cursor = dialog.fields[field].value.len();
            dialog.scroll = 0;
            dialog.error.clear();
        }
    }

    pub fn reveal(&mut self, id: usize) {
        self.switch(View::Queue);
        self.queue_catalog.query.clear();
        self.queue_catalog.filter = 0;
        self.queue_catalog.selected = Some(id);
    }

    fn open_config(&mut self) {
        self.dialog = Some(Dialog::new(
            DialogKind::Config,
            "Configuration",
            "Mouse and keyboard are supported equally.\nChanging Demo dataset resets this mock workspace only.",
            vec![
                Field::choice(
                    "Mouse",
                    &["Enabled", "Disabled"],
                    if self.mouse_enabled {
                        "Enabled"
                    } else {
                        "Disabled"
                    },
                ),
                Field::text("Step delay (ms)", &self.tick_ms.to_string()),
                Field::choice(
                    "Demo dataset",
                    &["Keep", "Guided", "Empty", "Large", "Waits", "Approvals"],
                    "Keep",
                ),
                Field::choice(
                    "Planner approval",
                    &["Automatic", "Review required"],
                    if self.review_required {
                        "Review required"
                    } else {
                        "Automatic"
                    },
                ),
                Field::choice(
                    "Cleanup simulation",
                    &["Success", "Fail next switch"],
                    if self.cleanup_fail_once {
                        "Fail next switch"
                    } else {
                        "Success"
                    },
                ),
                Field::choice(
                    "Same-task retry",
                    &["Keep useful work", "Clean after checkpoint"],
                    if self.clean_retry {
                        "Clean after checkpoint"
                    } else {
                        "Keep useful work"
                    },
                ),
                Field::choice(
                    "Continue queue",
                    &["Disabled", "Enabled"],
                    if self.auto_advance {
                        "Enabled"
                    } else {
                        "Disabled"
                    },
                ),
            ],
        ));
    }

    fn open_help(&mut self) {
        self.dialog = Some(Dialog::new(
            DialogKind::Help,
            "Keyboard help",
            "1 Current  2 Queue  3 History  4 Config  5 About\nTab / Shift-Tab: next / previous screen\nClick tabs, rows, buttons, and fields; wheel scrolls.\nCurrent: r run/resume, p pause, t retry, d remediate\nCurrent: z resolve failed; x abandon a waiting attempt\nCurrent: c models/limits; v approve; n reject\nCurrent: y acknowledge Human, w answer AI\nCurrent: o toggles report / raw log; l all events\nQueue: a add, i insert, e edit, d delete, u undo\nQueue: m move, b break down, o import, x export\nQueue: c models/limits; v approve; n reject\nQueue/History: / search, f filter, Esc clear\nj/k or arrows: move/scroll; PgUp/PgDn: page\nHome/End: first/last; End follows current output\nEnter: inspect selected task; [ / ]: inspect attempts\ns: project statistics; ?: help; q: quit\nForms: Tab/arrows focus; Space cycles choices\nForms: Ctrl-A clears field; Ctrl-S/Enter applies\nEsc cancels dialogs/search; Ctrl-C always quits\nSearch: Enter applies; Esc restores previous query\nAll tasks, models, runs, and files are simulated.",
            vec![],
        ));
    }

    fn open_stats(&mut self) {
        let tasks: Vec<_> = self.queue.iter().chain(&self.history).collect();
        let cost: usize = tasks.iter().map(|t| t.cost()).sum();
        let failed = tasks
            .iter()
            .flat_map(|t| &t.attempts)
            .filter(|a| a.outcome.starts_with("Failed"))
            .count();
        let steps: usize = tasks.iter().flat_map(|t| &t.attempts).map(|a| a.step).sum();
        let elapsed_ms: u64 = tasks
            .iter()
            .flat_map(|t| &t.attempts)
            .map(|a| a.elapsed_ms)
            .sum();
        self.dialog = Some(Dialog::new(
            DialogKind::Stats,
            "Project statistics · simulated",
            &format!(
                "Tasks: {}\nQueue: {}\nHistory: {}\nCompleted: {}\nFailed attempts: {failed}\nExecution steps: {steps}\nElapsed execution: {:.1}s (simulated)\nTotal cost: ${:.2} (simulated)\nOverrides: {} · Skipped: {} · Superseded: {}",
                tasks.len(),
                self.queue.len(),
                self.history.len(),
                self.history
                    .iter()
                    .filter(|t| t.status() == Status::Done)
                    .count(),
                elapsed_ms as f64 / 1000.0,
                cost as f64 / 100.0,
                self.history
                    .iter()
                    .filter(|t| t.status() == Status::Override)
                    .count(),
                self.history
                    .iter()
                    .filter(|t| t.status() == Status::Skipped)
                    .count(),
                self.history
                    .iter()
                    .filter(|t| t.status() == Status::Superseded)
                    .count()
            ),
            vec![],
        ));
    }

    fn open_inspect(&mut self, id: usize, attempt: usize) {
        let Some(task) = self.task(id) else {
            return;
        };
        let selected = task.attempts.get(attempt);
        let body = format!(
            "#{} {}\n{} · {}\n\nGoal: {}\nAcceptance check: {}\nTotal cost: ${:.2} (simulated)\nAnswer: {}\n\nAttempt {} / {}\n{}\n{}",
            task.id,
            task.definition().title,
            task.status().label(),
            task.definition().kind.label(),
            task.definition().goal,
            task.definition().check,
            task.cost() as f64 / 100.0,
            if task.answer.is_empty() {
                "None"
            } else {
                &task.answer
            },
            if selected.is_some() { attempt + 1 } else { 0 },
            task.attempts.len(),
            selected.map_or("No attempts yet", |a| a.outcome.as_str()),
            selected.map_or(String::new(), |a| a.logs.join("\n"))
        );
        let pending = task.draft().map_or(String::new(), |r| {
            format!(
                "\nPending revision {}: {} (needs approval)",
                r.number, r.title
            )
        });
        let snapshot = selected.map_or(String::new(), |a| {
            format!(
                "\nAttempt inputs: revision {} · session {}\n{}\n{}",
                a.revision.number,
                a.session.map_or("not recorded".into(), |id| id.to_string()),
                a.outcome,
                a.revision.summary()
            )
        });
        let body = format!(
            "{body}\n\n{}\nResolution: {}{pending}{snapshot}",
            task.definition().summary(),
            task.resolution_reason
        );
        self.dialog = Some(Dialog::new(
            DialogKind::Inspect { id, attempt },
            "Task details / attempts",
            &body,
            vec![],
        ));
    }

    fn change_attempt(&mut self, next: bool) {
        if let Some(Dialog {
            kind: DialogKind::Inspect { id, attempt },
            ..
        }) = &self.dialog
        {
            let (id, attempt) = (*id, *attempt);
            let last = self
                .task(id)
                .map_or(0, |t| t.attempts.len().saturating_sub(1));
            self.open_inspect(
                id,
                if next {
                    (attempt + 1).min(last)
                } else {
                    attempt.saturating_sub(1)
                },
            );
            if self.service.is_some() {
                self.prepare_persistent_dialog();
            }
        }
    }

    fn submit_dialog(&mut self) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        match self.apply_dialog(&mut dialog) {
            Ok(true) => {}
            Ok(false) => self.dialog = Some(dialog),
            Err(error) => {
                dialog.error = error;
                self.dialog = Some(dialog);
            }
        }
        self.reconcile();
    }

    fn apply_dialog(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
        if self.service.is_some() {
            return self.apply_persistent_dialog(dialog);
        }
        match dialog.kind.clone() {
            DialogKind::Task { id, insert } => self.save_task(dialog, id, insert)?,
            DialogKind::Abandon(_) => return Err("Abandon applies to stored attempts only".into()),
            DialogKind::Delete(id) => {
                let pos = self
                    .queue
                    .iter()
                    .position(|t| t.id == id && t.status() == Status::Pending)
                    .ok_or("Task is no longer pending")?;
                let task = self.queue.remove(pos);
                self.undo = Some((pos, task));
                self.record(format!("#{id} deleted · Undo available"));
            }
            DialogKind::Move(id) => {
                let position = self.insertion_position(dialog.value(0), Some(id))?;
                let old = self
                    .queue
                    .iter()
                    .position(|t| t.id == id && t.status() == Status::Pending)
                    .ok_or("Only pending work can move")?;
                let task = self.queue.remove(old);
                self.queue.insert(position, task);
                self.reveal(id);
                self.record(format!("#{id} moved · execution order updated"));
            }
            DialogKind::Repair(id) => {
                self.remediate(id, dialog.value(0) == "Insert repair task")?
            }
            DialogKind::RemediationPending
            | DialogKind::RemediationReview { .. }
            | DialogKind::BreakdownPending
            | DialogKind::BreakdownReview { .. } => {
                return Err("Persistent proposal is unavailable in demo mode".into());
            }
            DialogKind::Models(id) => self.save_models(dialog, id)?,
            DialogKind::Resolve(id) => self.resolve_failed(dialog, id)?,
            DialogKind::Reject(id) => self.reject_revision(dialog, id)?,
            DialogKind::Breakdown(id) => self.accept_breakdown(dialog, id)?,
            DialogKind::Answer(id) => {
                if dialog.value(0).is_empty() {
                    return Err("An answer is required".into());
                }
                let task = self
                    .queue
                    .first_mut()
                    .filter(|t| t.id == id && t.status() == Status::Question)
                    .ok_or("Current is not waiting for an answer")?;
                task.answer = dialog.value(0).into();
                if let Some(attempt) = task.attempts.last_mut() {
                    attempt.state = Status::Running;
                    attempt.logs.push(format!("User answer: {}", task.answer));
                    attempt.outcome = "Running".into();
                }
                self.last_tick = Instant::now();
                self.record("Answer recorded · current continues");
            }
            DialogKind::Import => {
                let data = self
                    .files
                    .get(dialog.value(0))
                    .ok_or("Mock file not found · try /demo/import.tsv")?;
                let tasks = parse_tasks(data)?;
                dialog.title = "Import preview · simulated".into();
                dialog.body = tasks
                    .iter()
                    .map(|t| format!("{} · {}", t.definition().kind.label(), t.definition().title))
                    .collect::<Vec<_>>()
                    .join("\n");
                dialog.kind = DialogKind::ImportPreview(tasks);
                dialog.fields = vec![Field::text("Before task ID", "")];
                dialog.focus = 0;
                dialog.error.clear();
                return Ok(false);
            }
            DialogKind::ImportPreview(mut tasks) => {
                let position = self.insertion_position(dialog.value(0), None)?;
                for task in &mut tasks {
                    task.id = self.next_id;
                    self.next_id += 1;
                }
                let id = tasks[0].id;
                let count = tasks.len();
                self.queue.splice(position..position, tasks);
                self.reveal(id);
                self.record(format!("Imported {count} tasks · simulated files only"));
            }
            DialogKind::Export => {
                let path = dialog.value(0).to_owned();
                if !path.starts_with("/demo/") || !path.ends_with(".tsv") {
                    return Err(
                        "Use a virtual path /demo/<name>.tsv; no real files are written".into(),
                    );
                }
                let tasks: Vec<_> = match dialog.value(1) {
                    "History" => self.history.iter().collect(),
                    "All" => self.queue.iter().chain(&self.history).collect(),
                    _ => self.queue.iter().collect(),
                };
                let data = tasks
                    .iter()
                    .map(|t| export_task(t))
                    .collect::<Vec<_>>()
                    .join("\n");
                if self.files.contains_key(&path) {
                    dialog.kind = DialogKind::Overwrite {
                        path: path.clone(),
                        data,
                    };
                    dialog.title = "Overwrite mock file?".into();
                    dialog.body = format!(
                        "{path} already exists.\nConfirm replaces it; cancel preserves its contents."
                    );
                    dialog.fields.clear();
                    dialog.error.clear();
                    return Ok(false);
                }
                self.files.insert(path.clone(), data);
                self.record(format!("Exported to {path} · in memory, NOT disk"));
            }
            DialogKind::Overwrite { path, data } => {
                self.files.insert(path.clone(), data);
                self.record(format!("Exported to {path} · overwrite confirmed (mock)"));
            }
            DialogKind::Config | DialogKind::ResetConfig => {
                let delay = dialog
                    .value(1)
                    .parse::<u64>()
                    .ok()
                    .filter(|n| (100..=2000).contains(n))
                    .ok_or("Step delay must be 100–2000 ms")?;
                if dialog.value(2) != "Keep" && matches!(dialog.kind, DialogKind::Config) {
                    dialog.kind = DialogKind::ResetConfig;
                    dialog.title = "Reset mock workspace?".into();
                    dialog.body = "Apply resets all session tasks to the chosen demo dataset.\nNo production data is touched. Cancel keeps the workspace.".into();
                    dialog.error.clear();
                    return Ok(false);
                }
                self.mouse_enabled = dialog.value(0) == "Enabled";
                self.review_required = dialog.value(3) == "Review required";
                self.cleanup_fail_once = dialog.value(4) == "Fail next switch";
                self.clean_retry = dialog.value(5) == "Clean after checkpoint";
                self.auto_advance = dialog.value(6) == "Enabled";
                self.tick_ms = delay;
                if dialog.value(2) != "Keep" {
                    (self.queue, self.history) = mock::dataset(dialog.value(2));
                    self.queue_catalog = Catalog::default();
                    self.history_catalog = Catalog::default();
                    self.undo = None;
                    self.follow = true;
                    self.log_offset = 0;
                    self.events = vec!["Mock workspace reset".into()];
                    self.task_events = self.events.clone();
                    self.last_workspace = self.current().map(|t| t.id);
                }
                self.record("Configuration applied · session only");
            }
            DialogKind::Help
            | DialogKind::Stats
            | DialogKind::Events
            | DialogKind::Inspect { .. } => {}
            DialogKind::ProjectModels(_)
            | DialogKind::ProjectChecks(_)
            | DialogKind::ProjectCheck { .. }
            | DialogKind::Preferences(_)
            | DialogKind::StoredImport(_)
            | DialogKind::FileOverwrite { .. } => {
                return Err("Disk operations require SQLite mode".into());
            }
        }
        Ok(true)
    }

    fn save_task(
        &mut self,
        dialog: &Dialog,
        id: Option<usize>,
        insert: bool,
    ) -> Result<(), String> {
        if (0..3).any(|n| dialog.value(n).is_empty()) {
            return Err("Title, goal, and acceptance check are required".into());
        }
        let kind = if dialog.value(3) == "Human" {
            Kind::Human
        } else {
            Kind::Ai
        };
        let id = if let Some(id) = id {
            let task = self
                .queue
                .iter_mut()
                .find(|t| {
                    t.id == id
                        && matches!(t.status(), Status::New | Status::Pending | Status::Failed)
                })
                .ok_or("This task is read-only")?;
            if task.status() == Status::Failed && kind != task.definition().kind {
                return Err("Keep the kind of a failed task; use breakdown to replace it".into());
            }
            let mut revision = task.definition().clone();
            revision.title = dialog.value(0).into();
            revision.goal = dialog.value(1).into();
            revision.check = dialog.value(2).into();
            revision.kind = kind;
            revision.actor = "Human: owner".into();
            revision.state = if task.ready().is_some() {
                RevisionState::Ready
            } else {
                RevisionState::Draft
            };
            task.revise(revision);
            id
        } else {
            let position = if insert {
                self.insertion_position(dialog.value(4), None)?
            } else {
                self.queue.len()
            };
            let id = self.next_id;
            self.next_id += 1;
            let mut task = Task::pending(id, dialog.value(0));
            task.revisions[0].goal = dialog.value(1).into();
            task.revisions[0].check = dialog.value(2).into();
            task.revisions[0].kind = kind;
            self.queue.insert(position, task);
            id
        };
        if self.view != View::Current
            || !matches!(dialog.kind, DialogKind::Task { id: Some(_), .. })
        {
            self.reveal(id);
        }
        self.record(format!("#{id} saved · mock workspace"));
        Ok(())
    }

    fn accept_breakdown(&mut self, dialog: &Dialog, id: usize) -> Result<(), String> {
        if dialog.value(0).is_empty() || dialog.value(1).is_empty() {
            return Err("Both subtask titles are required".into());
        }
        let index = self
            .queue
            .iter()
            .position(|t| t.id == id && matches!(t.status(), Status::Pending | Status::Failed))
            .ok_or("Only pending or failed current work can be broken down")?;
        let mut original = self.queue.remove(index);
        original.resolution = Some(Status::Superseded);
        original.resolution_reason = "Replaced by planner subtasks".into();
        let tasks: Vec<_> = (0..2)
            .map(|n| {
                let mut task = Task::pending(self.next_id + n, dialog.value(n));
                task.revisions[0].goal = original.definition().goal.clone();
                task.revisions[0].check = original.definition().check.clone();
                task.revisions[0].actor = "AI: planner".into();
                if self.review_required {
                    task.revisions[0].state = RevisionState::Draft;
                }
                task
            })
            .collect();
        let first = self.next_id;
        self.next_id += 2;
        self.history.insert(0, original);
        self.queue.splice(index..index, tasks);
        self.reveal(first);
        self.record(format!(
            "#{id} replaced by 2 subtasks · original retained in History"
        ));
        Ok(())
    }
}

fn parse_tasks(data: &str) -> Result<Vec<Task>, String> {
    let mut tasks = Vec::new();
    for (n, line) in data.lines().enumerate() {
        let parts: Vec<_> = line.split('\t').map(str::trim).collect();
        if !matches!(parts.len(), 4 | 14)
            || parts.iter().take(4).any(|s| s.is_empty())
            || !matches!(parts[0], "AI" | "Human")
        {
            return Err(format!(
                "Invalid row {}: expected kind, title, goal, check (TSV)",
                n + 1
            ));
        }
        let mut task = Task::pending(0, parts[1]);
        task.revisions[0].goal = parts[2].into();
        task.revisions[0].check = parts[3].into();
        task.revisions[0].kind = if parts[0] == "Human" {
            Kind::Human
        } else {
            Kind::Ai
        };
        if parts.len() == 14 {
            import_settings(&parts, &mut task)?;
        }
        tasks.push(task);
    }
    if tasks.is_empty() {
        return Err("Import contains no tasks".into());
    }
    Ok(tasks)
}

fn export_task(task: &Task) -> String {
    let revision = task.definition();
    let mut fields = vec![
        revision.kind.label().to_owned(),
        revision.title.clone(),
        revision.goal.clone(),
        revision.check.clone(),
    ];
    for model in &revision.settings.models {
        fields.push(model.name.clone());
        fields.push(model.effort.clone().unwrap_or_else(|| "none".into()));
    }
    fields.push(revision.settings.attempt_limit.to_string());
    fields.push(revision.settings.timeout_ms.to_string());
    fields.join("\t")
}

fn import_settings(parts: &[&str], task: &mut Task) -> Result<(), String> {
    for (n, model) in task.revisions[0].settings.models.iter_mut().enumerate() {
        let name = parts[4 + 2 * n];
        let effort = parts[5 + 2 * n];
        if name.is_empty() || !matches!(effort, "none" | "low" | "medium" | "high" | "max") {
            return Err("Invalid imported model or effort".into());
        }
        model.name = name.into();
        model.effort = (effort != "none").then(|| effort.to_owned());
    }
    task.revisions[0].settings.attempt_limit = parts[12]
        .parse()
        .map_err(|_| "Invalid imported attempt limit")?;
    task.revisions[0].settings.timeout_ms = parts[13]
        .parse()
        .map_err(|_| "Invalid imported active time limit")?;
    Ok(())
}
