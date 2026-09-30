//! The one interface the terminal interface reaches every use case through.
//!
//! Each method names what a key or a form does — load the queue, add a task, switch
//! project — and returns the result and the error that use case's own name for it, never a
//! message already rendered into text: the loop turns a failure into what is shown only at
//! the point it is about to be shown, so the error keeps its meaning until then. [`run`](crate::run)
//! takes one value implementing this trait; the CLI hands it one that reaches the real
//! project, and this crate's own tests use a fake.

use std::fmt::Display;

use ktask_core::{Placement, Project, QueueView, SettingView, TaskDraft, TaskId};

/// Every use case the terminal interface calls, named the way the operator invokes them.
pub trait Application {
    /// Why [`load_queue`](Application::load_queue) failed.
    type LoadError: Display;
    /// Why [`remove_task`](Application::remove_task) failed.
    type RemoveError: Display;
    /// One reason [`add_task`](Application::add_task) refused the task.
    type AddProblem: Display;
    /// Why [`load_settings`](Application::load_settings) failed.
    type SettingsError: Display;
    /// Why [`save_setting`](Application::save_setting) refused the value.
    type SaveSettingError: Display;
    /// Why [`load_projects`](Application::load_projects) failed.
    type ProjectsError: Display;
    /// Why [`switch_project`](Application::switch_project) failed.
    type SwitchError: Display;
    /// Why [`forget_project`](Application::forget_project) failed.
    type ForgetError: Display;
    /// Why [`register`](Application::register) failed.
    type RegisterError: Display;

    /// The queue to show, with the cancelled tasks when `show_cancelled`.
    ///
    /// # Errors
    ///
    /// Fails when the queue cannot be read.
    fn load_queue(&self, show_cancelled: bool) -> Result<QueueView, Self::LoadError>;

    /// Removes the task numbered `id`, as the operator confirmed removing it.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when there is no such task, when it is running, or when it is
    /// cancelled already.
    fn remove_task(&self, id: TaskId) -> Result<(), Self::RemoveError>;

    /// Adds `draft` to the queue at `placement`, giving its number, or every rule it broke,
    /// so all of them can be put right at once.
    ///
    /// # Errors
    ///
    /// Fails, adding nothing, with every rule `draft` broke.
    fn add_task(
        &self,
        draft: &TaskDraft,
        placement: Placement,
    ) -> Result<TaskId, Vec<Self::AddProblem>>;

    /// Every project setting, for the settings screen to open on.
    ///
    /// # Errors
    ///
    /// Fails when the settings cannot be read.
    fn load_settings(&self) -> Result<Vec<SettingView>, Self::SettingsError>;

    /// Changes the setting `name` to `value`, as the settings screen was submitted.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when `name` is not a known setting or `value` is not one it
    /// accepts.
    fn save_setting(&self, name: &str, value: &str) -> Result<SettingView, Self::SaveSettingError>;

    /// Imports the tasks of the file `path` names into the queue, giving what to show for it
    /// — the same words `ktask-rs import` itself would print, whether it succeeded or was
    /// refused.
    ///
    /// # Errors
    ///
    /// Fails, importing nothing, with the same words `ktask-rs import` itself would print for
    /// the same refusal.
    fn import(&self, path: &str) -> Result<String, String>;

    /// The registered projects, for the project picker to open on — the same list
    /// `ktask-rs project list` prints.
    ///
    /// # Errors
    ///
    /// Fails when the registry cannot be read.
    fn load_projects(&self) -> Result<Vec<Project>, Self::ProjectsError>;

    /// Switches to the registered project called `name`, so every action from then on
    /// applies to it, giving its fresh queue.
    ///
    /// # Errors
    ///
    /// Fails, switching nothing, when `name` is not a registered project or its state cannot
    /// be opened.
    fn switch_project(&self, name: &str) -> Result<QueueView, Self::SwitchError>;

    /// Forgets the registered project called `name`, giving the registered projects that
    /// remain — the same list `ktask-rs project list` prints afterward.
    ///
    /// # Errors
    ///
    /// Fails, forgetting nothing, when `name` is not a registered project.
    fn forget_project(&self, name: &str) -> Result<Vec<Project>, Self::ForgetError>;

    /// Registers the current directory under `name`, exactly as `ktask-rs project register
    /// --name` would, and opens its queue on success so every action from then on applies to
    /// it.
    ///
    /// # Errors
    ///
    /// Fails, registering nothing, with the same refusal `ktask-rs project register --name`
    /// gives for the same name.
    fn register(&self, name: &str) -> Result<QueueView, Self::RegisterError>;

    /// Starts executing the pending tasks, exactly as `ktask-rs run` does, blocking until it
    /// ends or refuses to start, and gives what it printed either way — the same words
    /// `ktask-rs run` itself would show, one line per line it wrote.
    ///
    /// # Errors
    ///
    /// Fails with the same words `ktask-rs run` itself would show for the same refusal.
    fn start_run(&self) -> Result<String, String>;
}
