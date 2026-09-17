//! The names for why work stopped making progress.
//!
//! A failure reaches the supervisor as a class before it reaches anyone as a
//! sentence: what happens next — retry, wait for a reset, pause for a human,
//! stop the run — is chosen from the class, never from the text that came with
//! it. That is why the class is one closed enum shared by the journal, the
//! failures screen and `--json` output, rather than a string each reporter
//! invents.
//!
//! Only the vocabulary lives here. The function that decides which class an
//! observed failure belongs to is `classify()`, which arrives with the task
//! that owns remediation: a mapping needs the exit codes, gate results and
//! provider signals that task is written against, and guessing them here would
//! put a policy nobody reviewed into the one place every later task trusts.
//!
//! [`FailureClass`] is exactly the nine variants `docs/DESIGN.md` fixes, and
//! [`TddException`] is the four exception categories VISION.md §9 allows a task
//! to claim against test-first discipline; `docs/DESIGN.md` names the type as
//! the `TddExceptionUsed` payload without listing its variants, so
//! `docs/adr/0010-tdd-exception-categories-come-from-vision-md.md` records
//! where these four came from.

use serde::{Deserialize, Serialize};

/// Why a task did not get done, as the supervisor decides on it.
///
/// The nine classes partition by *what a correct response is*, not by what
/// noticed the problem: VISION.md §7 fixes the response for each — a
/// [`FailureClass::ProviderLimit`] with a known reset is waited out to the
/// exact instant, a [`FailureClass::ProviderConfiguration`] or
/// [`FailureClass::NeedsInput`] never loops at all and pauses for a human
/// immediately, and a [`FailureClass::PolicyFailure`] stops the run. Adding a
/// variant means adding a response, so the tests pin the count.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FailureClass {
    /// The agent could not complete the implementation.
    AgentFailure,
    /// Tests, lint, build, or the privacy checks failed.
    VerificationFailure,
    /// A usage limit, with a reset time known or unknown.
    ProviderLimit,
    /// A network error, a temporary service failure, or a provider process that
    /// crashed: the same work may succeed if asked again.
    ProviderTransient,
    /// Authentication, an invalid model, a missing executable. No retry can
    /// fix this, so nothing is retried.
    ProviderConfiguration,
    /// Branch drift, a rejected push, or a publication that conflicts with what
    /// the remote already holds.
    GitConflict,
    /// A missing SDK, dependency, or host capability: the machine is not the one
    /// the task was written for.
    EnvironmentFailure,
    /// A forbidden file was touched, the tree was dirty at verification time, or
    /// a gate was bypassed.
    PolicyFailure,
    /// A product or technical decision no one has made. The agent is not
    /// authorised to make it, and the supervisor is not either.
    NeedsInput,
}

/// The one legitimate reason a task was done without tests written first.
///
/// Test-first order cannot be proved after the fact, so the `tdd` protocol
/// enforces it (VISION.md §9) and a task that genuinely does not fit applies
/// for one of these four categories instead. The point of naming them is that
/// the override is *recorded and visible* rather than silent: an exception is
/// an entry in task history with a reason beside it, which is what makes
/// "no logic without a failing test" enforceable instead of aspirational.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TddException {
    /// Documentation changed; there is no behaviour for a test to pin.
    Documentation,
    /// The behaviour is unchanged, so the coverage that exists already holds.
    PureRefactoring,
    /// Build configuration changed, which the build and the gates verify.
    BuildConfiguration,
    /// A bug a failing test already reproduces: the red step arrived with the
    /// report rather than waiting to be written.
    ExistingFailingTest,
}

#[cfg(test)]
mod tests {
    use super::{FailureClass, TddException};
    use serde::de::DeserializeOwned;
    use std::fmt::Debug;

    /// Every `FailureClass`, in the order `docs/DESIGN.md` declares them.
    const FAILURE_CLASSES: [FailureClass; 9] = [
        FailureClass::AgentFailure,
        FailureClass::VerificationFailure,
        FailureClass::ProviderLimit,
        FailureClass::ProviderTransient,
        FailureClass::ProviderConfiguration,
        FailureClass::GitConflict,
        FailureClass::EnvironmentFailure,
        FailureClass::PolicyFailure,
        FailureClass::NeedsInput,
    ];

    /// The same nine names as `docs/DESIGN.md` spells them, written a second
    /// time so a rename cannot pass by matching itself.
    const FAILURE_CLASS_NAMES: [&str; 9] = [
        "AgentFailure",
        "VerificationFailure",
        "ProviderLimit",
        "ProviderTransient",
        "ProviderConfiguration",
        "GitConflict",
        "EnvironmentFailure",
        "PolicyFailure",
        "NeedsInput",
    ];

    /// Every `TddException`, the four categories `VISION.md` allows.
    const TDD_EXCEPTIONS: [TddException; 4] = [
        TddException::Documentation,
        TddException::PureRefactoring,
        TddException::BuildConfiguration,
        TddException::ExistingFailingTest,
    ];

    /// The same four names, spelled out a second time.
    const TDD_EXCEPTION_NAMES: [&str; 4] = [
        "Documentation",
        "PureRefactoring",
        "BuildConfiguration",
        "ExistingFailingTest",
    ];

    /// Encodes `value`, insists it carries the documented name, reads it back.
    fn round_trips<T>(value: &T, name: &str)
    where
        T: Copy + serde::Serialize + DeserializeOwned + PartialEq + Debug,
    {
        let encoded = serde_json::to_string(value).expect("vocabulary encodes as JSON");
        assert_eq!(
            encoded,
            format!("\"{name}\""),
            "{name} must encode as its own name"
        );
        let decoded: T =
            serde_json::from_str(&encoded).expect("a documented encoding is read back");
        assert_eq!(&decoded, value, "{name} must survive the round trip");
    }

    #[test]
    fn failure_class_has_the_nine_variants_docs_design_md_names() {
        assert_eq!(FAILURE_CLASSES.len(), 9);
        assert_eq!(FAILURE_CLASS_NAMES.len(), FAILURE_CLASSES.len());
        for (class, name) in FAILURE_CLASSES.iter().zip(FAILURE_CLASS_NAMES) {
            round_trips(class, name);
        }
    }

    #[test]
    fn tdd_exception_has_the_four_categories_vision_md_allows() {
        assert_eq!(TDD_EXCEPTIONS.len(), 4);
        assert_eq!(TDD_EXCEPTION_NAMES.len(), TDD_EXCEPTIONS.len());
        for (exception, name) in TDD_EXCEPTIONS.iter().zip(TDD_EXCEPTION_NAMES) {
            round_trips(exception, name);
        }
    }

    #[test]
    fn the_vocabulary_refuses_a_name_no_document_names() {
        for rejected in ["RateLimited", "agent_failure", "Unknown", ""] {
            assert!(
                serde_json::from_str::<FailureClass>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a failure class and must not deserialise as one",
            );
        }
        for rejected in ["Style", "Documentation ", "documentation"] {
            assert!(
                serde_json::from_str::<TddException>(&format!("\"{rejected}\"")).is_err(),
                "{rejected} is not a tdd exception and must not deserialise as one",
            );
        }
    }
}
