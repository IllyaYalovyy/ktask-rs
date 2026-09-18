//! The settings one run obeys, and the values they take when nobody sets them.
//!
//! One field per key in the *Configuration defaults* section of `docs/DESIGN.md`,
//! spelled exactly as the TOML key is spelled, and nothing else: the effective
//! configuration an operator reads on the Configuration screen is this
//! struct, so a setting that is not here does not exist.
//!
//! Defaults live in one place — [`Default`] — and `#[serde(default)]` makes the
//! deserializer use it, so a value cannot be documented in one place and
//! actually applied in another. A document therefore overrides only what it
//! sets, and an empty document is exactly [`Config::default()`].
//!
//! An unknown key is an error rather than something to ignore: a setting that
//! is misspelled, or left over from another version, would otherwise be
//! silently unwritten while the operator believed it was in effect.
//!
//! ## Where a value comes from
//!
//! [`load`] merges four layers in a fixed order: the defaults this module
//! holds, the global document, the project document, the environment. A layer
//! overrides only the keys it sets, so a project that pins one setting keeps
//! everything else the machine was configured with. Every key then reports the
//! layer that won it ([`Config::provenance`]), because an operator reading a
//! value needs to know why it is that value: a setting edited in the wrong file
//! is otherwise indistinguishable from one that never took effect.
//!
//! [`load`] is handed its paths, which is what makes every layer testable
//! without a home directory to write into; [`load_for`] is the entry point that
//! knows them — the machine's document where `crate::paths` puts it, the
//! project's own below its state directory, and the process environment.
//!
//! A configuration file that is not there is not a layer. A repository that
//! never wrote one, or a home directory with no global settings, is the
//! ordinary case and loads the layers below it. A file that is there and cannot
//! be read, or that is not valid TOML, is refused by name — settings an
//! operator wrote are about to be ignored, and silence would let them believe
//! those settings applied.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::paths::{config_file_with, process_env};
use crate::project::{Project, project_config_path};
use crate::{Error, Result};

/// The effective configuration: every setting, with the documented default.
///
/// Reading a TOML document into this type applies the file over
/// [`Config::default()`], so a document holding only the settings an operator
/// cares about leaves the rest at the values below. A key that is not a field
/// here is rejected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Which provider runs an attempt: `"dummy"`, `"claude"` or `"codex"`.
    ///
    /// A name rather than an enum because the provider set is data-driven
    /// (`provider/<name>.rs`) and a run reports the provider it used as text.
    pub provider: String,
    /// The model to ask that provider for, or `None` to take the provider's own
    /// default. Phases may override it; this is the run-wide answer.
    pub model: Option<String>,
    /// How long one attempt may run before it is killed: 4 hours, because an
    /// attempt is an agent working, not a command finishing.
    pub attempt_timeout_secs: u64,
    /// How long one gate command may run: 30 minutes, because a cold Rust build
    /// is slow and a gate that times out early fails a task that was fine.
    pub gate_timeout_secs: u64,
    /// How long an attempt may emit nothing before it is killed: 30 minutes.
    /// Measured against output rather than wall time so a quiet, long build is
    /// not confused with an agent that has stopped answering.
    pub idle_timeout_secs: u64,
    /// How many attempts a task gets in total, counted the way [`crate::AttemptId`]
    /// counts them. Two: one normal, one remediation.
    pub max_attempts: u32,
    /// How many of those attempts may be remediations of an earlier failure.
    pub max_remediation_attempts: u32,
    /// How many failures with an identical signature trip the circuit breaker
    /// and pause the queue rather than spending another attempt on them.
    pub circuit_breaker_threshold: u32,
    /// The remote mainline is fetched from and pushed to.
    pub mainline_remote: String,
    /// The branch mainline runs on. Publication compares the candidate against
    /// this branch's remote SHA before a task is called verified.
    pub mainline_branch: String,
    /// How many bytes of context an attempt is given at most. The supervisor
    /// truncates to this budget rather than letting a provider decide how much
    /// of the journal to read.
    pub context_budget_bytes: usize,
    /// How large a failure bundle may be: classification, gate output and diff
    /// summary are cut to this so a remediation prompt stays affordable.
    pub failure_bundle_bytes: usize,
    /// How many lines of agent output the ring for one subscriber holds before
    /// the oldest line is dropped. Unbounded output would otherwise be an
    /// out-of-memory crash in the middle of a run.
    pub output_ring_lines: usize,
    /// How much longer than a reported provider limit the supervisor waits
    /// before it tries again, so a limit that resets a second early does not
    /// cause a second rejection.
    pub limit_wait_margin_secs: u64,
    /// The longest wait a provider limit may impose before the run gives up and
    /// reports a limit rather than appearing to hang.
    pub limit_max_wait_secs: u64,
    /// The work protocol a task gets when its own front matter names none.
    pub default_protocol: String,
    /// A scripted scenario for the `dummy` provider, or `None` to use its
    /// built-in behavior. A path rather than a URL: it is a file on this
    /// machine, and tests point it at one.
    pub dummy_scenario_path: Option<PathBuf>,
    /// Which paths count as tests, matched against the diff of an attempt. The
    /// `tdd` protocol's red phase may write only here.
    pub test_globs: Vec<String>,
    /// Extra patterns whose matches are redacted out of stored output. Added to
    /// the built-in set, never replacing it.
    pub secret_patterns: Vec<String>,
    /// The command that proves the project was green before the task started,
    /// or `None` to run no baseline gate.
    ///
    /// A command is the words to execute, kept split, because quoting and
    /// splitting a command line is a decision the configuration has to have made
    /// already — see [`crate::Gate`]. Every gate command in this struct is
    /// spelled the same way, and `Config` is the only place a gate command is
    /// written down: the profile the runner executes is built from these fields
    /// by `profile_from`.
    pub baseline_command: Option<Vec<String>>,
    /// The fast check of an edit loop — the tests the change touches, run while
    /// the agent is still working — or `None` to run no targeted gate.
    pub targeted_test_command: Option<Vec<String>>,
    /// The complete local suite, and the one gate command that is not optional:
    /// `profile_from` refuses a configuration that leaves this unset,
    /// because a task is never called done on an agent's say-so (VISION.md §8).
    pub verify_command: Option<Vec<String>>,
    /// The lints, run by the runner rather than trusted from a report, or `None`
    /// to run no lint gate.
    pub lint_command: Option<Vec<String>>,
    /// The formatting check, or `None` to run no format gate.
    pub format_command: Option<Vec<String>>,
    /// The build, including every target, or `None` to run no build gate.
    pub build_command: Option<Vec<String>>,
    /// The privacy scan of staged files, tracked files and the outgoing commit
    /// range, or `None` to run no privacy gate.
    pub privacy_command: Option<Vec<String>>,
    /// The affected tests run repeatedly, or `None` to run no flake gate.
    /// `flake_runs` says how many times it runs when it is configured.
    pub flake_command: Option<Vec<String>>,
    /// How many times the affected tests run in the flake gate. Zero would
    /// disable it, so the default is five.
    pub flake_runs: u32,
    /// How many days of journal and artifacts retention keeps, after which
    /// records are pruned. The journal itself is append-only and never edited.
    pub retention_days: u32,
    /// How many bytes must be free on the state filesystem before a run will
    /// start: 2 GiB, because a journal that fills the disk mid-run loses the
    /// evidence the run exists to produce.
    pub min_free_disk_bytes: u64,
    /// Which layer supplied each key's value, as recorded by [`load`].
    ///
    /// Kept out of a written document (`#[serde(skip)]`): provenance is a fact
    /// about how a value arrived in this process, and a file cannot set it — a
    /// reader would meet it as an undocumented key. A `Config` built some other
    /// way records nothing, and a key with nothing recorded reports
    /// [`Source::Default`], which is the honest answer for a value nobody
    /// traced back to a layer.
    #[serde(skip)]
    sources: BTreeMap<String, Source>,
}

impl Default for Config {
    /// The defaults `docs/DESIGN.md` documents, which are also the values an
    /// empty configuration document deserializes to.
    ///
    /// Provenance is recorded as empty: the defaults are the layer every other
    /// layer overrides, so there is nothing below them to have come from.
    fn default() -> Self {
        Self {
            provider: "dummy".to_owned(),
            model: None,
            attempt_timeout_secs: 14_400,
            gate_timeout_secs: 1_800,
            idle_timeout_secs: 1_800,
            max_attempts: 2,
            max_remediation_attempts: 1,
            circuit_breaker_threshold: 3,
            mainline_remote: "origin".to_owned(),
            mainline_branch: "main".to_owned(),
            context_budget_bytes: 65_536,
            failure_bundle_bytes: 16_384,
            output_ring_lines: 4_096,
            limit_wait_margin_secs: 60,
            limit_max_wait_secs: 86_400,
            default_protocol: "direct".to_owned(),
            dummy_scenario_path: None,
            test_globs: vec![
                "**/tests/**".to_owned(),
                "**/*_test.rs".to_owned(),
                "src/**/tests.rs".to_owned(),
            ],
            secret_patterns: Vec::new(),
            baseline_command: None,
            targeted_test_command: None,
            verify_command: None,
            lint_command: None,
            format_command: None,
            build_command: None,
            privacy_command: None,
            flake_command: None,
            flake_runs: 5,
            retention_days: 90,
            min_free_disk_bytes: 2_147_483_648,
            sources: BTreeMap::new(),
        }
    }
}

impl Config {
    /// Where every documented key's value came from, in the order
    /// `docs/DESIGN.md` lists the keys.
    ///
    /// There is one entry per documented key, always, so the Configuration
    /// screen can render a row per setting without holding a list of its own.
    /// A key that no layer recorded reports [`Source::Default`], which is what
    /// every key of a [`Config`] that was deserialized rather than [`load`]ed
    /// reports: nothing wrote those values from a layer, so the default is all
    /// that is known about them.
    #[must_use]
    pub fn provenance(&self) -> Vec<(String, Source)> {
        KEYS.iter()
            .map(|key| (key.name.to_owned(), self.source_of(key.name)))
            .collect()
    }

    /// The layer recorded for one key, or [`Source::Default`] when none was.
    fn source_of(&self, key: &str) -> Source {
        self.sources.get(key).copied().unwrap_or(Source::Default)
    }

    /// Records the layer a key's value was written from.
    fn set_source(&mut self, key: &str, source: Source) {
        self.sources.insert(key.to_owned(), source);
    }
}

/// The layer a configuration value came from.
///
/// The variants are declared in ascending order of precedence, which is the
/// order [`load`] applies the layers in: a later one overrides an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// Nobody set the key, so it holds the value `docs/DESIGN.md` documents.
    Default,
    /// The user-wide document, which is whoever configured the machine saying
    /// what they want.
    GlobalFile,
    /// The repository's own document, which may disagree with the machine it
    /// happens to sit on because the work is the repository's.
    ProjectFile,
    /// A `KTASK_*` environment variable: one run overridden without editing a
    /// file.
    Env,
    /// A command-line flag, which beats every file and the environment. The one
    /// layer [`load`] never reports, because only an argument parser knows a
    /// flag was passed: recording it is the CLI's to do.
    Flag,
}

impl fmt::Display for Source {
    /// The layer as the Configuration screen names it.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Default => "default",
            Self::GlobalFile => "global file",
            Self::ProjectFile => "project file",
            Self::Env => "environment",
            Self::Flag => "flag",
        };
        f.write_str(label)
    }
}

/// A value and the layer that supplied it.
///
/// This is what the loader carries between reading one layer and writing it
/// into a [`Config`], so a value never travels beyond the place that read it
/// without saying where it came from.
#[derive(Debug, Clone)]
pub struct Resolved<T> {
    /// The value that won.
    pub value: T,
    /// The layer that supplied it.
    pub source: Source,
}

/// The configuration the documented layers add up to, every key tagged with the
/// layer that set it.
///
/// The layers are applied lowest precedence first: [`Source::Default`] (the
/// values [`Config::default`] holds), then the `global` document, then the
/// `project` document, then the environment. A layer overrides only the keys it
/// actually sets, so a project that pins one setting keeps what the global
/// document said about all the others. [`Config::provenance`] then reports who
/// won each key.
///
/// `env` is handed in rather than read from the process because a test has to
/// hold a set of variables without touching process state, which
/// `docs/DESIGN.md` Conventions requires; a caller that wants the real
/// environment passes an accessor over `std::env::var`. A variable whose value
/// is empty or all whitespace carries no setting — an empty value means unset,
/// exactly as it does for the XDG directories.
///
/// # Errors
///
/// [`Error::Config`] when a document is not valid TOML (naming the file), when
/// a document or a variable sets a key that is not documented (naming the key
/// and where it was set), or when a value cannot be read as the setting that
/// bears its name (naming the setting and its origin). [`Error::Io`] when a
/// document is there but cannot be read. A path that is simply not there is
/// none of these: it is a layer that does not exist.
pub fn load(
    global: Option<&Path>,
    project: Option<&Path>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config> {
    let mut config = Config::default();
    for (path, layer) in [(global, Source::GlobalFile), (project, Source::ProjectFile)] {
        let Some(path) = path else {
            continue;
        };
        if let Some(document) = read_document(path)? {
            apply_document(&mut config, path, layer, &document)?;
        }
    }
    apply_environment(&mut config, env)?;
    Ok(config)
}

/// The configuration one registered project runs on, read from where its files
/// actually are.
///
/// [`load`] takes its paths as arguments, which is what makes it testable; this
/// is the entry point that knows where those files are. It resolves the three
/// layers above the defaults — the machine's document at
/// [`crate::paths::config_file`], the project's own at
/// [`crate::project_config_path`], and the process environment — and everything
/// [`load`] then decides stands: a key keeps the value the layer below gave it
/// unless a layer above it wrote the key, and [`Config::provenance`] says who
/// won each one afterwards.
///
/// Neither document has to exist. A repository that never wrote one and a home
/// directory with no global settings are the ordinary case, and the answer is
/// [`Config::default()`] with nothing overridden — not a failure to report.
///
/// # Errors
///
/// [`Error::Config`] when the global path cannot be resolved because neither
/// `XDG_CONFIG_HOME` nor `HOME` names a base directory, or when a document or a
/// variable holds a value its setting cannot be; [`Error::Io`] when a document
/// is there and cannot be read.
pub fn load_for(project: &Project) -> Result<Config> {
    load_for_with(&process_env, project)
}

/// [`load_for`] with the environment supplied by the caller, which is how a test
/// holds the variables that name both documents without touching process state
/// (`docs/DESIGN.md` Conventions).
fn load_for_with(env: &dyn Fn(&str) -> Option<String>, project: &Project) -> Result<Config> {
    let global = config_file_with(env)?;
    load(Some(&global), Some(&project_config_path(project)), env)
}

/// A configuration document as read from a file: keys in the order the reader
/// keeps them, values still untyped.
///
/// A document is read as a map rather than straight into [`Config`] because the
/// loader has to know which layer each value arrived from, and deserializing a
/// whole struct at once loses the keys it did not contain.
type Document = BTreeMap<String, toml::Value>;

/// The document at `path`, or `None` when nothing is there.
///
/// Absence is the ordinary case and costs the configuration one layer. A file
/// that is present and cannot be read for any other reason is reported, because
/// it means settings somebody wrote are about to be ignored.
///
/// # Errors
///
/// [`Error::Io`] for a read that failed for a reason other than absence,
/// [`Error::Config`] for a file whose contents are not a TOML document.
fn read_document(path: &Path) -> Result<Option<Document>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match toml::from_str::<Document>(&text) {
        Ok(document) => Ok(Some(document)),
        Err(error) => Err(Error::Config {
            key: path.display().to_string(),
            detail: format!("not a valid TOML document: {error}"),
        }),
    }
}

/// Writes every key one document holds into `config`, each tagged with `layer`.
///
/// # Errors
///
/// [`Error::Config`] for a key that is not documented, or for a value the
/// setting it names cannot hold.
fn apply_document(
    config: &mut Config,
    path: &Path,
    layer: Source,
    document: &Document,
) -> Result<()> {
    let origin = Origin::File(path);
    for (name, value) in document {
        let key = documented(name).ok_or_else(|| Error::Config {
            key: name.clone(),
            detail: format!("{origin} sets a key that is not documented"),
        })?;
        (key.write)(config, key, Raw::Document(value, origin), layer)?;
    }
    Ok(())
}

/// Writes every key the environment sets into `config`, above both documents.
///
/// # Errors
///
/// [`Error::Config`] for a variable whose text its setting cannot be read from.
fn apply_environment(config: &mut Config, env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    for key in KEYS {
        let Some(text) = variable(env, key.variable) else {
            continue;
        };
        (key.write)(
            config,
            key,
            Raw::Text(&text, Origin::Variable(key.variable)),
            Source::Env,
        )?;
    }
    Ok(())
}

/// The text one `KTASK_*` variable carries, or `None` when it carries nothing.
///
/// The value is trimmed, because text typed into a shell profile or a service
/// unit picks up whitespace at the ends, and what is left empty counts as
/// absent rather than as the empty string: `KTASK_MODEL=` sets no model, which
/// is what an operator means by writing it.
fn variable(env: &dyn Fn(&str) -> Option<String>, name: &str) -> Option<String> {
    env(name)
        .map(|held| held.trim().to_owned())
        .filter(|held| !held.is_empty())
}

/// One documented setting: the key as it is spelled in a file, the variable that
/// carries the same setting, and how to write a value of it into a [`Config`].
#[derive(Debug, Clone, Copy)]
struct Key {
    /// The key in a document, which is also the name of the field it writes.
    name: &'static str,
    /// The environment variable that overrides the same setting.
    variable: &'static str,
    /// Reads a value into the field this entry is named for.
    write: fn(&mut Config, &Key, Raw<'_>, Source) -> Result<()>,
}

/// The table entry for one documented setting, whose field of the same name
/// gives the value the type it has to be read as.
macro_rules! setting {
    ($name:literal, $variable:literal, $field:ident) => {{
        fn write(config: &mut Config, key: &Key, raw: Raw<'_>, source: Source) -> Result<()> {
            let resolved: Resolved<_> = resolve(key, raw, source)?;
            config.$field = resolved.value;
            config.set_source(key.name, resolved.source);
            Ok(())
        }
        Key {
            name: $name,
            variable: $variable,
            write,
        }
    }};
}

/// Every documented setting, in the order `docs/DESIGN.md` lists them, which is
/// also the order the Configuration screen renders them in. A key outside this
/// table is refused rather than skipped, so the list of settings is one list
/// rather than one per layer.
const KEYS: &[Key] = &[
    setting!("provider", "KTASK_PROVIDER", provider),
    setting!("model", "KTASK_MODEL", model),
    setting!(
        "attempt_timeout_secs",
        "KTASK_ATTEMPT_TIMEOUT_SECS",
        attempt_timeout_secs
    ),
    setting!(
        "gate_timeout_secs",
        "KTASK_GATE_TIMEOUT_SECS",
        gate_timeout_secs
    ),
    setting!(
        "idle_timeout_secs",
        "KTASK_IDLE_TIMEOUT_SECS",
        idle_timeout_secs
    ),
    setting!("max_attempts", "KTASK_MAX_ATTEMPTS", max_attempts),
    setting!(
        "max_remediation_attempts",
        "KTASK_MAX_REMEDIATION_ATTEMPTS",
        max_remediation_attempts
    ),
    setting!(
        "circuit_breaker_threshold",
        "KTASK_CIRCUIT_BREAKER_THRESHOLD",
        circuit_breaker_threshold
    ),
    setting!("mainline_remote", "KTASK_MAINLINE_REMOTE", mainline_remote),
    setting!("mainline_branch", "KTASK_MAINLINE_BRANCH", mainline_branch),
    setting!(
        "context_budget_bytes",
        "KTASK_CONTEXT_BUDGET_BYTES",
        context_budget_bytes
    ),
    setting!(
        "failure_bundle_bytes",
        "KTASK_FAILURE_BUNDLE_BYTES",
        failure_bundle_bytes
    ),
    setting!(
        "output_ring_lines",
        "KTASK_OUTPUT_RING_LINES",
        output_ring_lines
    ),
    setting!(
        "limit_wait_margin_secs",
        "KTASK_LIMIT_WAIT_MARGIN_SECS",
        limit_wait_margin_secs
    ),
    setting!(
        "limit_max_wait_secs",
        "KTASK_LIMIT_MAX_WAIT_SECS",
        limit_max_wait_secs
    ),
    setting!(
        "default_protocol",
        "KTASK_DEFAULT_PROTOCOL",
        default_protocol
    ),
    setting!(
        "dummy_scenario_path",
        "KTASK_DUMMY_SCENARIO_PATH",
        dummy_scenario_path
    ),
    setting!("test_globs", "KTASK_TEST_GLOBS", test_globs),
    setting!("secret_patterns", "KTASK_SECRET_PATTERNS", secret_patterns),
    setting!(
        "baseline_command",
        "KTASK_BASELINE_COMMAND",
        baseline_command
    ),
    setting!(
        "targeted_test_command",
        "KTASK_TARGETED_TEST_COMMAND",
        targeted_test_command
    ),
    setting!("verify_command", "KTASK_VERIFY_COMMAND", verify_command),
    setting!("lint_command", "KTASK_LINT_COMMAND", lint_command),
    setting!("format_command", "KTASK_FORMAT_COMMAND", format_command),
    setting!("build_command", "KTASK_BUILD_COMMAND", build_command),
    setting!("privacy_command", "KTASK_PRIVACY_COMMAND", privacy_command),
    setting!("flake_command", "KTASK_FLAKE_COMMAND", flake_command),
    setting!("flake_runs", "KTASK_FLAKE_RUNS", flake_runs),
    setting!("retention_days", "KTASK_RETENTION_DAYS", retention_days),
    setting!(
        "min_free_disk_bytes",
        "KTASK_MIN_FREE_DISK_BYTES",
        min_free_disk_bytes
    ),
];

/// The entry for a documented key, or `None` when a document holds a key nobody
/// defined.
fn documented(name: &str) -> Option<&'static Key> {
    KEYS.iter().find(|key| key.name == name)
}

/// Reads a raw value as the type of the setting that names it, and pairs it with
/// the layer it arrived from.
///
/// # Errors
///
/// [`Error::Config`] naming the setting and where the value came from, when a
/// document holds the key as a type the field cannot be, or when environment
/// text cannot be read as that type.
fn resolve<T>(key: &Key, raw: Raw<'_>, source: Source) -> Result<Resolved<T>>
where
    T: DeserializeOwned + FromEnvText,
{
    let value = match raw {
        Raw::Document(value, origin) => {
            value
                .clone()
                .try_into()
                .map_err(|error: toml::de::Error| Error::Config {
                    key: key.name.to_owned(),
                    detail: format!("{origin} holds a value this setting cannot hold: {error}"),
                })?
        }
        Raw::Text(text, origin) => T::from_text(text).ok_or_else(|| Error::Config {
            key: key.name.to_owned(),
            detail: format!("{origin} is not a `{}`", T::TEXT_TYPE),
        })?,
    };
    Ok(Resolved { value, source })
}

/// Where a raw value was read from, so a refusal can name it instead of
/// paraphrasing around it.
#[derive(Debug, Clone, Copy)]
enum Origin<'src> {
    /// A configuration document on disk.
    File(&'src Path),
    /// An environment variable.
    Variable(&'src str),
}

impl fmt::Display for Origin<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File(path) => write!(f, "file `{}`", path.display()),
            Self::Variable(name) => write!(f, "environment variable `{name}`"),
        }
    }
}

/// A setting's value as one layer holds it, before it has been read into a type.
#[derive(Debug, Clone, Copy)]
enum Raw<'src> {
    /// A value a TOML document already typed.
    Document(&'src toml::Value, Origin<'src>),
    /// Text an environment variable carries.
    Text(&'src str, Origin<'src>),
}

/// How a setting's type is spelled as text, which only the environment needs: a
/// document arrives with its types already there.
trait FromEnvText: Sized {
    /// What this type is called in a message saying that some text was not it.
    const TEXT_TYPE: &'static str;

    /// The value the text describes, or `None` when it describes none.
    fn from_text(text: &str) -> Option<Self>;
}

impl FromEnvText for String {
    const TEXT_TYPE: &'static str = "name";

    fn from_text(text: &str) -> Option<Self> {
        Some(text.to_owned())
    }
}

impl FromEnvText for Option<String> {
    const TEXT_TYPE: &'static str = "name";

    /// A variable that reached this point was set, so the answer is `Some` of
    /// the text; the outer `None` means the text named no setting at all.
    fn from_text(text: &str) -> Option<Self> {
        Some(Some(text.to_owned()))
    }
}

impl FromEnvText for Option<PathBuf> {
    const TEXT_TYPE: &'static str = "path";

    fn from_text(text: &str) -> Option<Self> {
        Some(Some(PathBuf::from(text)))
    }
}

impl FromEnvText for Option<Vec<String>> {
    const TEXT_TYPE: &'static str = "comma-separated list";

    /// One word per comma-separated entry, so `KTASK_BUILD_COMMAND=cargo,build`
    /// is the two-word command `build_command = ["cargo", "build"]` writes in a
    /// document. A gate command is a list like any other here, and a variable
    /// that reached this point was set: `KTASK_LINT_COMMAND=,` is an operator
    /// having configured a gate with no words in it, which
    /// `profile_from` refuses rather than reading as no gate at all.
    fn from_text(text: &str) -> Option<Self> {
        <Vec<String>>::from_text(text).map(Some)
    }
}

impl FromEnvText for u64 {
    const TEXT_TYPE: &'static str = "u64";

    fn from_text(text: &str) -> Option<Self> {
        text.parse().ok()
    }
}

impl FromEnvText for u32 {
    const TEXT_TYPE: &'static str = "u32";

    fn from_text(text: &str) -> Option<Self> {
        text.parse().ok()
    }
}

impl FromEnvText for usize {
    const TEXT_TYPE: &'static str = "usize";

    fn from_text(text: &str) -> Option<Self> {
        text.parse().ok()
    }
}

impl FromEnvText for Vec<String> {
    const TEXT_TYPE: &'static str = "comma-separated list";

    /// Split on commas with each entry trimmed and empty entries dropped, so a
    /// trailing comma is one entry and `KTASK_TEST_GLOBS=,` is an empty list an
    /// operator wrote on purpose rather than an absent setting.
    fn from_text(text: &str) -> Option<Self> {
        Some(
            text.split(',')
                .map(str::trim)
                .filter(|entry| !entry.is_empty())
                .map(str::to_owned)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, Error, Source, load, load_for, load_for_with};
    use crate::project::Project;
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    /// Reads a TOML document as `ktask-rs` would read a configuration file.
    fn parse(document: &str) -> Config {
        toml::from_str(document)
            .unwrap_or_else(|error| panic!("`{document}` is a configuration: {error}"))
    }

    /// Compares two configurations field by field.
    ///
    /// `Config` derives `Debug` and deliberately not `PartialEq`, so the
    /// derived rendering is the honest way to ask whether two of them hold the
    /// same values.
    fn assert_same_config(actual: &Config, expected: &Config) {
        assert_eq!(format!("{actual:?}"), format!("{expected:?}"));
    }

    /// Every value `docs/DESIGN.md` documents as a default, spelled out here
    /// rather than read back from `Config::default()`, so that changing the
    /// implementation is caught rather than agreed with.
    fn assert_documented_defaults(config: &Config) {
        assert_eq!(config.provider, "dummy");
        assert_eq!(config.model, None);
        assert_eq!(config.attempt_timeout_secs, 14_400);
        assert_eq!(config.gate_timeout_secs, 1_800);
        assert_eq!(config.idle_timeout_secs, 1_800);
        assert_eq!(config.max_attempts, 2);
        assert_eq!(config.max_remediation_attempts, 1);
        assert_eq!(config.circuit_breaker_threshold, 3);
        assert_eq!(config.mainline_remote, "origin");
        assert_eq!(config.mainline_branch, "main");
        assert_eq!(config.context_budget_bytes, 65_536);
        assert_eq!(config.failure_bundle_bytes, 16_384);
        assert_eq!(config.output_ring_lines, 4_096);
        assert_eq!(config.limit_wait_margin_secs, 60);
        assert_eq!(config.limit_max_wait_secs, 86_400);
        assert_eq!(config.default_protocol, "direct");
        assert_eq!(config.dummy_scenario_path, None);
        assert_eq!(
            config.test_globs,
            ["**/tests/**", "**/*_test.rs", "src/**/tests.rs"]
        );
        assert!(
            config.secret_patterns.is_empty(),
            "{:?}",
            config.secret_patterns
        );
        assert_eq!(config.flake_runs, 5);
        assert_eq!(config.retention_days, 90);
        assert_eq!(config.min_free_disk_bytes, 2_147_483_648);
        assert_eq!(
            config.baseline_command, None,
            "a project that named no baseline gate runs none"
        );
        assert_eq!(config.targeted_test_command, None);
        assert_eq!(
            config.verify_command, None,
            "no suite is configured until a project writes one, which is why building a \
             profile from it is a refusal rather than a profile that verifies nothing"
        );
        assert_eq!(config.lint_command, None);
        assert_eq!(config.format_command, None);
        assert_eq!(config.build_command, None);
        assert_eq!(config.privacy_command, None);
        assert_eq!(config.flake_command, None);
    }

    /// A configuration with a value in every field, none of them a default.
    fn every_field_set() -> Config {
        Config {
            provider: "claude".to_owned(),
            model: Some("sonnet".to_owned()),
            attempt_timeout_secs: 600,
            gate_timeout_secs: 300,
            idle_timeout_secs: 120,
            max_attempts: 5,
            max_remediation_attempts: 2,
            circuit_breaker_threshold: 7,
            mainline_remote: "upstream".to_owned(),
            mainline_branch: "trunk".to_owned(),
            context_budget_bytes: 1_024,
            failure_bundle_bytes: 512,
            output_ring_lines: 64,
            limit_wait_margin_secs: 5,
            limit_max_wait_secs: 3_600,
            default_protocol: "tdd".to_owned(),
            dummy_scenario_path: Some(PathBuf::from("/state/scenario.json")),
            test_globs: vec!["tests/**".to_owned()],
            secret_patterns: vec!["secret_[a-z0-9]+".to_owned()],
            baseline_command: Some(vec!["cargo".to_owned(), "check".to_owned()]),
            targeted_test_command: Some(vec!["cargo".to_owned(), "nextest".to_owned()]),
            verify_command: Some(vec!["./scripts/quality.sh".to_owned()]),
            lint_command: Some(vec!["cargo".to_owned(), "clippy".to_owned()]),
            format_command: Some(vec!["cargo".to_owned(), "fmt".to_owned()]),
            build_command: Some(vec!["cargo".to_owned(), "build".to_owned()]),
            privacy_command: Some(vec!["ktask-rs".to_owned(), "privacy".to_owned()]),
            flake_command: Some(vec!["cargo".to_owned(), "test".to_owned()]),
            flake_runs: 20,
            retention_days: 7,
            min_free_disk_bytes: 1_073_741_824,
            sources: BTreeMap::new(),
        }
    }

    #[test]
    fn the_default_configuration_is_the_values_the_design_documents() {
        assert_documented_defaults(&Config::default());
    }

    #[test]
    fn an_empty_toml_document_deserializes_to_the_documented_defaults() {
        let config = parse("");
        assert_documented_defaults(&config);
        assert_same_config(&config, &Config::default());
    }

    #[test]
    fn an_unknown_key_is_rejected_and_names_the_key() {
        let error = toml::from_str::<Config>("not_a_documented_setting = 1")
            .expect_err("a setting nobody defined must not be ignored in silence");
        let message = error.to_string();
        assert!(message.contains("unknown field"), "{message}");
        assert!(message.contains("not_a_documented_setting"), "{message}");
    }

    #[test]
    fn a_document_overrides_only_the_keys_it_sets() {
        let config = parse(
            "provider = \"codex\"\nmax_attempts = 3\nsecret_patterns = ['secret_[a-z0-9]+']\n",
        );
        let expected = Config {
            provider: "codex".to_owned(),
            max_attempts: 3,
            secret_patterns: vec!["secret_[a-z0-9]+".to_owned()],
            ..Config::default()
        };
        assert_same_config(&config, &expected);
    }

    #[test]
    fn an_optional_setting_is_read_from_a_document_that_sets_it() {
        let config = parse("model = \"sonnet\"\ndummy_scenario_path = \"/state/scenario.json\"\n");
        assert_eq!(config.model.as_deref(), Some("sonnet"));
        assert_eq!(
            config.dummy_scenario_path.as_deref(),
            Some(Path::new("/state/scenario.json"))
        );
    }

    #[test]
    fn a_value_of_the_wrong_type_is_rejected_naming_the_type_the_setting_has() {
        for (document, expected) in [
            ("attempt_timeout_secs = \"4 hours\"", "u64"),
            ("max_attempts = 1.5", "u32"),
            ("provider = 7", "a string"),
            ("test_globs = \"tests/**\"", "a sequence"),
        ] {
            let error = toml::from_str::<Config>(document)
                .expect_err("`{document}` does not fit the settings the design types");
            let message = error.to_string();
            assert!(
                message.contains(expected),
                "`{message}` does not mention {expected}"
            );
        }
    }

    #[test]
    fn a_negative_number_is_rejected_by_a_setting_that_counts() {
        let error = toml::from_str::<Config>("flake_runs = -1")
            .expect_err("a number of runs cannot be negative");
        let message = error.to_string();
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn a_configuration_written_out_is_read_back_unchanged() {
        for config in [Config::default(), every_field_set()] {
            let document = toml::to_string(&config).expect("a configuration is writable as TOML");
            let read_back: Config = toml::from_str(&document)
                .unwrap_or_else(|error| panic!("`{document}` was written by this crate: {error}"));
            assert_same_config(&read_back, &config);
        }
    }

    #[test]
    fn the_toml_keys_are_exactly_the_documented_settings() {
        assert_eq!(written_keys(&every_field_set()), documented_keys());
    }

    /// The 30 documented keys, in the order `docs/DESIGN.md` lists them, which
    /// is also the order the Configuration screen renders them in.
    const DOCUMENTED_KEYS: [&str; 30] = [
        "provider",
        "model",
        "attempt_timeout_secs",
        "gate_timeout_secs",
        "idle_timeout_secs",
        "max_attempts",
        "max_remediation_attempts",
        "circuit_breaker_threshold",
        "mainline_remote",
        "mainline_branch",
        "context_budget_bytes",
        "failure_bundle_bytes",
        "output_ring_lines",
        "limit_wait_margin_secs",
        "limit_max_wait_secs",
        "default_protocol",
        "dummy_scenario_path",
        "test_globs",
        "secret_patterns",
        "baseline_command",
        "targeted_test_command",
        "verify_command",
        "lint_command",
        "format_command",
        "build_command",
        "privacy_command",
        "flake_command",
        "flake_runs",
        "retention_days",
        "min_free_disk_bytes",
    ];

    /// A document that sets every documented key to something other than its
    /// default, so a key read into the wrong field shows up as a wrong value.
    const EVERY_KEY_DOCUMENT: &str = r#"
provider = "codex"
model = "gpt-5.6-sol"
attempt_timeout_secs = 900
gate_timeout_secs = 600
idle_timeout_secs = 300
max_attempts = 5
max_remediation_attempts = 3
circuit_breaker_threshold = 9
mainline_remote = "upstream"
mainline_branch = "trunk"
context_budget_bytes = 4096
failure_bundle_bytes = 2048
output_ring_lines = 64
limit_wait_margin_secs = 15
limit_max_wait_secs = 7200
default_protocol = "tdd"
dummy_scenario_path = "/state/dummy.json"
test_globs = ["tests/**", "crates/**/tests.rs"]
secret_patterns = ["ghp_[A-Za-z0-9]{36}"]
baseline_command = ["cargo", "check"]
targeted_test_command = ["cargo", "nextest", "run"]
verify_command = ["./scripts/quality.sh"]
lint_command = ["cargo", "clippy"]
format_command = ["cargo", "fmt", "--all"]
build_command = ["cargo", "build", "--locked"]
privacy_command = ["ktask-rs", "privacy", "audit"]
flake_command = ["cargo", "test", "--repeat"]
flake_runs = 11
retention_days = 30
min_free_disk_bytes = 1073741824
"#;

    /// Every documented key set through the environment instead, as the text
    /// an operator would write, to the same value `EVERY_KEY_DOCUMENT` gives
    /// it — a gate command as one word per comma-separated entry.
    const EVERY_KEY_VARIABLES: [(&str, &str); 30] = [
        ("KTASK_PROVIDER", "codex"),
        ("KTASK_MODEL", "gpt-5.6-sol"),
        ("KTASK_ATTEMPT_TIMEOUT_SECS", "900"),
        ("KTASK_GATE_TIMEOUT_SECS", "600"),
        ("KTASK_IDLE_TIMEOUT_SECS", "300"),
        ("KTASK_MAX_ATTEMPTS", "5"),
        ("KTASK_MAX_REMEDIATION_ATTEMPTS", "3"),
        ("KTASK_CIRCUIT_BREAKER_THRESHOLD", "9"),
        ("KTASK_MAINLINE_REMOTE", "upstream"),
        ("KTASK_MAINLINE_BRANCH", "trunk"),
        ("KTASK_CONTEXT_BUDGET_BYTES", "4096"),
        ("KTASK_FAILURE_BUNDLE_BYTES", "2048"),
        ("KTASK_OUTPUT_RING_LINES", "64"),
        ("KTASK_LIMIT_WAIT_MARGIN_SECS", "15"),
        ("KTASK_LIMIT_MAX_WAIT_SECS", "7200"),
        ("KTASK_DEFAULT_PROTOCOL", "tdd"),
        ("KTASK_DUMMY_SCENARIO_PATH", "/state/dummy.json"),
        ("KTASK_TEST_GLOBS", "tests/**,crates/**/tests.rs"),
        ("KTASK_SECRET_PATTERNS", "ghp_[A-Za-z0-9]{36}"),
        ("KTASK_BASELINE_COMMAND", "cargo,check"),
        ("KTASK_TARGETED_TEST_COMMAND", "cargo,nextest,run"),
        ("KTASK_VERIFY_COMMAND", "./scripts/quality.sh"),
        ("KTASK_LINT_COMMAND", "cargo,clippy"),
        ("KTASK_FORMAT_COMMAND", "cargo,fmt,--all"),
        ("KTASK_BUILD_COMMAND", "cargo,build,--locked"),
        ("KTASK_PRIVACY_COMMAND", "ktask-rs,privacy,audit"),
        ("KTASK_FLAKE_COMMAND", "cargo,test,--repeat"),
        ("KTASK_FLAKE_RUNS", "11"),
        ("KTASK_RETENTION_DAYS", "30"),
        ("KTASK_MIN_FREE_DISK_BYTES", "1073741824"),
    ];

    /// An environment with nothing set, which is how every test that is not
    /// about the environment reads it.
    fn no_variables(_: &str) -> Option<String> {
        None
    }

    /// An environment accessor over a fixed table of variables, so no test has
    /// to touch the process environment.
    fn variables(values: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let table: BTreeMap<String, String> = values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| table.get(name).cloned()
    }

    /// Writes a configuration document and returns the path it lives at.
    fn write_document(directory: &Path, name: &str, document: &str) -> PathBuf {
        let path = directory.join(name);
        fs::write(&path, document)
            .unwrap_or_else(|error| panic!("`{}` is writable: {error}", path.display()));
        path
    }

    /// A configuration's provenance as a lookup, for the tests that name a
    /// handful of keys rather than the whole list.
    fn sources_of(config: &Config) -> BTreeMap<String, Source> {
        config.provenance().into_iter().collect()
    }

    /// Which layer a configuration says one key's value came from.
    fn source_of(config: &Config, key: &str) -> Source {
        *sources_of(config)
            .get(key)
            .unwrap_or_else(|| panic!("`{key}` reports no provenance"))
    }

    /// The provenance every documented key reports when one layer set all of
    /// them, in the order `docs/DESIGN.md` lists them.
    fn every_key_from(layer: Source) -> Vec<(String, Source)> {
        DOCUMENTED_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), layer))
            .collect()
    }

    /// The provenance a configuration should report when `overrides` names the
    /// keys a layer set and every other key was left at the default.
    fn keys_from(overrides: &[(&str, Source)]) -> BTreeMap<String, Source> {
        DOCUMENTED_KEYS
            .iter()
            .map(|key| ((*key).to_owned(), Source::Default))
            .chain(
                overrides
                    .iter()
                    .map(|(key, layer)| ((*key).to_owned(), *layer)),
            )
            .collect()
    }

    /// The keys a configuration holds when it is written out as TOML.
    fn written_keys(config: &Config) -> BTreeSet<String> {
        let document = toml::to_string(config).expect("a configuration is writable as TOML");
        let value: toml::Value = toml::from_str(&document)
            .unwrap_or_else(|error| panic!("`{document}` was written by this crate: {error}"));
        value
            .as_table()
            .expect("a configuration is a table")
            .keys()
            .cloned()
            .collect()
    }

    /// The documented keys as a set.
    fn documented_keys() -> BTreeSet<String> {
        DOCUMENTED_KEYS
            .iter()
            .map(|key| (*key).to_owned())
            .collect()
    }

    /// The words of a gate command, as the setting that holds one holds them, so
    /// a test can name the words rather than rebuild the type each time.
    fn words(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|word| (*word).to_owned()).collect()
    }

    /// Asserts every setting took the value the two fixtures above give it,
    /// setting by setting so a failure names the setting rather than a struct.
    fn assert_every_setting_is_set(config: &Config) {
        assert_eq!(config.provider, "codex");
        assert_eq!(config.model.as_deref(), Some("gpt-5.6-sol"));
        assert_eq!(config.attempt_timeout_secs, 900);
        assert_eq!(config.gate_timeout_secs, 600);
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.max_attempts, 5);
        assert_eq!(config.max_remediation_attempts, 3);
        assert_eq!(config.circuit_breaker_threshold, 9);
        assert_eq!(config.mainline_remote, "upstream");
        assert_eq!(config.mainline_branch, "trunk");
        assert_eq!(config.context_budget_bytes, 4_096);
        assert_eq!(config.failure_bundle_bytes, 2_048);
        assert_eq!(config.output_ring_lines, 64);
        assert_eq!(config.limit_wait_margin_secs, 15);
        assert_eq!(config.limit_max_wait_secs, 7_200);
        assert_eq!(config.default_protocol, "tdd");
        assert_eq!(
            config.dummy_scenario_path.as_deref(),
            Some(Path::new("/state/dummy.json"))
        );
        assert_eq!(config.test_globs, ["tests/**", "crates/**/tests.rs"]);
        assert_eq!(config.secret_patterns, ["ghp_[A-Za-z0-9]{36}"]);
        assert_eq!(config.baseline_command, Some(words(&["cargo", "check"])));
        assert_eq!(
            config.targeted_test_command,
            Some(words(&["cargo", "nextest", "run"]))
        );
        assert_eq!(
            config.verify_command,
            Some(words(&["./scripts/quality.sh"]))
        );
        assert_eq!(config.lint_command, Some(words(&["cargo", "clippy"])));
        assert_eq!(
            config.format_command,
            Some(words(&["cargo", "fmt", "--all"]))
        );
        assert_eq!(
            config.build_command,
            Some(words(&["cargo", "build", "--locked"]))
        );
        assert_eq!(
            config.privacy_command,
            Some(words(&["ktask-rs", "privacy", "audit"]))
        );
        assert_eq!(
            config.flake_command,
            Some(words(&["cargo", "test", "--repeat"]))
        );
        assert_eq!(config.flake_runs, 11);
        assert_eq!(config.retention_days, 30);
        assert_eq!(config.min_free_disk_bytes, 1_073_741_824);
    }

    #[test]
    fn a_configuration_nobody_configured_reports_the_default_for_every_key() {
        let config = load(None, None, &no_variables).expect("the defaults are always loadable");
        assert_documented_defaults(&config);
        assert_eq!(config.provenance(), every_key_from(Source::Default));
    }

    #[test]
    fn a_global_document_sets_every_documented_key_and_reports_itself_as_the_source() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(scratch.path(), "global.toml", EVERY_KEY_DOCUMENT);
        let config = load(Some(&global), None, &no_variables)
            .expect("a document that sets every key is a configuration");
        assert_every_setting_is_set(&config);
        assert_eq!(config.provenance(), every_key_from(Source::GlobalFile));
    }

    #[test]
    fn a_project_document_sets_every_documented_key_and_reports_itself_as_the_source() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let project = write_document(scratch.path(), "project.toml", EVERY_KEY_DOCUMENT);
        let config = load(None, Some(&project), &no_variables)
            .expect("a project document is a configuration");
        assert_every_setting_is_set(&config);
        assert_eq!(config.provenance(), every_key_from(Source::ProjectFile));
    }

    #[test]
    fn an_environment_variable_sets_every_documented_key_and_reports_itself_as_the_source() {
        let env = variables(&EVERY_KEY_VARIABLES);
        let config = load(None, None, &env).expect("the environment alone is a configuration");
        assert_every_setting_is_set(&config);
        assert_eq!(config.provenance(), every_key_from(Source::Env));
    }

    #[test]
    fn a_name_resolves_to_the_highest_layer_that_sets_it() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(scratch.path(), "global.toml", "provider = \"claude\"\n");
        let project = write_document(scratch.path(), "project.toml", "provider = \"codex\"\n");
        let env = variables(&[("KTASK_PROVIDER", "from-environment")]);

        let nobody = load(None, None, &no_variables).expect("the defaults are loadable");
        assert_eq!(nobody.provider, "dummy");
        assert_eq!(source_of(&nobody, "provider"), Source::Default);

        let global_only =
            load(Some(&global), None, &no_variables).expect("a global document is a configuration");
        assert_eq!(global_only.provider, "claude");
        assert_eq!(source_of(&global_only, "provider"), Source::GlobalFile);

        let with_project =
            load(Some(&global), Some(&project), &no_variables).expect("both documents load");
        assert_eq!(with_project.provider, "codex");
        assert_eq!(source_of(&with_project, "provider"), Source::ProjectFile);

        let with_env =
            load(Some(&global), Some(&project), &env).expect("the environment loads too");
        assert_eq!(with_env.provider, "from-environment");
        assert_eq!(source_of(&with_env, "provider"), Source::Env);
    }

    #[test]
    fn a_count_resolves_to_the_highest_layer_that_sets_it() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(scratch.path(), "global.toml", "max_attempts = 3\n");
        let project = write_document(scratch.path(), "project.toml", "max_attempts = 5\n");
        let env = variables(&[("KTASK_MAX_ATTEMPTS", "7")]);

        let nobody = load(None, None, &no_variables).expect("the defaults are loadable");
        assert_eq!(nobody.max_attempts, 2);
        assert_eq!(source_of(&nobody, "max_attempts"), Source::Default);

        let global_only =
            load(Some(&global), None, &no_variables).expect("a global document is a configuration");
        assert_eq!(global_only.max_attempts, 3);
        assert_eq!(source_of(&global_only, "max_attempts"), Source::GlobalFile);

        let with_project =
            load(Some(&global), Some(&project), &no_variables).expect("both documents load");
        assert_eq!(with_project.max_attempts, 5);
        assert_eq!(
            source_of(&with_project, "max_attempts"),
            Source::ProjectFile
        );

        let with_env =
            load(Some(&global), Some(&project), &env).expect("the environment loads too");
        assert_eq!(with_env.max_attempts, 7);
        assert_eq!(source_of(&with_env, "max_attempts"), Source::Env);
    }

    #[test]
    fn a_layer_overrides_only_the_keys_it_sets_and_leaves_the_rest_to_their_layers() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(
            scratch.path(),
            "global.toml",
            "provider = \"claude\"\nattempt_timeout_secs = 600\nmax_attempts = 3\n",
        );
        let project = write_document(scratch.path(), "project.toml", "provider = \"codex\"\n");
        let env = variables(&[("KTASK_MAINLINE_BRANCH", "trunk")]);
        let config = load(Some(&global), Some(&project), &env).expect("three layers load");

        assert_eq!(config.provider, "codex");
        assert_eq!(config.attempt_timeout_secs, 600);
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.mainline_branch, "trunk");
        assert_eq!(config.model, None);
        assert_eq!(
            sources_of(&config),
            keys_from(&[
                ("provider", Source::ProjectFile),
                ("attempt_timeout_secs", Source::GlobalFile),
                ("max_attempts", Source::GlobalFile),
                ("mainline_branch", Source::Env),
            ])
        );
    }

    #[test]
    fn a_configuration_file_that_is_not_there_is_not_a_layer() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = scratch.path().join("global.toml");
        let project = scratch.path().join("project.toml");
        let config = load(Some(&global), Some(&project), &no_variables)
            .expect("a project that never wrote a configuration is normal");
        assert_documented_defaults(&config);
        assert_eq!(config.provenance(), every_key_from(Source::Default));
    }

    #[test]
    fn a_configuration_path_that_holds_no_document_is_refused_rather_than_skipped() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let directory = scratch.path().join("config.toml");
        fs::create_dir(&directory)
            .unwrap_or_else(|error| panic!("`{}` is creatable: {error}", directory.display()));
        let error = load(Some(&directory), None, &no_variables)
            .expect_err("a directory is not a configuration document");
        assert!(
            matches!(error, Error::Io(_)),
            "a document that is there and cannot be read is reported, not skipped: {error}"
        );
    }

    #[test]
    fn a_document_that_is_not_valid_toml_is_refused_naming_the_file_that_holds_it() {
        for written_as_global in [true, false] {
            let scratch = tempdir().expect("a scratch directory outside the repository");
            let name = if written_as_global {
                "global.toml"
            } else {
                "project.toml"
            };
            let path = write_document(scratch.path(), name, "provider = \n");
            let (global, project) = if written_as_global {
                (Some(path.as_path()), None)
            } else {
                (None, Some(path.as_path()))
            };
            let error = load(global, project, &no_variables)
                .expect_err("a truncated document is not a configuration");
            let message = error.to_string();
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(message.contains("not a valid TOML document"), "{message}");
        }
    }

    #[test]
    fn a_key_nobody_documents_is_refused_naming_the_key_and_the_file_that_set_it() {
        for written_as_global in [true, false] {
            let scratch = tempdir().expect("a scratch directory outside the repository");
            let name = if written_as_global {
                "global.toml"
            } else {
                "project.toml"
            };
            let path = write_document(scratch.path(), name, "not_a_documented_setting = 1\n");
            let (global, project) = if written_as_global {
                (Some(path.as_path()), None)
            } else {
                (None, Some(path.as_path()))
            };
            let error = load(global, project, &no_variables)
                .expect_err("an undocumented key must not be ignored in silence");
            let message = error.to_string();
            assert!(message.contains("not_a_documented_setting"), "{message}");
            assert!(message.contains(&path.display().to_string()), "{message}");
            assert!(message.contains("not documented"), "{message}");
        }
    }

    #[test]
    fn a_document_value_the_setting_cannot_hold_is_refused_naming_the_key_and_the_file() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(scratch.path(), "global.toml", "max_attempts = \"lots\"\n");
        let error = load(Some(&global), None, &no_variables).expect_err("`lots` is not a count");
        let message = error.to_string();
        assert!(message.contains("max_attempts"), "{message}");
        assert!(message.contains(&global.display().to_string()), "{message}");
        assert!(message.contains("u32"), "{message}");
    }

    #[test]
    fn an_environment_value_the_setting_cannot_hold_is_refused_naming_the_variable() {
        let env = variables(&[("KTASK_MAX_ATTEMPTS", "lots")]);
        let error = load(None, None, &env).expect_err("`lots` is not a count");
        let message = error.to_string();
        assert!(message.contains("max_attempts"), "{message}");
        assert!(message.contains("KTASK_MAX_ATTEMPTS"), "{message}");
        assert!(message.contains("`u32`"), "{message}");
    }

    #[test]
    fn an_environment_value_that_is_empty_or_blank_carries_no_setting() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(
            scratch.path(),
            "global.toml",
            "provider = \"claude\"\nmax_attempts = 3\ntest_globs = [\"tests/**\"]\n",
        );
        let env = variables(&[
            ("KTASK_PROVIDER", "   "),
            ("KTASK_MAX_ATTEMPTS", ""),
            ("KTASK_TEST_GLOBS", ""),
        ]);
        let config = load(Some(&global), None, &env)
            .expect("an environment value with nothing in it is not a layer");

        assert_eq!(config.provider, "claude");
        assert_eq!(config.max_attempts, 3);
        assert_eq!(config.test_globs, ["tests/**"]);
        assert_eq!(
            sources_of(&config),
            keys_from(&[
                ("provider", Source::GlobalFile),
                ("max_attempts", Source::GlobalFile),
                ("test_globs", Source::GlobalFile),
            ])
        );
    }

    #[test]
    fn an_environment_list_is_split_on_commas_and_emptied_by_separators_alone() {
        let env = variables(&[
            ("KTASK_TEST_GLOBS", " tests/** , crates/**/tests.rs ,, "),
            ("KTASK_SECRET_PATTERNS", ","),
        ]);
        let config = load(None, None, &env).expect("a list written in the environment is a value");

        assert_eq!(config.test_globs, ["tests/**", "crates/**/tests.rs"]);
        assert!(
            config.secret_patterns.is_empty(),
            "{:?}",
            config.secret_patterns
        );
        assert_eq!(source_of(&config, "test_globs"), Source::Env);
        assert_eq!(source_of(&config, "secret_patterns"), Source::Env);
    }

    #[test]
    fn a_configuration_deserialized_without_layers_reports_the_default_layer() {
        let config = parse("provider = \"codex\"");
        assert_eq!(config.provider, "codex");
        assert_eq!(source_of(&config, "provider"), Source::Default);
    }

    #[test]
    fn recorded_provenance_is_never_written_into_a_configuration_document() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let global = write_document(scratch.path(), "global.toml", EVERY_KEY_DOCUMENT);
        let config = load(Some(&global), None, &no_variables).expect("a loaded configuration");
        assert_eq!(written_keys(&config), documented_keys());
    }

    #[test]
    fn a_source_is_named_the_way_the_configuration_screen_shows_it() {
        for (source, label) in [
            (Source::Default, "default"),
            (Source::GlobalFile, "global file"),
            (Source::ProjectFile, "project file"),
            (Source::Env, "environment"),
            (Source::Flag, "flag"),
        ] {
            assert_eq!(source.to_string(), label);
        }
    }

    /// The environment of the machine a test means: `XDG_CONFIG_HOME` at
    /// `config_home`, `set` on top of it, and nothing else.
    ///
    /// `HOME` is deliberately absent, so a test cannot read the settings of
    /// whoever runs it through the fallback `crate::paths` documents.
    fn machine(config_home: &Path, set: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let held = config_home.display().to_string();
        let table: BTreeMap<String, String> = set
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        move |name| match name {
            "XDG_CONFIG_HOME" => Some(held.clone()),
            other => table.get(other).cloned(),
        }
    }

    /// A registered project whose state directory is below `scratch`, with its
    /// repository nowhere near it.
    ///
    /// A loader asks a project for its state directory and nothing else, so the
    /// fixture holds that one path honestly; the identity and the working copy
    /// only keep the shape of a real registration.
    fn scratch_project(scratch: &Path) -> Project {
        let id = "0123456789abcdef-0123456789abcdef".to_owned();
        Project {
            root: scratch.join("repository"),
            state_dir: scratch.join("state").join("ktask-rs").join(&id),
            id,
        }
    }

    /// The project's own document, written below its state directory.
    ///
    /// The filename is spelled out here rather than asked of
    /// `crate::project::project_config_path`, so a loader reading some other
    /// file fails this test rather than agreeing with itself.
    fn write_project_document(project: &Project, document: &str) -> PathBuf {
        let state_dir = &project.state_dir;
        fs::create_dir_all(state_dir)
            .unwrap_or_else(|error| panic!("`{}` is creatable: {error}", state_dir.display()));
        write_document(state_dir, "config.toml", document)
    }

    /// The global document, written where `docs/DESIGN.md` Paths puts it below
    /// `config_home` — again spelled out, for the same reason.
    fn write_global_document(config_home: &Path, document: &str) -> PathBuf {
        let directory = config_home.join("ktask-rs");
        fs::create_dir_all(&directory)
            .unwrap_or_else(|error| panic!("`{}` is creatable: {error}", directory.display()));
        write_document(&directory, "config.toml", document)
    }

    #[test]
    fn a_project_with_no_configuration_documents_loads_the_documented_defaults() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let project = scratch_project(scratch.path());

        let config = load_for_with(&machine(scratch.path(), &[]), &project)
            .expect("a project nobody has configured is loadable");

        assert_documented_defaults(&config);
        assert_eq!(
            config.provenance(),
            every_key_from(Source::Default),
            "with no file and no variable, every key reports the layer that set it"
        );
    }

    #[test]
    fn a_project_load_reads_the_global_document_where_paths_puts_it() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let config_home = scratch.path().join("config-home");
        write_global_document(&config_home, "provider = \"claude\"\nflake_runs = 9\n");
        let project = scratch_project(scratch.path());

        let config = load_for_with(&machine(&config_home, &[]), &project)
            .expect("the machine's own settings are a layer");

        assert_eq!(config.provider, "claude");
        assert_eq!(config.flake_runs, 9);
        assert_eq!(
            config.max_attempts, 2,
            "a key the global document does not set stays at its default"
        );
        assert_eq!(
            sources_of(&config),
            keys_from(&[
                ("provider", Source::GlobalFile),
                ("flake_runs", Source::GlobalFile),
            ])
        );
    }

    #[test]
    fn a_project_document_overrides_the_global_document_key_by_key() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let config_home = scratch.path().join("config-home");
        write_global_document(
            &config_home,
            "provider = \"claude\"\nmax_attempts = 4\nflake_runs = 9\n",
        );
        let project = scratch_project(scratch.path());
        write_project_document(
            &project,
            "provider = \"codex\"\nmainline_branch = \"trunk\"\n",
        );

        let config = load_for_with(&machine(&config_home, &[]), &project)
            .expect("a repository may disagree with the machine it sits on");

        assert_eq!(config.provider, "codex", "the project's file wins");
        assert_eq!(config.mainline_branch, "trunk");
        assert_eq!(
            config.max_attempts, 4,
            "a key only the global document sets keeps its value"
        );
        assert_eq!(config.flake_runs, 9);
        assert_eq!(
            config.default_protocol, "direct",
            "a key neither document sets stays at its default"
        );
        assert_eq!(
            sources_of(&config),
            keys_from(&[
                ("provider", Source::ProjectFile),
                ("mainline_branch", Source::ProjectFile),
                ("max_attempts", Source::GlobalFile),
                ("flake_runs", Source::GlobalFile),
            ]),
            "the effective source of every value is retrievable"
        );
    }

    #[test]
    fn the_environment_overrides_both_documents_for_the_keys_it_sets() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let config_home = scratch.path().join("config-home");
        write_global_document(&config_home, "provider = \"claude\"\nretention_days = 7\n");
        let project = scratch_project(scratch.path());
        write_project_document(&project, "provider = \"codex\"\nflake_runs = 11\n");
        let env = machine(
            &config_home,
            &[("KTASK_PROVIDER", "zed"), ("KTASK_RETENTION_DAYS", "30")],
        );

        let config = load_for_with(&env, &project).expect("one run may override both files");

        assert_eq!(config.provider, "zed");
        assert_eq!(config.retention_days, 30);
        assert_eq!(
            config.flake_runs, 11,
            "a document the environment says nothing about still applies"
        );
        assert_eq!(
            sources_of(&config),
            keys_from(&[
                ("provider", Source::Env),
                ("retention_days", Source::Env),
                ("flake_runs", Source::ProjectFile),
            ])
        );
    }

    #[test]
    fn a_project_document_that_is_not_valid_toml_is_refused_naming_its_own_path() {
        let scratch = tempdir().expect("a scratch directory outside the repository");
        let project = scratch_project(scratch.path());
        let written = write_project_document(&project, "provider = \n");

        let error = load_for_with(&machine(scratch.path(), &[]), &project)
            .expect_err("a half-written document is not a configuration");
        let message = error.to_string();
        assert!(
            message.contains(&written.display().to_string()),
            "{message}"
        );
        assert!(message.contains("not a valid TOML document"), "{message}");
    }

    #[test]
    fn a_project_whose_configuration_home_cannot_be_resolved_is_refused_naming_home() {
        let scratch = tempdir().expect("a scratch directory outside the repository");

        let error = load_for_with(&no_variables, &scratch_project(scratch.path()))
            .expect_err("the global path has to be resolvable before anything is read");
        let message = error.to_string();
        assert!(message.contains("XDG_CONFIG_HOME"), "{message}");
        assert!(message.contains("HOME"), "{message}");
    }

    /// A variable read straight from the process, an empty one treated as
    /// absent, as `crate::paths` reads it.
    fn process(key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|found| !found.is_empty())
    }

    #[test]
    fn load_for_reads_the_environment_the_process_actually_has() {
        // The public entry point reads the ambient environment, so exercising
        // it means a project that wrote nothing: its state directory is a
        // scratch path no configuration of this machine can own, and loading
        // reads and never writes wherever the ambient paths resolve to.
        let scratch = tempdir().expect("a scratch directory outside any project");
        let project = scratch_project(scratch.path());

        match (process("XDG_CONFIG_HOME"), process("HOME")) {
            (Some(_), _) | (None, Some(_)) => {
                let config = load_for(&project)
                    .expect("a project that wrote nothing loads whatever the machine holds");
                for (key, source) in config.provenance() {
                    assert_ne!(
                        source,
                        Source::ProjectFile,
                        "`{key}` reports a project document that was never written"
                    );
                }
            }
            (None, None) => {
                let error = load_for(&project).expect_err("nothing names a configuration file");
                assert!(
                    matches!(&error, Error::Config { key, .. } if key == "HOME"),
                    "{error}"
                );
            }
        }
    }
}
