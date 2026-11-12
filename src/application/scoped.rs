//! Definitions kept in three layers, shared by work categories and review lenses: a
//! project's own row overrides the user-scope row, which overrides the compiled-in
//! built-ins, all by case-insensitive name. One place owns the lookup per layer, the
//! override order, the merged list and the version-guarded save and delete transactions;
//! each definition kind supplies only its table, row reader and entity-specific SQL.
use super::categories::Scope;
use super::*;
use rusqlite::{Connection, Row};

/// A definition that carries its name and the stored row version (0 for a built-in).
pub(crate) trait Layered {
    fn name(&self) -> &str;
    fn version(&self) -> i64;
}

/// Where one kind of layered definition lives and how it is read.
pub(super) struct Table<E: 'static> {
    pub name: &'static str,
    pub columns: &'static str,
    /// Capitalised singular for messages, such as "Category".
    pub subject: &'static str,
    /// Lower-case plural for messages, such as "categories".
    pub plural: &'static str,
    pub read: fn(Scope, &Row<'_>) -> rusqlite::Result<E>,
    pub built_in: fn(&str) -> Option<E>,
    pub built_ins: fn() -> Vec<E>,
}

/// The scope a project filter selects and its SQL condition: `?1` is the project id.
fn layer(project: Option<i64>) -> (Scope, &'static str) {
    match project {
        Some(_) => (Scope::Project, "project_id = ?1"),
        None => (Scope::User, "project_id IS NULL AND ?1 IS NULL"),
    }
}

impl<E: Layered + 'static> Table<E> {
    /// The row of `name` in one layer (`None` is the user scope), without falling through.
    pub fn stored(&self, db: &Connection, project: Option<i64>, name: &str) -> Result<Option<E>> {
        let (scope, filter) = layer(project);
        Ok(db
            .query_row(
                &format!(
                    "SELECT {} FROM {} WHERE {filter} AND name = ?2",
                    self.columns, self.name
                ),
                params![project, name.trim()],
                |row| (self.read)(scope, row),
            )
            .optional()?)
    }

    fn stored_all(&self, db: &Connection, project: Option<i64>) -> Result<Vec<E>> {
        let (scope, filter) = layer(project);
        let mut statement = db.prepare(&format!(
            "SELECT {} FROM {} WHERE {filter} ORDER BY name",
            self.columns, self.name
        ))?;
        Ok(statement
            .query_map([project], |row| (self.read)(scope, row))?
            .collect::<rusqlite::Result<_>>()?)
    }

    /// `name` for `project`: its override, the user scope, a built-in.
    pub fn resolve(&self, db: &Connection, project: i64, name: &str) -> Result<Option<E>> {
        for layer in [Some(project), None] {
            if let Some(found) = self.stored(db, layer, name)? {
                return Ok(Some(found));
            }
        }
        Ok((self.built_in)(name))
    }

    /// Every definition `project` can choose, by name, each from the layer that wins.
    pub fn list(&self, db: &Connection, project: i64) -> Result<Vec<E>> {
        let mut all = (self.built_ins)();
        for layer in [None, Some(project)] {
            for entry in self.stored_all(db, layer)? {
                match all
                    .iter_mut()
                    .find(|e| e.name().eq_ignore_ascii_case(entry.name()))
                {
                    Some(slot) => *slot = entry,
                    None => all.push(entry),
                }
            }
        }
        Ok(all)
    }

    /// The definition in one scope only, without falling through.
    pub fn in_scope(
        &self,
        db: &Connection,
        project: i64,
        scope: Scope,
        name: &str,
    ) -> Result<Option<E>> {
        match scope {
            Scope::BuiltIn => Ok((self.built_in)(name)),
            Scope::User => self.stored(db, None, name),
            Scope::Project => self.stored(db, Some(project), name),
        }
    }

    fn project_of(scope: Scope, project: i64) -> Option<i64> {
        (scope == Scope::Project).then_some(project)
    }

    fn stale(&self, found: &E, expected: Option<i64>) -> Result<()> {
        match expected {
            Some(version) if version != found.version() => Err(Error::Conflict(format!(
                "{} {} is version {}, not {version}; read it again before retrying",
                self.subject,
                found.name(),
                found.version()
            ))),
            _ => Ok(()),
        }
    }

    /// Creates or replaces `name` in `scope` in one immediate transaction. `expected` is
    /// the version the caller read: a stale one, or one for a row that is gone, is refused.
    /// `write` receives the transaction, the project filter, the existing row if any and
    /// the time, and performs the entity's UPDATE or INSERT.
    pub fn save(
        &self,
        db: &mut Connection,
        project_id: i64,
        scope: Scope,
        name: &str,
        expected: Option<i64>,
        write: impl FnOnce(&Transaction<'_>, Option<i64>, Option<&E>, i64) -> Result<()>,
    ) -> Result<E> {
        require(
            scope != Scope::BuiltIn,
            &format!(
                "Built-in {} are fixed; override one in the user or project scope",
                self.plural
            ),
        )?;
        let project = Self::project_of(scope, project_id);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = self.stored(&tx, project, name)?;
        match &existing {
            Some(found) => self.stale(found, expected)?,
            None => require(
                expected.is_none(),
                &format!(
                    "{} {name} no longer exists in the {} scope",
                    self.subject,
                    scope.label()
                ),
            )?,
        }
        write(&tx, project, existing.as_ref(), now_ms())?;
        let saved = self
            .stored(&tx, project, name)?
            .expect("a saved definition exists");
        tx.commit()?;
        Ok(saved)
    }

    /// Removes the user or project row of `name` so the name inherits again.
    pub fn delete(
        &self,
        db: &mut Connection,
        project_id: i64,
        scope: Scope,
        name: &str,
        expected: Option<i64>,
    ) -> Result<()> {
        require(
            scope != Scope::BuiltIn,
            &format!("Built-in {} cannot be deleted", self.plural),
        )?;
        let project = Self::project_of(scope, project_id);
        let tx = db.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let found = self.stored(&tx, project, name)?.ok_or_else(|| {
            Error::Conflict(format!(
                "No {} {} named {name}",
                scope.label(),
                self.subject.to_lowercase()
            ))
        })?;
        self.stale(&found, expected)?;
        tx.execute(
            &format!(
                "DELETE FROM {} WHERE name=?2 AND ((?1 IS NULL AND project_id IS NULL) OR project_id=?1)",
                self.name
            ),
            params![project, found.name()],
        )?;
        tx.commit()?;
        Ok(())
    }
}
