//! The verdict a step's ending was routed to, as the journal keeps it and a frontend shows it.

use super::StopCause;

/// Why a failure went to the decider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecideWhy {
    /// The provider was killed at the attempt's time limit.
    TimeLimit,
    /// Every transport retry failed the same way.
    RetriesExhausted,
    /// The agent reported that it failed, or could not do the work.
    AgentFailed,
    /// A review or a test turned the work down.
    Rejected,
    /// No rule matched, so a decider reads it.
    Unmatched,
}

/// What a step's ending was routed to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Routed {
    /// The step waited for its provider's limit to reset.
    Wait,
    /// The step was run again after a back-off: retry `n` of `of`.
    Retry {
        /// This retry's number.
        n: u32,
        /// How many the project allows.
        of: u32,
    },
    /// The failure went to the decider.
    Decide(DecideWhy),
    /// The run stopped at a fault the operator fixes.
    Stop(StopCause),
}

impl DecideWhy {
    const ALL: [Self; 5] = [
        Self::TimeLimit,
        Self::RetriesExhausted,
        Self::AgentFailed,
        Self::Rejected,
        Self::Unmatched,
    ];

    /// The name this reason is kept under in the journal.
    #[must_use]
    pub fn token(self) -> &'static str {
        match self {
            Self::TimeLimit => "time-limit",
            Self::RetriesExhausted => "retries-exhausted",
            Self::AgentFailed => "agent-failed",
            Self::Rejected => "rejected",
            Self::Unmatched => "unmatched",
        }
    }
}

impl Routed {
    /// The journal's one-word form: `wait`, `retry:2:3`, `decide:time-limit`, `stop:disk-full`.
    #[must_use]
    pub fn token(self) -> String {
        match self {
            Self::Wait => "wait".to_owned(),
            Self::Retry { n, of } => format!("retry:{n}:{of}"),
            Self::Decide(why) => format!("decide:{}", why.token()),
            Self::Stop(cause) => format!("stop:{}", cause.token()),
        }
    }

    /// The verdict kept as `token`; `None` for a word this build does not know.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        if token == "wait" {
            return Some(Self::Wait);
        }
        let (kind, rest) = token.split_once(':')?;
        match kind {
            "retry" => {
                let (n, of) = rest.split_once(':')?;
                Some(Self::Retry {
                    n: n.parse().ok()?,
                    of: of.parse().ok()?,
                })
            }
            "decide" => DecideWhy::ALL
                .into_iter()
                .find(|why| why.token() == rest)
                .map(Self::Decide),
            "stop" => StopCause::from_token(rest).map(Self::Stop),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_verdict_round_trips_through_its_token() {
        let mut every = vec![Routed::Wait, Routed::Retry { n: 2, of: 3 }];
        every.extend(DecideWhy::ALL.map(Routed::Decide));
        every.extend(StopCause::ALL.map(Routed::Stop));
        for routed in every {
            assert_eq!(Routed::from_token(&routed.token()), Some(routed));
        }
        assert_eq!(Routed::from_token("decide:nonsense"), None);
        assert_eq!(Routed::from_token("retry:x:3"), None);
    }
}
