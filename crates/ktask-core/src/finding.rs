//! A reviewer's finding: a thing to fix, located and stated once, that the fixer, the operator
//! and the next reviewer all read the same way — read from a JSON or TOML file, the same two
//! formats [`crate::import_tasks`] takes.

use std::error::Error;
use std::fmt;
use std::str::FromStr;

use serde::Deserialize;

use crate::TaskFormat;
use crate::json_field::field_error;

/// Whether a finding is inside the change under review, or outside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindingScope {
    /// Inside the change under review: a fix belongs in this task.
    Task,
    /// Outside the change under review: becomes a task of its own, not a fix in this one.
    Elsewhere,
}

impl FindingScope {
    /// The name this scope is written with.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Elsewhere => "elsewhere",
        }
    }
}

impl fmt::Display for FindingScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for FindingScope {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "task" => Ok(Self::Task),
            "elsewhere" => Ok(Self::Elsewhere),
            _ => Err(format!(
                "unknown scope {text:?}: expected task or elsewhere"
            )),
        }
    }
}

/// A reviewer's finding: a thing to fix, located and stated once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Where it is: `path:line` or `path`.
    pub location: String,
    /// What is wrong.
    pub problem: String,
    /// What would make it right.
    pub fix: String,
    /// Whether it is inside the change under review, or outside it.
    pub scope: FindingScope,
}

/// A finding as the JSON array, or the TOML `[[findings]]` table, writes it: every field is
/// required, and a field that is neither of these four is refused, naming it, by
/// `deny_unknown_fields`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Item {
    location: String,
    problem: String,
    fix: String,
    scope: String,
}

/// One finding of the file that cannot be added.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidFinding {
    /// Its place in the file, counting from 1.
    pub index: usize,
    /// Everything wrong with it.
    pub problems: Vec<String>,
}

/// Why no finding was read from a `changes-requested --findings` file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingsError {
    /// The text is not JSON; the message says where it stops making sense.
    Malformed(String),
    /// The text is JSON but not an array.
    NotAnArray,
    /// The text is not TOML; the message says where it stops making sense.
    MalformedToml(String),
    /// The text is TOML but not a list of `[[findings]]` tables; the message says why.
    NotAFindingsTable(String),
    /// The file is neither a `.json` nor a `.toml` file.
    UnsupportedFormat,
    /// Some findings are missing a required field, or carry a field that is neither of the
    /// four, or a `scope` that is neither `task` nor `elsewhere`; every one is listed, by its
    /// place in the file and the field it is about.
    Invalid(Vec<InvalidFinding>),
    /// The file named no finding at all: `changes-requested` is refused without at least one.
    Empty,
}

impl fmt::Display for FindingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(message) => write!(f, "not valid JSON: {message}"),
            Self::NotAnArray => f.write_str("expected a JSON array of findings"),
            Self::MalformedToml(message) => write!(f, "not valid TOML: {message}"),
            Self::NotAFindingsTable(message) => {
                write!(f, "expected [[findings]] tables: {message}")
            }
            Self::UnsupportedFormat => f.write_str("only .json and .toml files are read"),
            Self::Invalid(findings) => {
                write!(
                    f,
                    "{} invalid, so nothing was recorded",
                    match findings.len() {
                        1 => "1 finding is".to_owned(),
                        n => format!("{n} findings are"),
                    }
                )?;
                findings.iter().try_for_each(|finding| {
                    finding.problems.iter().try_for_each(|problem| {
                        write!(f, "\n  - finding {}: {problem}", finding.index)
                    })
                })
            }
            Self::Empty => f.write_str("changes-requested needs at least one finding"),
        }
    }
}

impl Error for FindingsError {}

/// The findings a `format` file's elements decode to, in order, or everything wrong with one
/// of them: a value that is not an object with every one of `location`, `problem`, `fix` and
/// `scope` — each a string — or a `scope` other than `task` or `elsewhere`.
fn read_finding(value: serde_json::Value, index: usize) -> Result<Finding, InvalidFinding> {
    let item: Item = serde_path_to_error::deserialize(value).map_err(|e| InvalidFinding {
        index,
        problems: vec![field_error(&e)],
    })?;
    match item.scope.parse::<FindingScope>() {
        Ok(scope) => Ok(Finding {
            location: item.location,
            problem: item.problem,
            fix: item.fix,
            scope,
        }),
        Err(message) => Err(InvalidFinding {
            index,
            problems: vec![format!("scope: {message}")],
        }),
    }
}

/// The findings a `format` file's `[[findings]]` tables, or JSON array, decode to.
fn findings_values(
    text: &str,
    format: TaskFormat,
) -> Result<Vec<serde_json::Value>, FindingsError> {
    match format {
        TaskFormat::Json => {
            match serde_json::from_str(text).map_err(|e| FindingsError::Malformed(e.to_string()))? {
                serde_json::Value::Array(values) => Ok(values),
                _ => Err(FindingsError::NotAnArray),
            }
        }
        TaskFormat::Toml => toml_findings(text),
    }
}

fn toml_findings(text: &str) -> Result<Vec<serde_json::Value>, FindingsError> {
    let mut table: toml::Table = text
        .parse()
        .map_err(|e: toml::de::Error| FindingsError::MalformedToml(e.to_string()))?;
    let findings = table.remove("findings");
    if let Some(other) = table.keys().next() {
        return Err(FindingsError::NotAFindingsTable(format!(
            "unknown top-level key {other:?}"
        )));
    }
    match findings {
        Some(toml::Value::Array(values)) => values
            .iter()
            .map(|value| {
                serde_json::to_value(value).map_err(|e| FindingsError::MalformedToml(e.to_string()))
            })
            .collect(),
        Some(_) => Err(FindingsError::NotAFindingsTable(
            "`findings` is not a list of [[findings]] tables".to_owned(),
        )),
        None => Err(FindingsError::NotAFindingsTable(
            "no [[findings]] table".to_owned(),
        )),
    }
}

/// Reads every finding of the `format` file `text`, in order.
///
/// # Errors
///
/// Fails, reading nothing, when `text` is not a findings file of `format`, when any finding
/// breaks a rule or carries a field that is not one of the four — all of them are listed, by
/// their place in the file — or when the file names no finding at all.
pub fn parse_findings(text: &str, format: TaskFormat) -> Result<Vec<Finding>, FindingsError> {
    let values = findings_values(text, format)?;
    let mut findings = Vec::new();
    let mut invalid = Vec::new();
    for (index, value) in values.into_iter().enumerate() {
        match read_finding(value, index + 1) {
            Ok(finding) => findings.push(finding),
            Err(problem) => invalid.push(problem),
        }
    }
    if !invalid.is_empty() {
        return Err(FindingsError::Invalid(invalid));
    }
    if findings.is_empty() {
        return Err(FindingsError::Empty);
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str =
        r#"[{"location": "src/a.rs:10", "problem": "p", "fix": "f", "scope": "task"}]"#;

    #[test]
    fn a_well_formed_json_array_reads_back_every_field() {
        let findings = parse_findings(ONE, TaskFormat::Json).unwrap();
        assert_eq!(
            findings,
            vec![Finding {
                location: "src/a.rs:10".to_owned(),
                problem: "p".to_owned(),
                fix: "f".to_owned(),
                scope: FindingScope::Task,
            }]
        );
    }

    #[test]
    fn toml_findings_table_reads_the_same_fields() {
        let toml = "[[findings]]\nlocation = \"src/a.rs:10\"\nproblem = \"p\"\nfix = \"f\"\nscope = \"elsewhere\"\n";
        let findings = parse_findings(toml, TaskFormat::Toml).unwrap();
        assert_eq!(findings[0].scope, FindingScope::Elsewhere);
        assert_eq!(findings[0].location, "src/a.rs:10");
    }

    #[test]
    fn an_empty_array_is_refused() {
        assert_eq!(
            parse_findings("[]", TaskFormat::Json),
            Err(FindingsError::Empty)
        );
    }

    #[test]
    fn a_missing_field_is_refused_naming_the_index_and_the_field() {
        let json = r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "task"},
            {"location": "a", "problem": "p", "fix": "f"}]"#;
        let Err(FindingsError::Invalid(invalid)) = parse_findings(json, TaskFormat::Json) else {
            panic!("expected invalid findings");
        };
        assert_eq!(invalid.len(), 1);
        assert_eq!(invalid[0].index, 2);
        assert!(
            invalid[0].problems[0].contains("scope"),
            "{:?}",
            invalid[0].problems
        );
    }

    #[test]
    fn an_unknown_scope_is_refused_naming_the_index_and_the_field() {
        let json = r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "urgent"}]"#;
        let Err(FindingsError::Invalid(invalid)) = parse_findings(json, TaskFormat::Json) else {
            panic!("expected invalid findings");
        };
        assert_eq!(invalid[0].index, 1);
        assert!(
            invalid[0].problems[0].contains("scope"),
            "{:?}",
            invalid[0].problems
        );
        assert!(
            invalid[0].problems[0].contains("urgent"),
            "{:?}",
            invalid[0].problems
        );
    }

    #[test]
    fn an_unknown_field_is_refused() {
        let json = r#"[{"location": "a", "problem": "p", "fix": "f", "scope": "task", "severity": "high"}]"#;
        let Err(FindingsError::Invalid(invalid)) = parse_findings(json, TaskFormat::Json) else {
            panic!("expected invalid findings");
        };
        assert!(
            invalid[0].problems[0].contains("severity"),
            "{:?}",
            invalid[0].problems
        );
    }

    #[test]
    fn not_json_or_not_an_array_is_refused() {
        assert!(matches!(
            parse_findings("not json", TaskFormat::Json),
            Err(FindingsError::Malformed(_))
        ));
        assert_eq!(
            parse_findings("{}", TaskFormat::Json),
            Err(FindingsError::NotAnArray)
        );
    }

    #[test]
    fn toml_without_a_findings_table_is_refused() {
        assert!(matches!(
            parse_findings("x = 1\n", TaskFormat::Toml),
            Err(FindingsError::NotAFindingsTable(_))
        ));
    }
}
