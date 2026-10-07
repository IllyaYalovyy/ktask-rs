//! The formats a file of tasks is written in, and reading each into the same list of task
//! values — the only place the formats differ.

use crate::ImportError;

/// A format a file of tasks is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskFormat {
    /// A JSON array of tasks.
    Json,
    /// TOML with a `[[tasks]]` table for each task.
    Toml,
}

impl TaskFormat {
    /// The format of the file `path` names, decided by its extension alone; `-`, standard
    /// input, has none and is JSON.
    ///
    /// # Errors
    ///
    /// Fails, naming both formats, when the extension is neither `.json` nor `.toml`.
    pub fn of_path(path: &str) -> Result<Self, ImportError> {
        if path == "-" {
            return Ok(Self::Json);
        }
        match std::path::Path::new(path)
            .extension()
            .and_then(|extension| extension.to_str())
        {
            Some("json") => Ok(Self::Json),
            Some("toml") => Ok(Self::Toml),
            _ => Err(ImportError::UnsupportedFormat),
        }
    }

    /// The tasks `text` holds, in order, each as the value its fields are read from.
    pub(crate) fn tasks(self, text: &str) -> Result<Vec<serde_json::Value>, ImportError> {
        match self {
            Self::Json => match serde_json::from_str(text)
                .map_err(|e| ImportError::Malformed(e.to_string()))?
            {
                serde_json::Value::Array(values) => Ok(values),
                _ => Err(ImportError::NotAnArray),
            },
            Self::Toml => toml_tasks(text),
        }
    }
}

fn toml_tasks(text: &str) -> Result<Vec<serde_json::Value>, ImportError> {
    let mut table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| ImportError::MalformedToml(e.to_string()))?;
    let tasks = table.remove("tasks");
    if let Some(other) = table.keys().next() {
        return Err(ImportError::NotATaskTable(format!(
            "unknown top-level key {other:?}"
        )));
    }
    match tasks {
        Some(toml::Value::Array(values)) => values
            .iter()
            .map(|value| {
                serde_json::to_value(value).map_err(|e| ImportError::MalformedToml(e.to_string()))
            })
            .collect(),
        Some(_) => Err(ImportError::NotATaskTable(
            "`tasks` is not a list of [[tasks]] tables".to_owned(),
        )),
        None => Err(ImportError::NotATaskTable("no [[tasks]] table".to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extension_decides_and_standard_input_is_json() {
        assert_eq!(TaskFormat::of_path("a/b.json"), Ok(TaskFormat::Json));
        assert_eq!(TaskFormat::of_path("b.toml"), Ok(TaskFormat::Toml));
        assert_eq!(TaskFormat::of_path("-"), Ok(TaskFormat::Json));
        for path in ["x.yaml", "x.md", "x", "x.JSON", "json", "x.json.bak"] {
            assert_eq!(
                TaskFormat::of_path(path),
                Err(ImportError::UnsupportedFormat),
                "{path}"
            );
        }
    }

    #[test]
    fn toml_tasks_come_out_in_order() {
        let values = TaskFormat::Toml
            .tasks("[[tasks]]\ntitle = \"a\"\n[[tasks]]\ntitle = \"b\"\n")
            .unwrap();
        assert_eq!(
            values,
            [
                serde_json::json!({"title": "a"}),
                serde_json::json!({"title": "b"})
            ]
        );
    }

    #[test]
    fn toml_that_is_not_a_list_of_task_tables_is_refused() {
        for text in ["", "x = 1\n[[tasks]]\n", "tasks = 3\n"] {
            assert!(
                matches!(
                    TaskFormat::Toml.tasks(text),
                    Err(ImportError::NotATaskTable(_))
                ),
                "{text:?}"
            );
        }
        assert!(matches!(
            TaskFormat::Toml.tasks("[[tasks"),
            Err(ImportError::MalformedToml(_))
        ));
    }
}
