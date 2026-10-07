//! The waits a provider step records: for a usage limit, and for a transport retry.

use std::time::{Duration, SystemTime};

/// How long a step waited, in total, for its provider's own usage limit, and when it last
/// resumed after the most recent of those waits — shown alongside the step's own outcome once
/// it has ended, so a limit hit while it ran is never lost once the wait is over, unlike
/// [`crate::Attempt::waiting_until`], which only shows while the wait is still live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LimitWait {
    /// How long the step waited, in total, across every time its provider's usage limit was
    /// hit before it ended.
    pub waited: Duration,
    /// When it last resumed running, after the most recent of those waits.
    pub resumed_at: SystemTime,
}

/// Why a provider step waits before it runs again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitReason {
    /// The provider's usage limit was hit; the wait ends at its reset.
    UsageLimit,
    /// Codex's stream disconnected; the wait is the back-off before consecutive failure
    /// `failure` of at most `limit` is retried.
    TransportRetry {
        /// How many consecutive transport failures the step has had.
        failure: u32,
        /// How many consecutive failures end the step.
        limit: u32,
    },
}

impl WaitReason {
    /// The router's verdict this wait carries out.
    #[must_use]
    pub fn routed(self) -> crate::Routed {
        match self {
            Self::UsageLimit => crate::Routed::Wait,
            Self::TransportRetry { failure, limit } => crate::Routed::Retry {
                n: failure,
                of: limit,
            },
        }
    }
}
