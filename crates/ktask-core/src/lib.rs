//! Supervisor state machine, event journal, gates, providers.
//!
//! Everything that decides *what happened* lives here and stays free of I/O:
//! the crate is the reason the invariants in VISION.md can be tested at all.

/// Placeholder proving the workspace builds and tests at the seed commit.
/// The first task replaces it.
#[must_use]
pub fn seed_marker() -> &'static str {
    "ktask-core"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_marker_identifies_the_crate() {
        assert_eq!(seed_marker(), "ktask-core");
    }
}
