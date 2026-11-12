//! Review lenses: named specialist instructions for one extra, independent review turn.
//!
//! A lens is a name and an editable instruction template. A name resolves through the
//! project's own override, then the user scope, then the built-in concurrency, security and
//! simplicity lenses. A task revision pins the ordered selection with the instructions the
//! lenses had when it was defined, so editing a lens never changes a running or resumed
//! attempt. A selection that names no lens keeps the ordinary single review.
use super::{
    categories::{self, MAX_TEMPLATE_CHARS, Scope, template},
    execution::Run,
    scoped::{Layered, Table},
    *,
};
use rusqlite::Connection;
use serde::Serialize;

pub mod builtin;

/// Most lenses one task can select; each one is a full read-only model turn.
pub const MAX_SELECTED: usize = 5;
pub const MAX_INSTRUCTION_CHARS: usize = MAX_TEMPLATE_CHARS;

/// A lens as defined, or as a revision pinned it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lens {
    pub name: String,
    pub description: String,
    /// A prompt template over the task fields and the turn's evidence.
    pub instructions: String,
    pub scope: Scope,
}

impl Lens {
    /// Rejects names, sizes and placeholders that cannot be saved.
    pub fn validate(&self) -> Result<()> {
        valid_name(&self.name)?;
        require(
            !self.instructions.trim().is_empty(),
            "The lens instructions are required",
        )?;
        require(
            self.instructions.chars().count() <= MAX_INSTRUCTION_CHARS,
            &format!("The lens instructions are longer than {MAX_INSTRUCTION_CHARS} characters"),
        )?;
        template::validate(&self.instructions)
            .map_err(|e| Error::Conflict(format!("The lens instructions: {e}")))
    }
}

/// A lens as the layered lookup found it, with the guard to change it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Stored row version; 0 for a built-in lens.
    pub version: i64,
    #[serde(flatten)]
    pub lens: Lens,
}

pub fn valid_name(name: &str) -> Result<()> {
    categories::check_name("Lens", name)
}

const COLUMNS: &str = "name, description, instructions, row_version";

fn read(scope: Scope, row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        version: row.get(3)?,
        lens: Lens {
            name: row.get(0)?,
            description: row.get(1)?,
            instructions: row.get(2)?,
            scope,
        },
    })
}

fn built_in(name: &str) -> Option<Entry> {
    builtin::named(name).map(|lens| Entry { version: 0, lens })
}

fn built_ins() -> Vec<Entry> {
    builtin::all()
        .into_iter()
        .map(|lens| Entry { version: 0, lens })
        .collect()
}

impl Layered for Entry {
    fn name(&self) -> &str {
        &self.lens.name
    }
    fn version(&self) -> i64 {
        self.version
    }
}

const TABLE: Table<Entry> = Table {
    name: "review_lenses",
    columns: COLUMNS,
    subject: "Lens",
    plural: "lenses",
    read,
    built_in,
    built_ins,
};

/// The lens `name` resolves to for `project`: its override, the user scope, a built-in.
fn resolve(db: &Connection, project: i64, name: &str) -> Result<Option<Entry>> {
    TABLE.resolve(db, project, name)
}

/// The latest lens of this name an earlier revision of the task pinned.
fn pinned_named(db: &Connection, task: i64, name: &str) -> Result<Option<Lens>> {
    Ok(db
        .query_row(
            "SELECT name, scope, instructions FROM revision_lenses WHERE task_id = ?1 AND name = ?2 COLLATE NOCASE ORDER BY revision DESC LIMIT 1",
            params![task, name],
            read_pin,
        )
        .optional()?)
}

fn read_pin(row: &rusqlite::Row<'_>) -> rusqlite::Result<Lens> {
    Ok(Lens {
        name: row.get(0)?,
        description: String::new(),
        scope: Scope::parse(&row.get::<_, String>(1)?).unwrap_or(Scope::BuiltIn),
        instructions: row.get(2)?,
    })
}

fn default_names(db: &Connection, project: i64) -> Result<Vec<String>> {
    let mut statement = db.prepare_cached(
        "SELECT lens FROM project_lens_defaults WHERE project_id = ?1 ORDER BY ordinal",
    )?;
    Ok(statement
        .query_map([project], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?)
}

/// Rejects a selection that repeats a lens or is longer than a task can run.
fn check_selection(names: &[String]) -> Result<()> {
    require(
        names.len() <= MAX_SELECTED,
        &format!("A task can select at most {MAX_SELECTED} review lenses"),
    )?;
    for (index, name) in names.iter().enumerate() {
        require(
            !names[..index].iter().any(|n| n.eq_ignore_ascii_case(name)),
            &format!("Review lens {name} is selected twice"),
        )?;
    }
    Ok(())
}

/// The lenses copied into a revision being defined, in review order. `None` selects the
/// project's defaults; an empty selection keeps the ordinary single review. A name that no
/// longer resolves keeps the lens an earlier revision of the same task pinned.
pub(super) fn pin_for(
    db: &Connection,
    project: i64,
    task: i64,
    definition: &Definition,
) -> Result<Vec<Lens>> {
    if definition.kind == Kind::Human {
        require(
            definition.lenses.as_ref().is_none_or(Vec::is_empty),
            "Human tasks are done by the owner and have no AI review lenses",
        )?;
        return Ok(Vec::new());
    }
    let inherited = definition.lenses.is_none();
    let names = match &definition.lenses {
        Some(names) => names.iter().map(|n| n.trim().to_owned()).collect(),
        None => default_names(db, project)?,
    };
    check_selection(&names)?;
    names
        .iter()
        .map(|name| {
            if let Some(entry) = resolve(db, project, name)? {
                return Ok(entry.lens);
            }
            pinned_named(db, task, name)?.ok_or_else(|| {
                Error::Conflict(if inherited {
                    format!(
                        "The project's default review lens {name} no longer exists; fix the defaults with `ktask lens defaults`"
                    )
                } else {
                    format!("Unknown review lens {name}; list them with `ktask lens list`")
                })
            })
        })
        .collect()
}

/// Pins `lenses` to a revision in review order.
pub(super) fn write_pin(
    tx: &Transaction<'_>,
    task: i64,
    revision: i64,
    lenses: &[Lens],
) -> Result<()> {
    for (index, lens) in lenses.iter().enumerate() {
        tx.execute(
            "INSERT INTO revision_lenses(task_id, revision, ordinal, name, scope, instructions) VALUES (?1,?2,?3,?4,?5,?6)",
            params![task, revision, index as i64 + 1, lens.name, lens.scope.as_str(), lens.instructions],
        )?;
    }
    Ok(())
}

/// The ordered lenses a revision pinned; none for the ordinary single review.
pub(super) fn pinned(db: &Connection, task: i64, revision: i64) -> Result<Vec<Lens>> {
    let mut statement = db.prepare_cached(
        "SELECT name, scope, instructions FROM revision_lenses WHERE task_id = ?1 AND revision = ?2 ORDER BY ordinal",
    )?;
    Ok(statement
        .query_map(params![task, revision], read_pin)?
        .collect::<rusqlite::Result<_>>()?)
}

/// One finding of a lens review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub critical: bool,
    pub description: String,
}

/// A finished review of one source state through one lens.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Review {
    /// Position of the lens in the revision's selection.
    pub ordinal: i64,
    pub lens: String,
    pub step: i64,
    /// Repair rounds already spent when the review ran.
    pub round: i64,
    pub critical: bool,
    pub summary: String,
    pub findings: Vec<Finding>,
}

/// The findings of one lens review, in the order the reviewer reported them.
fn findings_of(
    db: &Connection,
    attempt: i64,
    round: i64,
    tree: &str,
    ordinal: i64,
) -> Result<Vec<Finding>> {
    let mut statement = db.prepare_cached(
        "SELECT severity, description FROM lens_review_findings WHERE attempt_id=?1 AND repair_round=?2 AND source_tree=?3 AND ordinal=?4 ORDER BY finding",
    )?;
    Ok(statement
        .query_map(params![attempt, round, tree, ordinal], read_finding)?
        .collect::<rusqlite::Result<_>>()?)
}

/// Every lens review of an attempt, in the order they finished.
pub(super) fn reviews_of(db: &Connection, attempt: i64) -> Result<Vec<Review>> {
    let mut statement = db.prepare_cached(
        "SELECT source_tree, ordinal, lens, step_id, repair_round, critical, summary FROM lens_reviews WHERE attempt_id = ?1 ORDER BY step_id",
    )?;
    let reviews: Vec<(String, Review)> = statement
        .query_map([attempt], |r| {
            Ok((
                r.get(0)?,
                Review {
                    ordinal: r.get(1)?,
                    lens: r.get(2)?,
                    step: r.get(3)?,
                    round: r.get(4)?,
                    critical: r.get(5)?,
                    summary: r.get(6)?,
                    findings: Vec::new(),
                },
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    reviews
        .into_iter()
        .map(|(tree, mut review)| {
            review.findings = findings_of(db, attempt, review.round, &tree, review.ordinal)?;
            Ok(review)
        })
        .collect()
}

fn read_finding(row: &rusqlite::Row<'_>) -> rusqlite::Result<Finding> {
    Ok(Finding {
        critical: row.get::<_, String>(0)? == "critical",
        description: row.get(1)?,
    })
}

impl Service {
    /// Every lens this project can choose, by name, each from the layer that wins.
    pub fn lenses(&self) -> Result<Vec<Entry>> {
        TABLE.list(self.db.connection(), self.project_id)
    }

    /// The lens `name` resolves to for this project.
    pub fn lens(&self, name: &str) -> Result<Option<Entry>> {
        resolve(self.db.connection(), self.project_id, name)
    }

    /// The definition of `name` in one scope only, without falling through.
    pub fn lens_in(&self, scope: Scope, name: &str) -> Result<Option<Entry>> {
        TABLE.in_scope(self.db.connection(), self.project_id, scope, name)
    }

    /// Creates the lens in `scope`, or replaces the one of that name there. `expected` is
    /// the version the caller read; a stale one is refused.
    pub fn save_lens(&mut self, scope: Scope, expected: Option<i64>, lens: &Lens) -> Result<Entry> {
        lens.validate()?;
        TABLE.save(
            self.db.connection_mut(),
            self.project_id,
            scope,
            &lens.name,
            expected,
            |tx, project, existing, now| {
                match existing {
                    Some(found) => tx.execute(
                        "UPDATE review_lenses SET description=?3, instructions=?4, row_version=row_version+1, updated_at_ms=?5 WHERE name=?2 AND ((?1 IS NULL AND project_id IS NULL) OR project_id=?1)",
                        params![project, found.lens.name, lens.description.trim(), lens.instructions, now],
                    )?,
                    None => tx.execute(
                        "INSERT INTO review_lenses(project_id, name, description, instructions, created_at_ms, updated_at_ms) VALUES (?1,?2,?3,?4,?5,?5)",
                        params![project, lens.name, lens.description.trim(), lens.instructions, now],
                    )?,
                };
                Ok(())
            },
        )
    }

    /// Removes a user or project definition so the name inherits again. Tasks that pinned
    /// it keep their copy.
    pub fn delete_lens(&mut self, scope: Scope, name: &str, expected: Option<i64>) -> Result<()> {
        TABLE.delete(
            self.db.connection_mut(),
            self.project_id,
            scope,
            name,
            expected,
        )
    }

    /// The ordered lens names this project's new tasks select unless a task chooses its own.
    pub fn lens_defaults(&self) -> Result<Vec<String>> {
        default_names(self.db.connection(), self.project_id)
    }

    /// Replaces the project's default selection. `expected` is the selection the caller
    /// read; a selection changed elsewhere is refused. Every name must resolve.
    pub fn save_lens_defaults(
        &mut self,
        expected: &[String],
        names: &[String],
    ) -> Result<Vec<String>> {
        let names: Vec<String> = names.iter().map(|n| n.trim().to_owned()).collect();
        check_selection(&names)?;
        let project = self.project_id;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require(
            default_names(&tx, project)? == expected,
            "The default review lenses changed elsewhere; read them again before retrying",
        )?;
        let mut canonical = Vec::new();
        for name in &names {
            let entry = resolve(&tx, project, name)?.ok_or_else(|| {
                Error::Conflict(format!(
                    "Unknown review lens {name}; list them with `ktask lens list`"
                ))
            })?;
            canonical.push(entry.lens.name);
        }
        tx.execute(
            "DELETE FROM project_lens_defaults WHERE project_id = ?1",
            [project],
        )?;
        for (index, name) in canonical.iter().enumerate() {
            tx.execute(
                "INSERT INTO project_lens_defaults(project_id, ordinal, lens) VALUES (?1,?2,?3)",
                params![project, index as i64 + 1, name],
            )?;
        }
        tx.commit()?;
        Ok(canonical)
    }

    /// The finished review of `tree` in repair round `round` through the lens at `ordinal`,
    /// when the attempt has one.
    pub fn lens_review(
        &self,
        run: &Run,
        round: i64,
        tree: &str,
        ordinal: i64,
    ) -> Result<Option<Review>> {
        let db = self.db.connection();
        let found = db
            .query_row(
                "SELECT lens, step_id, critical, summary FROM lens_reviews WHERE attempt_id=?1 AND repair_round=?2 AND source_tree=?3 AND ordinal=?4",
                params![run.attempt, round, tree, ordinal],
                |r| {
                    Ok(Review {
                        ordinal,
                        lens: r.get(0)?,
                        step: r.get(1)?,
                        round,
                        critical: r.get(2)?,
                        summary: r.get(3)?,
                        findings: Vec::new(),
                    })
                },
            )
            .optional()?;
        found
            .map(|mut review| {
                review.findings = findings_of(db, run.attempt, round, tree, ordinal)?;
                Ok(review)
            })
            .transpose()
    }

    /// Keeps a finished lens review of `tree` so a resumed attempt does not repeat it.
    pub fn record_lens_review(&mut self, run: &Run, tree: &str, review: &Review) -> Result<()> {
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO lens_reviews(attempt_id, repair_round, source_tree, ordinal, lens, step_id, critical, summary) VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![run.attempt, review.round, tree, review.ordinal, review.lens, review.step, review.critical, review.summary],
        )?;
        for (index, finding) in review.findings.iter().enumerate() {
            tx.execute(
                "INSERT INTO lens_review_findings(attempt_id, repair_round, source_tree, ordinal, finding, severity, description) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![
                    run.attempt,
                    review.round,
                    tree,
                    review.ordinal,
                    index as i64 + 1,
                    if finding.critical { "critical" } else { "minor" },
                    finding.description
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Ends the provider session of a lens turn, so no later turn resumes it: every lens
    /// review of every round starts from nothing but its own prompt.
    pub fn close_session(&self, step: &execution::Step) -> Result<()> {
        self.db.connection().execute(
            "UPDATE ai_sessions SET state='closed' WHERE id=?1 AND state='open'",
            [step.session],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_are_valid_and_ask_for_reachable_defects_and_proportionate_fixes() {
        let names: Vec<_> = builtin::all().iter().map(|l| l.name.clone()).collect();
        assert_eq!(names, ["concurrency", "security", "simplicity"]);
        for lens in builtin::all() {
            lens.validate().expect("built-in instructions are valid");
            let text = &lens.instructions;
            assert!(
                text.contains("Report only a defect you can reach")
                    && text.contains("smallest proportionate correction"),
                "{}",
                lens.name
            );
            for placeholder in ["{{title}}", "{{goal}}", "{{criteria}}", "{{context}}"] {
                assert!(text.contains(placeholder), "{} {placeholder}", lens.name);
            }
        }
    }

    #[test]
    fn selections_reject_repeats_and_excess() {
        let names = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert!(check_selection(&names(&["security", "Concurrency"])).is_ok());
        assert!(check_selection(&names(&["security", "SECURITY"])).is_err());
        assert!(check_selection(&names(&["a", "b", "c", "d", "e", "f"])).is_err());
    }

    #[test]
    fn names_follow_the_category_rules() {
        assert!(valid_name("security").is_ok());
        assert!(valid_name("Data races").is_ok());
        for bad in ["", " x", "none", "a/b"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        assert!(
            valid_name("none")
                .unwrap_err()
                .to_string()
                .contains("Lens names default")
        );
    }
}
