use super::*;
use crate::fakes::at;

const TRANSPORT_STDERR: &str = "Reconnecting... 5/5\n\
ERROR: stream disconnected before completion: Transport error: error decoding response body\n";

fn facts(signals: &Signals) -> Facts<'_> {
    Facts {
        status: TaskStatus::FailedUnknown,
        exit_code: Some(1),
        reason: Some("the provider exited with code 1 and reported nothing"),
        reported: None,
        signals,
        retried: 0,
        transport_retries: 3,
        now: at(1_000),
        decider: false,
    }
}

fn decision(route: Option<Route>) -> Decision {
    match route {
        Some(Route::Decide(decision)) => decision,
        other => panic!("expected a decision, got {other:?}"),
    }
}

#[test]
fn a_rate_limit_waits_until_the_providers_own_reset_time() {
    let signals = Signals {
        limit: Some(LimitSignal {
            reset_at: Some(at(5_000)),
        }),
        ..Signals::default()
    };
    assert_eq!(
        route(&facts(&signals)),
        Some(Route::Wait { until: at(5_000) })
    );
}

#[test]
fn a_rate_limit_naming_no_reset_waits_the_default_back_off() {
    let signals = Signals {
        limit: Some(LimitSignal { reset_at: None }),
        ..Signals::default()
    };
    assert_eq!(
        route(&facts(&signals)),
        Some(Route::Wait {
            until: at(1_000) + DEFAULT_LIMIT_BACKOFF
        })
    );
}

#[test]
fn a_kill_at_the_time_limit_goes_to_the_decider_with_the_facts() {
    let signals = Signals {
        killed: Some(Killed {
            after: Duration::from_mins(30),
            last_output_ago: Some(Duration::from_mins(12)),
            tail: "compiling\nstill compiling".to_owned(),
        }),
        ..Signals::default()
    };
    let decision = decision(route(&facts(&signals)));
    assert_eq!(decision.why, DecideWhy::TimeLimit);
    assert_eq!(
        decision.reason.as_deref(),
        Some("killed after 30 min, last output 12 min ago")
    );
    let detail = decision.detail.expect("the facts for the prompt");
    assert!(detail.contains("still compiling"), "{detail}");
}

#[test]
fn a_kill_with_no_output_says_so() {
    let signals = Signals {
        killed: Some(Killed {
            after: Duration::from_secs(30),
            ..Killed::default()
        }),
        ..Signals::default()
    };
    let reason = decision(route(&facts(&signals))).reason;
    assert_eq!(reason.as_deref(), Some("killed after 30 s, no output seen"));
}

#[test]
fn a_transport_failure_retries_with_the_growing_back_off() {
    let signals = Signals {
        stderr: TRANSPORT_STDERR.to_owned(),
        ..Signals::default()
    };
    let mut facts = facts(&signals);
    let mut seen = Vec::new();
    for retried in 0..2 {
        facts.retried = retried;
        seen.push(route(&facts));
    }
    assert_eq!(
        seen,
        [
            Some(Route::Retry {
                n: 1,
                of: 3,
                after: Duration::from_secs(1)
            }),
            Some(Route::Retry {
                n: 2,
                of: 3,
                after: Duration::from_secs(2)
            }),
        ]
    );
}

#[test]
fn exhausted_transport_retries_go_to_the_decider_naming_count_and_cause_once() {
    let signals = Signals {
        stderr: TRANSPORT_STDERR.to_owned(),
        ..Signals::default()
    };
    let mut facts = facts(&signals);
    facts.retried = 2;
    let decision = decision(route(&facts));
    assert_eq!(decision.why, DecideWhy::RetriesExhausted);
    let reason = decision.reason.expect("a reason");
    assert_eq!(
        reason,
        "Codex transport failed 3 consecutive times: stream disconnected before completion: \
         Transport error: error decoding response body"
    );
    assert!(
        decision
            .detail
            .expect("detail")
            .contains("3 transport retries"),
    );
}

#[test]
fn an_environment_fault_stops_with_its_fix() {
    let signals = Signals::default();
    let mut facts = facts(&signals);
    facts.exit_code = Some(127);
    let Some(Route::Stop { cause, reason }) = route(&facts) else {
        panic!("expected a stop");
    };
    assert_eq!(cause, StopCause::ProgramNotFound);
    assert!(reason.contains("run again"), "{reason}");
}

#[test]
fn an_agent_that_reported_failed_goes_to_the_decider() {
    let signals = Signals::default();
    let mut facts = facts(&signals);
    facts.status = TaskStatus::Failed;
    facts.reported = Some(Outcome::Failed);
    assert_eq!(decision(route(&facts)).why, DecideWhy::AgentFailed);
}

#[test]
fn a_review_or_test_that_turned_the_work_down_goes_to_the_decider() {
    let signals = Signals::default();
    let mut facts = facts(&signals);
    facts.status = TaskStatus::Failed;
    for reported in [Outcome::ChangesRequested, Outcome::Rejected] {
        facts.reported = Some(reported);
        assert_eq!(decision(route(&facts)).why, DecideWhy::Rejected);
    }
}

#[test]
fn anything_unmatched_goes_to_the_decider() {
    let signals = Signals::default();
    assert_eq!(decision(route(&facts(&signals))).why, DecideWhy::Unmatched);
}

#[test]
fn a_step_that_did_not_fail_is_not_routed() {
    let signals = Signals::default();
    let mut facts = facts(&signals);
    for status in [TaskStatus::Done, TaskStatus::Blocked, TaskStatus::Skipped] {
        facts.status = status;
        assert_eq!(route(&facts), None);
    }
}

#[test]
fn a_rate_limit_outranks_every_other_rule() {
    let signals = Signals {
        limit: Some(LimitSignal { reset_at: None }),
        stderr: TRANSPORT_STDERR.to_owned(),
        killed: Some(Killed::default()),
    };
    assert!(matches!(route(&facts(&signals)), Some(Route::Wait { .. })));
}

#[test]
fn the_decider_is_never_handed_to_the_decider_but_still_waits_and_stops() {
    let signals = Signals::default();
    let mut ending = facts(&signals);
    ending.decider = true;
    assert_eq!(route(&ending), None);

    ending.reason = Some("git identity is not configured");
    ending.exit_code = None;
    assert!(matches!(route(&ending), Some(Route::Stop { .. })));

    let limited = Signals {
        limit: Some(LimitSignal { reset_at: None }),
        ..Signals::default()
    };
    let mut ending = facts(&limited);
    ending.decider = true;
    assert!(matches!(route(&ending), Some(Route::Wait { .. })));
}
