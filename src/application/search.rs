//! Task search shared by the TUI and the CLI: the text a user sees in a row.
use super::lifecycle::State;

/// Whether `query` (case-insensitive) occurs in the task's `#id`, its displayed state label
/// or its title. An empty query matches everything.
pub fn matches(id: impl std::fmt::Display, state: State, title: &str, query: &str) -> bool {
    format!("#{id} {} {title}", state.label())
        .to_lowercase()
        .contains(&query.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_id_label_and_title_ignoring_case() {
        assert!(matches(7, State::Done, "Fix", "DONE"));
        assert!(matches(7, State::Cancelled, "Fix", "cancelled"));
        assert!(matches(7, State::NeedsInput, "Fix", "needs input"));
        assert!(matches(7, State::Pending, "Fix bug", "#7"));
        assert!(matches(7, State::Pending, "Fix bug", "bug"));
        assert!(matches(7, State::Pending, "Fix bug", ""));
        assert!(!matches(7, State::Cancelled, "Fix", "deleted"));
        assert!(!matches(7, State::Done, "Fix", "succeeded"));
    }
}
