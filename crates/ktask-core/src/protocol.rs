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

use crate::{Config, Error, GateKind, Phase, Result, Task};

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
}
