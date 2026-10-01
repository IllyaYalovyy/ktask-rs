//! Waiting for real time to pass.

use std::time::Duration;

/// Port: blocks for a real span of time — how the resolve role's own limit handling waits out
/// a provider's reset time without spending a token, and how a test proves what it was asked
/// to wait for without ever actually waiting.
pub trait Sleep {
    /// Blocks the calling thread for `duration`.
    fn sleep(&self, duration: Duration);
}
