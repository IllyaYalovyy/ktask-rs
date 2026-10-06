//! The providers a queue run chooses between.

use std::collections::BTreeMap;
use std::fmt;

use crate::Provider;

/// Providers available for task-level agent-step selections. The resolver remains a project
/// role setting; a task's provider applies to implementation, review and test.
#[derive(Clone, Copy)]
pub struct TaskProviders<'a> {
    /// Provider used when a task has no override.
    pub default: &'a Provider,
    /// Provider used for the resolve role.
    pub resolver: &'a Provider,
    /// Every configured provider, keyed by its configured name.
    pub named: &'a BTreeMap<String, Provider>,
}

impl fmt::Debug for TaskProviders<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskProviders")
            .field("default", &self.default.name)
            .field("resolver", &self.resolver.name)
            .field("named", &self.named.keys().collect::<Vec<_>>())
            .finish()
    }
}

impl<'a> TaskProviders<'a> {
    pub(super) fn agent_for(&self, task: &crate::Task) -> &'a Provider {
        task.provider
            .as_ref()
            .and_then(|name| self.named.get(name))
            .unwrap_or(self.default)
    }
}
