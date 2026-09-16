//! Terminal user interface.
//!
//! Must stay headlessly testable: keep decision logic pure and confine
//! terminal I/O to a thin shell. See docs/TESTING.md.

/// Placeholder proving the workspace builds and tests at the seed commit,
/// and that the interface is wired to the core it renders.
/// The first TUI task replaces it.
#[must_use]
pub fn seed_marker() -> String {
    format!("{}+ktask-tui", ktask_core::seed_marker())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_marker_names_the_core_it_renders() {
        assert_eq!(seed_marker(), "ktask-core+ktask-tui");
    }
}
