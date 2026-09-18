//! The gates a task is proved against, as typed configuration.
//!
//! A gate is a runner-executed command with a timeout, a directory to run in and
//! an environment to run with (VISION.md §8) — never a loose string another
//! component has to interpret. `docs/DESIGN.md` names the kinds under *Core
//! types* and VISION.md §8 names the same set as the commands a verification
//! profile holds. [`GateKind`] is the eight that leaves, and
//! `docs/adr/0034-the-gate-kind-set-is-what-the-documents-name.md` records why
//! that is the set rather than the count the task prompt gave.
//!
//! A [`Profile`] is the gates one project runs, and it is loaded rather than
//! assembled by whoever happens to need a gate: a profile missing the mandatory
//! `Verify` gate is refused at load time, so "verification was configured out"
//! is not a state the runner can ever reach. [`Profile::get`] is the only way to
//! reach a gate, and a kind answers with at most one — the identity of a gate is
//! its kind.
//!
//! Reading and writing go through TOML, the format every configuration document
//! in this project uses. A profile is read from text rather than from a path
//! because the document that carries it is a project's configuration document,
//! whose location [`crate::config`] owns.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Which mechanical check a gate performs.
///
/// The kind, not the command, is what identifies a gate: an event names the
/// gate that started by its kind, a failure names the gate that refused by its
/// kind, and a human acknowledges a gate by kind. That is why the set is an
/// enum rather than a string a configuration file is free to get wrong.
///
/// The set is what the documents name. [`GateKind::Baseline`] through
/// [`GateKind::Privacy`] are the seven spelled out under *Core types* in
/// `docs/DESIGN.md`; [`GateKind::Flake`] is the eighth, which VISION.md §8
/// lists among the commands a verification profile holds and which
/// `docs/DESIGN.md` already carries a setting for (`flake_runs`). The kind a
/// later task will call `targeted_test_command` is [`GateKind::Targeted`], the
/// name `docs/DESIGN.md` spells for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GateKind {
    /// Prove the project was green before the task started, so a failure later
    /// can be attributed to the work rather than inherited from it.
    Baseline,
    /// The fast check of an edit loop: the tests the change touches, run while
    /// the agent is still working rather than after it stopped.
    Targeted,
    /// The complete local suite. Mandatory: no profile is loadable without it,
    /// because a task is never done on an agent's say-so.
    Verify,
    /// The lints, run by the runner rather than trusted from a report.
    Lint,
    /// The formatting check.
    Format,
    /// The build, including every target.
    Build,
    /// The privacy scan: staged files, tracked files, and the outgoing commit
    /// range checked for forbidden paths and content patterns.
    Privacy,
    /// The affected tests run repeatedly or in a randomized order, to surface a
    /// test that passes once and fails on the fifth run.
    Flake,
}

impl GateKind {
    /// Every gate kind, in the order `docs/DESIGN.md` lists them.
    ///
    /// The ledger the tests count against: a kind added or removed without
    /// being named in a document moves this array and fails them.
    pub const ALL: [GateKind; 8] = [
        Self::Baseline,
        Self::Targeted,
        Self::Verify,
        Self::Lint,
        Self::Format,
        Self::Build,
        Self::Privacy,
        Self::Flake,
    ];

    /// The kind as an operator writes it on a command line and reads in a
    /// failure: lower-case, one word.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Baseline => "baseline",
            Self::Targeted => "targeted",
            Self::Verify => "verify",
            Self::Lint => "lint",
            Self::Format => "format",
            Self::Build => "build",
            Self::Privacy => "privacy",
            Self::Flake => "flake",
        }
    }
}

impl fmt::Display for GateKind {
    /// The lower-case word, which is what [`crate::Error::Gate`] holds as its
    /// `kind` text (ADR-0001) and what `rerun-gate --gate` names a gate by.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One mechanical check: the command to run and the conditions it must run
/// under. Each gate carries its own timeout, directory and environment, because
/// a privacy scan over the repository and a cold workspace build are not the
/// same job and must not be given the same budget.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Gate {
    /// Which check this is. A profile holds at most one gate per kind.
    pub kind: GateKind,
    /// The program and its arguments, as separate words. A command is stored
    /// split because splitting a string is a decision about quoting, and the
    /// configuration is where that decision should have been made already.
    pub command: Vec<String>,
    /// How long this command may run before it is killed and the gate reported
    /// as timed out rather than as failed.
    pub timeout_secs: u64,
    /// The directory to run in, or `None` for the project root the run was
    /// given. A gate that checks one crate says so here rather than by
    /// beginning its command with `cd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// Variables set for this command alone, on top of the environment the
    /// supervisor passes down. Ordered, so a written profile is byte-identical
    /// to the one it was read from.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// The gates one project runs, in the order they run.
///
/// A profile is loaded, not assembled at the point of use: the rules below are
/// what make "the gates were configured out" impossible rather than unlikely.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// The configured gates, in the order a run executes them. Empty is
    /// readable but never loadable: [`Profile::validate`] refuses it for the
    /// missing mandatory gate, which is the answer an operator can act on.
    #[serde(default)]
    pub gates: Vec<Gate>,
}

impl Profile {
    /// The gate configured for `kind`, or `None` when the project configured
    /// none. Never a default standing in for a gate nobody set: a caller that
    /// needs one has to say what happens without it.
    #[must_use]
    pub fn get(&self, kind: GateKind) -> Option<&Gate> {
        self.gates.iter().find(|gate| gate.kind == kind)
    }

    /// Reads a profile from a TOML document and applies [`Profile::validate`]
    /// to it, so a document that sets a partial set of gates is refused before
    /// any task is started rather than discovered at verification time.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for a document that is not a profile — malformed TOML,
    /// a key a gate or a profile does not have, a gate kind nobody defined —
    /// and for a profile that breaks one of the rules [`Profile::validate`]
    /// holds.
    pub fn from_toml(document: &str) -> Result<Self> {
        let profile: Self = toml::from_str(document).map_err(|error| Error::Config {
            key: "profile".to_owned(),
            detail: format!("the verification profile is not readable: {error}"),
        })?;
        profile.validate()?;
        Ok(profile)
    }

    /// Writes the profile as a TOML document, the form a project's
    /// configuration document holds it in.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a field holds something TOML cannot write, which
    /// for a profile means a path or a variable name that is not text.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string(self).map_err(|error| Error::Config {
            key: "profile".to_owned(),
            detail: format!("the verification profile cannot be written as TOML: {error}"),
        })
    }

    /// Refuses a profile that could not decide what `done` means.
    ///
    /// Two rules, both structural rather than a matter of taste. The mandatory
    /// [`GateKind::Verify`] gate has to be there: VISION.md §8 makes the
    /// complete local suite non-optional, so a profile is not free to omit it,
    /// and the strictness belongs here rather than in every caller that would
    /// otherwise have to remember the rule. And one kind answers to one gate:
    /// [`Profile::get`] promises a single gate per kind, and a profile holding
    /// two for one kind would have that promise decided by table order.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] naming the first rule broken: a kind configured more
    /// than once, or the missing mandatory gate.
    pub fn validate(&self) -> Result<()> {
        for kind in GateKind::ALL {
            let configured = self.gates.iter().filter(|gate| gate.kind == kind).count();
            if configured > 1 {
                return Err(Error::Config {
                    key: "gates".to_owned(),
                    detail: format!(
                        "the `{kind}` gate is configured {configured} times; a kind answers to \
                         one gate"
                    ),
                });
            }
        }
        if self.get(GateKind::Verify).is_none() {
            return Err(Error::Config {
                key: "gates".to_owned(),
                detail: format!(
                    "no `{}` gate is configured; the complete local suite is mandatory and \
                     cannot be configured out",
                    GateKind::Verify
                ),
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Gate, GateKind, Profile};
    use crate::Error;
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    /// One gate, spelled the way a TOML document spells it.
    fn gate_document(kind: GateKind, command: &str) -> String {
        format!("[[gates]]\nkind = \"{kind:?}\"\ncommand = [{command:?}]\ntimeout_secs = 1800\n")
    }

    /// A document holding `gates` and nothing else.
    fn profile_document(gates: impl IntoIterator<Item = (GateKind, &'static str)>) -> String {
        gates
            .into_iter()
            .map(|(kind, command)| gate_document(kind, command))
            .collect()
    }

    /// The mandatory gate, as a value rather than as a document.
    fn verify() -> Gate {
        Gate {
            kind: GateKind::Verify,
            command: vec!["./scripts/quality.sh".to_owned()],
            timeout_secs: 1800,
            working_dir: None,
            env: BTreeMap::new(),
        }
    }

    /// A profile assembled rather than loaded.
    fn assembled(gates: Vec<Gate>) -> Profile {
        Profile { gates }
    }

    /// Loads a profile, failing the test with the refusal if it will not load.
    fn loaded(document: &str) -> Profile {
        Profile::from_toml(document).unwrap_or_else(|error| {
            panic!("a profile carrying the mandatory gate must load, refused with: {error}")
        })
    }

    #[test]
    fn the_gate_kinds_are_the_ones_the_documents_name() {
        let documented = [
            GateKind::Baseline,
            GateKind::Targeted,
            GateKind::Verify,
            GateKind::Lint,
            GateKind::Format,
            GateKind::Build,
            GateKind::Privacy,
            GateKind::Flake,
        ];
        assert_eq!(GateKind::ALL, documented);
    }

    #[test]
    fn each_gate_kind_prints_the_word_an_operator_writes() {
        let spelled: Vec<String> = GateKind::ALL.iter().map(ToString::to_string).collect();
        assert_eq!(
            spelled,
            [
                "baseline", "targeted", "verify", "lint", "format", "build", "privacy", "flake",
            ]
        );
    }

    #[test]
    fn a_gate_kind_survives_the_encoding_the_journal_uses() {
        for kind in GateKind::ALL {
            let text = serde_json::to_string(&kind).expect("a gate kind is writable as JSON");
            let read_back: GateKind = serde_json::from_str(&text)
                .expect("a written gate kind reads back as the kind it was written from");
            assert_eq!(read_back, kind);
        }
        assert_eq!(
            serde_json::to_string(&GateKind::Targeted).unwrap(),
            "\"Targeted\"",
            "a kind is written by its variant name, the way every other stored enum here is"
        );
    }

    #[test]
    fn a_kind_the_type_does_not_have_is_refused_rather_than_held_as_text() {
        let document = "[[gates]]\nkind = \"made_up\"\ncommand = [\"true\"]\ntimeout_secs = 1\n";
        let error =
            Profile::from_toml(document).expect_err("a gate kind nobody defined is not a gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("made_up")),
            "an unknown gate kind must be refused, naming the kind it refused: {error}"
        );
    }

    #[test]
    fn a_profile_hands_back_the_gate_configured_for_a_kind_and_only_that_one() {
        let document = profile_document([
            (GateKind::Baseline, "./scripts/check-prereqs.sh"),
            (GateKind::Verify, "./scripts/quality.sh"),
        ]);
        let profile = loaded(&document);

        assert_eq!(
            profile.get(GateKind::Baseline),
            Some(&Gate {
                kind: GateKind::Baseline,
                command: vec!["./scripts/check-prereqs.sh".to_owned()],
                timeout_secs: 1800,
                working_dir: None,
                env: BTreeMap::new(),
            })
        );
        assert_eq!(profile.get(GateKind::Verify), Some(&verify()));
        assert_eq!(
            profile.get(GateKind::Flake),
            None,
            "a gate nobody configured must not be invented from a default"
        );
    }

    #[test]
    fn a_profile_without_the_mandatory_verify_gate_is_refused_whatever_it_holds() {
        for kind in GateKind::ALL {
            if kind == GateKind::Verify {
                continue;
            }
            let error = Profile::from_toml(&profile_document([(kind, "true")]))
                .expect_err("a profile with no verify gate is not loadable");
            assert!(
                matches!(error, Error::Config { ref key, ref detail }
                    if key == "gates" && detail.contains("verify")),
                "a missing verify gate must be named as the missing gate, was: {error}"
            );
        }
    }

    #[test]
    fn a_profile_that_sets_no_gates_at_all_is_refused_by_the_same_rule() {
        let error = Profile::from_toml("").expect_err("an empty profile holds no verify gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("verify")),
            "an empty profile must be refused for the reason an operator can act \
             on, was: {error}"
        );
    }

    #[test]
    fn the_mandatory_rule_holds_for_an_assembled_profile_too() {
        assert!(
            Profile::validate(&assembled(vec![verify()])).is_ok(),
            "a profile holding the mandatory gate is valid"
        );

        let lint = Gate {
            kind: GateKind::Lint,
            ..verify()
        };
        let error = Profile::validate(&assembled(vec![lint]))
            .expect_err("an assembled profile is no more excused than a loaded one");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("verify")),
            "the refusal must name the gate that is missing, was: {error}"
        );
    }

    #[test]
    fn a_kind_configured_twice_is_refused_because_one_gate_answers_to_it() {
        let document = profile_document([
            (GateKind::Verify, "true"),
            (GateKind::Lint, "cargo clippy"),
            (GateKind::Lint, "cargo clippy --fix"),
        ]);
        let error =
            Profile::from_toml(&document).expect_err("two gates cannot both be the lint gate");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "gates" && detail.contains("lint")),
            "the refusal must name the kind that was configured twice, was: {error}"
        );
    }

    #[test]
    fn every_field_of_a_profile_survives_a_trip_through_toml() {
        let mut env = BTreeMap::new();
        env.insert("RUST_BACKTRACE".to_owned(), "1".to_owned());
        env.insert("CARGO_PROFILE_OVERRIDE".to_owned(), "off".to_owned());
        let written = assembled(vec![
            Gate {
                kind: GateKind::Privacy,
                command: vec!["git".to_owned(), "diff".to_owned(), "--check".to_owned()],
                timeout_secs: 30,
                working_dir: Some(PathBuf::from("/repo")),
                env: env.clone(),
            },
            verify(),
        ]);

        let document = written.to_toml().expect("a profile is writable as TOML");
        let read_back = loaded(&document);
        assert_eq!(read_back, written);
        assert_eq!(
            read_back.get(GateKind::Privacy).map(|gate| &gate.env),
            Some(&env),
            "an environment that arrives reordered or dropped is not the environment that was set"
        );
    }

    #[test]
    fn a_gate_written_in_toml_holds_its_own_timeout_directory_and_environment() {
        let document = r#"
[[gates]]
kind = "Flake"
command = ["cargo", "nextest", "run", "--repeated-count", "5"]
timeout_secs = 900
working_dir = "crates/ktask-core"
env = { KTASK_SEED = "7" }

[[gates]]
kind = "Verify"
command = ["./scripts/quality.sh"]
timeout_secs = 1800
"#;
        let profile = loaded(document);
        let flake = profile.get(GateKind::Flake).expect("the flake gate is set");
        assert_eq!(flake.command[3], "--repeated-count");
        assert_eq!(flake.command[4], "5");
        assert_eq!(flake.timeout_secs, 900);
        assert_eq!(
            flake.working_dir.as_deref(),
            Some(Path::new("crates/ktask-core"))
        );
        assert_eq!(flake.env.get("KTASK_SEED").map(String::as_str), Some("7"));
    }

    #[test]
    fn a_gate_may_leave_out_the_optional_keys_and_reads_them_back_as_none() {
        let profile = loaded(&profile_document([
            (GateKind::Format, "cargo"),
            (GateKind::Verify, "./scripts/quality.sh"),
        ]));
        let format = profile
            .get(GateKind::Format)
            .expect("the format gate is set");
        assert_eq!(format.working_dir, None);
        assert!(format.env.is_empty());
    }

    #[test]
    fn a_field_that_is_not_a_gate_field_is_refused() {
        let document = gate_document(GateKind::Verify, "true") + "retries = 3\n";
        let error =
            Profile::from_toml(&document).expect_err("a gate does not grow a field nobody defined");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("retries")),
            "an unknown gate field must be refused, naming the field it refused: {error}"
        );
    }

    #[test]
    fn a_key_that_is_not_a_profile_field_is_refused() {
        // Written first rather than appended: a bare key after a table header
        // belongs to that table, and this one has to be the profile's own.
        let document =
            "flake_runs = 9\n".to_owned() + &profile_document([(GateKind::Verify, "true")]);
        let error = Profile::from_toml(&document)
            .expect_err("a profile does not grow a key nobody defined either");
        assert!(
            matches!(error, Error::Config { ref key, ref detail }
                if key == "profile" && detail.contains("flake_runs")),
            "an unknown profile key must be refused, naming the key it refused: {error}"
        );
    }

    #[test]
    fn a_profile_is_written_with_its_gates_in_the_order_it_holds_them() {
        let written = assembled(vec![
            Gate {
                kind: GateKind::Lint,
                ..verify()
            },
            verify(),
        ]);
        let document = written.to_toml().expect("a profile is writable as TOML");
        let value: toml::Value = toml::from_str(&document).expect("a profile writes valid TOML");
        let gates = value
            .get("gates")
            .and_then(toml::Value::as_array)
            .expect("a profile writes its gates as a table array");
        assert_eq!(gates.len(), 2);
        assert_eq!(
            gates[0].get("kind").and_then(toml::Value::as_str),
            Some("Lint"),
            "the order a profile holds its gates in is the order it runs them in"
        );
        assert_eq!(
            gates[1].get("kind").and_then(toml::Value::as_str),
            Some("Verify")
        );
    }

    #[test]
    fn a_gate_writes_only_the_keys_it_actually_sets() {
        let document = assembled(vec![verify()])
            .to_toml()
            .expect("a profile is writable as TOML");
        let value: toml::Value = toml::from_str(&document).expect("a profile writes valid TOML");
        let gate = &value
            .get("gates")
            .and_then(toml::Value::as_array)
            .expect("a profile writes its gates as a table array")[0];
        assert_eq!(
            gate.as_table()
                .expect("a gate writes as a table")
                .keys()
                .collect::<Vec<_>>(),
            ["command", "kind", "timeout_secs"],
            "a gate that writes an empty environment and a null directory reads back as              having set them: {document}"
        );
    }

    #[test]
    fn a_command_the_gate_was_given_survives_as_the_words_it_was_given() {
        let document = gate_document(GateKind::Build, "cargo build --workspace --locked")
            + &profile_document([(GateKind::Verify, "true")]);
        let profile = loaded(&document);
        let build = profile.get(GateKind::Build).expect("the build gate is set");
        assert_eq!(
            build.command,
            vec!["cargo build --workspace --locked".to_owned()],
            "a command is the words it was written with, not a string split by whoever runs it"
        );
    }
}
