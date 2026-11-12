//! `ktask lens`: shared review-lens commands with JSON and text output.
use super::{Failure, Result, args::*, category::template_text, output::Reply, output::safe};
use crate::application::{
    Service,
    categories::{Scope, builtin},
    lenses::{Entry, Lens},
    prompt,
};
use serde_json::json;

fn found(service: &Service, name: &str, in_scope: Option<CategoryScope>) -> Result<Entry> {
    match in_scope {
        Some(chosen) => service.lens_in(Scope::from(chosen), name)?,
        None => service.lens(name)?,
    }
    .ok_or_else(|| {
        Failure::missing(format!(
            "No review lens named {name}{}; list them with `lens list`",
            in_scope.map_or(String::new(), |s| format!(
                " in the {} scope",
                Scope::from(s).label()
            ))
        ))
    })
}

fn text(entry: &Entry) -> String {
    let l = &entry.lens;
    format!(
        "Lens {} ({} scope, v{})\n{}\n\n[instructions]\n{}\n",
        safe(&l.name),
        l.scope.label(),
        entry.version,
        safe(&l.description),
        l.instructions
    )
}

fn save(
    service: &mut Service,
    name: &str,
    target: Scope,
    description: Option<&str>,
    instructions: Option<&str>,
    if_version: Option<i64>,
) -> Result<Reply> {
    if target == Scope::BuiltIn {
        return Err(Failure::usage(
            "Built-in lenses are fixed; save into --scope user or --scope project to override one",
        ));
    }
    let existing = service.lens_in(target, name)?;
    let base = match &existing {
        Some(entry) => Some(entry.lens.clone()),
        None => service.lens(name)?.map(|entry| entry.lens),
    };
    let mut lens = match base {
        Some(mut lens) => {
            // An override keeps the canonical spelling of the lens it overrides.
            if existing.is_none() {
                lens.name = name.to_owned();
            }
            lens
        }
        None => {
            let Some(instructions) = instructions else {
                return Err(Failure::usage(
                    "A new lens needs --instructions (text or @PATH)",
                ));
            };
            Lens {
                name: name.to_owned(),
                description: String::new(),
                instructions: template_text(instructions)?,
                scope: target,
            }
        }
    };
    if let Some(description) = description {
        lens.description = description.to_owned();
    }
    if let Some(instructions) = instructions {
        lens.instructions = template_text(instructions)?;
    }
    lens.scope = target;
    let created = existing.is_none();
    let saved = service.save_lens(target, if_version, &lens)?;
    Ok(Reply {
        data: json!({"action": if created { "created" } else { "saved" }, "lens": saved}),
        text: format!(
            "{}; only tasks defined later use it\n{}",
            if created { "created" } else { "saved" },
            text(&saved)
        ),
    })
}

fn defaults(
    service: &mut Service,
    set: &[String],
    clear: bool,
    current: Option<&[String]>,
) -> Result<Reply> {
    let before = service.lens_defaults()?;
    let changed = clear || !set.is_empty();
    let names = if changed {
        let expected: Vec<String> = current.map_or_else(
            || before.clone(),
            |names| names.iter().filter(|n| !n.is_empty()).cloned().collect(),
        );
        service.save_lens_defaults(&expected, set)?
    } else {
        before
    };
    Ok(Reply {
        data: json!({"changed": changed, "defaults": names}),
        text: format!(
            "{}{}\n",
            if changed {
                "saved; only tasks defined later use them. Default review lenses: "
            } else {
                "Default review lenses: "
            },
            if names.is_empty() {
                "none (ordinary single review)".to_owned()
            } else {
                names.join(", ")
            }
        ),
    })
}

fn preview(service: &mut Service, name: Option<&str>, task: Option<i64>) -> Result<Reply> {
    let (lens, category, content, from_task) = match task {
        Some(id) => {
            let task = service.task(id)?.ok_or_else(|| {
                Failure::missing(format!("Task #{id} does not exist in this project"))
            })?;
            let revision = task.definition();
            let lens = match name {
                Some(name) => found(service, name, None)?.lens,
                None => revision.lenses.first().cloned().ok_or_else(|| {
                    Failure::usage("The task pins no review lens; name one to preview")
                })?,
            };
            let category = revision.category.clone().ok_or_else(|| {
                Failure::usage("Human tasks have no AI review; there is nothing to preview")
            })?;
            let d = &revision.definition;
            (
                lens,
                category,
                (d.title.clone(), d.goal.clone(), d.criteria.clone()),
                true,
            )
        }
        None => (
            found(
                service,
                name.expect("clap requires a name without --task"),
                None,
            )?
            .lens,
            builtin::named(builtin::CODE).expect("code is built in"),
            (
                "Example task".to_owned(),
                "Example goal: the outcome wanted.".to_owned(),
                vec![
                    "First example criterion".to_owned(),
                    "Second example criterion".to_owned(),
                ],
            ),
            false,
        ),
    };
    let text = prompt::preview_lens(
        &category,
        &lens,
        &prompt::Task {
            title: &content.0,
            goal: &content.1,
            criteria: &content.2,
        },
        !from_task,
    )?;
    Ok(Reply {
        data: json!({
            "lens": lens.name, "scope": lens.scope, "category": category.name,
            "example_task": !from_task, "runtime_only": ["context"], "prompt": text,
        }),
        text: format!("{text}\n"),
    })
}

pub(super) fn lens(service: &mut Service, action: &LensAction) -> Result<Reply> {
    match action {
        LensAction::List => {
            let list = service.lenses()?;
            let selected = service.lens_defaults()?;
            Ok(Reply {
                data: json!({
                    "lenses": list.iter().map(|e| json!({
                        "name": e.lens.name, "scope": e.lens.scope, "version": e.version,
                        "description": e.lens.description,
                        "default": selected.iter().any(|n| n.eq_ignore_ascii_case(&e.lens.name)),
                    })).collect::<Vec<_>>(),
                    "defaults": selected,
                }),
                text: list
                    .iter()
                    .map(|e| {
                        format!(
                            "{}\t{}\tv{}\t{}\t{}\n",
                            safe(&e.lens.name),
                            e.lens.scope.label(),
                            e.version,
                            if selected
                                .iter()
                                .any(|n| n.eq_ignore_ascii_case(&e.lens.name))
                            {
                                "default"
                            } else {
                                "-"
                            },
                            safe(&e.lens.description)
                        )
                    })
                    .collect(),
            })
        }
        LensAction::Show { name, scope } => {
            let entry = found(service, name, *scope)?;
            Ok(Reply {
                data: json!({"lens": entry}),
                text: text(&entry),
            })
        }
        LensAction::Save {
            name,
            scope: chosen,
            description,
            instructions,
            if_version,
        } => save(
            service,
            name,
            Scope::from(*chosen),
            description.as_deref(),
            instructions.as_deref(),
            *if_version,
        ),
        LensAction::Delete {
            name,
            scope: chosen,
            if_version,
        } => {
            service.delete_lens(Scope::from(*chosen), name, *if_version)?;
            Ok(Reply {
                data: json!({"action": "deleted", "name": name, "scope": Scope::from(*chosen)}),
                text: format!(
                    "deleted {} lens {}; tasks that pinned it keep their copy\n",
                    Scope::from(*chosen).label(),
                    safe(name)
                ),
            })
        }
        LensAction::Defaults {
            set,
            clear,
            if_current,
        } => defaults(service, set, *clear, if_current.as_deref()),
        LensAction::Preview { name, task } => preview(service, name.as_deref(), *task),
    }
}
