//! Work-category forms: list, create, edit, override per project, preview and delete
//! through the shared category commands.
use crate::{
    app::Screen,
    application::{
        categories::{Category, Entry, Scope, TEMPLATE_ROLES, builtin, template::names},
        prompt::{self, Turn},
    },
    dialog::{Dialog, DialogKind, Field},
    persistent::StoredWorkspace,
};

const OPEN: &str = "Category";
const NEW: &str = "New category";
const NAME: &str = "Category name";
const ACTION: &str = "Action";
pub(crate) const SCOPE: &str = "Save in";
const CHECKS: &str = "Project checks";
const DESCRIPTION: &str = "Description";
const SAVE: &str = "Save";
pub(crate) const USER: &str = "User scope";
pub(crate) const PROJECT: &str = "Project scope";
const APPLY: &str = "Apply";
const SKIP: &str = "Do not apply";
const DELETE: &str = "Delete (inherit again)";
/// Entries before the numbered categories in the list's choice field.
const FIXED: usize = 1;

fn template_label(role: &str) -> String {
    let mut label = role.to_owned();
    label[..1].make_ascii_uppercase();
    format!("{label} template")
}

fn preview_label(role: &str) -> String {
    format!("Preview {role} prompt")
}

/// A template on one form line: backslashes and line breaks are written `\\` and `\n`.
pub(crate) fn to_line(text: &str) -> String {
    text.replace('\\', "\\\\").replace('\n', "\\n")
}

/// The template a form line stands for: `\n` is a line break, `\\` a backslash, and any other
/// backslash is kept (so `\{{` still writes a literal `{{`).
pub(crate) fn from_line(line: &str) -> String {
    let mut out = String::new();
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        match (ch, chars.peek()) {
            ('\\', Some('n')) => {
                chars.next();
                out.push('\n');
            }
            ('\\', Some('\\')) => {
                chars.next();
                out.push('\\');
            }
            _ => out.push(ch),
        }
    }
    out
}

pub(crate) fn scope_label(scope: Scope) -> &'static str {
    if scope == Scope::Project {
        PROJECT
    } else {
        USER
    }
}

/// The position of the list entry a list dialog's choice field `label` has selected.
pub(crate) fn chosen_position(
    dialog: &Dialog,
    label: &str,
    missing: &str,
) -> Result<usize, String> {
    let chosen = dialog.get(label);
    dialog
        .fields
        .iter()
        .find(|field| field.label == label)
        .and_then(|field| field.choices.iter().position(|c| c == chosen))
        .ok_or_else(|| missing.to_owned())
}

/// The version a save may be guarded by: that of the entry the form opened, when it is
/// still the one being saved (same scope and name).
pub(crate) fn save_guard(
    opened: Option<(Scope, &str, i64)>,
    scope: Scope,
    name: &str,
) -> Option<i64> {
    opened
        .filter(|(opened_scope, opened_name, _)| {
            *opened_scope == scope && opened_name.eq_ignore_ascii_case(name)
        })
        .map(|(_, _, version)| version)
}

/// Saving onto an entry that was not the one opened (new, renamed or moved to another
/// scope) must never replace what is already there.
pub(crate) fn refuse_existing<E>(
    guard: Option<i64>,
    subject: &str,
    scope: Scope,
    name: &str,
    lookup: impl FnOnce() -> Result<Option<E>, String>,
) -> Result<(), String> {
    if guard.is_none() && lookup()?.is_some() {
        return Err(format!(
            "{subject} {name} already exists in the {} scope; open it from the list to edit it",
            scope.label()
        ));
    }
    Ok(())
}

fn checks_text(checks: bool) -> &'static str {
    if checks { "checks apply" } else { "no checks" }
}

fn help_text() -> String {
    format!(
        "Placeholders: {}. Task fields and context are always delivered; a placeholder only chooses where. {{{{context}}}} is runtime-only: previews show a labelled stand-in.\nIn a template line, \\n is a line break; write \\{{{{ for a literal {{{{.\nThe supervisor's rules, evidence handling and response schema are added around every template and cannot be edited here.",
        names()
    )
}

impl StoredWorkspace {
    pub(crate) fn open_categories(&mut self) -> Result<Dialog, String> {
        let entries = self.service.categories().map_err(|e| e.to_string())?;
        let labels: Vec<String> = entries
            .iter()
            .enumerate()
            .map(|(index, e)| {
                format!(
                    "{}. {} ({}, {})",
                    index + 1,
                    e.category.name,
                    e.category.scope.label(),
                    checks_text(e.category.checks)
                )
            })
            .collect();
        let mut choices = vec![NEW];
        choices.extend(labels.iter().map(String::as_str));
        let listing: String = entries
            .iter()
            .map(|e| {
                format!(
                    "{} · {} scope · project checks {} · {}\n",
                    e.category.name,
                    e.category.scope.label(),
                    if e.category.checks {
                        "apply"
                    } else {
                        "do not apply"
                    },
                    e.category.description
                )
            })
            .collect();
        let body = format!(
            "A work category tells AI turns how to execute, review, validate and remediate a kind of work. A project's own category overrides the user scope and the built-in code, documents and art categories of the same name; choose one on a task's form.\n\nCategories:\n{}\nChanges reach only tasks defined afterwards; a task revision keeps the templates it was defined with.\nSelect a category and choose Apply to edit, preview, override or delete it.\n{}",
            listing.trim_end(),
            help_text()
        );
        Ok(Dialog::new(
            DialogKind::Categories(entries),
            "Work categories",
            &body,
            vec![Field::choice(OPEN, &choices, NEW)],
        ))
    }
}

fn form(expected: Option<Entry>, base: &Category, from_template: bool, task_note: &str) -> Dialog {
    let stored = expected
        .as_ref()
        .is_some_and(|e| e.category.scope != Scope::BuiltIn);
    let mut actions = vec![SAVE.to_owned()];
    actions.extend(TEMPLATE_ROLES.iter().map(|role| preview_label(role)));
    if stored {
        actions.push(DELETE.into());
    }
    let mut fields = vec![
        Field::text(NAME, if from_template { "" } else { &base.name }),
        Field::choice(
            ACTION,
            &actions.iter().map(String::as_str).collect::<Vec<_>>(),
            SAVE,
        ),
        Field::choice(SCOPE, &[USER, PROJECT], scope_label(base.scope)),
        Field::choice(
            CHECKS,
            &[APPLY, SKIP],
            if base.checks { APPLY } else { SKIP },
        ),
        Field::text(DESCRIPTION, &base.description),
    ];
    for role in TEMPLATE_ROLES {
        fields.push(Field::text(
            &template_label(role),
            &to_line(base.templates.get(role).expect("template role")),
        ));
    }
    let title = match &expected {
        Some(e) if !from_template => format!(
            "Category {} ({})",
            e.category.name,
            e.category.scope.label()
        ),
        _ => "New category".into(),
    };
    Dialog::new(
        DialogKind::Category {
            expected: expected.map(Box::new),
        },
        &title,
        &format!(
            "{}{}\nSave in the project scope to override a user or built-in category of the same name for this project only. Choose Project checks Do not apply for documents or art-only work.\nPreview shows the effective prompt for {task_note}, with runtime-only parts labelled; nothing is saved until Save.",
            if from_template {
                "A new category starts as a copy of the code category; change what differs.\n"
            } else {
                ""
            },
            help_text()
        ),
        fields,
    )
}

/// The category the form describes, validated like any saved category.
fn category_from(dialog: &Dialog) -> Result<Category, String> {
    let scope = if dialog.get(SCOPE) == PROJECT {
        Scope::Project
    } else {
        Scope::User
    };
    let mut base = builtin::named(builtin::CODE).expect("code is built in");
    base.name = dialog.get(NAME).to_owned();
    base.description = dialog.get(DESCRIPTION).to_owned();
    base.checks = dialog.get(CHECKS) == APPLY;
    base.scope = scope;
    for role in TEMPLATE_ROLES {
        let line = dialog
            .fields
            .iter()
            .find(|field| field.label == template_label(role))
            .map_or("", |field| field.value.as_str());
        base.templates.set(role, from_line(line.trim()));
    }
    base.validate().map_err(|e| e.to_string())?;
    Ok(base)
}

/// What a preview fills in: the selected task, else an example.
pub(crate) fn preview_task(screen: &Screen) -> (String, String, Vec<String>, String) {
    match screen
        .selected_task()
        .filter(|t| t.definition().kind.runs_ai())
    {
        Some(task) => {
            let d = task.definition();
            (
                d.title.clone(),
                d.goal.clone(),
                d.check.lines().map(str::to_owned).collect(),
                format!("task #{}", task.id),
            )
        }
        None => (
            "Example task".into(),
            "Example goal: the outcome wanted.".into(),
            vec![
                "First example criterion".into(),
                "Second example criterion".into(),
            ],
            "an example task (select a task first to preview its content)".into(),
        ),
    }
}

fn preview_text(screen: &Screen, category: &Category, role: &str) -> Result<String, String> {
    let (title, goal, criteria, label) = preview_task(screen);
    let turn = Turn::parse(if role == "remediation" { "judge" } else { role });
    let text = prompt::preview(
        turn,
        category,
        &prompt::Task {
            title: &title,
            goal: &goal,
            criteria: &criteria,
        },
        label.starts_with("an example"),
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "Preview of the {role} prompt for {label}. Nothing is saved.\n\n{text}"
    ))
}

impl StoredWorkspace {
    pub(crate) fn save_category_form(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        match &dialog.kind {
            DialogKind::Categories(entries) => {
                let position = chosen_position(dialog, OPEN, "Select a category")?;
                let note = preview_task(screen).3;
                if position == 0 {
                    let base = builtin::named(builtin::CODE).expect("code is built in");
                    *dialog = form(None, &base, true, &note);
                } else {
                    let entry = entries
                        .get(position - FIXED)
                        .ok_or("Category is no longer listed; reopen the list")?
                        .clone();
                    *dialog = form(Some(entry.clone()), &entry.category, false, &note);
                }
                Ok(false)
            }
            DialogKind::Category { expected } => {
                let expected = expected.as_deref().cloned();
                let action = dialog.get(ACTION).to_owned();
                let service = &mut self.service;
                if action == DELETE {
                    let entry = expected.as_ref().ok_or("Nothing to delete")?;
                    service
                        .delete_category(
                            entry.category.scope,
                            &entry.category.name,
                            Some(entry.version),
                        )
                        .map_err(|e| e.to_string())?;
                    screen.message(format!(
                        "Deleted {} category {}; it inherits again",
                        entry.category.scope.label(),
                        entry.category.name
                    ));
                    return Ok(true);
                }
                let category = category_from(dialog)?;
                if let Some(role) = TEMPLATE_ROLES
                    .iter()
                    .find(|role| action == preview_label(role))
                {
                    dialog.body = preview_text(screen, &category, role)?;
                    dialog.scroll = 0;
                    return Ok(false);
                }
                let guard = save_guard(
                    expected
                        .as_ref()
                        .map(|e| (e.category.scope, e.category.name.as_str(), e.version)),
                    category.scope,
                    &category.name,
                );
                refuse_existing(guard, "Category", category.scope, &category.name, || {
                    service
                        .category_in(category.scope, &category.name)
                        .map_err(|e| e.to_string())
                })?;
                let saved = service
                    .save_category(category.scope, guard, &category)
                    .map_err(|e| e.to_string())?;
                screen.message(format!(
                    "Saved {} category {}; only tasks defined afterwards use it",
                    saved.category.scope.label(),
                    saved.category.name
                ));
                Ok(true)
            }
            _ => Err("Not a category form".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::categories::template::PLACEHOLDERS;

    #[test]
    fn form_lines_round_trip_templates() {
        for text in ["one\ntwo", "back\\slash", "\\{{title}} and \\n literal", ""] {
            assert_eq!(from_line(&to_line(text)), text, "{text:?}");
        }
        assert_eq!(from_line("a\\nb \\{{x}}"), "a\nb \\{{x}}");
    }

    #[test]
    fn help_names_every_placeholder() {
        let help = help_text();
        for placeholder in PLACEHOLDERS {
            assert!(help.contains(placeholder.name));
        }
    }
}
