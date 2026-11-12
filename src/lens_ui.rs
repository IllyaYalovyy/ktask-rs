//! Review-lens forms: list, create, edit, override per project, preview, delete, the
//! project's default selection, and the ordered lens choice on a task form, all through the
//! shared lens commands.
use crate::{
    app::Screen,
    application::{
        categories::{Scope, builtin as categories, template::names},
        lenses::{Entry, Lens, MAX_SELECTED},
        prompt,
    },
    category_ui::{
        PROJECT, SCOPE, USER, chosen_position, from_line, preview_task, refuse_existing,
        save_guard, scope_label, to_line,
    },
    dialog::{Dialog, DialogKind, Field},
    persistent::StoredWorkspace,
};

const OPEN: &str = "Lens";
const NEW: &str = "New lens";
const DEFAULTS: &str = "Project default lenses";
const NAME: &str = "Lens name";
const ACTION: &str = "Action";
const DESCRIPTION: &str = "Description";
const INSTRUCTIONS: &str = "Instructions";
const SAVE: &str = "Save";
const PREVIEW: &str = "Preview prompt";
const DELETE: &str = "Delete (inherit again)";
/// Entries before the numbered lenses in the list's choice field.
const FIXED: usize = 2;

/// The first slot's choices that mean no explicit lens.
pub(crate) const PROJECT_DEFAULT: &str = "Project default";
pub(crate) const NO_LENS: &str = "None (ordinary review)";
/// An empty later slot.
const EMPTY: &str = "-";
const SLOT: &str = "Review lens";

fn slot_label(index: usize) -> String {
    format!("{SLOT} {}", index + 1)
}

/// The ordered lens slots of a task or defaults form: the first picks the project default,
/// none, or its lens; the following ones add lenses in review order.
pub(crate) fn slots(current: &[String], available: &[String], first_default: bool) -> Vec<Field> {
    (0..MAX_SELECTED)
        .map(|index| {
            let mut choices: Vec<String> = if index == 0 && first_default {
                vec![PROJECT_DEFAULT.into(), NO_LENS.into()]
            } else if index == 0 {
                vec![NO_LENS.into()]
            } else {
                vec![EMPTY.into()]
            };
            choices.extend(available.iter().cloned());
            let chosen = current.get(index).cloned();
            if let Some(name) = chosen.as_ref().filter(|n| !choices.contains(n)) {
                choices.push(name.clone());
            }
            let value = chosen.unwrap_or_else(|| choices[0].clone());
            let refs: Vec<&str> = choices.iter().map(String::as_str).collect();
            Field::choice(&slot_label(index), &refs, &value)
        })
        .collect()
}

/// The selection the slots of `dialog` make: `None` for the project default, otherwise the
/// chosen lenses in order (empty for the ordinary review).
pub(crate) fn selection(dialog: &Dialog) -> Result<Option<Vec<String>>, String> {
    let chosen: Vec<&str> = (0..MAX_SELECTED)
        .map(slot_label)
        .filter_map(|label| {
            dialog
                .fields
                .iter()
                .find(|f| f.label == label)
                .map(|f| f.value.as_str())
        })
        .collect();
    let first = chosen.first().copied().unwrap_or(PROJECT_DEFAULT);
    let rest: Vec<String> = chosen
        .iter()
        .skip(1)
        .filter(|v| **v != EMPTY)
        .map(|v| (*v).to_owned())
        .collect();
    match first {
        PROJECT_DEFAULT | NO_LENS => {
            if rest.is_empty() {
                Ok((first == NO_LENS).then(Vec::new))
            } else {
                Err(format!(
                    "Choose a lens in {} before adding more, or clear the later lenses",
                    slot_label(0)
                ))
            }
        }
        name => Ok(Some(std::iter::once(name.to_owned()).chain(rest).collect())),
    }
}

/// The names of the lenses a form can offer.
pub(crate) fn available(entries: Vec<Entry>) -> Vec<String> {
    entries.into_iter().map(|e| e.lens.name).collect()
}

fn help_text() -> String {
    format!(
        "Placeholders: {}. Task fields and context are always delivered; a placeholder only chooses where. {{{{context}}}} is runtime-only: previews show a labelled stand-in.\nIn the instructions line, \\n is a line break; write \\{{{{ for a literal {{{{.\nThe supervisor's rules, evidence handling and response schema are added around every lens and cannot be edited here.",
        names()
    )
}

fn defaults_text(defaults: &[String]) -> String {
    if defaults.is_empty() {
        "none (ordinary single review)".into()
    } else {
        defaults.join(", ")
    }
}

impl StoredWorkspace {
    pub(crate) fn open_lenses(&mut self) -> Result<Dialog, String> {
        let entries = self.service.lenses().map_err(|e| e.to_string())?;
        let defaults = self.service.lens_defaults().map_err(|e| e.to_string())?;
        let mut choices = vec![NEW.to_owned(), DEFAULTS.to_owned()];
        choices.extend(entries.iter().enumerate().map(|(index, e)| {
            format!("{}. {} ({})", index + 1, e.lens.name, e.lens.scope.label())
        }));
        let listing: String = entries
            .iter()
            .map(|e| {
                format!(
                    "{} · {} scope · {}\n",
                    e.lens.name,
                    e.lens.scope.label(),
                    e.lens.description
                )
            })
            .collect();
        let body = format!(
            "A review lens is a specialist review, such as concurrency, security or simplicity. A task that selects lenses is reviewed once per lens, each in its own read-only session, instead of by the single ordinary review; one repair is requested only after every lens has finished. Specialist reviews are opt-in: choose lenses on a task's form or as the project's defaults.\n\nDefault lenses of this project: {}\n\nLenses:\n{}\nChanges reach only tasks defined afterwards; a task revision keeps the instructions it was defined with.\nSelect a lens and choose Apply to edit, preview, override or delete it.\n{}",
            defaults_text(&defaults),
            listing.trim_end(),
            help_text()
        );
        let refs: Vec<&str> = choices.iter().map(String::as_str).collect();
        Ok(Dialog::new(
            DialogKind::Lenses(entries),
            "Review lenses",
            &body,
            vec![Field::choice(OPEN, &refs, NEW)],
        ))
    }

    fn defaults_form(&self, entries: Vec<Entry>) -> Result<Dialog, String> {
        let defaults = self.service.lens_defaults().map_err(|e| e.to_string())?;
        Ok(Dialog::new(
            DialogKind::LensDefaults(defaults.clone()),
            "Project default lenses",
            "The lenses new tasks of this project select, in review order. Choose None to keep the ordinary single review for new tasks. A task can always choose its own; existing tasks keep the lenses their revisions pinned.",
            slots(&defaults, &available(entries), false),
        ))
    }
}

fn lens_form(expected: Option<Entry>, base: &Lens, from_new: bool, note: &str) -> Dialog {
    let stored = expected
        .as_ref()
        .is_some_and(|e| e.lens.scope != Scope::BuiltIn);
    let mut actions = vec![SAVE, PREVIEW];
    if stored {
        actions.push(DELETE);
    }
    let fields = vec![
        Field::text(NAME, if from_new { "" } else { &base.name }),
        Field::choice(ACTION, &actions, SAVE),
        Field::choice(SCOPE, &[USER, PROJECT], scope_label(base.scope)),
        Field::text(DESCRIPTION, if from_new { "" } else { &base.description }),
        Field::text(INSTRUCTIONS, &to_line(&base.instructions)),
    ];
    let title = match &expected {
        Some(e) if !from_new => format!("Lens {} ({})", e.lens.name, e.lens.scope.label()),
        _ => "New lens".into(),
    };
    Dialog::new(
        DialogKind::Lens {
            expected: expected.map(Box::new),
        },
        &title,
        &format!(
            "{}{}\nSave in the project scope to override a user or built-in lens of the same name for this project only. Preview shows the effective prompt for {note}, with runtime-only parts labelled; nothing is saved until Save.",
            if from_new {
                "A new lens starts as a copy of the security lens; change what differs.\n"
            } else {
                ""
            },
            help_text()
        ),
        fields,
    )
}

fn lens_from(dialog: &Dialog) -> Result<Lens, String> {
    let lens = Lens {
        name: dialog.get(NAME).to_owned(),
        description: dialog.get(DESCRIPTION).to_owned(),
        instructions: from_line(dialog.get(INSTRUCTIONS).trim()),
        scope: if dialog.get(SCOPE) == PROJECT {
            Scope::Project
        } else {
            Scope::User
        },
    };
    lens.validate().map_err(|e| e.to_string())?;
    Ok(lens)
}

fn preview_text(screen: &Screen, lens: &Lens) -> Result<String, String> {
    let (title, goal, criteria, label) = preview_task(screen);
    let category =
        categories::named(crate::application::categories::builtin::CODE).expect("code is built in");
    let text = prompt::preview_lens(
        &category,
        lens,
        &prompt::Task {
            title: &title,
            goal: &goal,
            criteria: &criteria,
        },
        label.starts_with("an example"),
    )
    .map_err(|e| e.to_string())?;
    Ok(format!(
        "Preview of the {} lens prompt for {label}. Nothing is saved.\n\n{text}",
        lens.name
    ))
}

impl StoredWorkspace {
    pub(crate) fn save_lens_form(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        match &dialog.kind {
            DialogKind::Lenses(entries) => {
                let position = chosen_position(dialog, OPEN, "Select a lens")?;
                let note = preview_task(screen).3;
                let entries = entries.clone();
                *dialog = match position {
                    0 => lens_form(
                        None,
                        &crate::application::lenses::builtin::named(
                            crate::application::lenses::builtin::SECURITY,
                        )
                        .expect("security is built in"),
                        true,
                        &note,
                    ),
                    1 => self.defaults_form(entries)?,
                    _ => {
                        let entry = entries
                            .get(position - FIXED)
                            .ok_or("Lens is no longer listed; reopen the list")?
                            .clone();
                        lens_form(Some(entry.clone()), &entry.lens, false, &note)
                    }
                };
                Ok(false)
            }
            DialogKind::LensDefaults(expected) => {
                let names = selection(dialog)?.unwrap_or_default();
                let expected = expected.clone();
                let saved = self
                    .service
                    .save_lens_defaults(&expected, &names)
                    .map_err(|e| e.to_string())?;
                screen.message(format!(
                    "Default review lenses: {}; only tasks defined afterwards use them",
                    defaults_text(&saved)
                ));
                Ok(true)
            }
            DialogKind::Lens { expected } => {
                let expected = expected.as_deref().cloned();
                let action = dialog.get(ACTION).to_owned();
                if action == DELETE {
                    let entry = expected.as_ref().ok_or("Nothing to delete")?;
                    self.service
                        .delete_lens(entry.lens.scope, &entry.lens.name, Some(entry.version))
                        .map_err(|e| e.to_string())?;
                    screen.message(format!(
                        "Deleted {} lens {}; it inherits again",
                        entry.lens.scope.label(),
                        entry.lens.name
                    ));
                    return Ok(true);
                }
                let lens = lens_from(dialog)?;
                if action == PREVIEW {
                    dialog.body = preview_text(screen, &lens)?;
                    dialog.scroll = 0;
                    return Ok(false);
                }
                let guard = save_guard(
                    expected
                        .as_ref()
                        .map(|e| (e.lens.scope, e.lens.name.as_str(), e.version)),
                    lens.scope,
                    &lens.name,
                );
                refuse_existing(guard, "Lens", lens.scope, &lens.name, || {
                    self.service
                        .lens_in(lens.scope, &lens.name)
                        .map_err(|e| e.to_string())
                })?;
                let saved = self
                    .service
                    .save_lens(lens.scope, guard, &lens)
                    .map_err(|e| e.to_string())?;
                screen.message(format!(
                    "Saved {} lens {}; only tasks defined afterwards use it",
                    saved.lens.scope.label(),
                    saved.lens.name
                ));
                Ok(true)
            }
            _ => Err("Not a lens form".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn dialog(current: &[&str], first_default: bool) -> Dialog {
        Dialog::new(
            DialogKind::Help,
            "t",
            "",
            slots(
                &names(current),
                &names(&["security", "simplicity"]),
                first_default,
            ),
        )
    }

    #[test]
    fn slots_offer_every_selectable_lens_and_start_at_the_project_default() {
        assert_eq!(dialog(&[], true).fields.len(), MAX_SELECTED);
        assert_eq!(selection(&dialog(&[], true)), Ok(None));
        let five = ["a", "b", "c", "d", "e"];
        assert_eq!(dialog(&five, true).fields.len(), 5);
        assert_eq!(selection(&dialog(&five, true)), Ok(Some(names(&five))));
    }

    #[test]
    fn the_first_slot_chooses_default_none_or_a_lens_and_later_ones_follow_it() {
        let mut form = dialog(&["security", "simplicity"], true);
        assert_eq!(
            selection(&form),
            Ok(Some(names(&["security", "simplicity"])))
        );
        form.fields[0].value = NO_LENS.into();
        assert!(selection(&form).is_err(), "a later lens needs a first lens");
        form.fields[1].value = EMPTY.into();
        assert_eq!(selection(&form), Ok(Some(Vec::new())));
        form.fields[0].value = PROJECT_DEFAULT.into();
        assert_eq!(selection(&form), Ok(None));
    }

    #[test]
    fn a_lens_a_form_pinned_stays_selectable_after_it_was_deleted() {
        let form = dialog(&["old lens"], true);
        assert!(form.fields[0].choices.contains(&"old lens".to_owned()));
        assert_eq!(selection(&form), Ok(Some(names(&["old lens"]))));
    }
}
