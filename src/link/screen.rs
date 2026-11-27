//! What a parent's screens ask of a child, and how. Reads are one hidden command, `ktask
//! screen QUERY`, answered by the child from its application queries; changes are the
//! ordinary task commands. Both cross the link as command lines, so the child decides.
//! Only stored read models travel: never a screen, never a file or a database.
use super::Called;
use crate::{
    application::{self as domain, EvidenceCursor, session::ExecutionSession},
    instance::Paths,
    project_view::FormChoices,
    readiness::Readiness,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Query {
    Project(Known),
    Stats,
    Watermark {
        attempt: i64,
    },
    ArtifactId {
        attempt: i64,
        artifact: usize,
    },
    ArtifactCount {
        attempt: i64,
    },
    /// Pages of an attempt's output, or of one of its transcripts.
    PageCount {
        attempt: i64,
        watermark: i64,
        transcript: Option<usize>,
    },
    ChunkCount {
        attempt: i64,
        page: usize,
        watermark: i64,
    },
    StepCount {
        attempt: i64,
        reviews: bool,
    },
    Evidence {
        attempt: i64,
        section: usize,
        page: usize,
        chunk: usize,
        artifact: usize,
        step: usize,
        selected_match: Option<(i64, String)>,
        watermark: i64,
    },
    Search {
        attempt: i64,
        section: usize,
        artifact: usize,
        watermark: i64,
        query: String,
        cursor: EvidenceCursor,
    },
}

impl Query {
    /// The attempt a query reads, which has to be one of the project's.
    pub fn attempt(&self) -> Option<i64> {
        match self {
            Self::Project(_) | Self::Stats => None,
            Self::Watermark { attempt }
            | Self::ArtifactId { attempt, .. }
            | Self::ArtifactCount { attempt }
            | Self::PageCount { attempt, .. }
            | Self::ChunkCount { attempt, .. }
            | Self::StepCount { attempt, .. }
            | Self::Evidence { attempt, .. }
            | Self::Search { attempt, .. } => Some(*attempt),
        }
    }
}

/// What the parent already holds, so that an unchanged part is not sent again.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Known {
    pub queue: String,
    pub history: String,
    pub events: String,
    /// The task whose live run the held readiness was inspected during.
    pub running: Option<i64>,
    /// The held readiness is missing or old: inspect again.
    pub inspect: bool,
}

/// One part of the project; `data` is absent when the parent already holds this digest.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Section<T> {
    pub digest: String,
    pub data: Option<T>,
}

impl<T: Serialize> Section<T> {
    pub fn new(data: T, known: &str) -> Self {
        let bytes = serde_json::to_vec(&data).unwrap_or_default();
        let digest: String = Sha256::digest(bytes)
            .iter()
            .take(16)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        Self {
            data: (digest != known).then_some(data),
            digest,
        }
    }
}

/// A project as its host reads it now.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ProjectView {
    /// Whether the parent may change anything.
    pub control: bool,
    pub supervisor_alive: bool,
    pub session: Option<ExecutionSession>,
    /// The project's run mode: continue the queue, or run one task.
    pub continue_queue: bool,
    pub launch_issue: Option<String>,
    pub choices: FormChoices,
    pub queue: Section<Vec<domain::Task>>,
    pub history: Section<Vec<domain::Task>>,
    pub events: Section<Vec<(Option<i64>, String)>>,
    /// Sent when inspected for this answer.
    pub readiness: Option<Readiness>,
}

/// Why a request had no answer.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Failed {
    /// Nothing was sent: the host is offline or cannot link.
    Unreachable(String),
    /// The connection ended first: what a change did is unknown.
    Lost,
    /// The host answered with this error.
    Refused(String),
}

/// Runs one task command of `project` on `host` and returns its JSON data.
pub(crate) fn command(
    paths: &Paths,
    host: &str,
    project: i64,
    command: &[String],
) -> Result<Value, Failed> {
    let mut args = vec!["--json".into(), "--project".into(), project.to_string()];
    args.extend_from_slice(command);
    match super::call(paths, host, args) {
        Err(error) => Err(Failed::Unreachable(error.to_string())),
        Ok(Called::Ran {
            stdout,
            stderr,
            status,
        }) => reply(&stdout, &stderr, status),
        Ok(Called::Unknown) => Err(Failed::Unreachable("it is no longer linked".into())),
        Ok(Called::Offline(why) | Called::Incompatible(why)) => Err(Failed::Unreachable(why)),
        Ok(Called::Refused(why)) => Err(Failed::Refused(why)),
        Ok(Called::Lost) => Err(Failed::Lost),
    }
}

/// Asks one read of `project` on `host`.
pub(crate) fn ask<T: DeserializeOwned>(
    paths: &Paths,
    host: &str,
    project: i64,
    query: &Query,
) -> Result<T, Failed> {
    let query = serde_json::to_string(query).map_err(|e| Failed::Refused(e.to_string()))?;
    let data = command(paths, host, project, &["screen".into(), query])?;
    serde_json::from_value(data)
        .map_err(|error| Failed::Refused(format!("Unreadable answer from {host}: {error}")))
}

/// When `host` was last heard from.
pub(crate) fn last_contact(paths: &Paths, host: &str) -> Option<i64> {
    super::hosts(paths)
        .ok()?
        .into_iter()
        .find(|record| record.host == host)
        .map(|record| record.last_contact_ms)
}

fn reply(stdout: &str, stderr: &str, status: u8) -> Result<Value, Failed> {
    let parse = |text: &str| {
        text.lines()
            .rev()
            .find_map(|line| serde_json::from_str::<Value>(line).ok())
    };
    if status == 0
        && let Some(mut value) = parse(stdout)
    {
        let mut data = value["data"].take();
        printable(&mut data);
        return Ok(data);
    }
    let mut message = parse(stderr)
        .and_then(|value| value["error"]["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("The command ended with status {status}: {}", stderr.trim()));
    strip(&mut message);
    Err(Failed::Refused(message))
}

fn strip(text: &mut String) {
    text.retain(|c| !c.is_control() || matches!(c, '\n' | '\t'));
}

/// Text from another computer must not drive this terminal.
fn printable(value: &mut Value) {
    match value {
        Value::String(text) => strip(text),
        Value::Array(values) => values.iter_mut().for_each(printable),
        Value::Object(fields) => {
            for (name, value) in fields.iter_mut() {
                // A search position is sent back to the host as it is and never shown.
                if name != "carry" && name != "stream_overlap" {
                    printable(value);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_lose_terminal_controls_and_errors_keep_their_message() {
        let data = reply(
            "{\"ok\":true,\"data\":{\"title\":\"a\\u001b[2J\\r\\u009bb\\n\\tc\"}}\n",
            "",
            0,
        )
        .unwrap();
        assert_eq!(data["title"], "a[2Jb\n\tc");
        let refused = reply(
            "",
            "note\n{\"ok\":false,\"error\":{\"code\":\"conflict\",\"message\":\"Task changed\"}}\n",
            4,
        );
        assert_eq!(refused, Err(Failed::Refused("Task changed".into())));
    }

    #[test]
    fn an_unchanged_section_is_not_sent_again() {
        let first = Section::new(vec![1, 2], "");
        assert_eq!(first.data, Some(vec![1, 2]));
        assert!(Section::new(vec![1, 2], &first.digest).data.is_none());
        assert!(Section::new(vec![1, 3], &first.digest).data.is_some());
    }
}
