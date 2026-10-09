//! The token an attempt is reported with — pulled out of [`super`] so that file stays within
//! the workspace's file-length limit.

use std::fmt;
use std::str::FromStr;

use crate::TaskId;

/// The token an attempt is reported with: names the project, task and attempt it belongs to,
/// so `ktask-rs report` needs no `--project` and works from any directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptToken {
    /// The project the attempt belongs to.
    pub project: String,
    /// The task the attempt belongs to.
    pub task: TaskId,
    /// The attempt's number.
    pub number: u32,
}

impl AttemptToken {
    /// The token for attempt `number` of `task` in `project`.
    #[must_use]
    pub fn new(project: impl Into<String>, task: TaskId, number: u32) -> Self {
        Self {
            project: project.into(),
            task,
            number,
        }
    }
}

impl fmt::Display for AttemptToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.project, self.task, self.number)
    }
}

impl FromStr for AttemptToken {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let malformed = || format!("malformed token {text:?}: expected PROJECT/TASK/ATTEMPT");
        let mut parts = text.split('/');
        let (Some(project), Some(task), Some(number), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return Err(malformed());
        };
        if project.is_empty() {
            return Err(malformed());
        }
        let task = task.parse::<u64>().map_err(|_| malformed())?;
        let number = number.parse::<u32>().map_err(|_| malformed())?;
        Ok(Self {
            project: project.to_owned(),
            task: TaskId(task),
            number,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TaskId;

    #[test]
    fn a_token_reads_back_from_its_display_form() {
        let token = AttemptToken::new("proj", TaskId(7), 3);
        assert_eq!(token.to_string(), "proj/7/3");
        assert_eq!(token.to_string().parse(), Ok(token));
    }

    #[test]
    fn a_malformed_token_names_the_problem() {
        for text in [
            "",
            "proj",
            "proj/7",
            "proj/7/3/extra",
            "/7/3",
            "proj/x/3",
            "proj/7/x",
        ] {
            assert!(
                text.parse::<AttemptToken>()
                    .unwrap_err()
                    .contains("malformed token"),
                "{text}"
            );
        }
    }
}
