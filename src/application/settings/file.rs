//! The settings file: one readable, versioned JSON document. Every setting in it is optional,
//! so a file that names only some of them leaves the rest as they are.
use crate::{
    application::{
        ModelConfig,
        categories::Templates,
        configuration::MODEL_ROLES,
        preferences::{Continuation, PlannerApproval},
        require,
    },
    storage::{Error, Result},
};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub const FORMAT: &str = "ktask-settings";
/// The format this build writes and the newest it reads.
pub const VERSION: u64 = 1;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsFile {
    pub format: String,
    pub version: u64,
    /// The folder the file was written for. Informational; import never uses it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_folder: Option<String>,
    #[serde(default)]
    pub project: Project,
    #[serde(default)]
    pub shared: Shared,
    /// Tool bindings the source project used. Registrations belong to a computer and are
    /// never copied; import only reports which of these are not bound.
    #[serde(default)]
    pub tools: Vec<ToolNeed>,
}

/// Settings of the one project.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Project {
    /// In the order they run.
    pub checks: Vec<Check>,
    pub models: Models,
    /// The selected profile; null selects none.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub profile: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_network: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_before_task: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub automatic_provisioning: Option<bool>,
    pub preferences: PreferenceValues,
    pub categories: Vec<Category>,
    pub lenses: Vec<Lens>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lens_defaults: Option<Vec<String>>,
    pub delivery: Delivery,
    pub retention: Retention,
    pub report_limits: Limits,
    pub fix_limits: Limits,
}

/// Settings every project of an instance shares that this project can use.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Shared {
    pub user_defaults: UserValues,
    pub models: Models,
    pub profiles: Vec<Profile>,
    pub categories: Vec<Category>,
    pub lenses: Vec<Lens>,
    pub build_cache: BuildCache,
}

/// Models by role.
pub type Models = BTreeMap<String, Model>;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub provider: String,
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

impl From<ModelConfig> for Model {
    fn from(value: ModelConfig) -> Self {
        Self {
            provider: value.provider,
            model: value.model,
            effort: value.effort,
        }
    }
}

impl From<&Model> for ModelConfig {
    fn from(value: &Model) -> Self {
        Self {
            provider: value.provider.clone(),
            model: value.model.clone(),
            effort: value.effort.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub command: String,
    /// Relative to the project root.
    #[serde(default = "project_root")]
    pub directory: String,
    pub timeout_ms: i64,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn project_root() -> String {
    ".".into()
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub run_checks: bool,
    pub templates: Templates,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lens {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub instructions: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub name: String,
    pub models: Models,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PreferenceValues {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mouse: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_mode: Option<Continuation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner_approval: Option<PlannerApproval>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Delivery {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_branch: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Retention {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_files: Option<i64>,
}

/// Time and cost of one agent report or fix.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_limit_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_limit_microusd: Option<i64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UserValues {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_network: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_before_task: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_timeout_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_mode: Option<Continuation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub planner_approval: Option<PlannerApproval>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<String>,
    /// The profile new projects select; null selects none.
    #[serde(deserialize_with = "present", skip_serializing_if = "Option::is_none")]
    pub default_profile: Option<Option<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BuildCache {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_files: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keep_generations: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_free_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolNeed {
    pub name: String,
    pub version: String,
}

/// A field that is present, even as null, differs from one the file does not name.
fn present<'de, D: Deserializer<'de>>(
    value: D,
) -> std::result::Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(value).map(Some)
}

/// What changing a setting touches in execution that may be going on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Effect {
    /// Nothing live: tasks pin it, or only the screens and later commands read it.
    Later,
    /// The runner reads it while it works.
    Runner,
    /// A project check, matched by its exact name and fixed while an attempt is live.
    Check,
    /// Delivery, fixed while an attempt or queue session is live.
    Delivery,
}

/// One setting as the file names it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Entry {
    pub setting: String,
    pub value: Value,
    #[serde(skip)]
    pub(super) effect: Effect,
}

impl Entry {
    /// Whether `other` is the same setting. Names of categories, lenses and profiles are
    /// unique without regard to case; check names are not.
    pub(super) fn same_setting(&self, other: &Self) -> bool {
        if self.effect == Effect::Check {
            self.setting == other.setting
        } else {
            self.setting.eq_ignore_ascii_case(&other.setting)
        }
    }
}

#[derive(Default)]
struct Entries(Vec<Entry>);

impl Entries {
    fn add(&mut self, setting: String, value: &impl Serialize, effect: Effect) {
        self.0.push(Entry {
            setting,
            value: json(value),
            effect,
        });
    }
    fn optional(&mut self, setting: &str, value: &Option<impl Serialize>, effect: Effect) {
        if let Some(value) = value {
            self.add(setting.into(), value, effect);
        }
    }
    /// One setting per field the group names.
    fn group(&mut self, prefix: &str, group: &impl Serialize, effect: Effect) {
        if let Value::Object(fields) = json(group) {
            for (field, value) in fields {
                self.add(format!("{prefix} {field}"), &value, effect);
            }
        }
    }
}

fn json(value: &impl Serialize) -> Value {
    serde_json::to_value(value).expect("settings are JSON values")
}

impl Shared {
    fn add_to(&self, all: &mut Entries) {
        for profile in &self.profiles {
            for (role, model) in &profile.models {
                let setting = format!("shared profile {} {role}", profile.name);
                all.add(setting, model, Effect::Later);
            }
        }
        for category in &self.categories {
            let setting = format!("shared category {}", category.name);
            all.add(setting, category, Effect::Later);
        }
        for lens in &self.lenses {
            all.add(format!("shared lens {}", lens.name), lens, Effect::Later);
        }
        for (role, model) in &self.models {
            all.add(format!("shared model {role}"), model, Effect::Later);
        }
        all.group("shared user default", &self.user_defaults, Effect::Later);
        all.group("shared build cache", &self.build_cache, Effect::Later);
    }
}

impl Project {
    fn add_to(&self, all: &mut Entries) {
        use Effect::{Check, Delivery, Later, Runner};
        for (role, model) in &self.models {
            all.add(format!("model {role}"), model, Later);
        }
        all.optional("selected profile", &self.profile, Later);
        all.optional("agent network", &self.agent_network, Runner);
        all.optional("check before task", &self.check_before_task, Runner);
        let provisioning = &self.automatic_provisioning;
        all.optional("automatic provisioning", provisioning, Runner);
        let preferences = &self.preferences;
        all.optional("preferences mouse", &preferences.mouse, Later);
        all.optional("preferences run_mode", &preferences.run_mode, Runner);
        let approval = &preferences.planner_approval;
        all.optional("preferences planner_approval", approval, Runner);
        for category in &self.categories {
            all.add(format!("category {}", category.name), category, Later);
        }
        for lens in &self.lenses {
            all.add(format!("lens {}", lens.name), lens, Later);
        }
        all.optional("default lenses", &self.lens_defaults, Later);
        for check in &self.checks {
            all.add(format!("check {}", check.name), check, Check);
        }
        all.group("delivery", &self.delivery, Delivery);
        all.group("retention", &self.retention, Runner);
        all.group("report limits", &self.report_limits, Later);
        all.group("fix limits", &self.fix_limits, Later);
    }
}

impl SettingsFile {
    pub(crate) fn new() -> Self {
        Self {
            format: FORMAT.into(),
            version: VERSION,
            ..Self::default()
        }
    }

    /// Every setting the file names, in the order import applies them.
    pub fn entries(&self) -> Vec<Entry> {
        let mut all = Entries::default();
        self.shared.add_to(&mut all);
        self.project.add_to(&mut all);
        all.0
    }

    pub fn encode(&self) -> String {
        serde_json::to_string_pretty(self).expect("settings are JSON values") + "\n"
    }

    /// Reads a file of this or an older format and checks its structure; the values are
    /// validated where they are applied.
    pub fn decode(data: &[u8]) -> Result<Self> {
        let malformed = |why: String| Error::Conflict(format!("Malformed settings file: {why}"));
        let value: Value = serde_json::from_slice(data).map_err(|e| malformed(e.to_string()))?;
        require(
            value["format"] == FORMAT,
            "Malformed settings file: not a ktask-settings file",
        )?;
        let version = value["version"]
            .as_u64()
            .filter(|version| *version > 0)
            .ok_or_else(|| malformed("it has no format version".into()))?;
        require(
            version <= VERSION,
            &format!(
                "Settings file format {version} is newer than this ktask reads ({VERSION}); upgrade ktask to import it"
            ),
        )?;
        let mut file: Self = serde_json::from_value(value).map_err(|e| malformed(e.to_string()))?;
        file.normalize()?;
        Ok(file)
    }

    /// Spells values as they are stored, and refuses names given twice and unknown roles.
    fn normalize(&mut self) -> Result<()> {
        unique(
            "check",
            self.project.checks.iter().map(|c| c.name.to_owned()),
        )?;
        for profile in &mut self.shared.profiles {
            profile.name = profile.name.trim().to_owned();
            roles(&profile.models)?;
        }
        unique(
            "profile",
            self.shared.profiles.iter().map(|p| p.name.to_lowercase()),
        )?;
        roles(&self.project.models)?;
        roles(&self.shared.models)?;
        for categories in [&mut self.project.categories, &mut self.shared.categories] {
            for category in categories.iter_mut() {
                category.description = category.description.trim().to_owned();
            }
            unique("category", categories.iter().map(|c| c.name.to_lowercase()))?;
        }
        for lenses in [&mut self.project.lenses, &mut self.shared.lenses] {
            for lens in lenses.iter_mut() {
                lens.description = lens.description.trim().to_owned();
            }
            unique("lens", lenses.iter().map(|l| l.name.to_lowercase()))?;
        }
        Ok(())
    }
}

fn unique(kind: &str, names: impl Iterator<Item = String>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        require(
            seen.insert(name.clone()),
            &format!("Malformed settings file: {kind} {name} is named twice"),
        )?;
    }
    Ok(())
}

fn roles(models: &Models) -> Result<()> {
    for role in models.keys() {
        require(
            MODEL_ROLES.contains(&role.as_str()),
            &format!("Malformed settings file: unknown model role {role}"),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(text: &str) -> String {
        SettingsFile::decode(text.as_bytes())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn newer_foreign_and_malformed_files_are_refused_with_the_reason() {
        for (text, reason) in [
            ("not json", "Malformed settings file: expected"),
            (r#"{"format":"other","version":1}"#, "not a ktask-settings"),
            (r#"{"format":"ktask-settings"}"#, "no format version"),
            (
                r#"{"format":"ktask-settings","version":2,"new_section":{}}"#,
                "format 2 is newer than this ktask reads (1); upgrade ktask",
            ),
            (
                r#"{"format":"ktask-settings","version":1,"project":{"chekcs":[]}}"#,
                "unknown field `chekcs`",
            ),
            (
                r#"{"format":"ktask-settings","version":1,"project":{"models":{"coder":{"provider":"codex","model":"m"}}}}"#,
                "unknown model role coder",
            ),
            (
                r#"{"format":"ktask-settings","version":1,"shared":{"lenses":[{"name":"A","instructions":"x"},{"name":"a","instructions":"y"}]}}"#,
                "lens a is named twice",
            ),
        ] {
            let message = refused(text);
            assert!(message.contains(reason), "{text}: {message}");
        }
    }

    #[test]
    fn a_null_selection_is_a_setting_and_an_absent_one_is_not() {
        let named = SettingsFile::decode(
            br#"{"format":"ktask-settings","version":1,"project":{"profile":null}}"#,
        )
        .unwrap();
        assert_eq!(named.project.profile, Some(None));
        assert_eq!(named.entries().len(), 1);
        let absent = SettingsFile::decode(br#"{"format":"ktask-settings","version":1}"#).unwrap();
        assert_eq!(absent.project.profile, None);
        assert!(absent.entries().is_empty());
        assert_eq!(
            SettingsFile::decode(named.encode().as_bytes()).unwrap(),
            named
        );
    }
}
