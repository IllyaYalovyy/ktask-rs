//! The one suite every provider adapter must pass.
//!
//! VISION.md §12 makes adapters additive: `dummy`, Claude and Codex at launch,
//! and further CLIs behind them, all reached through [`Provider`] so that
//! nothing above the layer can tell them apart. A layer that additive is only
//! safe if what an adapter owes is checked in one place. Asserting it per
//! adapter is asserting it per author: the second adapter's author writes the
//! assertions they thought of, the third writes different ones, and the
//! difference between two adapters' test suites is a behavioral difference
//! between the adapters that no code can see. ADR-0050 built [`Provider`] so
//! that an adapter's identity answers nothing; this file is the other half of
//! that, where an adapter's *conduct* is answered once, for all of them.
//!
//! Four rules, and they are the four an adapter cannot be substituted for:
//! a name, because it is the key of [`crate::Error::Provider`] and the word an
//! operator reads; stable capabilities, because a caller is only allowed to ask
//! for what detection found, and an answer that drifts makes every decision
//! taken on the earlier answer stale; an exit code of zero with something to
//! read on stdout for a session that worked, because a silent success is
//! indistinguishable from a session that never ran; and a non-zero exit code
//! carried as an [`Outcome`] for a session that failed, because the difference
//! between "the CLI refused to start" and "the work failed" is the difference
//! between a preflight problem and a task failure, and an error collapses it.
//!
//! ## What the adapter must be built to do
//!
//! The suite cannot tell an adapter to fail: `Provider` takes `&self`, and
//! [`Invocation`] carries no word by which a caller may order a failure — a
//! prompt an adapter reads as a command to fail is a prompt that decides
//! behavior nobody declared, which is exactly what `Dummy::invoke` refuses to
//! do. So the two sessions are decided before the adapter is built, by the same
//! fixture that configures everything else about it: a `Dummy` is built on a
//! scenario whose first step succeeds and whose second fails, and a real CLI
//! under VISION.md §15's optional smoke tier is built on a command that answers
//! the second session with a non-zero status. An adapter handed to the suite
//! unconfigured for that is refused, which is the correct verdict: an adapter
//! whose behavior the suite cannot steer is an adapter whose failure path was
//! never tested.
//!
//! ## Registering an adapter
//!
//! No new assertions. One test, which is the whole of what a new adapter adds:
//!
//! ```text
//! #[test]
//! fn the_new_adapter_passes_the_suite_every_adapter_must_pass() {
//!     conformance_suite(&NewAdapter::fixture());
//! }
//! ```
//!
//! The refusal tests below are what make that enough. A suite no one has seen
//! fail is indistinguishable from a suite that asserts nothing, so each rule is
//! run against an adapter scripted to break exactly that rule, and the suite is
//! required to notice.
//!
//! ## Why this is test code
//!
//! Its failure mode is a panic, and a panic is what an assertion is. This crate
//! forbids both in a run — errors are values there, because a supervisor that
//! panics loses the run it was supervising — so the suite is compiled only for
//! tests, beside the adapters it checks, and not into a binary that supervises
//! anything.
//!
//! [`Dummy`]: super::dummy::Dummy
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{Capabilities, Invocation, Outcome, Provider};
use crate::Error;

/// The prompt the suite's first session is asked to work on.
///
/// Two prompts rather than one reused: an adapter that answered both sessions
/// out of one cached reply would pass a suite whose entire second rule depends
/// on the two sessions being different. The words ask for nothing an adapter
/// could read as an instruction — see the module docs on why they may not.
const WORK_PROMPT: &str = "conformance session one: do the work this session was scripted to do";

/// The prompt the suite's second session is asked to work on.
const FAILURE_PROMPT: &str =
    "conformance session two: do the work this session was scripted to refuse";

/// One of the suite's two sessions, in a directory the suite made for it.
///
/// `model` is `None` on purpose. Asking for a model id is only allowed of an
/// adapter whose [`Capabilities::model_selection`] said yes, and detection is
/// what T062's preflight runs; a suite that asked would be asking every adapter,
/// including the ones that answered no.
fn session(prompt: &str, working_dir: &Path) -> Invocation {
    Invocation {
        prompt: prompt.to_owned(),
        model: None,
        working_dir: working_dir.to_path_buf(),
    }
}

/// Every rule a provider adapter must satisfy, in one call.
///
/// The adapter is expected to have been built for this suite: its first session
/// succeeds and prints, its second fails. The module docs say why that is the
/// adapter's fixture's job and not this call's, and what it means when the
/// suite is handed an adapter that was not configured that way.
///
/// Sessions run unwatched. A live view is a listener and not an input — the
/// trait says a provider that behaves differently when watched is a provider
/// whose scenario cannot be reproduced headlessly — and that claim is the
/// bus's and the recorder's to test, not an adapter's conformance to answer.
///
/// # Panics
///
/// When `p` breaks any of the four rules, with a message naming the rule and
/// what the adapter answered instead. That is the suite's verdict, and the only
/// way a rule that is not reached can be distinguished from a rule that holds.
pub fn conformance_suite(p: &dyn Provider) {
    // Rule one. There is no other way to report an adapter, or to name it in a
    // configuration file, and whitespace is not a name: it survives no join,
    // no `Error::Provider` and no TUI row a human reads.
    let name = p.name();
    assert!(
        !name.trim().is_empty(),
        "a provider that answers no name cannot be reported or configured: \
         `Provider::name` is the key of `Error::Provider`, the word an operator \
         writes for a provider, and the label every attempt of its sessions is \
         read under; this adapter answered {name:?}"
    );

    // Rule two, first half: two calls answer the same thing.
    let detected = p.capabilities();
    assert_eq!(
        p.capabilities(),
        detected,
        "capabilities are what startup detection found, so the same adapter \
         answers the same three questions every time it is asked — and a caller \
         that was told one thing and acts on another is choosing work this \
         adapter was never detected able to do"
    );

    // The suite runs real sessions, so it gives an adapter a real directory to
    // run them in — a task's worktree is a directory that exists, and a session
    // refused by its own working directory would fail this suite for a reason
    // that has nothing to do with the adapter.
    let scratch =
        tempfile::tempdir().expect("the suite has nowhere to run a session without a directory");
    let working_dir = scratch.path();

    // Rule three: a session that did the work answers with an exit status of
    // zero and with something to read.
    let ran = p
        .invoke(&session(WORK_PROMPT, working_dir), None)
        .unwrap_or_else(|error| {
            panic!(
                "the suite's working session came back as an error ({error}); an \
                 error is reserved for a session that never started, so either \
                 this adapter cannot run the session it was configured to run or \
                 it is reporting a ran session as a refused one"
            )
        });
    assert_eq!(
        ran.exit_code, 0,
        "the session the suite asked to succeed reported exit code {}; a status \
         read backwards turns every queue into a failing run, and the adapter is \
         where a status is chosen",
        ran.exit_code
    );
    assert!(
        !ran.stdout.is_empty(),
        "the session the suite asked to succeed printed nothing, and a success \
         with nothing to read is indistinguishable from a session that never \
         ran: stdout is the work a gate is later run over and the transcript an \
         attempt record preserves"
    );

    // Rule two, second half: running a session did not change the answers. A
    // CLI that reports a capability only after it has been started, or only
    // before, would pass the check above and fail here.
    let after_a_session = p.capabilities();
    assert_eq!(
        after_a_session, detected,
        "capabilities moved across a session, so what a caller may ask of this \
         adapter now depends on whether a session has run: detection answers \
         once, and the journal keeps the answer it recorded"
    );

    // Rule four: a session that failed answers with its exit code, as an
    // `Outcome`. This is the rule that decides what a failed task *is* — see
    // [`Provider::invoke`]'s errors section — so it is asserted as narrowly as
    // it can be: any non-zero status passes, and only the absence of a status
    // fails.
    let failed = p
        .invoke(&session(FAILURE_PROMPT, working_dir), None)
        .unwrap_or_else(|error| {
            panic!(
                "the suite's failing session came back as an error ({error}); a \
                 session that ran and failed surfaces the status it left, \
                 because `the CLI would not start` and `the work did not finish` \
                 are two different findings, one for a preflight and one for the \
                 classifier, and an error collapses them into the first"
            )
        });
    assert_ne!(
        failed.exit_code, 0,
        "the session the suite asked to fail reported exit code 0, which is an \
         adapter reporting a failed session as a successful one; VISION.md §3's \
         fourth invariant keeps a task from being done on a status alone, but a \
         status that lies is still the first thing every gate reads"
    );
}

/// What a [`Scripted`] adapter answers when it is asked.
#[derive(Clone)]
enum Reply {
    /// Ran, and left this behind.
    Ran(Outcome),
    /// Refused to start, for this reason.
    Refused(&'static str),
}

/// An adapter scripted to break exactly one rule of [`conformance_suite`].
///
/// Both of its answers come from a list and a cursor, because the rule under
/// test is always *which* answer arrives: a list of one is a constant answer,
/// and a list that changes is the drift or the misreported session. The last
/// answer repeats forever, so a test says only what it makes wrong.
struct Scripted {
    provider_name: &'static str,
    detections: Vec<Capabilities>,
    replies: Vec<Reply>,
    detection_calls: AtomicUsize,
    sessions: AtomicUsize,
}

impl Scripted {
    /// An adapter answering `detections` and then `replies`, one entry per call.
    fn new(
        provider_name: &'static str,
        detections: Vec<Capabilities>,
        replies: Vec<Reply>,
    ) -> Self {
        Self {
            provider_name,
            detections,
            replies,
            detection_calls: AtomicUsize::new(0),
            sessions: AtomicUsize::new(0),
        }
    }

    /// The two answers the suite expects, so a test deviates from one of them
    /// and nothing else.
    fn conforming_sessions() -> Vec<Reply> {
        vec![Reply::Ran(succeeded()), Reply::Ran(failed_session())]
    }

    /// The answer call number `call` gets out of a list that runs out last.
    fn next<T: Clone>(call: usize, answers: &[T]) -> T {
        answers
            .get(call.min(answers.len().saturating_sub(1)))
            .cloned()
            .expect("a scripted adapter was built with at least one answer")
    }
}

impl Provider for Scripted {
    fn name(&self) -> &str {
        self.provider_name
    }

    fn capabilities(&self) -> Capabilities {
        let call = self.detection_calls.fetch_add(1, Ordering::Relaxed);
        Self::next(call, &self.detections)
    }

    fn invoke(&self, _inv: &Invocation, _bus: Option<&crate::Bus>) -> crate::Result<Outcome> {
        let session = self.sessions.fetch_add(1, Ordering::Relaxed);
        match Self::next(session, &self.replies) {
            Reply::Ran(outcome) => Ok(outcome),
            Reply::Refused(detail) => Err(Error::Provider {
                provider: self.provider_name.to_owned(),
                detail: detail.to_owned(),
            }),
        }
    }
}

/// Detection that found nothing, which is all the suite is entitled to assume:
/// it asks for no structured output, no model, and no telemetry.
const NOTHING_DETECTED: Capabilities = Capabilities {
    structured_output: false,
    model_selection: false,
    usage_telemetry: false,
};

/// The outcome of the session the suite asks to succeed.
fn succeeded() -> Outcome {
    Outcome {
        exit_code: 0,
        stdout: "read the task, wrote the fix, ran the gates\n".to_owned(),
        stderr: String::new(),
        usage: None,
        session_id: None,
    }
}

/// The outcome of the session the suite asks to fail.
///
/// Exit code 7 rather than the 1 a `Dummy` step implies, because the suite
/// accepts any non-zero status and a fixture that only ever produced one would
/// leave that untested.
fn failed_session() -> Outcome {
    Outcome {
        exit_code: 7,
        stdout: "the gate refused the change\n".to_owned(),
        stderr: String::new(),
        usage: None,
        session_id: None,
    }
}

#[cfg(test)]
mod enforced {
    //! The suite passes the reference adapter, and refuses each way an adapter
    //! can be wrong.
    //!
    //! The refusals are the part that makes "every adapter passes this" mean
    //! something. A suite no one has watched fail is indistinguishable from a
    //! suite whose assertions were never reached, so every rule is run against
    //! a [`Scripted`] adapter that breaks that rule alone — and the suite is
    //! required to say so, in the words that name the rule.

    use super::{
        Capabilities, NOTHING_DETECTED, Outcome, Reply, Scripted, conformance_suite,
        failed_session, succeeded,
    };
    use crate::provider::dummy::{Dummy, Scenario};

    /// The scenario a [`Dummy`] is built on to face the suite: one session that
    /// succeeds and prints, one that fails and prints. Two steps because the
    /// suite asks an adapter for two sessions, and a scenario is the only door
    /// a `Dummy` can be told through.
    const DUMMY_SCENARIO: &str = r#"
[[steps]]
on_attempt = 1
outcome = "success"
stdout = "read the task, wrote the fix, ran the gates\n"

[[steps]]
on_attempt = 2
outcome = "failure"
stdout = "the gate refused the change\n"
"#;

    #[test]
    fn the_dummy_adapter_passes_the_suite_every_adapter_must_pass() {
        // The done-when of the suite in one line: an adapter's own test is this
        // call, and nothing else. Everything asserted here is asserted for the
        // next adapter from VISION.md §12's backlog without a new assertion.
        let scenario = Scenario::from_toml(DUMMY_SCENARIO)
            .expect("the reference scenario is a legal scenario");
        let dummy = Dummy::new(scenario).expect("a legal scenario builds a live adapter");

        conformance_suite(&dummy);
    }

    #[test]
    fn the_two_sessions_are_asked_with_two_prompts() {
        // The suite's fourth rule is about the *second* session, and holds only
        // while an adapter cannot answer both from one reply.
        assert_ne!(
            super::WORK_PROMPT,
            super::FAILURE_PROMPT,
            "one prompt for both sessions lets an adapter pass on its first answer"
        );
    }

    #[test]
    #[should_panic(expected = "a provider that answers no name cannot be reported")]
    fn an_adapter_that_answers_no_name_is_refused() {
        conformance_suite(&Scripted::new(
            "",
            vec![NOTHING_DETECTED],
            Scripted::conforming_sessions(),
        ));
    }

    #[test]
    #[should_panic(expected = "a provider that answers no name cannot be reported")]
    fn an_adapter_that_answers_only_spaces_is_refused() {
        // Spaces are the name that survives a configuration file and disappears
        // in a report, which is worse than no name at all.
        conformance_suite(&Scripted::new(
            "   ",
            vec![NOTHING_DETECTED],
            Scripted::conforming_sessions(),
        ));
    }

    #[test]
    #[should_panic(expected = "capabilities are what startup detection found")]
    fn capabilities_that_differ_between_two_calls_are_refused() {
        let drifted = Capabilities {
            structured_output: true,
            ..NOTHING_DETECTED
        };
        conformance_suite(&Scripted::new(
            "drifting",
            vec![NOTHING_DETECTED, drifted],
            Scripted::conforming_sessions(),
        ));
    }

    #[test]
    #[should_panic(expected = "capabilities moved across a session")]
    fn capabilities_that_change_after_a_session_are_refused() {
        // The first two calls answer alike, so the check between them passes; it
        // is running a session that changes the answer, which is what a CLI
        // asked for its capabilities lazily would do.
        let learned = Capabilities {
            usage_telemetry: true,
            ..NOTHING_DETECTED
        };
        conformance_suite(&Scripted::new(
            "learns",
            vec![NOTHING_DETECTED, NOTHING_DETECTED, learned],
            Scripted::conforming_sessions(),
        ));
    }

    #[test]
    #[should_panic(expected = "the suite's working session came back as an error")]
    fn a_working_session_refused_as_an_error_is_refused() {
        conformance_suite(&Scripted::new(
            "absent-cli",
            vec![NOTHING_DETECTED],
            vec![
                Reply::Refused("the CLI is not on PATH"),
                Reply::Ran(failed_session()),
            ],
        ));
    }

    #[test]
    #[should_panic(expected = "reported exit code 3")]
    fn a_working_session_that_reports_a_nonzero_status_is_refused() {
        let limped = Outcome {
            exit_code: 3,
            ..succeeded()
        };
        conformance_suite(&Scripted::new(
            "backwards",
            vec![NOTHING_DETECTED],
            vec![Reply::Ran(limped), Reply::Ran(failed_session())],
        ));
    }

    #[test]
    #[should_panic(expected = "printed nothing")]
    fn a_working_session_that_prints_nothing_is_refused() {
        let silent = Outcome {
            stdout: String::new(),
            ..succeeded()
        };
        conformance_suite(&Scripted::new(
            "silent",
            vec![NOTHING_DETECTED],
            vec![Reply::Ran(silent), Reply::Ran(failed_session())],
        ));
    }

    #[test]
    #[should_panic(expected = "the suite's failing session came back as an error")]
    fn a_failing_session_refused_as_an_error_is_refused() {
        // The rule this file exists for. A session that ran and failed is an
        // `Outcome`; reporting it as an error buries a task failure inside a
        // provider-unavailable finding, which is the one confusion a run cannot
        // recover from.
        conformance_suite(&Scripted::new(
            "collapser",
            vec![NOTHING_DETECTED],
            vec![
                Reply::Ran(succeeded()),
                Reply::Refused("the session exited 1"),
            ],
        ));
    }

    #[test]
    #[should_panic(expected = "reported exit code 0, which is an adapter reporting a failed")]
    fn a_failing_session_that_reports_zero_is_refused() {
        let lied = Outcome {
            exit_code: 0,
            ..failed_session()
        };
        conformance_suite(&Scripted::new(
            "optimist",
            vec![NOTHING_DETECTED],
            vec![Reply::Ran(succeeded()), Reply::Ran(lied)],
        ));
    }
}
