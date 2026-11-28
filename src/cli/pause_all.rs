use super::{Failure, Result, output};
use crate::{
    application::{Service, investigation::AgentKind},
    instance::{Instance, Paths, TuiShutdown},
    runner::{control, investigate, registration, workspace::Workspace},
    storage::Database,
};
use serde_json::json;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

struct Project<'a> {
    instance: Instance,
    paths: &'a Paths,
    id: i64,
    name: String,
    root: PathBuf,
    lease: Option<File>,
    common: Option<PathBuf>,
}

struct Target<'a> {
    project: &'a Project<'a>,
    lease: File,
    task: Option<i64>,
}

fn projects<'a>(
    instance: Instance,
    paths: &'a Paths,
    errors: &mut Vec<Failure>,
) -> Result<Vec<Project<'a>>> {
    let Some(db) = Database::open_existing(paths)? else {
        return Ok(Vec::new());
    };
    let mut projects = Vec::new();
    for (id, name, root) in Service::project_registrations(&db)? {
        let project = (|| -> Result<_> {
            let lease = registration::active(paths, id)?;
            let common = Workspace::common_dir(&root)?;
            Ok((lease, common))
        })();
        match project {
            Ok((lease, common)) => projects.push(Project {
                instance,
                paths,
                id,
                name,
                root,
                lease,
                common,
            }),
            Err(error) => errors.push(context(error, instance, Some(&root))),
        }
    }
    Ok(projects)
}

fn service(project: &Project<'_>) -> Result<Service> {
    let db = Database::open_existing(project.paths)?
        .ok_or_else(|| Failure::missing("Registered project database disappeared"))?;
    Ok(Service::registered(db, project.id, project.name.clone())?)
}

/// An upgrade also ends agent reports still being written and running fixes; what it
/// ended.
fn end_investigations(
    projects: &[Project<'_>],
    errors: &mut Vec<Failure>,
) -> Vec<serde_json::Value> {
    let mut ended = Vec::new();
    for project in projects {
        match end_investigation(project) {
            Ok(Some((id, kind))) => ended.push(json!({
                "instance": project.instance.name(),
                "project": project.root.to_string_lossy(),
                "id": id,
                "kind": kind,
            })),
            Ok(None) => {}
            Err(error) => errors.push(context(error, project.instance, Some(&project.root))),
        }
    }
    ended
}

/// The report or fix it ended, if one was running.
fn end_investigation(project: &Project<'_>) -> Result<Option<(i64, AgentKind)>> {
    let mut service = service(project)?;
    investigate::settle(&mut service, project.paths)?;
    let Some((id, kind)) = service.running_agent()? else {
        return Ok(None);
    };
    investigate::stop(&mut service, project.paths, id)?;
    Ok(Some((id, kind)))
}

fn request<'a>(project: &'a Project<'a>, mut service: Service, lease: File) -> Result<Target<'a>> {
    let task = match service.request_current_pause() {
        Ok(task) => task,
        Err(crate::storage::Error::Conflict(_)) => None,
        Err(error) => return Err(error.into()),
    };
    Ok(Target {
        project,
        lease,
        task,
    })
}

/// Older supervisors hold a Git-common-directory lease without identifying the registry.
fn legacy<'a>(common: &Path, projects: &[&'a Project<'a>]) -> Result<Option<Target<'a>>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let Some(lease) = Workspace::live_lease(common)? else {
            return Ok(None);
        };
        if let [project] = projects {
            let service = service(project)?;
            if service.execution_session()?.is_some_and(|session| {
                matches!(
                    session.state.as_str(),
                    "running" | "waiting" | "resume_requested"
                ) || (session.state == "paused" && session.reason.starts_with("Pause requested;"))
            }) {
                // The registry lookup may outlive the workspace lease.
                if Workspace::live_lease(common)?.is_some() {
                    return request(project, service, lease).map(Some);
                }
                return Ok(None);
            }
        }
        if Instant::now() >= deadline {
            return Err(Failure {
                status: 4,
                code: "conflict",
                message: format!(
                    "Cannot identify the older runner holding {}. Pause it from its project using its own executable, then retry.",
                    common.display(),
                ),
            });
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub(super) fn run(shutdown: bool) -> Result<output::Reply> {
    let mut errors = Vec::new();
    let mut instances = Vec::new();
    let mut terminals = Vec::new();
    for instance in [Instance::Production, Instance::Development] {
        match instance.paths() {
            Ok(paths) => {
                if shutdown {
                    match TuiShutdown::request(&paths) {
                        Ok(handle) => terminals.push((instance, handle)),
                        Err(error) => errors.push(context(error.into(), instance, None)),
                    }
                }
                instances.push((instance, paths));
            }
            Err(error) => errors.push(context(error.into(), instance, None)),
        }
    }
    for (instance, handle) in &terminals {
        if let Err(error) = handle.wait() {
            errors.push(context(error.into(), *instance, None));
        }
    }
    let mut gates = Vec::new();
    let mut candidates = Vec::new();
    let deadline = Instant::now() + control::PATIENCE;
    for (instance, paths) in &instances {
        match registration::close_admission(paths, deadline) {
            Ok(gate) => gates.push(gate),
            Err(error) => errors.push(context(error.into(), *instance, None)),
        }
    }
    for (instance, paths) in &instances {
        match projects(*instance, paths, &mut errors) {
            Ok(projects) => candidates.extend(projects),
            Err(error) => errors.push(context(error, *instance, None)),
        }
    }
    let tracked = candidates
        .iter()
        .filter(|project| project.lease.is_some())
        .filter_map(|project| project.common.as_deref())
        .collect::<BTreeSet<_>>();
    let mut paused = Vec::new();
    let mut older = BTreeMap::<_, Vec<_>>::new();
    for project in &candidates {
        if let Some(lease) = &project.lease {
            match service(project).and_then(|service| request(project, service, lease.try_clone()?))
            {
                Ok(target) => paused.push(target),
                Err(error) => errors.push(context(error, project.instance, Some(&project.root))),
            }
        } else if let Some(common) = project.common.as_deref()
            && !tracked.contains(common)
        {
            older.entry(common).or_default().push(project);
        }
    }
    for (common, projects) in older {
        match legacy(common, &projects) {
            Ok(Some(target)) => paused.push(target),
            Ok(None) => {}
            Err(error) => errors.push(error),
        }
    }
    let mut runners = Vec::new();
    let deadline = Instant::now() + control::PATIENCE;
    for target in paused {
        let project = target.project;
        if let Err(error) = control::wait_for_lease(&target.lease, deadline) {
            errors.push(context(error.into(), project.instance, Some(&project.root)));
            continue;
        }
        runners.push(json!({
            "instance": project.instance.name(),
            "project": project.root.to_string_lossy(),
            "id": target.task,
        }));
    }
    let investigations = if shutdown {
        end_investigations(&candidates, &mut errors)
    } else {
        Vec::new()
    };
    if !errors.is_empty() {
        let message = errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(Failure {
            message,
            ..errors.remove(0)
        });
    }
    let count = runners.len();
    let fixes = investigations
        .iter()
        .filter(|ended| ended["kind"] == "fix")
        .count();
    let reports = investigations.len() - fixes;
    let mut ended = String::new();
    if reports > 0 {
        ended.push_str(&format!(" Stopped {reports} agent report(s)."));
    }
    if fixes > 0 {
        ended.push_str(&format!(" Stopped {fixes} agent fix(es)."));
    }
    let text = match (shutdown, count) {
        (true, _) => format!("Shutdown complete. Paused {count} runner(s).{ended}\n"),
        (false, 0) => "Nothing is running\n".into(),
        (false, _) => format!("Paused {count} runner(s). Run resumes each project.\n"),
    };
    let action = if shutdown { "shutdown" } else { "paused" };
    let mut data = json!({"action":action,"count":count,"runners":runners});
    if !investigations.is_empty() {
        data["investigations"] = json!(investigations);
    }
    Ok(output::Reply { data, text })
}

fn context(mut error: Failure, instance: Instance, root: Option<&Path>) -> Failure {
    let scope = root.map_or_else(
        || instance.name().to_owned(),
        |root| format!("{} {}", instance.name(), root.display()),
    );
    error.message = format!("{scope}: {}", error.message);
    error
}
