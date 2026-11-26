//! `ktask link` and the project list, which includes what linked hosts reported.
use super::{
    Failure, Result,
    args::LinkAction,
    emit,
    output::{self, Reply},
};
use crate::{
    application::now_ms,
    clock,
    instance::{Instance, Paths},
    link::{self, Forgotten, HostRecord, fleet},
    storage::Database,
};
use serde_json::{Value, json};

pub(super) fn run(instance: Instance, action: &LinkAction, json_output: bool) -> Result<()> {
    let paths = instance.paths()?;
    let reply = match action {
        LinkAction::Run => return Ok(link::run(instance, &paths)?),
        LinkAction::Accept => return Ok(link::accept(&paths)?),
        LinkAction::Parent {
            destination: Some(destination),
            name,
            ..
        } => {
            let config = link::configure(&paths, destination, name.as_deref())?;
            Reply {
                text: format!(
                    "Parent {}; it lists this computer as {}. Keep ktask link run running here and on the parent.\n",
                    output::safe(&config.parent),
                    config.name
                ),
                data: json!({"parent": config.parent, "name": config.name}),
            }
        }
        LinkAction::Parent { .. } => {
            let cleared = link::clear(&paths)?.map(|config| config.parent);
            Reply {
                text: cleared
                    .as_ref()
                    .map_or("No parent was configured\n".into(), |parent| {
                        format!("Parent {} cleared\n", output::safe(parent))
                    }),
                data: json!({"parent": null, "cleared": cleared}),
            }
        }
        LinkAction::Status => status(&paths)?,
        LinkAction::Forget { host } => match link::forget(&paths, host)? {
            Forgotten::Removed => Reply {
                text: format!("Forgot host {host}\n"),
                data: json!({"forgotten": host}),
            },
            Forgotten::Unknown => {
                return Err(Failure::missing(format!(
                    "No linked host is named {host}; see ktask link status"
                )));
            }
        },
    };
    emit(reply, json_output)
}

fn contact(ms: Option<i64>) -> String {
    ms.map_or("never".into(), |ms| clock::format_local(ms, now_ms()))
}

fn status(paths: &Paths) -> Result<Reply> {
    let report = link::report(paths)?;
    let yes = |value| if value { "yes" } else { "no" };
    let or_dash =
        |value: &Option<String>| value.as_deref().map_or_else(|| "-".into(), output::safe);
    let mut text = format!("role\t{}\n", report.role);
    if report.name.is_some() {
        text.push_str(&format!(
            "peer\t{}\nname\t{}\n",
            or_dash(&report.peer),
            or_dash(&report.name)
        ));
    } else if report.peers.is_empty() {
        text.push_str("peer\t-\n");
    }
    text.push_str(&format!(
        "link run\t{}\nconnected\t{}\n",
        if report.running {
            "running"
        } else {
            "not running"
        },
        yes(report.connected)
    ));
    if report.name.is_some() && report.incompatible {
        text.push_str("incompatible\tyes\n");
    }
    text.push_str(&format!(
        "last contact\t{}\n",
        contact(report.last_contact_ms)
    ));
    if report.peers.is_empty() {
        text.push_str(&format!("last error\t{}\n", or_dash(&report.last_error)));
    }
    for peer in &report.peers {
        let state = if peer.incompatible {
            "incompatible"
        } else if peer.connected {
            "connected"
        } else {
            "offline"
        };
        text.push_str(&format!(
            "peer\t{}\t{state}\tktask {}\tlast contact {}\tlast error {}\n",
            peer.host,
            output::safe(&peer.version),
            contact(Some(peer.last_contact_ms)),
            or_dash(&peer.last_error)
        ));
    }
    Ok(Reply {
        data: json!(report),
        text,
    })
}

fn host_value(record: &HostRecord) -> Value {
    json!({
        "host": record.host,
        "online": record.online,
        "incompatible": record.incompatible,
        "version": record.version,
        "protocol": record.protocol,
        "last_contact_ms": record.last_contact_ms,
        "last_error": record.last_error,
    })
}

/// `ktask projects`: every project of the instance and of its linked hosts, without
/// selecting or registering one. An instance without a database has none of its own.
pub(super) fn projects(paths: &Paths) -> Result<Reply> {
    let listed = match Database::open_existing(paths)? {
        Some(db) => crate::runner::registration::listed(paths, &db)?,
        None => Vec::new(),
    };
    let hosts = link::hosts(paths)?;
    // Hosts are marked only once there is another one to tell apart.
    let linked = !hosts.is_empty();
    let mut text = String::new();
    let mut data = Vec::new();
    for entry in fleet::merge(listed, &hosts) {
        let summary = entry.summary();
        let remote = entry.remote.as_ref();
        let placeholder = remote.is_some_and(|remote| remote.placeholder);
        let incompatible = remote.and_then(|remote| remote.incompatible.as_deref());
        let state = if incompatible.is_some() {
            "incompatible"
        } else if !entry.online() {
            "offline"
        } else {
            summary.state.name()
        };
        let current = summary.current.as_ref().map_or("-".into(), |task| {
            format!(
                "#{} {} · {}",
                task.id,
                task.state.label(),
                output::safe(&task.title)
            )
        });
        if placeholder {
            text.push_str(&format!("-\t{state}\t-\t-\t-\t-"));
        } else {
            text.push_str(&format!(
                "{}\t{state}\t{}\t{}\t{}\t{current}",
                summary.id,
                summary.queued,
                output::safe(&summary.name),
                output::safe(&summary.root),
            ));
        }
        if linked {
            text.push_str(&format!("\t{}", entry.host_label()));
        }
        if let Some(at) = remote.and_then(|remote| remote.last_contact_ms) {
            text.push_str(&format!("\tlast contact {}", contact(Some(at))));
        }
        if let Some(reason) = incompatible {
            text.push_str(&format!("\t{}", output::safe(reason)));
        }
        text.push('\n');
        if placeholder {
            continue;
        }
        let mut value = json!(summary);
        value["host"] = json!(entry.host());
        value["online"] = json!(entry.online());
        if let Some(at) = remote.and_then(|remote| remote.last_contact_ms) {
            value["last_contact_ms"] = json!(at);
        }
        if let Some(reason) = incompatible {
            value["incompatible"] = json!(reason);
        }
        data.push(value);
    }
    if text.is_empty() {
        text.push_str("No projects.\n");
    }
    let count = data.len();
    let mut reply = json!({"projects": data, "count": count});
    if linked {
        reply["hosts"] = hosts.iter().map(host_value).collect();
    }
    Ok(Reply { data: reply, text })
}
