//! Terminal user interface.
//!
//! Must stay headlessly testable: keep decision logic pure and confine
//! terminal I/O to a thin shell. See docs/TESTING.md.

/// Placeholder proving the workspace builds and tests at the seed commit.
/// The first TUI task replaces it.
pub fn seed_marker() -> &'static str {
    "ktask-tui"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_marker_identifies_the_crate() {
        assert_eq!(seed_marker(), "ktask-tui");
    }
}
