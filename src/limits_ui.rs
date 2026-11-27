//! Configuration's resource limits and Current's Raise and resume. Both are forms over the
//! same application commands as `ktask limits` and `ktask run --raise`.
use crate::{
    app::Screen,
    application::resource_limits::{LimitChange, LimitScope, LimitSetting, ResourcePause},
    dialog::{Dialog, DialogKind, Field},
    persistent::StoredWorkspace,
    resource::{Limit, Stored},
};

const CHOOSE: &str = "Limit to edit";
const PROJECT_VALUE: &str = "Project value (blank = inherit)";
const PROJECT_CEILING: &str = "Project ceiling (blank = inherit)";
const INSTANCE_VALUE: &str = "Instance default value (blank = built-in)";
const INSTANCE_CEILING: &str = "Instance default ceiling (blank = built-in)";
const NEW_VALUE: &str = "New value";

fn used(setting: &LimitSetting) -> String {
    setting
        .high_water
        .map_or_else(String::new, |need| format!(" · used {need}"))
}

/// Every limit, its value in force and what a successful task needed.
fn overview(settings: &[LimitSetting]) -> String {
    let mut text = String::from(
        "Project values override instance defaults. A change applies to the next step of a paused attempt. Value / ceiling · source · most a successful task used:\n",
    );
    for setting in settings {
        text.push_str(&format!(
            "{}: {} / {} · {}{}\n",
            setting.limit,
            setting.value,
            setting.ceiling,
            setting.source,
            used(setting)
        ));
    }
    for warning in settings.iter().filter_map(|s| s.warning.as_deref()) {
        text.push_str(&format!("Warning: {warning}\n"));
    }
    text
}

fn edit_form(setting: LimitSetting) -> Dialog {
    let shown = |value: Option<u64>| value.map_or_else(String::new, |v| v.to_string());
    let Stored { value, ceiling, .. } = setting.project;
    let shared = setting.instance;
    let fields = vec![
        Field::text(PROJECT_VALUE, &shown(value)),
        Field::text(PROJECT_CEILING, &shown(ceiling)),
        Field::text(INSTANCE_VALUE, &shown(shared.value)),
        Field::text(INSTANCE_CEILING, &shown(shared.ceiling)),
    ];
    let body = format!(
        "In force: {} ({}) · ceiling {} · built-in {}{}{}\n{}\nThe ceiling bounds automatic extensions and Raise and resume.",
        setting.value,
        setting.source,
        setting.ceiling,
        setting.default,
        used(&setting),
        setting
            .project
            .extended
            .map_or_else(String::new, |to| format!(
                " · extended automatically to {to}"
            )),
        if setting.source == "env" {
            format!(
                "{} overrides every stored value while it is set.",
                setting.limit.env()
            )
        } else {
            "Values are bytes or counts.".into()
        }
    );
    Dialog::new(
        DialogKind::Limit(Box::new(setting.clone())),
        &format!("Resource limit · {}", setting.limit),
        &body,
        fields,
    )
}

fn number(dialog: &Dialog, label: &str) -> Result<Option<u64>, String> {
    let text = dialog.get(label).trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .ok()
        .filter(|&value| value > 0)
        .map(Some)
        .ok_or_else(|| format!("{label}: enter a positive integer or leave it blank"))
}

/// Only the fields the owner changed, so an untouched value is never rewritten.
fn change(
    dialog: &Dialog,
    (value, ceiling): (&str, &str),
    saved: Stored,
) -> Result<Option<LimitChange>, String> {
    let (value, ceiling) = (number(dialog, value)?, number(dialog, ceiling)?);
    let change = LimitChange {
        value: (value != saved.value).then_some(value),
        ceiling: (ceiling != saved.ceiling).then_some(ceiling),
    };
    Ok((change.value.is_some() || change.ceiling.is_some()).then_some(change))
}

fn raise_body(task: usize, pause: &ResourcePause, refusal: Option<&str>) -> String {
    let explanation = refusal.map_or_else(
        || {
            format!(
                "Task #{task} is paused on a resource limit: {}.",
                pause.describe()
            )
        },
        str::to_owned,
    );
    let action = pause.blocked.clone().unwrap_or_else(|| {
        format!(
            "Raise this project's {} (ceiling {}) and resume the same attempt. Recorded results are not repeated.",
            pause.limit, pause.ceiling
        )
    });
    format!("{explanation}\n{action}")
}

impl StoredWorkspace {
    pub(crate) fn limits_dialog(&mut self) -> Result<Dialog, String> {
        let settings = self.service.resource_limits().map_err(|e| e.to_string())?;
        let names: Vec<_> = Limit::SETTINGS.iter().map(|limit| limit.name()).collect();
        Ok(Dialog::new(
            DialogKind::Limits(settings.clone()),
            "Resource limits",
            &overview(&settings),
            vec![Field::choice(CHOOSE, &names, names[0])],
        ))
    }

    pub(crate) fn save_limits_form(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        match &dialog.kind {
            DialogKind::Limits(settings) => {
                let chosen = dialog.get(CHOOSE);
                let setting = settings
                    .iter()
                    .find(|setting| setting.limit.name() == chosen)
                    .ok_or("Select a limit")?;
                *dialog = edit_form(setting.clone());
                Ok(false)
            }
            DialogKind::Limit(expected) => {
                let mut changes = Vec::new();
                // The instance default first: it replaces an automatic raise.
                if let Some(shared) = change(
                    dialog,
                    (INSTANCE_VALUE, INSTANCE_CEILING),
                    expected.instance,
                )? {
                    changes.push((LimitScope::Instance, shared));
                }
                if let Some(own) =
                    change(dialog, (PROJECT_VALUE, PROJECT_CEILING), expected.project)?
                {
                    changes.push((LimitScope::Project, own));
                }
                if changes.is_empty() {
                    screen.message("Resource limit unchanged");
                    return Ok(true);
                }
                let saved = self
                    .service
                    .set_resource_limits(expected.limit, &changes, Some(expected))
                    .map_err(|e| e.to_string())?;
                self.committed(
                    screen,
                    &format!(
                        "Saved {}: {} in force ({})",
                        saved.limit, saved.value, saved.source
                    ),
                );
                Ok(true)
            }
            _ => Err("Not a resource limit form".into()),
        }
    }

    /// The Raise and resume dialog of the current paused task, with a refused Resume's
    /// explanation when there is one.
    pub(crate) fn raise_dialog(
        &mut self,
        task: usize,
        refusal: Option<&str>,
    ) -> Result<Dialog, String> {
        let pause = self
            .stored
            .get(&task)
            .and_then(|task| task.attempts.last())
            .and_then(|attempt| attempt.resource_pause.clone())
            .ok_or("This task is not paused on a resource limit")?;
        let proposal = pause.proposal.map_or_else(String::new, |to| to.to_string());
        Ok(Dialog::new(
            DialogKind::RaiseLimit(task),
            "Raise and resume",
            &raise_body(task, &pause, refusal),
            vec![Field::text(NEW_VALUE, &proposal)],
        ))
    }

    pub(crate) fn raise_and_resume(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
        task: usize,
    ) -> Result<bool, String> {
        let to = number(dialog, NEW_VALUE)?;
        let (limit, from, to) = self
            .service
            .raise_paused_limit(task as i64, to)
            .map_err(|e| e.to_string())?;
        self.reload(screen)?;
        screen.message(format!("Raised {limit} from {from} to {to}"));
        self.run(screen);
        Ok(true)
    }
}
