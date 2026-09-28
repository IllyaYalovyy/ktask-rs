//! One module per command: each reads its arguments, calls the use case, and renders the
//! result.

pub(crate) mod add;
pub(crate) mod import;
pub(crate) mod list;
pub(crate) mod project;
pub(crate) mod provider;
pub(crate) mod remove;
pub(crate) mod report;
pub(crate) mod run;
pub(crate) mod status;
pub(crate) mod tui;
