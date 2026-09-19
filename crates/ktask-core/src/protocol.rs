//! Work protocols: per-task state machines defining sequence of phases.

use crate::gate::GateKind;
use crate::state::Phase;
use serde::{Deserialize, Serialize};

/// Write scope for a phase, determining what the agent can modify.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteScope {
    /// Agent can modify all paths.
    All,
    /// Agent can modify only test-related paths.
    TestsOnly,
    /// Agent cannot modify any paths (read-only).
    None,
}

/// Specification of a single phase in a protocol.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseSpec {
    /// The phase identifier.
    pub phase: Phase,
    /// Write scope for this phase.
    pub write_scope: WriteScope,
    /// Optional gate to run at the end of this phase.
    pub gate: Option<GateKind>,
    /// Whether this phase records evidence.
    pub records_evidence: bool,
}

/// A work protocol: a typed sequence of phases with constraints.
///
/// Every protocol must end with Verify then Publish phases.
/// This is enforced by the constructors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Protocol {
    /// Human-readable name of the protocol.
    pub name: String,
    /// Ordered sequence of phases in the protocol.
    pub phases: Vec<PhaseSpec>,
}

impl Protocol {
    /// The `direct` protocol: single implementation phase, then mandatory gates.
    ///
    /// Direct workflow: implement → verify → publish
    #[must_use]
    pub fn direct() -> Protocol {
        let p = Protocol {
            name: "direct".to_string(),
            phases: vec![
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
        };
        debug_assert!(
            p.validate(),
            "direct protocol must end with Verify then Publish"
        );
        p
    }

    /// The `tdd` protocol: test-driven development with red/green/refactor.
    ///
    /// TDD workflow:
    /// 1. Red: write failing tests
    /// 2. Green: implement to pass tests
    /// 3. Refactor: cleanup while keeping tests passing
    /// 4. Verify and publish
    #[must_use]
    pub fn tdd() -> Protocol {
        let p = Protocol {
            name: "tdd".to_string(),
            phases: vec![
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
        };
        debug_assert!(
            p.validate(),
            "tdd protocol must end with Verify then Publish"
        );
        p
    }

    /// Validates that the protocol ends with Verify then Publish phases.
    fn validate(&self) -> bool {
        match (
            self.phases.get(self.phases.len().saturating_sub(2)),
            self.phases.last(),
        ) {
            (Some(second_last), Some(last)) => {
                second_last.phase == Phase::Verify && last.phase == Phase::Publish
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_direct_ends_with_verify_publish() {
        let p = Protocol::direct();
        assert_eq!(p.phases.len(), 3);
        assert_eq!(p.phases[1].phase, Phase::Verify);
        assert_eq!(p.phases[2].phase, Phase::Publish);
        assert!(p.validate());
    }

    #[test]
    fn protocol_tdd_ends_with_verify_publish() {
        let p = Protocol::tdd();
        assert!(p.phases.len() >= 2);
        let phases = &p.phases;
        assert_eq!(phases[phases.len() - 2].phase, Phase::Verify);
        assert_eq!(phases[phases.len() - 1].phase, Phase::Publish);
        assert!(p.validate());
    }

    #[test]
    fn protocol_missing_verify_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
                PhaseSpec {
                    phase: Phase::Implement,
                    write_scope: WriteScope::All,
                    gate: None,
                    records_evidence: true,
                },
                PhaseSpec {
                    phase: Phase::Publish,
                    write_scope: WriteScope::None,
                    gate: None,
                    records_evidence: true,
                },
            ],
        };
        assert!(!p.validate());
    }

    #[test]
    fn protocol_missing_publish_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
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
            ],
        };
        assert!(!p.validate());
    }

    #[test]
    fn protocol_verify_publish_out_of_order_fails_validation() {
        let p = Protocol {
            name: "invalid".to_string(),
            phases: vec![
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
        };
        assert!(!p.validate());
    }

    #[test]
    fn phase_spec_round_trips_through_json() {
        let spec = PhaseSpec {
            phase: Phase::Red,
            write_scope: WriteScope::TestsOnly,
            gate: Some(GateKind::Targeted),
            records_evidence: true,
        };
        let json = serde_json::to_string(&spec).unwrap();
        let deserialized: PhaseSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, deserialized);
    }

    #[test]
    fn write_scope_round_trips_through_json() {
        for scope in &[WriteScope::All, WriteScope::TestsOnly, WriteScope::None] {
            let json = serde_json::to_string(scope).unwrap();
            let deserialized: WriteScope = serde_json::from_str(&json).unwrap();
            assert_eq!(*scope, deserialized);
        }
    }

    #[test]
    fn protocol_round_trips_through_json() {
        let p = Protocol::tdd();
        let value = serde_json::to_value(&p).unwrap();
        let deserialized: Protocol = serde_json::from_value(value).unwrap();
        assert_eq!(p, deserialized);
    }

    #[test]
    fn protocol_direct_has_implement_verify_publish() {
        let p = Protocol::direct();
        assert_eq!(p.phases[0].phase, Phase::Implement);
        assert_eq!(p.phases[1].phase, Phase::Verify);
        assert_eq!(p.phases[2].phase, Phase::Publish);
    }

    #[test]
    fn protocol_tdd_has_red_green_refactor() {
        let p = Protocol::tdd();
        let phases: Vec<_> = p.phases.iter().map(|spec| spec.phase).collect();
        assert!(phases.contains(&Phase::Red));
        assert!(phases.contains(&Phase::Green));
        assert!(phases.contains(&Phase::Refactor));
    }

    #[test]
    fn phase_spec_write_scopes() {
        let p = Protocol::direct();
        assert_eq!(p.phases[0].write_scope, WriteScope::All);
        assert_eq!(p.phases[1].write_scope, WriteScope::None);
        assert_eq!(p.phases[2].write_scope, WriteScope::None);
    }
}
