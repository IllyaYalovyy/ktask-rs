//! Work categories: how to execute, review, validate and remediate a kind of work.
//!
//! A category is a name, whether the project's checks gate the work, and one template per
//! role. A name resolves through the project's own override, then the user scope, then the
//! built-in categories. A task revision copies the resolved category when it is defined and
//! keeps it, so editing a category later never changes a running or resumed attempt.
//! Templates only describe the work; the supervisor's rules, evidence handling and response
//! schema are added around them by [`super::prompt`] and cannot be edited here.
use super::*;
use rusqlite::Connection;
use serde::Serialize;

pub mod builtin;
pub mod template;

pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_TEMPLATE_CHARS: usize = 8000;
/// The roles a category has a template for, in form order.
pub const TEMPLATE_ROLES: [&str; 4] = ["execution", "review", "validation", "remediation"];

/// Where a category definition comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    BuiltIn,
    User,
    Project,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::User => "user",
            Self::Project => "project",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built-in",
            Self::User => "user",
            Self::Project => "project",
        }
    }
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "built_in" => Some(Self::BuiltIn),
            "user" => Some(Self::User),
            "project" => Some(Self::Project),
            _ => None,
        }
    }
}

/// One template per role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Templates {
    pub execution: String,
    pub review: String,
    pub validation: String,
    pub remediation: String,
}

impl Templates {
    pub fn get(&self, role: &str) -> Option<&str> {
        match role {
            "execution" => Some(&self.execution),
            "review" => Some(&self.review),
            "validation" => Some(&self.validation),
            "remediation" => Some(&self.remediation),
            _ => None,
        }
    }
    pub fn set(&mut self, role: &str, text: String) -> bool {
        let slot = match role {
            "execution" => &mut self.execution,
            "review" => &mut self.review,
            "validation" => &mut self.validation,
            "remediation" => &mut self.remediation,
            _ => return false,
        };
        *slot = text;
        true
    }
}

/// A category as defined, or as a revision pinned it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Category {
    pub name: String,
    pub description: String,
    /// Whether the project's checks gate this work.
    pub checks: bool,
    pub templates: Templates,
    pub scope: Scope,
}

impl Category {
    /// The built-in category a task of `kind` runs when none is chosen; Human work has none.
    pub fn for_kind(kind: Kind) -> Option<Self> {
        match kind {
            Kind::Human => None,
            Kind::Ai => builtin::named(builtin::CODE),
            Kind::Docs => builtin::named(builtin::DOCUMENTS),
        }
    }

    /// Rejects names, sizes and placeholders that cannot be saved.
    pub fn validate(&self) -> Result<()> {
        valid_name(&self.name)?;
        for role in TEMPLATE_ROLES {
            let text = self
                .templates
                .get(role)
                .expect("every template role exists");
            require(
                !text.trim().is_empty(),
                &format!("The {role} template is required"),
            )?;
            require(
                text.chars().count() <= MAX_TEMPLATE_CHARS,
                &format!("The {role} template is longer than {MAX_TEMPLATE_CHARS} characters"),
            )?;
            template::validate(text)
                .map_err(|e| Error::Conflict(format!("The {role} template: {e}")))?;
        }
        Ok(())
    }
}

/// A category as the layered lookup found it, with the guard to change it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Entry {
    /// Stored row version; 0 for a built-in category.
    pub version: i64,
    #[serde(flatten)]
    pub category: Category,
}

/// Names a category cannot take: forms and the CLI use them as "no choice".
const RESERVED: [&str; 2] = ["default", "none"];

pub fn valid_name(name: &str) -> Result<()> {
    require(!name.trim().is_empty(), "Category name is required")?;
    require(
        name == name.trim()
            && name.chars().count() <= MAX_NAME_CHARS
            && name
                .chars()
                .all(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
            && name.chars().next().is_some_and(char::is_alphanumeric),
        "Category name must start with a letter or digit and use at most 64 letters, digits, spaces, '-', '_' or '.'",
    )?;
    require(
        !RESERVED.iter().any(|r| name.eq_ignore_ascii_case(r)),
        "Category names default and none are reserved",
    )
}

const COLUMNS: &str = "name, description, run_checks, execution_template, review_template, validation_template, remediation_template, row_version";

fn read(scope: Scope, row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    Ok(Entry {
        version: row.get(7)?,
        category: Category {
            name: row.get(0)?,
            description: row.get(1)?,
            checks: row.get::<_, i64>(2)? != 0,
            templates: Templates {
                execution: row.get(3)?,
                review: row.get(4)?,
                validation: row.get(5)?,
                remediation: row.get(6)?,
            },
            scope,
        },
    })
}

fn stored(db: &Connection, project: Option<i64>, name: &str) -> Result<Option<Entry>> {
    let (scope, filter) = match project {
        Some(_) => (Scope::Project, "project_id = ?1"),
        None => (Scope::User, "project_id IS NULL AND ?1 IS NULL"),
    };
    Ok(db
        .query_row(
            &format!("SELECT {COLUMNS} FROM work_categories WHERE {filter} AND name = ?2"),
            params![project, name.trim()],
            |row| read(scope, row),
        )
        .optional()?)
}

fn stored_all(db: &Connection, project: Option<i64>) -> Result<Vec<Entry>> {
    let (scope, filter) = match project {
        Some(_) => (Scope::Project, "project_id = ?1"),
        None => (Scope::User, "project_id IS NULL AND ?1 IS NULL"),
    };
    let mut statement = db.prepare(&format!(
        "SELECT {COLUMNS} FROM work_categories WHERE {filter} ORDER BY name"
    ))?;
    Ok(statement
        .query_map([project], |row| read(scope, row))?
        .collect::<rusqlite::Result<_>>()?)
}

fn built_in(name: &str) -> Option<Entry> {
    builtin::named(name).map(|category| Entry {
        version: 0,
        category,
    })
}

/// The category `name` resolves to for `project`: its override, the user scope, a built-in.
pub(super) fn resolve(db: &Connection, project: i64, name: &str) -> Result<Option<Entry>> {
    if let Some(found) = stored(db, Some(project), name)? {
        return Ok(Some(found));
    }
    if let Some(found) = stored(db, None, name)? {
        return Ok(Some(found));
    }
    Ok(built_in(name))
}

/// The category copied into a revision being defined, or `None` for Human work. A name that
/// no longer resolves keeps the category an earlier revision of the same task pinned.
pub(super) fn pin_for(
    db: &Connection,
    project: i64,
    task: i64,
    definition: &Definition,
) -> Result<Option<Category>> {
    if definition.kind == Kind::Human {
        require(
            definition.category.is_none(),
            "Human tasks are done by the owner and have no work category",
        )?;
        return Ok(None);
    }
    let name = match &definition.category {
        Some(name) => name.clone(),
        None => {
            Category::for_kind(definition.kind)
                .expect("AI-run kinds have a default category")
                .name
        }
    };
    if let Some(entry) = resolve(db, project, &name)? {
        return Ok(Some(entry.category));
    }
    let earlier = pinned_named(db, task, &name)?;
    earlier.map(Some).ok_or_else(|| {
        Error::Conflict(format!(
            "Unknown work category {name}; list them with `ktask category list`"
        ))
    })
}

const SNAPSHOT_COLUMNS: &str = "s.name, s.scope, s.run_checks, s.execution_template, s.review_template, s.validation_template, s.remediation_template";

/// The latest category of this name that an earlier revision of the task pinned.
fn pinned_named(db: &Connection, task: i64, name: &str) -> Result<Option<Category>> {
    Ok(db
        .query_row(
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM revision_categories r JOIN category_snapshots s ON s.id = r.snapshot_id WHERE r.task_id = ?1 AND s.name = ?2 COLLATE NOCASE ORDER BY r.revision DESC LIMIT 1"),
            params![task, name],
            read_pin,
        )
        .optional()?)
}

fn read_pin(row: &rusqlite::Row<'_>) -> rusqlite::Result<Category> {
    Ok(Category {
        name: row.get(0)?,
        description: String::new(),
        scope: Scope::parse(&row.get::<_, String>(1)?).unwrap_or(Scope::BuiltIn),
        checks: row.get::<_, i64>(2)? != 0,
        templates: Templates {
            execution: row.get(3)?,
            review: row.get(4)?,
            validation: row.get(5)?,
            remediation: row.get(6)?,
        },
    })
}

/// Pins `category` to a revision, sharing the snapshot of an identical earlier pin.
pub(super) fn write_pin(
    tx: &Transaction<'_>,
    task: i64,
    revision: i64,
    category: &Category,
) -> Result<()> {
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    for part in [
        category.name.as_str(),
        category.scope.as_str(),
        if category.checks { "1" } else { "0" },
        &category.templates.execution,
        &category.templates.review,
        &category.templates.validation,
        &category.templates.remediation,
    ] {
        // Length-prefixed, so no two different categories share a digest input.
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    let digest = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let t = &category.templates;
    tx.execute(
        "INSERT OR IGNORE INTO category_snapshots(digest, name, scope, run_checks, execution_template, review_template, validation_template, remediation_template, created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        params![digest, category.name, category.scope.as_str(), category.checks, t.execution, t.review, t.validation, t.remediation, now_ms()],
    )?;
    tx.execute(
        "INSERT INTO revision_categories(task_id, revision, snapshot_id) SELECT ?1, ?2, id FROM category_snapshots WHERE digest = ?3",
        params![task, revision, digest],
    )?;
    Ok(())
}

/// The category a revision pinned; one defined before categories existed runs the built-in
/// category of its kind. `None` for Human work.
pub(super) fn pinned(
    db: &Connection,
    task: i64,
    revision: i64,
    kind: Kind,
) -> Result<Option<Category>> {
    let found = db
        .query_row(
            &format!("SELECT {SNAPSHOT_COLUMNS} FROM revision_categories r JOIN category_snapshots s ON s.id = r.snapshot_id WHERE r.task_id = ?1 AND r.revision = ?2"),
            params![task, revision],
            read_pin,
        )
        .optional()?;
    Ok(found.or_else(|| Category::for_kind(kind)))
}

impl Service {
    /// Every category this project can choose, by name, each from the layer that wins.
    pub fn categories(&self) -> Result<Vec<Entry>> {
        let db = self.db.connection();
        let mut all: Vec<Entry> = builtin::all()
            .into_iter()
            .map(|category| Entry {
                version: 0,
                category,
            })
            .collect();
        for layer in [
            stored_all(db, None)?,
            stored_all(db, Some(self.project_id))?,
        ] {
            for entry in layer {
                match all
                    .iter_mut()
                    .find(|e| e.category.name.eq_ignore_ascii_case(&entry.category.name))
                {
                    Some(slot) => *slot = entry,
                    None => all.push(entry),
                }
            }
        }
        Ok(all)
    }

    /// The category `name` resolves to for this project.
    pub fn category(&self, name: &str) -> Result<Option<Entry>> {
        resolve(self.db.connection(), self.project_id, name)
    }

    /// The definition of `name` in one scope only, without falling through.
    pub fn category_in(&self, scope: Scope, name: &str) -> Result<Option<Entry>> {
        match scope {
            Scope::BuiltIn => Ok(built_in(name)),
            Scope::User => stored(self.db.connection(), None, name),
            Scope::Project => stored(self.db.connection(), Some(self.project_id), name),
        }
    }

    /// Creates the category in `scope`, or replaces the one of that name there.
    /// `expected` is the version the caller read; a stale one is refused.
    pub fn save_category(
        &mut self,
        scope: Scope,
        expected: Option<i64>,
        category: &Category,
    ) -> Result<Entry> {
        require(
            scope != Scope::BuiltIn,
            "Built-in categories are fixed; override one in the user or project scope",
        )?;
        category.validate()?;
        let project = (scope == Scope::Project).then_some(self.project_id);
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let now = now_ms();
        let existing = stored(&tx, project, &category.name)?;
        match existing {
            Some(found) => {
                if let Some(version) = expected {
                    require(
                        version == found.version,
                        &format!(
                            "Category {} is version {}, not {version}; read it again before retrying",
                            found.category.name, found.version
                        ),
                    )?;
                }
                let t = &category.templates;
                tx.execute(
                    "UPDATE work_categories SET description=?3, run_checks=?4, execution_template=?5, review_template=?6, validation_template=?7, remediation_template=?8, row_version=row_version+1, updated_at_ms=?9 WHERE name=?2 AND ((?1 IS NULL AND project_id IS NULL) OR project_id=?1)",
                    params![project, found.category.name, category.description.trim(), category.checks, t.execution, t.review, t.validation, t.remediation, now],
                )?;
            }
            None => {
                require(
                    expected.is_none(),
                    &format!(
                        "Category {} no longer exists in the {} scope",
                        category.name,
                        scope.label()
                    ),
                )?;
                let t = &category.templates;
                tx.execute(
                    "INSERT INTO work_categories(project_id, name, description, run_checks, execution_template, review_template, validation_template, remediation_template, created_at_ms, updated_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?9)",
                    params![project, category.name, category.description.trim(), category.checks, t.execution, t.review, t.validation, t.remediation, now],
                )?;
            }
        }
        let saved = stored(&tx, project, &category.name)?.expect("saved category exists");
        tx.commit()?;
        Ok(saved)
    }

    /// Removes a user or project definition so the name inherits again. Tasks that pinned
    /// it keep their copy.
    pub fn delete_category(
        &mut self,
        scope: Scope,
        name: &str,
        expected: Option<i64>,
    ) -> Result<()> {
        require(
            scope != Scope::BuiltIn,
            "Built-in categories cannot be deleted",
        )?;
        let project = (scope == Scope::Project).then_some(self.project_id);
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = stored(&tx, project, name)?.ok_or_else(|| {
            Error::Conflict(format!("No {} category named {name}", scope.label()))
        })?;
        if let Some(version) = expected {
            require(
                version == found.version,
                &format!(
                    "Category {} is version {}, not {version}; read it again before retrying",
                    found.category.name, found.version
                ),
            )?;
        }
        tx.execute(
            "DELETE FROM work_categories WHERE name=?2 AND ((?1 IS NULL AND project_id IS NULL) OR project_id=?1)",
            params![project, found.category.name],
        )?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_ins_choose_checks_explicitly() {
        let checks: Vec<_> = builtin::all()
            .iter()
            .map(|c| (c.name.clone(), c.checks))
            .collect();
        assert_eq!(
            checks,
            [
                ("code".into(), true),
                ("documents".into(), false),
                ("art".into(), false)
            ]
        );
        for category in builtin::all() {
            category.validate().expect("built-in templates are valid");
        }
    }

    #[test]
    fn names_are_checked() {
        for bad in [
            "",
            " x",
            "x ",
            "a/b",
            "Default",
            "NONE",
            "-x",
            &"x".repeat(65),
        ] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        for good in ["code", "Concept art", "ui-icons_2.0"] {
            assert!(valid_name(good).is_ok(), "{good:?}");
        }
    }
}
