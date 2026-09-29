//! Why a command failed, and the exit code to report it with — every error conversion in
//! one place, so a command module only ever produces a [`Failure`].

use ktask_core::{
    AddError, CancelError, ImportError, RegisterError, ReportError, ResolveError, RunError,
    SetSettingError,
};

/// Why a command failed, and the exit code to report it with.
#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) message: String,
    pub(crate) code: u8,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self { message, code: 1 }
    }
}

impl From<ResolveError> for Failure {
    fn from(error: ResolveError) -> Self {
        match error {
            ResolveError::UnknownProject(_) => Self {
                message: format!("{error}; `ktask-rs project list` shows the registered projects"),
                code: 2,
            },
            ResolveError::NameTaken { ref path, .. } => Self {
                message: format!(
                    "{error}; to register {} under a different name, run in that directory: \
                     ktask-rs project register --name <NAME>",
                    path.display()
                ),
                code: 2,
            },
            _ => Self::from(error.to_string()),
        }
    }
}

impl From<RegisterError> for Failure {
    fn from(error: RegisterError) -> Self {
        match error {
            RegisterError::Registry(_) | RegisterError::Git(_) => Self::from(error.to_string()),
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<AddError> for Failure {
    fn from(error: AddError) -> Self {
        match error {
            AddError::Journal(_) => Self::from(error.to_string()),
            AddError::UnknownTask(_) | AddError::CancelledTask(_) => Self {
                message: format!(
                    "{error}; `ktask-rs list` shows the tasks a new one can be placed next to"
                ),
                code: 2,
            },
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

/// The failure a task refused for one or more reasons is reported with: every reason, not
/// only the first, one per line, when there is more than one; a placement or journal
/// failure is always alone and keeps the hint [`From<AddError>`] gives it.
pub(crate) fn failure_from_add_problems(problems: Vec<AddError>) -> Failure {
    match <[AddError; 1]>::try_from(problems) {
        Ok([error]) => Failure::from(error),
        Err(problems) => Failure {
            message: problems
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
            code: 2,
        },
    }
}

impl From<CancelError> for Failure {
    fn from(error: CancelError) -> Self {
        match error {
            CancelError::Journal(_) => Self::from(error.to_string()),
            CancelError::UnknownTask(_) | CancelError::AlreadyCancelled(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
            CancelError::Running(_) => Self {
                message: format!("{error}; wait for the run to finish, or stop it, then try again"),
                code: 2,
            },
        }
    }
}

impl From<ReportError> for Failure {
    fn from(error: ReportError) -> Self {
        match error {
            ReportError::Record(ktask_core::RecordReportError::Journal(_)) => {
                Self::from(error.to_string())
            }
            _ => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<RunError> for Failure {
    fn from(error: RunError) -> Self {
        match error {
            RunError::Locked(_) => Self {
                message: error.to_string(),
                code: 2,
            },
            RunError::Other(message) => Self::from(message),
        }
    }
}

impl From<SetSettingError> for Failure {
    fn from(error: SetSettingError) -> Self {
        match error {
            SetSettingError::Store(_) => Self::from(error.to_string()),
            SetSettingError::UnknownSetting(_) | SetSettingError::InvalidValue { .. } => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<ImportError> for Failure {
    fn from(error: ImportError) -> Self {
        match error {
            ImportError::Add(error) => Self::from(error),
            ImportError::Malformed(_) | ImportError::NotAnArray | ImportError::Invalid(_) => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}
