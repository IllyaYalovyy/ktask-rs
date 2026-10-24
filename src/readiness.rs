//! Read-only launch inspection for the Current Task screen.
use crate::{
    app::App,
    application::{Kind, ROLES, Service},
    presentation::Status,
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
    pub resume_blocked: bool,
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

fn ownership(common: &Path, name: &str, description: &str, marker: bool) -> Result<(), String> {
    let path = common.join(name);
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("Cannot inspect {description} ownership: {error}")),
    };
    match crate::runner::process::inspect(&file, |file| Ok(file.metadata()?.len())) {
        Ok(Some(marked)) if marker && marked != 0 => {
            Err("Worker cleanup is unconfirmed; inspect descendants, then use Recover".into())
        }
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
    if service
        .attempt_base(task)
        .map_err(|e| e.to_string())?
        .as_deref()
        == Some(&head)
    {
        return Ok(());
    }
    Err("Uncommitted work outside this task; preserve or commit it before switching".into())
}

pub(crate) fn inspect(app: &App) -> Readiness {
    let mut report = Readiness {
        lines: Vec::new(),
        blockers: 0,
        refresh_failed: false,
        resume_blocked: false,
    };
    let Some(service) = &app.service else {
        return report;
    };
    let Some(task) = app.current().and_then(|task| app.stored.get(&task.id)) else {
        // No task yet: report what the project itself still needs.
        match crate::runner::setup::problems(service) {
            Ok(problems) if problems.is_empty() => report
                .lines
                .push("OK · Project setup: checks and model defaults are ready; add a task".into()),
            Ok(problems) => {
                for problem in problems {
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
        return report;
    };
    let definition = task.definition();
    let root = service.project_root();
    let state = app.current().expect("current task").status();
    let waiting = service
        .waiting_resume(task.id)
        .map_err(|error| error.to_string());
    // A provider-limit wait never blocks the owner: Resume asks the provider again now.
    report.resume_blocked =
        state == Status::Waiting && !waiting.as_ref().is_ok_and(|waiting| waiting.is_some());
    let open_question = service
        .open_question(task.id)
        .map(|question| question.is_some())
        .map_err(|error| error.to_string());
    report.check(
        "Execution state",
        match state {
            Status::Pending => Ok(()),
            Status::Failed | Status::Interrupted => match &open_question {
                Ok(false) => Ok(()),
                Ok(true) => Err("Answer the open question before retrying".into()),
                Err(error) => Err(error.clone()),
            },
            Status::New => Err("Approve a revision before Run".into()),
            Status::Abandoned => Err("Abandoned attempt needs recovery".into()),
            Status::Question => match &open_question {
                Ok(false) => Ok(()),
                Ok(true) => Err("Answer the open question before resuming".into()),
                Err(error) => Err(error.clone()),
            },
            Status::Running | Status::Waiting if app.supervisor_alive => {
                Err("Supervisor already owns this attempt".into())
            }
            Status::Paused if app.supervisor_alive => {
                Err("Supervisor is still stopping this attempt".into())
            }
            Status::Waiting => waiting.and_then(|waiting| match waiting {
                Some(_) => Ok(()),
                None => Err("Waiting attempt changed; Refresh before resuming".into()),
            }),
            Status::Paused => Ok(()),
            _ => Err("Run is unavailable in this state".into()),
        },
        "v Approve / w Answer / R Recover / r Run",
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
            Err("Human work requires acknowledgment".into())
        },
        "y Acknowledge",
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
            _ if definition.definition.reuses_execution_model(index) => {
                Err("Must be stronger than the execution model".into())
            }
            _ => Ok(()),
        };
        report.check(&format!("{role} model"), valid, "m Models");
    }
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
            let common = git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            );
            report.check(
                "Supervisor ownership",
                common.as_ref().map_err(Clone::clone).and_then(|path| {
                    ownership(Path::new(path), "ktask-runner.lock", "supervisor", false)
                }),
                "wait for release",
            );
            report.check(
                "Worker ownership",
                common.as_ref().map_err(Clone::clone).and_then(|path| {
                    ownership(Path::new(path), "ktask-worker.lock", "worker", true)
                }),
                "wait or R Recover",
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
