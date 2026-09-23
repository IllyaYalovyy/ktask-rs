//! Work protocols: typed, ordered sequences of [`Phase`]s a task's attempt
//! executes, per VISION.md §9.
//!
//! "How you work is configurable; what done means is not... every protocol
//! must terminate in the mandatory verify-publish gates." That constraint
//! is checked structurally here, at construction, rather than left to the
//! convention of whoever writes the next protocol: this module's private
//! `checked` constructor panics if the phase list handed to it does not end
//! in [`Phase::Verify`] then [`Phase::Publish`], and both
//! [`Protocol::direct`] and [`Protocol::tdd`] are built through it.

use std::path::{Path, PathBuf};

use crate::{Config, Error, GateKind, Phase, Result, Task, TddException, TestSummary};

/// Which paths a [`PhaseSpec`]'s agent may modify while it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum WriteScope {
    /// The agent may modify any path.
    All,
    /// The agent may modify test paths only; production code is read-only.
    ///
    /// Used by the `tdd` protocol's `Red` phase (VISION.md §9): the runner
    /// enforces this via configured test-path globs, not the agent's word.
    TestsOnly,
    /// The agent may not modify any path.
    None,
}

/// One phase of a [`Protocol`]: what it is, what it may write, the
/// mechanical gate that must pass before it counts as done, and whether it
/// records evidence with the attempt.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PhaseSpec {
    /// Which phase this is.
    pub phase: Phase,
    /// What the agent may write while this phase is active.
    pub write_scope: WriteScope,
    /// The mechanical gate that must pass for this phase to complete, if
    /// any. `None` for phases that are not gated by a runner command (for
    /// example a git action).
    pub gate: Option<GateKind>,
    /// Whether this phase's outcome is recorded as attempt evidence.
    pub records_evidence: bool,
}

/// A named, ordered sequence of [`PhaseSpec`]s a task's attempt executes.
///
/// Only [`Protocol::direct`] and [`Protocol::tdd`] exist today; both are
/// built through a private constructor that panics if the phase list does
/// not end in [`Phase::Verify`] then [`Phase::Publish`] — the one part of
/// "how you work" that VISION.md §9 does not leave configurable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol {
    /// The protocol's name, as used in task/attempt records.
    pub name: &'static str,
    /// The ordered phases this protocol runs.
    pub phases: Vec<PhaseSpec>,
}

/// Builds a [`Protocol`], panicking if `phases` does not end in
/// [`Phase::Verify`] then [`Phase::Publish`].
///
/// This is the only way this module constructs a [`Protocol`], so no
/// built-in protocol can skip the mandatory completion gates by accident —
/// the check runs once, at the call site inside [`Protocol::direct`] or
/// [`Protocol::tdd`], not against every attempt.
fn checked(name: &'static str, phases: Vec<PhaseSpec>) -> Protocol {
    let mut tail = phases.iter().rev();
    let ends_correctly = tail.next().is_some_and(|spec| spec.phase == Phase::Publish)
        && tail.next().is_some_and(|spec| spec.phase == Phase::Verify);
    assert!(
        ends_correctly,
        "protocol {name:?} must end with Verify then Publish"
    );
    Protocol { name, phases }
}

impl Protocol {
    /// v0.1's single-phase protocol: implement, then the mandatory
    /// completion gates.
    #[must_use]
    pub fn direct() -> Protocol {
        checked(
            "direct",
            vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        )
    }

    /// v0.1's runner-enforced red/green/refactor protocol (VISION.md §9).
    #[must_use]
    pub fn tdd() -> Protocol {
        checked(
            "tdd",
            vec![
                PhaseSpec {
                    phase: Phase::Red,
                    write_scope: WriteScope::TestsOnly,
                    gate: Some(GateKind::Targeted),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Green,
                    write_scope: WriteScope::All,
                    gate: Some(GateKind::Targeted),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Refactor,
                    write_scope: WriteScope::All,
                    gate: Some(GateKind::Targeted),
                    records_evidence: false,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        )
    }
}

/// Parses a category name as it appears in a task's `**TDD-Exception:**`
/// section (the text before the first `:`) into the [`TddException`] type
/// `crate::classify` already defines, or `None` if `name` matches none of
/// the four declared categories.
///
/// The sole source of the section-string/variant mapping, so
/// `parse_tdd_exception`'s error message and this function can never
/// disagree about which names are valid.
#[must_use]
fn parse_exception_category(name: &str) -> Option<TddException> {
    match name {
        "documentation" => Some(TddException::Documentation),
        "pure-refactor" => Some(TddException::PureRefactoring),
        "build-config" => Some(TddException::BuildConfiguration),
        "existing-failing-test" => Some(TddException::PreExistingFailingTest),
        _ => None,
    }
}

/// The bold section label a task uses to declare a `tdd` protocol
/// exception, mirroring the `**Protocol:**` convention `crate::task` uses
/// for the task's work protocol.
const EXCEPTION_LABEL: &str = "**TDD-Exception:**";

/// Extracts `body`'s optional `**TDD-Exception:**` section: the text
/// following the label on the first line where it appears at the start of
/// the (trimmed) line. `None` if no such line exists.
fn exception_section(body: &str) -> Option<&str> {
    body.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix(EXCEPTION_LABEL)
            .map(str::trim)
    })
}

/// Parses a task's `**TDD-Exception:**` section text into a category and a
/// reason. The expected form is `"<category>: <reason>"`, where `<category>`
/// is one of `documentation`, `pure-refactor`, `build-config` or
/// `existing-failing-test` (VISION.md §9's four exception categories).
///
/// # Errors
///
/// Returns [`Error::Policy`] if `raw` has no `:` separator, names a category
/// this module's private category parser does not recognize, or leaves the
/// reason after the separator empty.
pub fn parse_tdd_exception(raw: &str) -> Result<(TddException, String)> {
    let (category, reason) = raw.split_once(':').ok_or_else(|| Error::Policy {
        detail: format!("TDD exception section must be \"<category>: <reason>\", got {raw:?}"),
        paths: Vec::new(),
    })?;

    let category = category.trim();
    let exception = parse_exception_category(category).ok_or_else(|| Error::Policy {
        detail: format!(
            "unknown TDD exception category {category:?}: expected documentation, \
             pure-refactor, build-config or existing-failing-test"
        ),
        paths: Vec::new(),
    })?;

    let reason = reason.trim();
    if reason.is_empty() {
        return Err(Error::Policy {
            detail: "TDD exception section must include a reason".to_string(),
            paths: Vec::new(),
        });
    }

    Ok((exception, reason.to_string()))
}

/// Resolves the `tdd` protocol's declared exception for `task`, if any.
///
/// `scope_violation` reports whether a write-scope violation has already
/// been recorded for this attempt (typically the caller's own prior
/// [`check_scope`] result). An exception is a decision a task makes up
/// front, not an escape hatch discovered after the fact: if `scope_violation`
/// is `true`, a declared exception is rejected even though the task names
/// one, so a scope violation can never be claimed away after the fact.
///
/// # Errors
///
/// Returns [`Error::Policy`] if `scope_violation` is `true` and the task
/// declares an exception, or if the task's `**TDD-Exception:**` section
/// fails to parse (see [`parse_tdd_exception`]).
pub fn claim_tdd_exception(
    task: &Task,
    scope_violation: bool,
) -> Result<Option<(TddException, String)>> {
    let Some(raw) = exception_section(&task.body) else {
        return Ok(None);
    };

    if scope_violation {
        return Err(Error::Policy {
            detail: "a TDD exception cannot be claimed after a scope violation".to_string(),
            paths: Vec::new(),
        });
    }

    parse_tdd_exception(raw).map(Some)
}

/// Builds the protocol named `name`, or `None` if `name` is neither
/// `"direct"` nor `"tdd"` — the only two protocols v0.1 knows (VISION.md §9:
/// "Protocols are chosen per task; they are not user-definable in v1").
///
/// The sole source of the `direct`/`tdd` name mapping, so [`for_task`] and
/// [`crate::task::validate`]'s rejection of an unknown protocol name can
/// never disagree about which names are valid.
#[must_use]
pub(crate) fn by_name(name: &str) -> Option<Protocol> {
    match name {
        "direct" => Some(Protocol::direct()),
        "tdd" => Some(Protocol::tdd()),
        _ => None,
    }
}

/// Resolves `task`'s work protocol: its own `**Protocol:**` section if it
/// named one, otherwise `config.default_protocol` (which itself defaults to
/// `"direct"`, per `Config::default`).
///
/// # Errors
///
/// Returns [`Error::Policy`] if the resolved name is neither `direct` nor
/// `tdd`. A task's own protocol name is already rejected at `add` time by
/// [`crate::task::validate`] (VISION.md §9: "an unknown protocol name is
/// rejected when the task is added, not when it runs"), so in practice this
/// only fires when `config.default_protocol` itself is misconfigured.
pub fn for_task(task: &Task, config: &Config) -> Result<Protocol> {
    let name = task
        .protocol
        .as_deref()
        .unwrap_or(config.default_protocol.as_str());
    by_name(name).ok_or_else(|| Error::Policy {
        detail: format!("unknown work protocol {name:?}: expected \"direct\" or \"tdd\""),
        paths: Vec::new(),
    })
}

/// Fails if `changed` contains a path `scope` does not permit a phase to
/// write.
///
/// `changed` is the set of paths the phase's attempt actually modified —
/// callers get this from [`crate::changed_paths`] against the pre-phase git
/// state, never from the agent's own account of what it touched, since the
/// point of this check is to not have to trust that account. `test_globs`
/// supplies the patterns [`WriteScope::TestsOnly`] treats as test paths
/// (typically [`Config::test_globs`]), so what counts as a test file is
/// configurable per language profile rather than hard-coded here.
///
/// # Errors
///
/// Returns [`Error::Policy`] naming every offending path: for
/// [`WriteScope::None`], every entry in `changed`; for
/// [`WriteScope::TestsOnly`], every entry that matches none of `test_globs`.
/// [`WriteScope::All`] never fails.
pub fn check_scope(scope: WriteScope, changed: &[PathBuf], test_globs: &[String]) -> Result<()> {
    let (offending, detail): (Vec<PathBuf>, &str) = match scope {
        WriteScope::All => return Ok(()),
        WriteScope::None => (changed.to_vec(), "phase writes are not permitted"),
        WriteScope::TestsOnly => (
            changed
                .iter()
                .filter(|path| !matches_any_glob(path, test_globs))
                .cloned()
                .collect(),
            "phase may only write test paths",
        ),
    };

    if offending.is_empty() {
        return Ok(());
    }

    Err(Error::Policy {
        detail: detail.to_string(),
        paths: offending,
    })
}

/// Confirms the `tdd` protocol's red phase produced a genuinely new test
/// failure (VISION.md §9 step 2: "confirms the expected *new* failure").
///
/// Returns the names of tests present in `after.failures` but absent from
/// `before.failures` — the runner's evidence that the just-written test
/// actually exercises unimplemented behavior, rather than merely re-reporting
/// a failure the codebase already had (a stale baseline, a flaky test, or a
/// test that never ran at all). Order matches `after.failures`, deduplicated.
///
/// # Errors
///
/// Returns [`Error::Gate`] when the two failure sets are identical: nothing
/// new failed, so red-phase evidence would be indistinguishable from doing
/// nothing.
pub fn verify_red(before: &TestSummary, after: &TestSummary) -> Result<Vec<String>> {
    let before_failures: std::collections::HashSet<&str> =
        before.failures.iter().map(String::as_str).collect();

    let mut seen = std::collections::HashSet::new();
    let new_failures: Vec<String> = after
        .failures
        .iter()
        .filter(|name| !before_failures.contains(name.as_str()))
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect();

    if new_failures.is_empty() {
        return Err(Error::Gate {
            kind: "red".to_string(),
            detail: "no new test failure: the failure set is unchanged from before the red phase"
                .to_string(),
        });
    }

    Ok(new_failures)
}

/// Confirms the `tdd` protocol's green phase made every test named in
/// `expected` pass, without a regression elsewhere (VISION.md §9 step 4:
/// "the runner confirms the new test passes").
///
/// `expected` is the set of test names the red phase reported as newly
/// failing (typically [`verify_red`]'s return value) — the tests the green
/// phase's implementation work was meant to satisfy. Any other name present
/// in `after.failures` is a regression: by the time the red phase completed,
/// every test other than `expected` was passing, so a new failure among them
/// means the green phase broke something it was not supposed to touch.
///
/// # Errors
///
/// Returns [`Error::Gate`] naming the offending tests: for a name in
/// `expected` still present in `after.failures`, it did not turn green; for
/// a name in `after.failures` absent from `expected`, a previously passing
/// test regressed. Both kinds of offense are reported together in one error
/// so the runner does not need to guess which check to fix first.
pub fn verify_green(expected: &[String], after: &TestSummary) -> Result<()> {
    let after_failures: std::collections::HashSet<&str> =
        after.failures.iter().map(String::as_str).collect();
    let expected_set: std::collections::HashSet<&str> =
        expected.iter().map(String::as_str).collect();

    let mut seen = std::collections::HashSet::new();
    let still_red: Vec<String> = expected
        .iter()
        .filter(|name| after_failures.contains(name.as_str()))
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect();

    let mut seen = std::collections::HashSet::new();
    let regressed: Vec<String> = after
        .failures
        .iter()
        .filter(|name| !expected_set.contains(name.as_str()))
        .filter(|name| seen.insert(name.as_str()))
        .cloned()
        .collect();

    if still_red.is_empty() && regressed.is_empty() {
        return Ok(());
    }

    let mut parts = Vec::new();
    if !still_red.is_empty() {
        parts.push(format!("still failing: {}", still_red.join(", ")));
    }
    if !regressed.is_empty() {
        parts.push(format!("regressed: {}", regressed.join(", ")));
    }
    let detail = parts.join("; ");

    Err(Error::Gate {
        kind: "green".to_string(),
        detail,
    })
}

/// Reports whether `path` matches at least one glob in `globs`.
///
/// A glob that fails to compile is skipped rather than treated as an error,
/// matching how [`crate::redact::redact`] treats a malformed configured
/// pattern: one typo in a language profile's globs must not make every path
/// look like a violation.
fn matches_any_glob(path: &Path, globs: &[String]) -> bool {
    let path = path.to_string_lossy();
    globs.iter().any(|glob| glob_matches(glob, &path))
}

/// Reports whether `path` matches glob pattern `pattern`.
///
/// Supports `*` (any run of characters within one path segment), `**` (any
/// run of characters, including `/`, spanning whole segments) and `?` (one
/// character within a segment) — the subset `Config::test_globs`'s
/// documented defaults use (`"**/tests/**"`, `"**/*_test.rs"`,
/// `"src/**/tests.rs"`). Every other character is matched literally.
fn glob_matches(pattern: &str, path: &str) -> bool {
    match regex::Regex::new(&glob_to_regex(pattern)) {
        Ok(re) => re.is_match(path),
        Err(_) => false,
    }
}

/// Compiles a glob pattern into an anchored regex, segment by segment so
/// that a `**` segment can absorb the slash on either side of it (`"**/"` at
/// the start, `"/**"` at the end, `"/**/"` in the middle) — without that,
/// `"**/tests/**"` would fail to match a bare `"tests/foo.rs"` at the
/// repository root, since a literal `.*` either side of literal slashes
/// would demand a slash that is not there.
fn glob_to_regex(pattern: &str) -> String {
    let segments: Vec<&str> = pattern.split('/').collect();
    let last = segments.len().saturating_sub(1);

    let mut regex = String::from("^");
    let mut pending_slash = false;
    for (index, segment) in segments.iter().enumerate() {
        if *segment == "**" {
            if index == 0 && index == last {
                regex.push_str(".*");
            } else if index == 0 {
                regex.push_str("(?:.*/)?");
            } else if index == last {
                regex.push_str("(?:/.*)?");
            } else {
                regex.push_str("/(?:.*/)?");
            }
            pending_slash = false;
        } else {
            if pending_slash {
                regex.push('/');
            }
            regex.push_str(&segment_to_regex(segment));
            pending_slash = true;
        }
    }
    regex.push('$');
    regex
}

/// Converts one path segment of a glob pattern (no `/`) into the regex
/// fragment matching it: `*` becomes `[^/]*`, `?` becomes `[^/]`, and every
/// other character is escaped literally.
fn segment_to_regex(segment: &str) -> String {
    let mut out = String::new();
    for ch in segment.chars() {
        match ch {
            '*' => out.push_str("[^/]*"),
            '?' => out.push_str("[^/]"),
            other => out.push_str(&regex::escape(&other.to_string())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn last_two_phases(protocol: &Protocol) -> Vec<Phase> {
        protocol.phases[protocol.phases.len() - 2..]
            .iter()
            .map(|spec| spec.phase)
            .collect()
    }

    #[test]
    fn direct_ends_with_verify_then_publish() {
        assert_eq!(
            last_two_phases(&Protocol::direct()),
            vec![Phase::Verify, Phase::Publish]
        );
    }

    #[test]
    fn tdd_ends_with_verify_then_publish() {
        assert_eq!(
            last_two_phases(&Protocol::tdd()),
            vec![Phase::Verify, Phase::Publish]
        );
    }

    #[test]
    fn tdd_gates_red_and_green_on_the_targeted_command_before_verify() {
        let phases = Protocol::tdd().phases;
        let red = phases.iter().find(|s| s.phase == Phase::Red).unwrap();
        let green = phases.iter().find(|s| s.phase == Phase::Green).unwrap();
        assert_eq!(red.write_scope, WriteScope::TestsOnly);
        assert_eq!(red.gate, Some(GateKind::Targeted));
        assert_eq!(green.write_scope, WriteScope::All);
        assert_eq!(green.gate, Some(GateKind::Targeted));
    }

    #[test]
    #[should_panic(expected = "must end with Verify then Publish")]
    fn a_constructor_that_omits_verify_then_publish_panics() {
        checked(
            "broken",
            vec![PhaseSpec {
                phase: Phase::Implement,
                write_scope: WriteScope::All,
                gate: None,
                records_evidence: true,
            }],
        );
    }

    #[test]
    #[should_panic(expected = "must end with Verify then Publish")]
    fn a_constructor_that_puts_publish_before_verify_panics() {
        checked(
            "broken",
            vec![
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Verify,
                    write_scope: WriteScope::None,
                    gate: Some(GateKind::Verify),
                    records_evidence: true,
                },
            ],
        );
    }

    #[test]
    fn direct_and_tdd_have_distinct_names() {
        assert_eq!(Protocol::direct().name, "direct");
        assert_eq!(Protocol::tdd().name, "tdd");
    }

    fn task_naming_protocol(protocol: Option<&str>) -> Task {
        Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: "## Do the thing\n".to_string(),
            outcome: "it happens".to_string(),
            done_when: "it happened".to_string(),
            verify: "cargo test".to_string(),
            refs: "VISION.md".to_string(),
            protocol: protocol.map(str::to_string),
        }
    }

    #[test]
    fn by_name_recognizes_direct_and_tdd() {
        assert_eq!(by_name("direct"), Some(Protocol::direct()));
        assert_eq!(by_name("tdd"), Some(Protocol::tdd()));
    }

    #[test]
    fn by_name_rejects_anything_else() {
        assert_eq!(by_name("waterfall"), None);
        assert_eq!(by_name(""), None);
    }

    #[test]
    fn for_task_uses_the_tasks_own_protocol_when_named() {
        let task = task_naming_protocol(Some("tdd"));
        let config = Config::default();
        assert_eq!(config.default_protocol, "direct", "sanity: config default");

        let protocol = for_task(&task, &config).expect("known protocol");
        assert_eq!(protocol.name, "tdd");
    }

    #[test]
    fn for_task_falls_back_to_the_configured_default_when_the_task_names_none() {
        let task = task_naming_protocol(None);
        let mut config = Config::default();
        config.default_protocol = "tdd".to_string();

        let protocol = for_task(&task, &config).expect("known protocol");
        assert_eq!(protocol.name, "tdd");
    }

    #[test]
    fn for_task_falls_back_to_direct_when_neither_task_nor_config_names_one() {
        let task = task_naming_protocol(None);
        let config = Config::default();

        let protocol = for_task(&task, &config).expect("known protocol");
        assert_eq!(protocol.name, "direct");
    }

    #[test]
    fn for_task_rejects_an_unknown_configured_default() {
        let task = task_naming_protocol(None);
        let mut config = Config::default();
        config.default_protocol = "waterfall".to_string();

        let err = for_task(&task, &config).expect_err("unknown default must be rejected");
        assert!(err.to_string().contains("waterfall"));
    }

    #[test]
    fn for_task_prefers_the_tasks_own_protocol_over_an_unknown_configured_default() {
        let task = task_naming_protocol(Some("tdd"));
        let mut config = Config::default();
        config.default_protocol = "waterfall".to_string();

        let protocol = for_task(&task, &config).expect("task's own name wins");
        assert_eq!(protocol.name, "tdd");
    }

    fn paths(entries: &[&str]) -> Vec<PathBuf> {
        entries.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn all_permits_any_change() {
        let changed = paths(&["src/lib.rs", "tests/it.rs", "README.md"]);
        assert!(check_scope(WriteScope::All, &changed, &[]).is_ok());
    }

    #[test]
    fn none_permits_no_changes() {
        assert!(check_scope(WriteScope::None, &[], &[]).is_ok());
    }

    #[test]
    fn none_rejects_any_edit() {
        let changed = paths(&["crates/ktask-core/src/lib.rs"]);
        let err = check_scope(WriteScope::None, &changed, &[]).expect_err("must reject");
        assert!(
            matches!(&err, Error::Policy { paths, .. } if paths == &[PathBuf::from("crates/ktask-core/src/lib.rs")])
        );
    }

    #[test]
    fn none_rejects_even_a_test_edit() {
        let changed = paths(&["crates/ktask-core/tests/it.rs"]);
        let globs = Config::default().test_globs;
        let err = check_scope(WriteScope::None, &changed, &globs).expect_err("must reject");
        assert!(matches!(&err, Error::Policy { .. }));
    }

    #[test]
    fn tests_only_permits_a_test_edit() {
        let globs = Config::default().test_globs;
        let changed = paths(&["crates/ktask-core/tests/protocol_it.rs"]);
        assert!(check_scope(WriteScope::TestsOnly, &changed, &globs).is_ok());
    }

    #[test]
    fn tests_only_rejects_a_production_edit() {
        let globs = Config::default().test_globs;
        let changed = paths(&["crates/ktask-core/src/protocol.rs"]);
        let err = check_scope(WriteScope::TestsOnly, &changed, &globs).expect_err("must reject");
        assert!(
            matches!(&err, Error::Policy { paths, .. } if paths == &[PathBuf::from("crates/ktask-core/src/protocol.rs")])
        );
    }

    #[test]
    fn tests_only_names_only_the_offending_paths_among_a_mixed_change() {
        let globs = Config::default().test_globs;
        let changed = paths(&["crates/ktask-core/tests/protocol_it.rs", "src/main.rs"]);
        let err = check_scope(WriteScope::TestsOnly, &changed, &globs).expect_err("must reject");
        assert!(
            matches!(&err, Error::Policy { paths, .. } if paths == &[PathBuf::from("src/main.rs")])
        );
    }

    #[test]
    fn tests_only_globs_are_configurable_per_language() {
        // A Python profile's test paths look nothing like Rust's; the check
        // must honor whatever globs it is handed rather than a hard-coded
        // Rust convention.
        let python_globs = vec!["test_*.py".to_string(), "**/tests/**".to_string()];
        let changed = paths(&["test_widgets.py", "app/widgets.py"]);
        let err =
            check_scope(WriteScope::TestsOnly, &changed, &python_globs).expect_err("must reject");
        assert!(
            matches!(&err, Error::Policy { paths, .. } if paths == &[PathBuf::from("app/widgets.py")])
        );

        let all_tests = paths(&["test_widgets.py", "pkg/tests/helpers.py"]);
        assert!(check_scope(WriteScope::TestsOnly, &all_tests, &python_globs).is_ok());
    }

    #[test]
    fn glob_star_star_matches_at_the_start_of_a_path() {
        assert!(glob_matches("**/tests/**", "tests/foo.rs"));
        assert!(glob_matches(
            "**/tests/**",
            "crates/ktask-core/tests/foo.rs"
        ));
        assert!(!glob_matches("**/tests/**", "crates/ktask-core/src/lib.rs"));
    }

    #[test]
    fn glob_single_star_stays_within_one_segment() {
        assert!(glob_matches("**/*_test.rs", "widget_test.rs"));
        assert!(glob_matches("**/*_test.rs", "crates/foo/widget_test.rs"));
        assert!(!glob_matches(
            "**/*_test.rs",
            "crates/foo/widget_test_helpers.rs.bak"
        ));
    }

    #[test]
    fn glob_star_star_in_the_middle_matches_zero_or_more_directories() {
        assert!(glob_matches("src/**/tests.rs", "src/tests.rs"));
        assert!(glob_matches("src/**/tests.rs", "src/a/b/tests.rs"));
        assert!(!glob_matches("src/**/tests.rs", "lib/tests.rs"));
    }

    fn summary(failed: u32, failures: &[&str]) -> TestSummary {
        TestSummary {
            passed: 0,
            failed,
            ignored: 0,
            failures: failures.iter().map(ToString::to_string).collect(),
        }
    }

    #[test]
    fn verify_red_returns_a_failure_absent_before_the_red_phase() {
        let before = summary(0, &[]);
        let after = summary(1, &["widget::tests::rejects_a_bad_size"]);

        let new_failures = verify_red(&before, &after).expect("a new failure must be reported");
        assert_eq!(new_failures, vec!["widget::tests::rejects_a_bad_size"]);
    }

    #[test]
    fn verify_red_rejects_an_unchanged_failure_set() {
        let before = summary(1, &["widget::tests::flaky"]);
        let after = summary(1, &["widget::tests::flaky"]);

        let err = verify_red(&before, &after).expect_err("unchanged failures must be rejected");
        assert!(matches!(&err, Error::Gate { kind, .. } if kind == "red"));
    }

    #[test]
    fn verify_red_rejects_when_after_has_no_failures_at_all() {
        let before = summary(0, &[]);
        let after = summary(0, &[]);

        assert!(verify_red(&before, &after).is_err());
    }

    #[test]
    fn verify_red_ignores_a_pre_existing_failure_that_is_still_present() {
        let before = summary(1, &["widget::tests::pre_existing"]);
        let after = summary(
            2,
            &["widget::tests::pre_existing", "widget::tests::new_one"],
        );

        let new_failures = verify_red(&before, &after).expect("the new failure must be reported");
        assert_eq!(new_failures, vec!["widget::tests::new_one"]);
    }

    #[test]
    fn verify_red_deduplicates_a_repeated_new_failure_name() {
        // Cargo can print the same test name twice when a workspace runs the
        // same crate's tests under more than one target; the evidence must
        // name it once, not once per binary.
        let before = summary(0, &[]);
        let after = summary(2, &["widget::tests::new_one", "widget::tests::new_one"]);

        let new_failures = verify_red(&before, &after).expect("a new failure must be reported");
        assert_eq!(new_failures, vec!["widget::tests::new_one"]);
    }

    #[test]
    fn verify_green_passes_when_every_expected_test_now_passes() {
        let expected = vec!["widget::tests::rejects_a_bad_size".to_string()];
        let after = summary(0, &[]);

        assert!(verify_green(&expected, &after).is_ok());
    }

    #[test]
    fn verify_green_rejects_when_an_expected_test_is_still_failing() {
        let expected = vec!["widget::tests::rejects_a_bad_size".to_string()];
        let after = summary(1, &["widget::tests::rejects_a_bad_size"]);

        let err = verify_green(&expected, &after).expect_err("still-red test must be rejected");
        assert!(matches!(&err, Error::Gate { kind, .. } if kind == "green"));
        assert!(
            err.to_string()
                .contains("widget::tests::rejects_a_bad_size")
        );
    }

    #[test]
    fn verify_green_rejects_a_regression_in_an_unrelated_test() {
        let expected = vec!["widget::tests::new_one".to_string()];
        let after = summary(1, &["other::tests::broke"]);

        let err = verify_green(&expected, &after).expect_err("a regression must be rejected");
        assert!(matches!(&err, Error::Gate { kind, .. } if kind == "green"));
        assert!(err.to_string().contains("other::tests::broke"));
    }

    #[test]
    fn verify_green_deduplicates_a_repeated_regression_name() {
        let expected = vec!["widget::tests::new_one".to_string()];
        let after = summary(2, &["other::tests::broke", "other::tests::broke"]);

        let err = verify_green(&expected, &after).expect_err("a regression must be rejected");
        let Error::Gate { detail, .. } = &err else {
            panic!("expected Error::Gate");
        };
        assert_eq!(detail.matches("other::tests::broke").count(), 1);
    }

    #[test]
    fn parse_exception_category_recognizes_every_declared_category() {
        assert_eq!(
            parse_exception_category("documentation"),
            Some(TddException::Documentation)
        );
        assert_eq!(
            parse_exception_category("pure-refactor"),
            Some(TddException::PureRefactoring)
        );
        assert_eq!(
            parse_exception_category("build-config"),
            Some(TddException::BuildConfiguration)
        );
        assert_eq!(
            parse_exception_category("existing-failing-test"),
            Some(TddException::PreExistingFailingTest)
        );
    }

    #[test]
    fn parse_exception_category_rejects_an_unknown_category() {
        assert_eq!(parse_exception_category("vibes"), None);
        assert_eq!(parse_exception_category(""), None);
    }

    fn task_with_body(body: &str) -> Task {
        Task {
            id: crate::TaskId::new(1),
            status: crate::TaskStatus::Pending,
            body: body.to_string(),
            outcome: "it happens".to_string(),
            done_when: "it happened".to_string(),
            verify: "cargo test".to_string(),
            refs: "VISION.md".to_string(),
            protocol: Some("tdd".to_string()),
        }
    }

    #[test]
    fn parse_tdd_exception_reads_the_category_and_reason() {
        let (exception, reason) =
            parse_tdd_exception("documentation: README.md only, no code changed.")
                .expect("well-formed exception must parse");
        assert_eq!(exception, TddException::Documentation);
        assert_eq!(reason, "README.md only, no code changed.");
    }

    #[test]
    fn parse_tdd_exception_rejects_a_missing_separator() {
        let err = parse_tdd_exception("documentation only").expect_err("must reject");
        assert!(matches!(&err, Error::Policy { .. }));
    }

    #[test]
    fn parse_tdd_exception_rejects_an_unknown_category() {
        let err = parse_tdd_exception("vibes: trust me").expect_err("must reject");
        let Error::Policy { detail, .. } = &err else {
            panic!("expected Error::Policy");
        };
        assert!(detail.contains("vibes"));
    }

    #[test]
    fn parse_tdd_exception_rejects_an_empty_reason() {
        let err = parse_tdd_exception("documentation:   ").expect_err("must reject");
        assert!(matches!(&err, Error::Policy { .. }));
    }

    #[test]
    fn claim_tdd_exception_returns_none_when_the_task_names_no_exception() {
        let task = task_with_body("## Do the thing\n\n**Outcome:** it happens.\n");
        assert_eq!(
            claim_tdd_exception(&task, false).expect("no exception"),
            None
        );
    }

    #[test]
    fn claim_tdd_exception_returns_none_when_scope_was_violated_but_none_is_named() {
        // A scope violation only matters once a task actually claims an
        // exception; a task naming none has nothing to launder.
        let task = task_with_body("## Do the thing\n\n**Outcome:** it happens.\n");
        assert_eq!(
            claim_tdd_exception(&task, true).expect("no exception"),
            None
        );
    }

    #[test]
    fn claim_tdd_exception_returns_the_declared_category_and_reason() {
        let task = task_with_body(
            "## Do the thing\n\n\
             **Outcome:** it happens.\n\n\
             **TDD-Exception:** pure-refactor: renaming a method, no behavior change.\n",
        );
        let (exception, reason) = claim_tdd_exception(&task, false)
            .expect("must parse")
            .expect("must be Some");
        assert_eq!(exception, TddException::PureRefactoring);
        assert_eq!(reason, "renaming a method, no behavior change.");
    }

    #[test]
    fn claim_tdd_exception_rejects_an_exception_claimed_after_a_scope_violation() {
        let task = task_with_body(
            "## Do the thing\n\n\
             **TDD-Exception:** documentation: just docs.\n",
        );
        let err = claim_tdd_exception(&task, true).expect_err("must reject");
        let Error::Policy { detail, .. } = &err else {
            panic!("expected Error::Policy");
        };
        assert!(detail.contains("scope violation"));
    }

    #[test]
    fn claim_tdd_exception_propagates_a_malformed_section_even_without_a_scope_violation() {
        let task = task_with_body(
            "## Do the thing\n\n\
             **TDD-Exception:** not-a-real-category: whatever\n",
        );
        let err = claim_tdd_exception(&task, false).expect_err("must reject");
        assert!(matches!(&err, Error::Policy { .. }));
    }
}
