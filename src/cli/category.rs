//! `ktask category`: shared work-category commands with JSON and text output.
use super::{Failure, Result, args::*, output::Reply, output::safe};
use crate::application::{
    Service,
    categories::{
        Category, Entry, Scope, TEMPLATE_ROLES,
        template::{PLACEHOLDERS, names},
    },
    prompt::{self, Turn},
};
use serde_json::json;

const MAX_FILE: u64 = 64 * 1024;

fn scope(value: CategoryScope) -> Scope {
    match value {
        CategoryScope::User => Scope::User,
        CategoryScope::Project => Scope::Project,
        CategoryScope::BuiltIn => Scope::BuiltIn,
    }
}

fn role(value: CategoryRole) -> &'static str {
    match value {
        CategoryRole::Execution => "execution",
        CategoryRole::Review => "review",
        CategoryRole::Validation => "validation",
        CategoryRole::Remediation => "remediation",
    }
}

fn found(service: &Service, name: &str, in_scope: Option<CategoryScope>) -> Result<Entry> {
    match in_scope {
        Some(chosen) => service.category_in(scope(chosen), name)?,
        None => service.category(name)?,
    }
    .ok_or_else(|| {
        Failure::missing(format!(
            "No work category named {name}{}; list them with `category list`",
            in_scope.map_or(String::new(), |s| format!(
                " in the {} scope",
                scope(s).label()
            ))
        ))
    })
}

fn summary_line(entry: &Entry) -> String {
    format!(
        "{}\t{}\tchecks {}\tv{}\t{}\n",
        safe(&entry.category.name),
        entry.category.scope.label(),
        if entry.category.checks { "yes" } else { "no" },
        entry.version,
        safe(&entry.category.description)
    )
}

fn text(entry: &Entry) -> String {
    let c = &entry.category;
    let mut out = format!(
        "Category {} ({} scope, v{}) · project checks {}\n{}\n",
        safe(&c.name),
        c.scope.label(),
        entry.version,
        if c.checks { "apply" } else { "do not apply" },
        safe(&c.description)
    );
    for role in TEMPLATE_ROLES {
        out.push_str(&format!(
            "\n[{role}]\n{}\n",
            c.templates.get(role).expect("template role")
        ));
    }
    out
}

/// A template given on the command line, or read from the file after `@`.
fn template_text(value: &str) -> Result<String> {
    if let Some(literal) = value.strip_prefix("@@") {
        return Ok(format!("@{literal}"));
    }
    let Some(path) = value.strip_prefix('@') else {
        return Ok(value.to_owned());
    };
    let file = std::fs::File::open(path)
        .map_err(|e| Failure::usage(format!("Cannot read template file {path}: {e}")))?;
    let meta = file.metadata().map_err(crate::storage::Error::Io)?;
    if !meta.is_file() || meta.len() > MAX_FILE {
        return Err(Failure::usage(format!(
            "Template file {path} must be a regular file of at most 64 KiB"
        )));
    }
    std::fs::read_to_string(path)
        .map(|text| text.trim_end().to_owned())
        .map_err(|e| Failure::usage(format!("Cannot read template file {path}: {e}")))
}

struct Save<'a> {
    name: &'a str,
    scope: CategoryScope,
    from: Option<&'a str>,
    description: Option<&'a str>,
    checks: Option<bool>,
    templates: [(&'static str, Option<&'a str>); 4],
    if_version: Option<i64>,
}

fn save(service: &mut Service, args: &Save<'_>) -> Result<Reply> {
    let target = scope(args.scope);
    if target == Scope::BuiltIn {
        return Err(Failure::usage(
            "Built-in categories are fixed; save into --scope user or --scope project to override one",
        ));
    }
    let existing = service.category_in(target, args.name)?;
    let base = match (&existing, args.from) {
        (Some(entry), None) => Some(entry.category.clone()),
        (_, Some(from)) => Some(found(service, from, None)?.category),
        (None, None) => service.category(args.name)?.map(|entry| entry.category),
    };
    let mut category = match base {
        Some(mut category) => {
            // An override keeps the canonical spelling of the category it overrides.
            if args.from.is_some() && existing.is_none() {
                category.name = args.name.to_owned();
            }
            category
        }
        None => {
            let missing: Vec<_> = args
                .templates
                .iter()
                .filter(|(_, text)| text.is_none())
                .map(|(role, _)| format!("--{role}"))
                .collect();
            if !missing.is_empty() || args.checks.is_none() {
                return Err(Failure::usage(format!(
                    "A new category needs all four templates and --checks or --no-checks (or --from NAME); missing: {}",
                    if args.checks.is_none() {
                        missing
                            .iter()
                            .map(String::as_str)
                            .chain(["--checks|--no-checks"])
                            .collect::<Vec<_>>()
                            .join(", ")
                    } else {
                        missing.join(", ")
                    }
                )));
            }
            Category {
                name: args.name.to_owned(),
                description: String::new(),
                checks: false,
                templates: crate::application::categories::builtin::named("code")
                    .expect("code is built in")
                    .templates,
                scope: target,
            }
        }
    };
    if let Some(description) = args.description {
        category.description = description.to_owned();
    }
    if let Some(checks) = args.checks {
        category.checks = checks;
    }
    for (role, value) in &args.templates {
        if let Some(value) = value {
            category.templates.set(role, template_text(value)?);
        }
    }
    category.scope = target;
    let created = existing.is_none();
    let saved = service.save_category(target, args.if_version, &category)?;
    Ok(Reply {
        data: json!({"action": if created { "created" } else { "saved" }, "category": saved}),
        text: format!(
            "{}; only tasks defined later use it\n{}",
            if created { "created" } else { "saved" },
            text(&saved)
        ),
    })
}

fn preview(
    service: &mut Service,
    name: Option<&str>,
    task: Option<i64>,
    role_only: Option<CategoryRole>,
    in_scope: Option<CategoryScope>,
) -> Result<Reply> {
    let example = (
        "Example task".to_owned(),
        "Example goal: the outcome wanted.".to_owned(),
        vec![
            "First example criterion".to_owned(),
            "Second example criterion".to_owned(),
        ],
    );
    let (category, content, from_task) = match task {
        Some(id) => {
            let task = service.task(id)?.ok_or_else(|| {
                Failure::missing(format!("Task #{id} does not exist in this project"))
            })?;
            let revision = task.definition();
            let category = match name {
                Some(name) => found(service, name, None)?.category,
                None => revision.category.clone().ok_or_else(|| {
                    Failure::usage("Human tasks have no work category; name one to preview")
                })?,
            };
            let d = &revision.definition;
            (
                category,
                (d.title.clone(), d.goal.clone(), d.criteria.clone()),
                true,
            )
        }
        None => (
            found(
                service,
                name.expect("clap requires a name without --task"),
                in_scope,
            )?
            .category,
            example,
            false,
        ),
    };
    let roles: Vec<&str> = role_only.map_or_else(|| TEMPLATE_ROLES.to_vec(), |r| vec![role(r)]);
    let mut out = String::new();
    let mut data = Vec::new();
    for role in roles {
        let turn = Turn::parse(if role == "remediation" { "judge" } else { role });
        let text = prompt::preview(
            turn,
            &category,
            &prompt::Task {
                title: &content.0,
                goal: &content.1,
                criteria: &content.2,
            },
            !from_task,
        )?;
        out.push_str(&format!("######## {role}\n{text}\n\n"));
        data.push(json!({"role": role, "prompt": text}));
    }
    Ok(Reply {
        data: json!({
            "category": category.name,
            "scope": category.scope,
            "checks": category.checks,
            "example_task": !from_task,
            "runtime_only": ["context", "worker callback"],
            "previews": data,
        }),
        text: out,
    })
}

pub(super) fn category(service: &mut Service, action: &CategoryAction) -> Result<Reply> {
    match action {
        CategoryAction::List => {
            let list = service.categories()?;
            Ok(Reply {
                data: json!({"categories": list.iter().map(|e| json!({
                    "name": e.category.name, "scope": e.category.scope, "version": e.version,
                    "checks": e.category.checks, "description": e.category.description,
                })).collect::<Vec<_>>()}),
                text: list.iter().map(summary_line).collect(),
            })
        }
        CategoryAction::Show { name, scope } => {
            let entry = found(service, name, *scope)?;
            Ok(Reply {
                data: json!({"category": entry}),
                text: text(&entry),
            })
        }
        CategoryAction::Save {
            name,
            scope,
            from,
            description,
            checks,
            no_checks,
            execution,
            review,
            validation,
            remediation,
            if_version,
        } => save(
            service,
            &Save {
                name,
                scope: *scope,
                from: from.as_deref(),
                description: description.as_deref(),
                checks: if *checks {
                    Some(true)
                } else if *no_checks {
                    Some(false)
                } else {
                    None
                },
                templates: [
                    ("execution", execution.as_deref()),
                    ("review", review.as_deref()),
                    ("validation", validation.as_deref()),
                    ("remediation", remediation.as_deref()),
                ],
                if_version: *if_version,
            },
        ),
        CategoryAction::Delete {
            name,
            scope: chosen,
            if_version,
        } => {
            service.delete_category(scope(*chosen), name, *if_version)?;
            Ok(Reply {
                data: json!({"action": "deleted", "name": name, "scope": scope(*chosen)}),
                text: format!(
                    "deleted {} category {}; tasks that pinned it keep their copy\n",
                    scope(*chosen).label(),
                    safe(name)
                ),
            })
        }
        CategoryAction::Preview {
            name,
            task,
            role,
            scope,
        } => preview(service, name.as_deref(), *task, *role, *scope),
        CategoryAction::Placeholders => Ok(Reply {
            data: json!({"placeholders": PLACEHOLDERS.iter().map(|p| json!({
                "name": p.name, "runtime_only": p.runtime, "description": p.description,
            })).collect::<Vec<_>>()}),
            text: format!(
                "Placeholders ({}):\n{}\nTask fields and context are always delivered: one a template leaves out is appended after it. Write \\{{{{ for a literal {{{{.\n",
                names(),
                PLACEHOLDERS
                    .iter()
                    .map(|p| format!(
                        "  {{{{{}}}}}\t{}{}\n",
                        p.name,
                        p.description,
                        if p.runtime {
                            " (runtime-only: previews show a labelled stand-in)"
                        } else {
                            ""
                        }
                    ))
                    .collect::<String>()
            ),
        }),
    }
}
