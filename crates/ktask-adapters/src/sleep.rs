//! Really waiting for time to pass.

use std::time::Duration;

use ktask_core::Sleep;

/// Blocks the calling thread for real, with [`std::thread::sleep`].
#[derive(Debug, Clone, Copy)]
pub struct RealSleep;

impl Sleep for RealSleep {
    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}
