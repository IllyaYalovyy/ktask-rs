//! The current time.

use std::time::SystemTime;

/// Port: what time it is.
pub trait Clock {
    /// The current time.
    fn now(&self) -> SystemTime;
}
