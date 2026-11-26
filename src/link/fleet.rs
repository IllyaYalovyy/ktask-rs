//! One flat list of this instance's projects and those its children last reported. The
//! Projects screen and `ktask projects` both list from here.
use super::HostRecord;
use crate::application::overview::{Attention, Project, Summary};

/// How listings name this computer's own projects once another host is listed.
pub(crate) const LOCAL: &str = "local";

/// What makes a listed project another host's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Remote {
    pub host: String,
    attention: Attention,
    pub online: bool,
    /// Why the host cannot link, when its version or protocol differs.
    pub incompatible: Option<String>,
    /// When the host was last heard from; only kept while it is offline, so that a
    /// heartbeat does not count as a change.
    pub last_contact_ms: Option<i64>,
    /// Stands for an incompatible host that never reported a project.
    pub placeholder: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entry {
    /// A remote project's ID and root are its own host's.
    pub project: Project,
    pub runner_alive: bool,
    pub remote: Option<Remote>,
}

impl Entry {
    pub fn host(&self) -> Option<&str> {
        self.remote.as_ref().map(|remote| remote.host.as_str())
    }

    pub fn host_label(&self) -> &str {
        self.host().unwrap_or(LOCAL)
    }

    pub fn online(&self) -> bool {
        self.remote.as_ref().is_none_or(|remote| remote.online)
    }

    /// The state as the project's host last saw it.
    pub fn attention(&self) -> Attention {
        self.remote.as_ref().map_or_else(
            || self.project.attention(self.runner_alive),
            |remote| remote.attention,
        )
    }

    pub fn state_label(&self) -> &'static str {
        match &self.remote {
            Some(remote) if remote.incompatible.is_some() => "Incompatible",
            Some(remote) if !remote.online => "Offline",
            _ => self
                .attention()
                .label(self.project.current.as_ref().map(|task| task.state)),
        }
    }

    pub fn summary(&self) -> Summary {
        Summary {
            id: self.project.id,
            name: self.project.name.clone(),
            root: self.project.root.to_string_lossy().into_owned(),
            state: self.attention(),
            runner_alive: self.runner_alive,
            queued: self.project.queued,
            finished: self.project.finished,
            current: self.project.current.clone(),
        }
    }
}

fn remote(record: &HostRecord, project: Option<&Summary>) -> Entry {
    let incompatible = record.incompatible.then(|| {
        record
            .last_error
            .clone()
            .unwrap_or_else(|| "incompatible".into())
    });
    let placeholder = project.is_none();
    Entry {
        project: Project {
            id: project.map_or(0, |p| p.id),
            name: project.map_or_else(|| "(no projects)".into(), |p| p.name.clone()),
            root: project.map_or_else(Default::default, |p| p.root.clone().into()),
            queued: project.map_or(0, |p| p.queued),
            finished: project.map_or(0, |p| p.finished),
            current: project.and_then(|p| p.current.clone()),
        },
        runner_alive: project.is_some_and(|p| p.runner_alive),
        remote: Some(Remote {
            host: record.host.clone(),
            attention: project.map_or(Attention::Idle, |p| p.state),
            online: record.online && !record.incompatible,
            incompatible,
            last_contact_ms: (!record.online).then_some(record.last_contact_ms),
            placeholder,
        }),
    }
}

/// Local projects and every host's, sorted by name then host. With no host the local list
/// is returned as it came.
pub(crate) fn merge(local: Vec<(Project, bool)>, hosts: &[HostRecord]) -> Vec<Entry> {
    let mut entries: Vec<_> = local
        .into_iter()
        .map(|(project, runner_alive)| Entry {
            project,
            runner_alive,
            remote: None,
        })
        .collect();
    if hosts.is_empty() {
        return entries;
    }
    for record in hosts {
        entries.extend(record.projects.iter().map(|p| remote(record, Some(p))));
        if record.incompatible && record.projects.is_empty() {
            entries.push(remote(record, None));
        }
    }
    entries.sort_by_cached_key(|entry| {
        (
            entry.project.name.to_ascii_lowercase(),
            entry.host_label().to_ascii_lowercase(),
            entry.project.id,
        )
    });
    entries
}
