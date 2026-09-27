//! The system clock.

use std::time::SystemTime;

use ktask_core::Clock;

/// The clock of the machine the tool runs on.
#[derive(Debug, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}
