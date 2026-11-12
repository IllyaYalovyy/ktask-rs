//! Model profile forms: list, create, edit, select and delete through shared commands.
use crate::{
    app::Screen,
    application::{
        Service,
        configuration::{Defaults, MODEL_ROLES},
        profiles::{Effective, Profile, Usage},
        user_defaults::model_text,
    },
    configuration_ui::{DEFAULT_PROFILE, model_fields, models_from},
    dialog::{Dialog, DialogKind, Field},
    persistent::StoredWorkspace,
};

const OPEN: &str = "Profile";
const NEW: &str = "New profile";
const COPY: &str = "New from this project's models";
const NONE_HERE: &str = "No profile for this project";
const NONE_DEFAULT: &str = "No default for new projects";
/// Entries before the numbered profiles in the list's choice field.
const FIXED: usize = 4;
const NAME: &str = "Profile name";
const ACTION: &str = "Action";
const SAVE: &str = "Save";
const SAVE_SELECT: &str = "Save and select for this project";
const SAVE_DEFAULT: &str = "Save and make the default for new projects";
const DELETE: &str = "Delete profile";
const NONE: &str = "None";

/// Each role with its effective model and the layer it came from.
pub(crate) fn effective_text(rows: &[Effective]) -> String {
    rows.iter()
        .map(|row| {
            format!(
                "{}: {} [{}]",
                row.role,
                model_text(row.model.as_ref()),
                row.source.label()
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The default-profile choice of the user defaults form.
pub(crate) fn default_profile_field(
    service: &Service,
    selected: Option<&str>,
) -> Result<Field, String> {
    let profiles = service.profiles().map_err(|e| e.to_string())?;
    let mut choices = vec![NONE];
    choices.extend(profiles.iter().map(|p| p.name.as_str()));
    Ok(Field::choice(
        DEFAULT_PROFILE,
        &choices,
        selected.unwrap_or(NONE),
    ))
}

/// The profile a form choice names, if any.
pub(crate) fn chosen_profile(value: &str) -> Option<String> {
    (!value.eq_ignore_ascii_case(NONE)).then(|| value.to_owned())
}

fn use_text(usage: &Usage) -> String {
    if usage.in_use() {
        format!("used by {}", usage.describe())
    } else {
        "unused".into()
    }
}

fn models_text(profile: &Profile) -> String {
    MODEL_ROLES
        .iter()
        .zip(&profile.models)
        .filter_map(|(role, model)| {
            model
                .as_ref()
                .map(|m| format!("{role} {}", model_text(Some(m))))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

impl StoredWorkspace {
    pub(crate) fn open_profiles(&mut self) -> Result<Dialog, String> {
        let service = &self.service;
        let profiles = service.profiles().map_err(|e| e.to_string())?;
        let selected = service.selected_profile().map_err(|e| e.to_string())?;
        let default = service
            .user_defaults()
            .map_err(|e| e.to_string())?
            .default_profile;
        let effective = service.effective_models().map_err(|e| e.to_string())?;
        let mut choices = vec![NEW, COPY, NONE_HERE, NONE_DEFAULT];
        let labels: Vec<String> = profiles
            .iter()
            .enumerate()
            .map(|(index, profile)| {
                let usage = service.profile_usage(profile).map_err(|e| e.to_string())?;
                Ok(format!(
                    "{}. {}{}",
                    index + 1,
                    profile.name,
                    match (
                        usage.in_use(),
                        selected.as_ref().is_some_and(|s| s.id == profile.id)
                    ) {
                        (_, true) => " (this project)",
                        (true, false) => " (in use)",
                        _ => "",
                    }
                ))
            })
            .collect::<Result<_, String>>()?;
        choices.extend(labels.iter().map(String::as_str));
        let mut listing = String::new();
        for profile in &profiles {
            let usage = service.profile_usage(profile).map_err(|e| e.to_string())?;
            listing.push_str(&format!(
                "{} v{} · {} · {}\n",
                profile.name,
                profile.version,
                use_text(&usage),
                models_text(profile)
            ));
        }
        if listing.is_empty() {
            listing.push_str("No profiles yet.\n");
        }
        let body = format!(
            "A profile maps roles to models the existing providers run; it does not add providers. Precedence: user defaults, then the project's profile, then project overrides, then a task's own models.\nThis project: {} · new projects: {}\n\nEffective models for tasks defined now:\n{}\n\nProfiles:\n{}\nChanges reach only tasks defined afterwards; existing revisions and attempts stay pinned, and pending tasks change only when you apply the current defaults from their Models form.\nSelect a profile and choose Apply to edit, select or delete it.",
            selected.as_ref().map_or("no profile", |p| p.name.as_str()),
            default.as_deref().unwrap_or("no default profile"),
            effective_text(&effective),
            listing.trim_end(),
        );
        let start = selected
            .as_ref()
            .and_then(|s| profiles.iter().position(|p| p.id == s.id))
            .map_or(NEW, |index| choices[FIXED + index]);
        Ok(Dialog::new(
            DialogKind::Profiles(profiles),
            "Model profiles",
            &body,
            vec![Field::choice(OPEN, &choices, start)],
        ))
    }
}

fn profile_form(
    expected: Option<Profile>,
    name: &str,
    models: &Defaults,
    usage: Option<Usage>,
) -> Dialog {
    let mut actions = vec![SAVE, SAVE_SELECT, SAVE_DEFAULT];
    if expected.is_some() {
        actions.push(DELETE);
    }
    // The action sits beside the name so it stays in view however long the form is.
    let mut fields = vec![
        Field::text(NAME, name),
        Field::choice(ACTION, &actions, SAVE),
    ];
    fields.extend(model_fields(models));
    let title = match &expected {
        Some(profile) => format!("Profile {}", profile.name),
        None => "New profile".into(),
    };
    let used = usage.map_or(String::new(), |u| {
        format!("This profile is {}. ", use_text(&u))
    });
    Dialog::new(
        DialogKind::Profile {
            expected: expected.map(Box::new),
        },
        &title,
        &format!(
            "{used}Blank roles inherit the user defaults. Use provider/model: codex/gpt-6-sol, claude/opus, opencode/anthropic/claude-sonnet-5-5; effort uses the provider's own spellings.\nA profile in use can change a role's model but not drop the role; Delete is refused while a project or the new-project default selects it.\nFuture tasks use the new values; existing revisions and attempts stay pinned."
        ),
        fields,
    )
}

impl StoredWorkspace {
    pub(crate) fn save_profile_form(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        match &dialog.kind {
            DialogKind::Profiles(profiles) => {
                let chosen = dialog.get(OPEN).to_owned();
                let position = dialog
                    .fields
                    .iter()
                    .find(|field| field.label == OPEN)
                    .and_then(|field| field.choices.iter().position(|c| *c == chosen))
                    .ok_or("Select a profile")?;
                let service = &mut self.service;
                match position {
                    0 => {
                        *dialog = profile_form(None, "", &std::array::from_fn(|_| None), None);
                        return Ok(false);
                    }
                    1 => {
                        let models = service.project_defaults().map_err(|e| e.to_string())?;
                        *dialog = profile_form(None, "", &models, None);
                        return Ok(false);
                    }
                    2 => {
                        service.select_profile(None).map_err(|e| e.to_string())?;
                        screen.message("Project uses the user defaults without a profile");
                    }
                    3 => {
                        service
                            .set_default_profile(None)
                            .map_err(|e| e.to_string())?;
                        screen.message("New projects start without a profile");
                    }
                    _ => {
                        let profile = profiles
                            .get(position - FIXED)
                            .ok_or("Profile is no longer listed; reopen the list")?
                            .clone();
                        let usage = service.profile_usage(&profile).map_err(|e| e.to_string())?;
                        *dialog = profile_form(
                            Some(profile.clone()),
                            &profile.name,
                            &profile.models,
                            Some(usage),
                        );
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            DialogKind::Profile { expected } => {
                let expected = expected.as_deref().cloned();
                let name = dialog.get(NAME).to_owned();
                let action = dialog.get(ACTION).to_owned();
                let service = &mut self.service;
                if action == DELETE {
                    let profile = expected.as_ref().ok_or("Nothing to delete")?;
                    service.delete_profile(profile).map_err(|e| e.to_string())?;
                    screen.message(format!("Deleted profile {}", profile.name));
                    return Ok(true);
                }
                let models = models_from(dialog)?;
                let saved = match &expected {
                    Some(profile) => service.save_profile(profile, &name, &models),
                    None => service.create_profile(&name, &models),
                }
                .map_err(|e| e.to_string())?;
                let mut message = format!("Saved profile {}", saved.name);
                if action == SAVE_SELECT {
                    service
                        .select_profile(Some(&saved.name))
                        .map_err(|e| e.to_string())?;
                    message.push_str("; this project uses it");
                } else if action == SAVE_DEFAULT {
                    service
                        .set_default_profile(Some(&saved.name))
                        .map_err(|e| e.to_string())?;
                    message.push_str("; new projects start with it");
                }
                screen.message(message);
                Ok(true)
            }
            _ => Err("Not a profile form".into()),
        }
    }
}
