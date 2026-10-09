//! Turning a serde JSON path error into the one line a bad field of an imported task or a
//! reported finding is shown with — [`crate::import`] and [`crate::finding`] both read a JSON
//! (or JSON-shaped TOML) array the same way, and refuse a bad element the same way too.

/// What went wrong reading one element, naming the field it is about when it is about one.
pub(crate) fn field_error(error: &serde_path_to_error::Error<serde_json::Error>) -> String {
    let path = error.path().to_string();
    if path == "." || error.inner().to_string().contains(&format!("`{path}`")) {
        error.inner().to_string()
    } else {
        format!("{path}: {}", error.inner())
    }
}
