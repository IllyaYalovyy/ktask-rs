//! Modal dialogs: body, fields, hint and buttons; details dialogs jump to search matches.
use super::{
    ACCENT, BG, MUTED, TEXT, WARN, buttons, panel, run_label,
    text::{clip, tail, wrapped},
};
use crate::{
    app::{Action, App},
    dialog::{Dialog, DialogKind, Field},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Clear, Paragraph},
};

fn hint(dialog: &Dialog, has_position: bool) -> &str {
    if !dialog.error.is_empty() {
        &dialog.error
    } else if has_position {
        "Alt-Up/Down: position · Ctrl-S: apply · Esc: cancel"
    } else if matches!(dialog.kind, DialogKind::Inspect { .. }) {
        "[ ] attempt · s summary · v reviews · f failures · n/p step · o output · t artifacts"
    } else if dialog.read_only() {
        "Wheel / PgUp / PgDn: scroll · Esc: close"
    } else if matches!(dialog.kind, DialogKind::StorageConfirm(_)) {
        "Enter / Ctrl-S: delete evidence · Esc: cancel"
    } else if matches!(dialog.kind, DialogKind::BuildCacheConfirm(_)) {
        "Enter / Ctrl-S: confirm · Esc: cancel"
    } else {
        "Tab: field · Space: choice · Ctrl-A: clear · Ctrl-S: apply"
    }
}

pub(super) fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    app.hits.clear();
    app.selection.begin_frame();
    let mut dialog = app.dialog.take().expect("dialog exists");
    let position = dialog.position_field();
    let body_text = if let Some((index, excluding)) = position {
        format!(
            "{}\n{}",
            app.position_preview(dialog.fields[index].value.trim(), excluding),
            dialog.body
        )
    } else {
        dialog.body.clone()
    };
    let width = area.width.min(78).saturating_sub(2) as usize;
    let lines = wrapped(&body_text, width);
    // Errors wrap in full: their end often names the file or value to fix.
    let hint = wrapped(hint(&dialog, position.is_some()).trim_end(), width);
    let hint_rows = if matches!(dialog.kind, DialogKind::Inspect { .. }) && dialog.error.is_empty()
    {
        1
    } else {
        hint.len().max(2)
    };
    let action_rows = if matches!(
        dialog.kind,
        DialogKind::Inspect { .. } | DialogKind::Readiness
    ) {
        3
    } else {
        2
    };
    let height = (lines.len() + dialog.fields.len() * 2 + 2 + hint_rows + action_rows)
        .max(12)
        .min(usize::from(area.height - 2)) as u16;
    let popup = Rect::new(
        area.x + (area.width - area.width.min(78)) / 2,
        area.y + (area.height - height) / 2,
        area.width.min(78),
        height,
    );
    frame.render_widget(Clear, popup);
    frame.render_widget(
        panel(format!(" {} ", dialog.title)).border_style(Style::new().fg(ACCENT)),
        popup,
    );
    let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
    // A long error takes at most half the dialog, so the fields it names stay in view.
    let hint_rows = (hint_rows as u16).min((inner.height / 2).max(1));
    let action_rows = action_rows as u16;
    dialog.field_capacity = dialog
        .fields
        .len()
        .min((inner.height.saturating_sub(1 + action_rows + hint_rows) / 2) as usize)
        .min(dialog.field_limit.unwrap_or(usize::MAX))
        .max(1);
    if dialog.focus < dialog.field_offset {
        dialog.field_offset = dialog.focus;
    }
    if dialog.focus >= dialog.field_offset + dialog.field_capacity {
        dialog.field_offset = dialog.focus + 1 - dialog.field_capacity;
    }
    let [body, fields, error, actions] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(dialog.fields.len().min(dialog.field_capacity) as u16 * 2),
        Constraint::Length(hint_rows),
        Constraint::Length(action_rows),
    ])
    .areas(inner);
    position_dialog(
        &mut dialog,
        &body_text,
        &lines,
        body.height as usize,
        popup.width.saturating_sub(2) as usize,
    );
    frame.render_widget(
        Paragraph::new(
            lines
                .into_iter()
                .skip(dialog.scroll)
                .take(body.height as usize)
                .map(Line::raw)
                .collect::<Vec<_>>(),
        ),
        body,
    );
    app.selection.zone(body);
    draw_fields(frame, app, &dialog, fields);
    frame.render_widget(
        Paragraph::new(hint.into_iter().map(Line::raw).collect::<Vec<_>>())
            .style(Style::new().fg(if dialog.error.is_empty() { MUTED } else { WARN })),
        error,
    );
    let controls = controls(app, &dialog, position.is_some());
    buttons(frame, app, actions, &controls, true);
    app.dialog = Some(dialog);
}

fn controls(app: &App, dialog: &Dialog, has_position: bool) -> Vec<(&'static str, Action)> {
    if let DialogKind::Inspect { section, .. } = dialog.kind {
        let mut controls = vec![
            ("Summary", Action::EvidenceOpen(3)),
            ("Reviews", Action::EvidenceOpen(4)),
            ("Failures", Action::EvidenceOpen(5)),
            ("Results", Action::EvidenceOpen(1)),
            ("Output", Action::EvidenceOpen(0)),
            ("Transcripts", Action::EvidenceOpen(2)),
            ("Prev attempt", Action::PreviousAttempt),
            ("Next attempt", Action::NextAttempt),
        ];
        match section {
            0 => controls.extend([
                ("Older", Action::EvidencePage(1)),
                ("Newer", Action::EvidencePage(-1)),
                ("Chunk -", Action::EvidenceChunk(-1)),
                ("Chunk +", Action::EvidenceChunk(1)),
                ("Find", Action::FindEvidence),
                ("Refresh", Action::RefreshEvidence),
            ]),
            2 => controls.extend([
                ("Check -", Action::EvidenceArtifact(-1)),
                ("Check +", Action::EvidenceArtifact(1)),
                ("Older", Action::EvidencePage(1)),
                ("Newer", Action::EvidencePage(-1)),
                ("Find", Action::FindEvidence),
            ]),
            4 | 5 => controls.extend([
                ("Prev step", Action::EvidenceStep(-1)),
                ("Next step", Action::EvidenceStep(1)),
            ]),
            1 => controls.push(("Find", Action::FindEvidence)),
            _ => {}
        }
        controls.extend([
            ("Up", Action::Scroll(-8)),
            ("Down", Action::Scroll(8)),
            ("Close", Action::Cancel),
            ("Copy", Action::Copy),
        ]);
        return controls;
    }
    if matches!(dialog.kind, DialogKind::Readiness) {
        return readiness_controls(app);
    }
    let mut controls = if matches!(dialog.kind, DialogKind::BreakdownPending) {
        vec![("Cancel", Action::Cancel)]
    } else if dialog.read_only() {
        vec![("Close", Action::Cancel)]
    } else if matches!(dialog.kind, DialogKind::StorageConfirm(_)) {
        vec![
            ("Delete evidence", Action::Submit),
            ("Cancel", Action::Cancel),
        ]
    } else if matches!(dialog.kind, DialogKind::BuildCacheConfirm(_)) {
        vec![("Confirm", Action::Submit), ("Cancel", Action::Cancel)]
    } else {
        vec![("Apply", Action::Submit), ("Cancel", Action::Cancel)]
    };
    controls.insert(0, ("Copy", Action::Copy));
    controls.extend([("Up", Action::Scroll(-8)), ("Down", Action::Scroll(8))]);
    if dialog.fields.len() > dialog.field_capacity {
        controls.extend([
            ("Fields -", Action::Fields(-1)),
            ("Fields +", Action::Fields(1)),
        ]);
    }
    if has_position {
        controls.extend([
            ("Earlier", Action::Position(-1)),
            ("Later", Action::Position(1)),
        ]);
    }
    controls
}

fn position_dialog(dialog: &mut Dialog, text: &str, lines: &[String], height: usize, width: usize) {
    dialog.max_scroll = lines.len().saturating_sub(height);
    if let DialogKind::Inspect {
        match_sequence: Some(sequence),
        jump_to_match: true,
        ..
    } = &mut dialog.kind
    {
        let marker = format!("#{sequence} [");
        let excerpt = format!("Match in #{sequence}: ");
        let content = text
            .find("Match: ")
            .map(|start| start + "Match: ".len())
            .or_else(|| text.find(&excerpt).map(|start| start + excerpt.len()))
            .or_else(|| {
                text.find(&marker)
                    .and_then(|start| text[start..].find("] ").map(|end| start + end + 2))
            });
        let query = dialog
            .fields
            .first()
            .map(|field| field.value.trim())
            .unwrap_or("");
        let found = content.and_then(|start| {
            text[start..]
                .to_ascii_lowercase()
                .find(&query.to_ascii_lowercase())
                .map(|offset| start + offset)
        });
        let index = found
            .and_then(|at| {
                let mut marked = text.to_owned();
                marked.insert(at, '\u{0001}');
                wrapped(&marked, width)
                    .iter()
                    .position(|line| line.contains('\u{0001}'))
            })
            .or_else(|| lines.iter().position(|line| line.contains(&marker)));
        if let Some(index) = index {
            dialog.scroll = index.min(dialog.max_scroll);
        }
        if let DialogKind::Inspect { jump_to_match, .. } = &mut dialog.kind {
            *jump_to_match = false;
        }
    }
    dialog.scroll = dialog.scroll.min(dialog.max_scroll);
}

fn readiness_controls(app: &App) -> Vec<(&'static str, Action)> {
    let mut controls = vec![("c Checks", Action::ProjectChecks)];
    if app.current().is_none() {
        controls.push(("d Defaults", Action::ProjectModels));
    }
    for (label, action) in [
        ("m Models", Action::Models),
        ("v Approve", Action::Approve),
        ("p Pause", Action::Pause),
        (run_label(app), Action::Run),
    ] {
        if app.enabled(action) {
            controls.push((label, action));
        }
    }
    controls.extend([("u Refresh", Action::Readiness), ("Close", Action::Cancel)]);
    controls
}

fn draw_fields(frame: &mut Frame, app: &mut App, dialog: &Dialog, area: Rect) {
    for (row, (index, field)) in dialog
        .fields
        .iter()
        .enumerate()
        .skip(dialog.field_offset)
        .take(dialog.field_capacity)
        .enumerate()
    {
        let rect = Rect::new(area.x, area.y + row as u16 * 2, area.width, 2);
        let focused = index == dialog.focus;
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(field.label.as_str(), Style::new().fg(MUTED)),
                Line::styled(
                    field_display(field, rect.width as usize, focused),
                    Style::new()
                        .fg(if focused { ACCENT } else { TEXT })
                        .bg(if focused { Color::Rgb(35, 50, 64) } else { BG }),
                ),
            ]),
            rect,
        );
        app.hits.push((rect, Action::Field(index)));
        app.selection
            .zone(Rect::new(rect.x, rect.y + 1, rect.width, 1));
    }
}

fn field_display(field: &Field, width: usize, focused: bool) -> String {
    if !field.choices.is_empty() {
        return clip(
            &format!("< {} >   click / Space to change", field.value),
            width,
        );
    }
    if !focused {
        return clip(&field.value, width);
    }
    let before: String = field.value.chars().take(field.cursor).collect();
    let after: String = field.value.chars().skip(field.cursor).collect();
    let before = tail(&before, width.saturating_sub(1));
    clip(&format!("{before}▏{after}"), width)
}
