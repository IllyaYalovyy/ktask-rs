//! The one interface the terminal interface reaches every use case through.
//!
//! Each method names what a key or a form does — load the queue, add a task, switch
//! project — and returns the result and the error that use case's own name for it, never a
//! message already rendered into text: the loop turns a failure into what is shown only at
//! the point it is about to be shown, so the error keeps its meaning until then. [`run`](crate::run)
//! takes one value implementing this trait; the CLI hands it one that reaches the real
//! project, and this crate's own tests use a fake.

use std::fmt::Display;

use ktask_core::{
    Import, Placement, Project, ProviderCheck, ProviderView, QueueView, RunReport, SettingView,
    TaskDraft, TaskId,
};

/// Every use case the terminal interface calls, named the way the operator invokes them.
pub trait Application {
    /// Why [`load_queue`](Application::load_queue) failed.
    type LoadError: Display;
    /// Why [`remove_task`](Application::remove_task) failed.
    type RemoveError: Display;
    /// Why [`retry_task`](Application::retry_task) refused to retry a task.
    type RetryError: Display;
    /// Why [`answer_task`](Application::answer_task) refused to answer a task.
    type AnswerError: Display;
    /// Why [`done_task`](Application::done_task) refused to mark a task done.
    type DoneError: Display;
    /// Why [`acknowledge_task`](Application::acknowledge_task) refused a human task.
    type AcknowledgeError: Display;
    /// One reason [`add_task`](Application::add_task) refused the task.
    type AddProblem: Display;
    /// Why [`load_settings`](Application::load_settings) failed.
    type SettingsError: Display;
    /// Why [`save_setting`](Application::save_setting) refused the value.
    type SaveSettingError: Display;
    /// Why the provider catalogue could not be read.
    type ProvidersError: Display;
    /// Why a provider readiness check could not be started.
    type ProviderCheckError: Display;
    /// Why [`load_projects`](Application::load_projects) failed.
    type ProjectsError: Display;
    /// Why [`switch_project`](Application::switch_project) failed.
    type SwitchError: Display;
    /// Why [`forget_project`](Application::forget_project) failed.
    type ForgetError: Display;
    /// Why [`register`](Application::register) failed.
    type RegisterError: Display;
    /// Why [`import`](Application::import) added nothing.
    type ImportError: Display;
    /// Why [`start_run`](Application::start_run) refused to start a run at all.
    type RunRefusal: Display;
    /// Why retained output could not be loaded.
    type OutputError: Display;

    /// The queue to show, with the cancelled and skipped tasks when `show_cancelled`.
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

    /// Sends the task numbered `id` — the operator pressed the key that asks for it — back to
    /// `pending`, so the next run picks it up again: every earlier attempt stays in its
    /// history, and the next one takes the next number.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when there is no such task, or its status is not `failed`,
    /// `failed-unknown` or `blocked`.
    fn retry_task(&self, id: TaskId) -> Result<(), Self::RetryError>;

    /// Records `text` as the answer to the question task `id`'s attempt asked — the operator
    /// submitted the answer form for it — and sends it back to `pending`, so the next run
    /// picks it up with it.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when there is no such task, its status is not `blocked`, or
    /// `text` is empty or only whitespace.
    fn answer_task(&self, id: TaskId, text: &str) -> Result<(), Self::AnswerError>;

    /// Marks the task numbered `id` `done` — the operator submitted the done form for it —
    /// with `reason`: work finished outside the tool, recorded as finished.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when there is no such task or `reason` is empty or only
    /// whitespace.
    fn done_task(&self, id: TaskId, reason: &str) -> Result<(), Self::DoneError>;

    /// Acknowledges a pending human task, recording `message` when it is not blank.
    ///
    /// # Errors
    ///
    /// Fails, changing nothing, when `id` does not name a pending human task.
    fn acknowledge_task(
        &self,
        id: TaskId,
        message: Option<&str>,
    ) -> Result<(), Self::AcknowledgeError>;

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

    /// Every effective provider definition for the active project.
    ///
    /// # Errors
    ///
    /// Fails when the active project's provider settings cannot be read or are invalid.
    fn load_providers(&self) -> Result<Vec<ProviderView>, Self::ProvidersError>;

    /// Checks the selected provider's executable, authentication and smallest possible call.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider definition cannot be read or probed.
    fn check_provider(&self, name: &str) -> Result<ProviderCheck, Self::ProviderCheckError>;

    /// Imports the tasks of the file `path` names into the queue, at its end: the tasks added,
    /// in order, and how many cancelled ones were left out — the same typed result
    /// `ktask_core::import_tasks` itself gives, for the screen to word as it shows it.
    ///
    /// # Errors
    ///
    /// Fails, importing nothing, with why: the file could not be read, or the same reason
    /// `ktask-rs import` itself would refuse for.
    fn import(&self, path: &str) -> Result<Import, Self::ImportError>;

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
    /// ends or refuses to start, and gives its own typed report of what it did — the same
    /// values `ktask_core::run_queue` itself gives, for the screen to word as it shows them.
    ///
    /// # Errors
    ///
    /// Fails, attempting nothing, when a run could not even be started at all — another one
    /// already in progress, or no project open yet.
    fn start_run(&self) -> Result<RunReport, Self::RunRefusal>;

    /// The selected task's latest attempt output, one transcript per agent step.
    ///
    /// # Errors
    ///
    /// Returns the application's output-read error when the task or its state cannot be read.
    fn load_output(&self, id: TaskId)
    -> Result<Vec<ktask_core::StepTranscript>, Self::OutputError>;
}
