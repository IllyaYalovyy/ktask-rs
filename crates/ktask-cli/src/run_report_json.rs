//! The JSON shape of a run's own report: written by `ktask-rs run --json`, and read back by
//! the terminal interface, which starts a run as a detached `ktask-rs run --json` of its own so
//! that quitting the screen does not stop it, then turns this JSON back into the same typed
//! [`RunReport`] the run itself produced — never into text, which is decided only where it is
//! shown.

use ktask_core::{Attempted, RunEnd, RunReport, SyncProblem, TaskId, TaskStatus};
use serde::{Deserialize, Serialize};

/// [`RunReport`] as JSON: every task attempted, in order, and why the run ended.
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct RunReportJson {
    attempted: Vec<AttemptedJson>,
    end: RunEndJson,
}

/// [`Attempted`] as JSON.
#[derive(Debug, Serialize, Deserialize)]
struct AttemptedJson {
    id: u64,
    status: String,
    reason: Option<String>,
}

/// [`RunEnd`] as JSON: tagged by `kind`, carrying only the fields that variant has.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum RunEndJson {
    EmptyQueue,
    NothingPending,
    HumanTask {
        id: u64,
    },
    Completed,
    Stopped {
        id: u64,
        status: String,
    },
    Blocked {
        id: u64,
        status: String,
        reason: Option<String>,
    },
    HealthCheckFailed {
        id: u64,
        command: String,
        reason: String,
        output_tail: String,
    },
    SyncFailed {
        id: u64,
        tracked_branch: String,
        problem: SyncProblemJson,
    },
}

/// [`SyncProblem`] as JSON: tagged by `kind`, carrying only the fields that variant has.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum SyncProblemJson {
    UncommittedChanges { status: String },
    RemoteUnreachable { reason: String },
    Conflict { files: Vec<String> },
    GitFailed { reason: String },
}

impl From<&RunReport> for RunReportJson {
    fn from(report: &RunReport) -> Self {
        Self {
            attempted: report.attempted.iter().map(AttemptedJson::from).collect(),
            end: RunEndJson::from(&report.end),
        }
    }
}

impl From<&Attempted> for AttemptedJson {
    fn from(attempt: &Attempted) -> Self {
        Self {
            id: attempt.id.0,
            status: attempt.status.as_str().to_owned(),
            reason: attempt.reason.clone(),
        }
    }
}

impl From<&RunEnd> for RunEndJson {
    fn from(end: &RunEnd) -> Self {
        match end {
            RunEnd::EmptyQueue => Self::EmptyQueue,
            RunEnd::NothingPending => Self::NothingPending,
            RunEnd::HumanTask(id) => Self::HumanTask { id: id.0 },
            RunEnd::Completed => Self::Completed,
            RunEnd::Stopped { id, status } => Self::Stopped {
                id: id.0,
                status: status.as_str().to_owned(),
            },
            RunEnd::Blocked { id, status, reason } => Self::Blocked {
                id: id.0,
                status: status.as_str().to_owned(),
                reason: reason.clone(),
            },
            RunEnd::HealthCheckFailed {
                id,
                command,
                reason,
                output_tail,
            } => Self::HealthCheckFailed {
                id: id.0,
                command: command.clone(),
                reason: reason.clone(),
                output_tail: output_tail.clone(),
            },
            RunEnd::SyncFailed {
                id,
                tracked_branch,
                problem,
            } => Self::SyncFailed {
                id: id.0,
                tracked_branch: tracked_branch.clone(),
                problem: SyncProblemJson::from(problem),
            },
        }
    }
}

impl From<&SyncProblem> for SyncProblemJson {
    fn from(problem: &SyncProblem) -> Self {
        match problem {
            SyncProblem::UncommittedChanges(status) => Self::UncommittedChanges {
                status: status.clone(),
            },
            SyncProblem::RemoteUnreachable(reason) => Self::RemoteUnreachable {
                reason: reason.clone(),
            },
            SyncProblem::Conflict(files) => Self::Conflict {
                files: files.clone(),
            },
            SyncProblem::GitFailed(reason) => Self::GitFailed {
                reason: reason.clone(),
            },
        }
    }
}

/// Why `RunReportJson::into_report` could not read back a [`RunReport`]: a status this build
/// does not know, read from a `ktask-rs run --json` of a different version.
#[derive(Debug)]
pub(crate) struct UnknownStatus(String);

impl std::fmt::Display for UnknownStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unknown task status {:?}", self.0)
    }
}

fn status(text: String) -> Result<TaskStatus, UnknownStatus> {
    text.parse().map_err(|_| UnknownStatus(text))
}

impl RunReportJson {
    /// Parses `text` as `ktask-rs run --json` writes it, giving the same typed [`RunReport`]
    /// the run itself produced.
    pub(crate) fn parse(text: &str) -> Result<RunReport, String> {
        let json: Self = serde_json::from_str(text).map_err(|e| e.to_string())?;
        json.into_report().map_err(|e| e.to_string())
    }

    fn into_report(self) -> Result<RunReport, UnknownStatus> {
        Ok(RunReport {
            attempted: self
                .attempted
                .into_iter()
                .map(AttemptedJson::into_attempted)
                .collect::<Result<_, _>>()?,
            end: self.end.into_end()?,
        })
    }
}

impl AttemptedJson {
    fn into_attempted(self) -> Result<Attempted, UnknownStatus> {
        Ok(Attempted {
            id: TaskId(self.id),
            status: status(self.status)?,
            reason: self.reason,
        })
    }
}

/// [`RunEnd::Stopped`] from its JSON fields.
fn stopped_end(id: u64, status_text: String) -> Result<RunEnd, UnknownStatus> {
    Ok(RunEnd::Stopped {
        id: TaskId(id),
        status: status(status_text)?,
    })
}

/// [`RunEnd::Blocked`] from its JSON fields.
fn blocked_end(
    id: u64,
    status_text: String,
    reason: Option<String>,
) -> Result<RunEnd, UnknownStatus> {
    Ok(RunEnd::Blocked {
        id: TaskId(id),
        status: status(status_text)?,
        reason,
    })
}

/// [`RunEnd::HealthCheckFailed`] from its JSON fields.
fn health_check_failed_end(
    id: u64,
    command: String,
    reason: String,
    output_tail: String,
) -> RunEnd {
    RunEnd::HealthCheckFailed {
        id: TaskId(id),
        command,
        reason,
        output_tail,
    }
}

/// [`RunEnd::SyncFailed`] from its JSON fields.
fn sync_failed_end(id: u64, tracked_branch: String, problem: SyncProblemJson) -> RunEnd {
    RunEnd::SyncFailed {
        id: TaskId(id),
        tracked_branch,
        problem: problem.into_problem(),
    }
}

impl RunEndJson {
    fn into_end(self) -> Result<RunEnd, UnknownStatus> {
        match self {
            Self::EmptyQueue => Ok(RunEnd::EmptyQueue),
            Self::NothingPending => Ok(RunEnd::NothingPending),
            Self::HumanTask { id } => Ok(RunEnd::HumanTask(TaskId(id))),
            Self::Completed => Ok(RunEnd::Completed),
            Self::Stopped { id, status: s } => stopped_end(id, s),
            Self::Blocked {
                id,
                status: s,
                reason,
            } => blocked_end(id, s, reason),
            Self::HealthCheckFailed {
                id,
                command,
                reason,
                output_tail,
            } => Ok(health_check_failed_end(id, command, reason, output_tail)),
            Self::SyncFailed {
                id,
                tracked_branch,
                problem,
            } => Ok(sync_failed_end(id, tracked_branch, problem)),
        }
    }
}

impl SyncProblemJson {
    fn into_problem(self) -> SyncProblem {
        match self {
            Self::UncommittedChanges { status } => SyncProblem::UncommittedChanges(status),
            Self::RemoteUnreachable { reason } => SyncProblem::RemoteUnreachable(reason),
            Self::Conflict { files } => SyncProblem::Conflict(files),
            Self::GitFailed { reason } => SyncProblem::GitFailed(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_round_trips_through_json_unchanged() {
        let report = RunReport {
            attempted: vec![Attempted {
                id: TaskId(1),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
            }],
            end: RunEnd::Blocked {
                id: TaskId(1),
                status: TaskStatus::Failed,
                reason: Some("it broke".to_owned()),
            },
        };
        let text = serde_json::to_string(&RunReportJson::from(&report)).unwrap();
        assert_eq!(RunReportJson::parse(&text).unwrap(), report);
    }

    #[test]
    fn every_run_end_variant_round_trips_through_json_unchanged() {
        let ends = [
            RunEnd::EmptyQueue,
            RunEnd::NothingPending,
            RunEnd::HumanTask(TaskId(3)),
            RunEnd::Completed,
            RunEnd::Stopped {
                id: TaskId(2),
                status: TaskStatus::FailedUnknown,
            },
            RunEnd::HealthCheckFailed {
                id: TaskId(4),
                command: "make check".to_owned(),
                reason: "exit 1".to_owned(),
                output_tail: "boom".to_owned(),
            },
            RunEnd::SyncFailed {
                id: TaskId(5),
                tracked_branch: "origin/main".to_owned(),
                problem: SyncProblem::Conflict(vec!["a.rs".to_owned()]),
            },
        ];
        for end in ends {
            let report = RunReport {
                attempted: vec![],
                end: end.clone(),
            };
            let text = serde_json::to_string(&RunReportJson::from(&report)).unwrap();
            assert_eq!(RunReportJson::parse(&text).unwrap(), report, "{end:?}");
        }
    }

    #[test]
    fn text_that_is_not_valid_json_fails_to_parse() {
        assert!(RunReportJson::parse("not json").is_err());
    }
}
