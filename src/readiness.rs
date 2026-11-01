//! Read-only launch inspection for the Current Task screen.
use crate::{
    app::App,
    application::{Kind, ROLES, Service},
    presentation::TaskAction,
    runner::provider::Provider,
};
use std::{fs::File, path::Path, process::Command};

fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("--no-optional-locks")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().into())
}

#[derive(Clone)]
pub(crate) struct Readiness {
    pub lines: Vec<String>,
    pub blockers: usize,
    pub refresh_failed: bool,
}

impl Readiness {
    fn check(&mut self, name: &str, result: Result<(), String>, action: &str) {
        match result {
            Ok(()) => self.lines.push(format!("OK · {name}")),
            Err(reason) => {
                self.blockers += 1;
                self.lines
                    .push(format!("BLOCK · {name}: {reason} → {action}"));
            }
        }
    }
}

fn commit_identity(report: &mut Readiness, root: &Path) {
    match crate::runner::workspace::git_identity(root) {
        Ok(identity) => report.check(
            &format!("Commit identity: {} <{}>", identity.name, identity.email),
            Ok(()),
            "",
        ),
        Err(_) => report.check(
            "Git commit identity",
            Err("Git user.name and user.email must be configured for this project".into()),
            crate::runner::workspace::GIT_IDENTITY_FIX,
        ),
    }
}

/// Only a live owner blocks. A dead guardian's leftover marker does not: the next runner
/// stops the workers it names and clears it before it starts.
fn ownership(common: &Path, name: &str, description: &str) -> Result<(), String> {
    let path = common.join(name);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Cannot inspect {description} ownership: {error}")),
    };
    match crate::runner::process::inspect(&file, |_| Ok(())) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(format!(
            "A live {description} owns the workspace; wait for release"
        )),
        Err(error) => Err(format!("Cannot inspect {description} ownership: {error}")),
    }
}

fn baseline(service: &Service, root: &Path, task: i64) -> Result<(), String> {
    let top = git(root, &["rev-parse", "--show-toplevel"])?;
    if root.canonicalize().map_err(|e| e.to_string())?
        != Path::new(&top).canonicalize().map_err(|e| e.to_string())?
    {
        return Err("Project must be Git root".into());
    }
    git(root, &["rev-parse", "--verify", "HEAD"])
        .map_err(|_| "Initial Git commit required".to_owned())?;
    if git(root, &["status", "--porcelain"])?.is_empty() {
        return Ok(());
    }
    let head = git(root, &["rev-parse", "HEAD"])?;
    let base = service.attempt_base(task).map_err(|e| e.to_string())?;
    if crate::runner::workspace::retains_work_from(root, base.as_deref(), &head)
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    Err("Uncommitted work outside this task; preserve or commit it before switching".into())
}

fn writable_network(report: &mut Readiness, service: &Service, provider: Option<Provider>) {
    let network = service.agent_network().map_err(|e| e.to_string());
    report.check(
        &format!(
            "Writable workers (execution and repair) · Agent network {} · {}",
            if network.as_ref().is_ok_and(|enabled| *enabled) {
                "On"
            } else {
                "Off"
            },
            provider.map_or("provider unset", |provider| provider.label())
        ),
        network.and_then(|enabled| {
            provider.map_or(Ok(()), |provider| {
                provider
                    .writable_network(enabled)
                    .map_err(|e| e.to_string())
            })
        }),
        "Esc, 4 Config, e Preferences: Agent network On; or m Models: Codex execution",
    );
}

pub(crate) fn inspect(app: &App) -> Readiness {
    let mut report = Readiness {
        lines: Vec::new(),
        blockers: 0,
        refresh_failed: false,
    };
    let Some(service) = &app.service else {
        return report;
    };
    let Some(task) = app.current().and_then(|task| app.stored.get(&task.id)) else {
        if let Ok(root) = service.project_root() {
            commit_identity(&mut report, &root);
        }
        // No task yet: report what the project itself still needs.
        match crate::runner::setup::problems(service) {
            Ok(problems) if problems.is_empty() => report
                .lines
                .push("OK · Project setup: checks and model defaults are ready; add a task".into()),
            Ok(problems) => {
                for problem in problems {
                    if problem.item == "Git commit identity" {
                        continue;
                    }
                    let action = if problem.item == "Project check" {
                        "c Checks"
                    } else if problem.item.ends_with(" model") {
                        "d Defaults"
                    } else {
                        &problem.fix
                    };
                    report.check(&problem.item, Err(problem.reason), action);
                }
            }
            Err(error) => report.check("Project setup", Err(error.to_string()), "u Refresh"),
        }
        let provider = service.project_defaults().ok().and_then(|models| {
            models[0]
                .as_ref()
                .and_then(|model| Provider::of(model).ok())
        });
        writable_network(&mut report, service, provider);
        return report;
    };
    let definition = task.definition();
    let root = service.project_root();
    let state = app.current().expect("current task").status();
    report.check(
        "Execution state",
        match state.forward() {
            _ if app.supervisor_alive => {
                Err("The runner is working on it · p Pause, s Stop".into())
            }
            Some(TaskAction::Approve) => Err("Approve a revision before Run".into()),
            Some(_) => Ok(()),
            None => Err(state.refusal(TaskAction::Run)),
        },
        "v Approve / w Answer / r Run",
    );
    report.check(
        "Project checks",
        service
            .checks()
            .map_err(|e| e.to_string())
            .and_then(|checks| {
                if checks.is_empty() {
                    Err("No enabled check".into())
                } else {
                    Ok(())
                }
            }),
        "c Checks",
    );
    report.check(
        "Approval",
        if definition.lifecycle == "ready" {
            Ok(())
        } else {
            Err("Current revision needs approval".into())
        },
        "v Approve",
    );
    report.check(
        "Task",
        if definition.definition.kind == Kind::Ai {
            Ok(())
        } else {
            Err("Human work has no AI run".into())
        },
        "D Mark done",
    );
    for (index, role) in ROLES.iter().enumerate() {
        let model = definition.definition.models[index].as_ref();
        let valid = match model {
            None => Err("No model selected".into()),
            Some(model) if Provider::of(model).is_err() => Err(format!(
                "Provider {} is not supported; use {}",
                model.provider,
                crate::runner::provider::SUPPORTED
            )),
            Some(model) => service
                .validate_model(role, model)
                .map_err(|error| error.to_string()),
        };
        report.check(&format!("{role} model"), valid, "m Models");
    }
    let writable = definition.definition.models[0]
        .as_ref()
        .and_then(|model| Provider::of(model).ok());
    writable_network(&mut report, service, writable);
    let mut providers: Vec<_> = definition
        .definition
        .models
        .iter()
        .flatten()
        .filter_map(|model| Provider::of(model).ok())
        .collect();
    providers.sort_by_key(|provider| provider.name());
    providers.dedup();
    for provider in providers {
        report.check(
            &format!("{} CLI", provider.label()),
            provider.executable().map(|_| ()).map_err(|e| e.to_string()),
            &format!("install/sign in to {}, then Refresh", provider.label()),
        );
    }
    report.check(
        "Read-only sandbox",
        crate::runner::sandbox::available().map_err(|e| e.to_string()),
        "install bubblewrap (bwrap), then Refresh",
    );
    match root {
        Ok(root) => {
            commit_identity(&mut report, &root);
            let common = git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            report.check(
                "Supervisor ownership",
                common
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|path| ownership(Path::new(path), "ktask-runner.lock", "supervisor")),
                "wait for release",
            );
            report.check(
                "Worker ownership",
                common
                    .as_ref()
                    .map_err(Clone::clone)
                    .and_then(|path| ownership(Path::new(path), "ktask-worker.lock", "worker")),
                "wait for release",
            );
            report.check(
                "Workspace baseline",
                baseline(service, &root, task.id),
                "inspect Git worktree",
            );
        }
        Err(error) => {
            report.check(
                "Supervisor ownership",
                Err(error.to_string()),
                "inspect project path",
            );
            report.check(
                "Worker ownership",
                Err("Project path unavailable".into()),
                "inspect project path",
            );
            report.check(
                "Workspace baseline",
                Err("Project path unavailable".into()),
                "inspect project path",
            );
        }
    }
    report
}
