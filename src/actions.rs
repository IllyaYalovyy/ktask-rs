//! Actions: what the state machine allows, view-only actions, and routing workspace
//! actions to the backend. Task details keep their evidence position per attempt.
use crate::{
    app::{
        Action, App, EvidenceAttemptKey, EvidenceAttemptState, EvidencePosition, EvidenceSection,
        EvidenceSourceKey, View,
    },
    application::EvidenceSearch,
    backend::EvidenceAt,
    dialog::{Dialog, DialogKind, Field},
    presentation::{Kind, Status, Task, TaskAction},
};

impl App {
    pub fn enabled(&self, action: Action) -> bool {
        if action == Action::Run
            && self
                .readiness
                .as_ref()
                .is_some_and(|report| report.refresh_failed)
        {
            return false;
        }
        let current = self.current();
        let selected = self.selected_task();
        let allows =
            |task: Option<&Task>, step: TaskAction| task.is_some_and(|t| t.status().allows(step));
        let queue_or_current = self.view != View::History;
        match action {
            Action::Run => {
                matches!(self.view, View::Current | View::Queue)
                    && self.supervisor_alive != Some(true)
                    && current.is_some_and(|t| {
                        t.definition().kind.runs_ai()
                            && t.status()
                                .forward()
                                .is_some_and(|step| step != TaskAction::Approve)
                    })
            }
            Action::Answer => self.view == View::Current && allows(current, TaskAction::Answer),
            // A live run can be paused from every screen, not only Current.
            Action::Pause => {
                allows(current, TaskAction::Pause)
                    || self.execution_session.as_ref().is_some_and(|session| {
                        // A provider wait is a Running current; an answer wait
                        // has nothing to pause (s Stop returns it to Pending).
                        session.mode == "queue"
                            && matches!(session.state.as_str(), "running" | "resume_requested")
                    })
            }
            Action::Stop => queue_or_current && allows(self.stop_target(), TaskAction::Stop),
            Action::MarkDone => queue_or_current && allows(selected, TaskAction::MarkDone),
            Action::CancelTask => queue_or_current && allows(selected, TaskAction::Cancel),
            Action::Supersede | Action::Breakdown => {
                queue_or_current && allows(selected, TaskAction::Supersede)
            }
            Action::CopyToQueue => {
                self.view == View::History && allows(selected, TaskAction::CopyToQueue)
            }
            Action::Approve | Action::Reject => {
                queue_or_current
                    && allows(selected, TaskAction::Approve)
                    && selected.is_some_and(|t| t.draft().is_some())
            }
            Action::Models | Action::Edit => queue_or_current && allows(selected, TaskAction::Edit),
            Action::Move => self.view == View::Queue && allows(selected, TaskAction::Move),
            Action::Output => {
                self.view == View::Current
                    && current.is_some_and(|t| {
                        t.attempts
                            .last()
                            .is_some_and(|a| !a.report.is_empty() || a.question.is_some())
                    })
            }
            Action::Insert | Action::Import => self.view == View::Queue,
            Action::Inspect | Action::InspectSection(_) => selected.is_some(),
            Action::Filter | Action::Search | Action::Clear | Action::Export => {
                matches!(self.view, View::Queue | View::History)
            }
            _ => true,
        }
    }

    /// Why `action` is unavailable: the task's state names what it offers instead.
    fn unavailable_message(&self, action: Action) -> String {
        const ELSEWHERE: &str =
            "Unavailable here · only the current task can execute; History offers Copy to queue";
        let step = match action {
            Action::Run => self
                .current()
                .and_then(|t| t.status().forward())
                .unwrap_or(TaskAction::Run),
            Action::Answer => TaskAction::Answer,
            Action::Pause => TaskAction::Pause,
            Action::Stop => TaskAction::Stop,
            Action::MarkDone => TaskAction::MarkDone,
            Action::CancelTask => TaskAction::Cancel,
            Action::Supersede | Action::Breakdown => TaskAction::Supersede,
            Action::CopyToQueue => TaskAction::CopyToQueue,
            Action::Approve | Action::Reject => TaskAction::Approve,
            Action::Models | Action::Edit => TaskAction::Edit,
            Action::Move => TaskAction::Move,
            _ => return ELSEWHERE.into(),
        };
        let target = match action {
            Action::Run | Action::Answer | Action::Pause => self.current(),
            Action::Stop => self.stop_target(),
            _ => self.selected_task(),
        };
        match target {
            Some(task) if !task.status().allows(step) => {
                format!("#{}: {}", task.id, task.status().refusal(step))
            }
            Some(task) if action == Action::Run && task.definition().kind == Kind::Human => {
                format!("#{} is a Human task · D marks it done", task.id)
            }
            Some(_) if action == Action::Run && step == TaskAction::Approve => {
                "Approve a revision first · v approves, n rejects".into()
            }
            Some(_) if action == Action::Run && self.supervisor_alive == Some(true) => {
                "The runner is already working · p pauses, s stops".into()
            }
            Some(task) if matches!(action, Action::Approve | Action::Reject) => {
                format!("#{} has no revision waiting for approval", task.id)
            }
            _ => ELSEWHERE.into(),
        }
    }

    pub fn act(&mut self, action: Action) {
        if matches!(action, Action::Copy) {
            self.copy_selection();
            return;
        }
        self.selection.clear();
        let readiness_open = self
            .dialog
            .as_ref()
            .is_some_and(|dialog| matches!(dialog.kind, DialogKind::Readiness));
        if action == Action::Run && readiness_open && !self.enabled(action) {
            return;
        }
        if readiness_open
            && matches!(
                action,
                Action::ProjectChecks
                    | Action::Models
                    | Action::Approve
                    | Action::Answer
                    | Action::Pause
                    | Action::Stop
                    | Action::MarkDone
                    | Action::Run
                    | Action::Readiness
            )
        {
            self.dialog = None;
        }
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
        if !self.enabled(action) {
            let message = self.unavailable_message(action);
            self.message(message);
            return;
        }
        match action {
            Action::View(view) => self.switch(view),
            Action::Back => {
                let previous = self.previous;
                self.switch(previous)
            }
            Action::Quit => self.quit = true,
            Action::Cancel => {
                self.save_evidence_position();
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
            Action::Field(index) => {
                if let Some(dialog) = &mut self.dialog {
                    dialog.focus = index;
                    if let DialogKind::Inspect { search_active, .. } = &mut dialog.kind {
                        *search_active = true;
                    }
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
            Action::EvidenceSection(delta) => self.change_section(delta),
            Action::EvidencePage(delta) => self.change_page(delta),
            Action::EvidenceChunk(delta) => self.change_evidence_chunk(delta),
            Action::EvidenceArtifact(delta) => self.change_artifact(delta),
            Action::EvidenceStep(delta) => self.change_evidence_step(delta),
            Action::EvidenceOpen(section) => self.open_evidence_section(section),
            Action::FindEvidence => self.find_evidence(),
            Action::RefreshEvidence => self.refresh_evidence(),
            Action::Search => self.search_before = Some(self.catalog().clone()),
            Action::Clear => {
                self.catalog_mut().query.clear();
                self.catalog_mut().filter = 0;
                self.reconcile();
            }
            Action::Filter => {
                self.catalog_mut().filter = (self.catalog().filter + 1) % 4;
                self.reconcile();
            }
            Action::Follow => self.follow = true,
            Action::Output => {
                self.show_log = !self.show_log;
                self.follow = true;
            }
            Action::Events => {
                self.dialog = Some(Dialog::new(
                    DialogKind::Events,
                    "Project events",
                    &self.events.join("\n"),
                    vec![],
                ))
            }
            Action::Inspect | Action::InspectSection(_) => {
                if let Some(task) = self.selected_task() {
                    let index = self
                        .inspected_attempts
                        .get(&(self.view, task.id))
                        .and_then(|remembered| {
                            task.attempts.iter().position(|a| a.id == Some(*remembered))
                        })
                        .unwrap_or_else(|| task.attempts.len().saturating_sub(1));
                    let id = task.id;
                    self.open_inspect(id, index);
                    if let Action::InspectSection(section) = action {
                        self.open_evidence_section(section);
                    }
                }
            }
            Action::Copy => unreachable!("handled before selection is cleared"),
            Action::Run | Action::Pause | Action::Approve | Action::Readiness => {
                self.backend.act(&mut self.screen, action)
            }
            Action::Stats => match self.backend.stats(&self.screen) {
                Ok(dialog) => self.dialog = Some(dialog),
                Err(error) => self.message(error),
            },
            Action::Add
            | Action::Insert
            | Action::Edit
            | Action::Move
            | Action::Breakdown
            | Action::Answer
            | Action::Models
            | Action::Reject
            | Action::Stop
            | Action::MarkDone
            | Action::CancelTask
            | Action::Supersede
            | Action::CopyToQueue
            | Action::Help
            | Action::Configure
            | Action::ProjectModels
            | Action::UserDefaults
            | Action::Profiles
            | Action::ProjectChecks
            | Action::Delivery
            | Action::Storage
            | Action::BuildCache
            | Action::Import
            | Action::Export => match self.backend.open(&mut self.screen, action) {
                Ok(Some(dialog)) => self.dialog = Some(dialog),
                Ok(None) => {}
                Err(error) => self.message(error),
            },
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
            .insertion_position(dialog.fields[field].value.trim(), excluding)
            .unwrap_or(tasks.len());
        let min = usize::from(tasks.first().is_some_and(|t| t.status() == Status::Running));
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

    fn open_inspect(&mut self, id: usize, attempt: usize) {
        if self.task(id).is_none() {
            return;
        }
        let key = self.evidence_attempt_key(id, attempt);
        if !self.evidence_attempts.contains_key(&key) {
            let watermark = self.backend.watermark(key.attempt).unwrap_or(0);
            self.evidence_attempts.insert(
                key,
                EvidenceAttemptState {
                    watermark,
                    section: EvidenceSection::Output,
                    ..Default::default()
                },
            );
        }
        let state = self.evidence_attempts[&key].clone();
        let source = self.evidence_source_key(key, state.section, state.artifact, state.step);
        let position = self
            .evidence_positions
            .get(&source)
            .cloned()
            .unwrap_or_default();
        let mut dialog = Dialog::new(
            DialogKind::Inspect {
                id,
                attempt,
                section: state.section.index(),
                page: position.page,
                chunk: position.chunk,
                artifact: state.artifact,
                step: state.step,
                match_sequence: position.match_sequence,
                search_active: false,
                jump_to_match: false,
                watermark: state.watermark,
                search: position.search,
                excerpt: position.excerpt,
            },
            "Task details / attempts",
            "",
            self.backend
                .find_label(state.section)
                .map_or_else(Vec::new, |label| vec![Field::text(label, &position.query)]),
        );
        dialog.body = self.backend.inspect_body(&self.screen, &dialog);
        dialog.scroll = position.scroll;
        self.dialog = Some(dialog);
    }

    /// Rebuild the open details dialog's body after its position or search changed.
    pub(crate) fn refresh_inspect_body(&mut self) {
        let body = match &self.dialog {
            Some(
                dialog @ Dialog {
                    kind: DialogKind::Inspect { .. },
                    ..
                },
            ) => self.backend.inspect_body(&self.screen, dialog),
            _ => return,
        };
        if let Some(dialog) = &mut self.dialog {
            dialog.body = body;
        }
    }

    fn evidence_attempt_key(&self, task: usize, attempt: usize) -> EvidenceAttemptKey {
        EvidenceAttemptKey {
            view: self.view,
            task,
            attempt: self
                .task(task)
                .and_then(|t| t.attempts.get(attempt))
                .and_then(|a| a.id)
                .unwrap_or(attempt as i64),
        }
    }

    fn evidence_source_key(
        &self,
        attempt: EvidenceAttemptKey,
        section: EvidenceSection,
        artifact: usize,
        step: usize,
    ) -> EvidenceSourceKey {
        EvidenceSourceKey {
            attempt,
            section,
            artifact: if section == EvidenceSection::Transcripts {
                self.backend.artifact_id(attempt.attempt, artifact)
            } else {
                None
            },
            step: matches!(
                section,
                EvidenceSection::Reviews | EvidenceSection::Failures
            )
            .then_some(step),
        }
    }

    /// The open details dialog's attempt, if one is open.
    fn inspected(&self) -> Option<EvidenceAt> {
        let Some(Dialog {
            kind:
                DialogKind::Inspect {
                    id,
                    attempt,
                    watermark,
                    ..
                },
            ..
        }) = self.dialog
        else {
            return None;
        };
        Some(EvidenceAt {
            task: id,
            index: attempt,
            attempt: self.evidence_attempt_key(id, attempt).attempt,
            watermark,
        })
    }

    fn save_evidence_position(&mut self) {
        let Some(dialog) = &self.dialog else {
            return;
        };
        let DialogKind::Inspect {
            id,
            attempt,
            section,
            page,
            chunk,
            artifact,
            step,
            match_sequence,
            watermark,
            ref search,
            ref excerpt,
            ..
        } = dialog.kind
        else {
            return;
        };
        let section = EvidenceSection::from_index(section);
        let key = self.evidence_attempt_key(id, attempt);
        let source = self.evidence_source_key(key, section, artifact, step);
        let mut state = self
            .evidence_attempts
            .get(&key)
            .cloned()
            .unwrap_or_default();
        state.select_section(section);
        state.select_step(step);
        state.artifact = artifact;
        state.watermark = watermark;
        let position = EvidencePosition {
            page,
            chunk,
            scroll: dialog.scroll,
            match_sequence,
            query: dialog
                .fields
                .first()
                .map(|field| field.value.clone())
                .unwrap_or_default(),
            search: search.clone(),
            excerpt: excerpt.clone(),
        };
        self.evidence_attempts.insert(key, state);
        let view = self.view;
        self.inspected_attempts.insert((view, id), key.attempt);
        self.evidence_positions.insert(source, position);
    }

    /// Save the position, change the attempt's evidence state, and reopen at it.
    fn reopen_inspect(&mut self, change: impl FnOnce(&mut EvidenceAttemptState)) {
        let Some(at) = self.inspected() else {
            return;
        };
        self.save_evidence_position();
        let key = self.evidence_attempt_key(at.task, at.index);
        if let Some(state) = self.evidence_attempts.get_mut(&key) {
            change(state);
        }
        self.open_inspect(at.task, at.index);
    }

    fn change_attempt(&mut self, next: bool) {
        let Some(at) = self.inspected() else {
            return;
        };
        let last = self
            .task(at.task)
            .map_or(0, |t| t.attempts.len().saturating_sub(1));
        self.save_evidence_position();
        self.open_inspect(
            at.task,
            if next {
                (at.index + 1).min(last)
            } else {
                at.index.saturating_sub(1)
            },
        );
    }

    fn open_evidence_section(&mut self, section: usize) {
        self.reopen_inspect(|state| state.select_section(EvidenceSection::from_index(section)));
    }

    fn change_section(&mut self, delta: i32) {
        let Some(Dialog {
            kind: DialogKind::Inspect { section, .. },
            ..
        }) = self.dialog
        else {
            return;
        };
        let next = section.saturating_add_signed(delta as isize).min(5);
        self.reopen_inspect(|state| state.select_section(EvidenceSection::from_index(next)));
    }

    fn change_evidence_step(&mut self, delta: i32) {
        let (
            Some(at),
            Some(Dialog {
                kind: DialogKind::Inspect { section, step, .. },
                ..
            }),
        ) = (self.inspected(), &self.dialog)
        else {
            return;
        };
        if !matches!(section, 4 | 5) {
            return;
        }
        let (section, step) = (*section, *step);
        let count = match self.backend.step_count(&self.screen, at, section == 4) {
            Ok(count) => count,
            Err(error) => {
                self.message(error);
                return;
            }
        };
        let next = step
            .saturating_add_signed(delta as isize)
            .min(count.saturating_sub(1));
        if next != step {
            self.reopen_inspect(|state| state.select_step(next));
        }
    }

    fn refresh_evidence(&mut self) {
        let Some(at) = self.inspected() else {
            return;
        };
        let watermark = match self.backend.watermark(at.attempt) {
            Ok(value) => value,
            Err(error) => {
                self.message(error);
                return;
            }
        };
        self.save_evidence_position();
        let key = self.evidence_attempt_key(at.task, at.index);
        if let Some(state) = self.evidence_attempts.get_mut(&key) {
            state.watermark = watermark;
        }
        let output = self.evidence_source_key(key, EvidenceSection::Output, 0, 0);
        self.evidence_positions.remove(&output);
        self.open_inspect(at.task, at.index);
    }

    fn find_evidence(&mut self) {
        let (Some(at), Some(dialog)) = (self.inspected(), &self.dialog) else {
            return;
        };
        let DialogKind::Inspect {
            section,
            artifact,
            ref search,
            ..
        } = dialog.kind
        else {
            return;
        };
        let query = dialog
            .fields
            .first()
            .map(|f| f.value.trim().to_owned())
            .unwrap_or_default();
        if query.is_empty() {
            return;
        }
        if section >= 3 {
            self.message("Find is available in Results, Output and Transcripts");
            return;
        }
        let result = self.backend.search(
            &self.screen,
            at,
            EvidenceSection::from_index(section),
            artifact,
            &query,
            search.clone(),
        );
        let Some(dialog) = &mut self.dialog else {
            return;
        };
        let DialogKind::Inspect {
            page,
            chunk,
            match_sequence,
            search_active,
            jump_to_match,
            search,
            excerpt,
            ..
        } = &mut dialog.kind
        else {
            return;
        };
        *search_active = false;
        match result {
            Ok(EvidenceSearch::Found(found, next)) => {
                *page = found.page;
                *chunk = found.chunk;
                *match_sequence = Some(found.sequence);
                *search = next;
                *excerpt = found.excerpt;
                *jump_to_match = true;
                dialog.error.clear();
                dialog.scroll = 0;
            }
            Ok(EvidenceSearch::Continue(next)) => {
                *search = next;
                *match_sequence = None;
                excerpt.clear();
                dialog.error = "Search continues · press Enter or Find".into();
            }
            Ok(EvidenceSearch::Exhausted) => {
                *search = Default::default();
                *match_sequence = None;
                excerpt.clear();
                dialog.error = "No matching evidence recorded · Find restarts".into();
            }
            Err(error) => {
                dialog.error = error;
                return;
            }
        }
        self.refresh_inspect_body();
    }

    fn change_artifact(&mut self, delta: i32) {
        let (
            Some(at),
            Some(Dialog {
                kind:
                    DialogKind::Inspect {
                        section: 2,
                        artifact,
                        ..
                    },
                ..
            }),
        ) = (self.inspected(), &self.dialog)
        else {
            return;
        };
        if delta == 0 {
            return;
        }
        let artifact = *artifact;
        let count = match self.backend.artifact_count(at.attempt) {
            Ok(value) => value,
            Err(error) => {
                self.message(error);
                return;
            }
        };
        let next = artifact
            .saturating_add_signed(delta as isize)
            .min(count.saturating_sub(1));
        self.reopen_inspect(|state| state.artifact = next);
    }

    fn change_page(&mut self, delta: i32) {
        let (
            Some(at),
            Some(Dialog {
                kind:
                    DialogKind::Inspect {
                        section,
                        page,
                        artifact,
                        ..
                    },
                ..
            }),
        ) = (self.inspected(), &self.dialog)
        else {
            return;
        };
        if delta == 0 || !matches!(section, 0 | 2) {
            return;
        }
        let (section, page, artifact) = (*section, *page, *artifact);
        let limit = match self.backend.page_count(
            &self.screen,
            at,
            EvidenceSection::from_index(section),
            artifact,
        ) {
            Ok(value) => value,
            Err(error) => {
                self.message(error);
                return;
            }
        };
        let next = page
            .saturating_add_signed(delta as isize)
            .min(limit.saturating_sub(1));
        if next == page {
            return;
        }
        if let Some(dialog) = &mut self.dialog {
            if let DialogKind::Inspect {
                page,
                chunk,
                excerpt,
                ..
            } = &mut dialog.kind
            {
                *page = next;
                *chunk = 0;
                excerpt.clear();
            }
            dialog.scroll = 0;
        }
        self.refresh_inspect_body();
    }

    fn change_evidence_chunk(&mut self, delta: i32) {
        let (
            Some(at),
            Some(Dialog {
                kind:
                    DialogKind::Inspect {
                        section: 0,
                        page,
                        chunk,
                        ..
                    },
                ..
            }),
        ) = (self.inspected(), &self.dialog)
        else {
            return;
        };
        let (page, chunk) = (*page, *chunk);
        let limit = match self.backend.chunk_count(at, page) {
            Ok(value) => value,
            Err(error) => {
                self.message(error);
                return;
            }
        };
        let next = chunk
            .saturating_add_signed(delta as isize)
            .min(limit.saturating_sub(1));
        if next == chunk {
            return;
        }
        if let Some(dialog) = &mut self.dialog {
            if let DialogKind::Inspect { chunk, excerpt, .. } = &mut dialog.kind {
                *chunk = next;
                excerpt.clear();
            }
            dialog.scroll = 0;
        }
        self.refresh_inspect_body();
    }

    fn submit_dialog(&mut self) {
        let Some(mut dialog) = self.dialog.take() else {
            return;
        };
        match self.backend.apply(&mut self.screen, &mut dialog) {
            Ok(true) => {}
            Ok(false) => self.dialog = Some(dialog),
            Err(error) => {
                dialog.error = error;
                self.dialog = Some(dialog);
            }
        }
        self.reconcile();
    }
}
