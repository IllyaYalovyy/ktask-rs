//! Why a command failed, and the exit code to report it with — every error conversion in
//! one place, so a command module only ever produces a [`Failure`].

use ktask_core::{
    AcknowledgeError, AddError, AnswerError, CancelError, DoneError, ForgetError, ImportError,
    RegisterError, ReportError, ResolveError, RetryError, RunError, SetSettingError,
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

impl From<ForgetError> for Failure {
    fn from(error: ForgetError) -> Self {
        match error {
            ForgetError::UnknownProject(_) => Self {
                message: format!("{error}; `ktask-rs project list` shows the registered projects"),
                code: 2,
            },
            ForgetError::Registry(_) => Self::from(error.to_string()),
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

impl From<RetryError> for Failure {
    fn from(error: RetryError) -> Self {
        match error {
            RetryError::Journal(_) => Self::from(error.to_string()),
            RetryError::UnknownTask(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
            RetryError::NotRetryable { .. } => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<AnswerError> for Failure {
    fn from(error: AnswerError) -> Self {
        match error {
            AnswerError::Journal(_) => Self::from(error.to_string()),
            AnswerError::UnknownTask(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
            AnswerError::NotBlocked { .. } | AnswerError::EmptyAnswer => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<AcknowledgeError> for Failure {
    fn from(error: AcknowledgeError) -> Self {
        match error {
            AcknowledgeError::Journal(_) => Self::from(error.to_string()),
            AcknowledgeError::UnknownTask(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
            AcknowledgeError::NotAcknowledgeable { .. } => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}

impl From<DoneError> for Failure {
    fn from(error: DoneError) -> Self {
        match error {
            DoneError::Journal(_) => Self::from(error.to_string()),
            DoneError::UnknownTask(_) => Self {
                message: format!("{error}; `ktask-rs list --all` shows every task"),
                code: 2,
            },
            DoneError::EmptyReason => Self {
                message: error.to_string(),
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
            SetSettingError::Store(_) | SetSettingError::Git(_) => Self::from(error.to_string()),
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
            ImportError::Malformed(_)
            | ImportError::NotAnArray
            | ImportError::MalformedToml(_)
            | ImportError::NotATaskTable(_)
            | ImportError::UnsupportedFormat
            | ImportError::Invalid(_) => Self {
                message: error.to_string(),
                code: 2,
            },
        }
    }
}
