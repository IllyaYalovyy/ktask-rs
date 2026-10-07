//! The names of the settings, and the defaults a setting has when a project has not set it.

/// `run`'s time limit for one attempt when nothing else sets it: four hours.
pub const DEFAULT_ATTEMPT_TIMEOUT_SECS: u64 = 14_400;

/// How long a running provider may produce no output before its line says it may be stuck.
pub const DEFAULT_SILENT_AFTER_SECS: u64 = 120;

/// How many attempts a task may have before the resolver is no longer run, when nothing else
/// sets it.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// How many consecutive Codex transport failures are retried before asking the operator.
pub const DEFAULT_TRANSPORT_RETRIES: u32 = 3;

/// The provider the implementation, review and test steps run with when nothing else sets it.
pub const DEFAULT_PROVIDER: &str = "echo";

/// The provider the resolve step runs with, when nothing else sets it.
pub const DEFAULT_RESOLVER_PROVIDER: &str = "echo";

/// The directory, relative to the project, that holds the instruction files every agent's
/// prompt opens with, when nothing else sets it.
pub const DEFAULT_INSTRUCTIONS_DIR: &str = "docs";

/// The attempt time limit setting's name.
pub const ATTEMPT_TIMEOUT: &str = "attempt-timeout";

/// The silence threshold setting's name.
pub const SILENT_AFTER: &str = "silent-after";

/// The health-check command setting's name.
pub const HEALTH_CHECK: &str = "health-check";

/// The check command setting's name.
pub const CHECK: &str = "check";

/// The tracked-branch setting's name.
pub const TRACKED_BRANCH: &str = "tracked-branch";

/// The sync step's on/off switch setting's name.
pub const STEP_SYNC: &str = "step-sync";

/// The health-check step's on/off switch setting's name.
pub const STEP_HEALTH_CHECK: &str = "step-health-check";

/// The check step's on/off switch setting's name.
pub const STEP_CHECK: &str = "step-check";

/// The review step's on/off switch setting's name.
pub const STEP_REVIEW: &str = "step-review";

/// The testing step's on/off switch setting's name.
pub const STEP_TESTING: &str = "step-testing";

/// The commit step's on/off switch setting's name.
pub const STEP_COMMIT: &str = "step-commit";

/// The push step's on/off switch setting's name.
pub const STEP_PUSH: &str = "step-push";

/// The max-attempts setting's name.
pub const MAX_ATTEMPTS: &str = "max-attempts";
/// The number of consecutive Codex transport failures retried in one attempt.
pub const TRANSPORT_RETRIES: &str = "transport-retries";

/// The provider for implementation, review and test steps.
pub const PROVIDER: &str = "provider";

/// The model for implementation, review and test steps.
pub const MODEL: &str = "model";

/// The resolver-provider setting's name.
pub const RESOLVER_PROVIDER: &str = "resolver-provider";

/// The resolver-model setting's name.
pub const RESOLVER_MODEL: &str = "resolver-model";

/// The instructions-dir setting's name.
pub const INSTRUCTIONS_DIR: &str = "instructions-dir";

/// The name [`set_setting`] refuses under: the implementation step always runs, for every
/// task, so it is never one of the switches [`show_settings`] lists — this name exists only
/// so trying to switch it off gets a clear refusal instead of "unknown setting".
pub const STEP_IMPLEMENTATION: &str = "step-implementation";
