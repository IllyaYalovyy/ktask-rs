//! The ktask commands an agent fix may run on its project. They are parsed like any command
//! line and applied by the same application commands, with the same validation and version
//! guards, as the fix's own AI actor. Everything outside the allowed scope is refused here.
use super::{
    args::{
        AgentAction, Approval, CategoryAction, CategoryScope, Cli, Command, LensAction,
        ProfileAction, Target, ToolAction,
    },
    execute,
    output::Reply,
};
use crate::{
    application::{Service, Task, VersionedTask, lifecycle::Action, preferences::PlannerApproval},
    instance::Paths,
    runner::control,
    storage::{Error, Result},
};
use clap::Parser;
use std::{collections::BTreeMap, path::Path};

/// What the fix was asked, which decides what only an instruction permits.
pub(crate) struct Authority<'a> {
    pub fix: i64,
    /// The owner's instruction; None when the fix carries out a report.
    pub instruction: Option<&'a str>,
    pub root: &'a Path,
}

/// What came of one command.
pub(crate) enum Answer {
    /// A command that only reads.
    Read(String),
    Applied {
        result: String,
        /// Settings that differ afterwards: name and "before -> after".
        changes: Vec<(String, String)>,
    },
    Refused(String),
    Failed(String),
}

enum Scope {
    Read,
    Change,
    Refused(&'static str),
}

const INSTANCE_WIDE: &str = "it changes settings or files shared with other projects";
const OWNER_JUDGMENT: &str = "giving up, resolving or replacing a task is the owner's judgment";

fn scope(command: &Command) -> Scope {
    use Scope::{Change, Read, Refused};
    match command {
        Command::Limits {
            value: None,
            ceiling: None,
            reset: false,
            ..
        } => Read,
        Command::Limits { instance: true, .. } => Refused(INSTANCE_WIDE),
        Command::Limits { .. } => Change,
        Command::Show { .. }
        | Command::List { .. }
        | Command::Current
        | Command::Checks
        | Command::Events { .. }
        | Command::Stats
        | Command::Artifact { .. } => Read,
        Command::CheckSet { .. }
        | Command::Init { .. }
        | Command::Add(_)
        | Command::Edit { .. }
        | Command::Move { .. }
        | Command::Stop { .. }
        | Command::Restart { .. }
        | Command::Run { .. }
        | Command::Answer { .. }
        | Command::Approve { .. }
        | Command::Require { .. }
        | Command::DependencyRecheck { .. }
        | Command::Pause { all: false } => Change,
        Command::ModelDefault {
            new_projects: false,
            ..
        } => Change,
        Command::ModelDefault { .. } => Refused(INSTANCE_WIDE),
        Command::Delivery {
            mode: None,
            remote: None,
            base: None,
            ..
        } => Read,
        Command::Delivery { .. } => Change,
        Command::Defaults {
            agent_network: None,
            check_before_task: None,
            check_timeout_ms: None,
            run_mode: None,
            planner_approval: None,
            delivery_mode: None,
            apply,
            ..
        } => {
            if *apply {
                Change
            } else {
                Read
            }
        }
        Command::Defaults { .. } => Refused(INSTANCE_WIDE),
        Command::Profile { action } => match action {
            ProfileAction::List | ProfileAction::Show { .. } => Read,
            ProfileAction::Select { .. } => Change,
            _ => Refused(INSTANCE_WIDE),
        },
        Command::Category { action } => match action {
            CategoryAction::List
            | CategoryAction::Show { .. }
            | CategoryAction::Preview { .. }
            | CategoryAction::Placeholders => Read,
            CategoryAction::Save {
                scope: CategoryScope::Project,
                ..
            }
            | CategoryAction::Delete {
                scope: CategoryScope::Project,
                ..
            } => Change,
            _ => Refused(INSTANCE_WIDE),
        },
        Command::Lens { action } => match action {
            LensAction::List | LensAction::Show { .. } | LensAction::Preview { .. } => Read,
            LensAction::Defaults { set, clear, .. } if set.is_empty() && !clear => Read,
            LensAction::Defaults { .. }
            | LensAction::Save {
                scope: CategoryScope::Project,
                ..
            }
            | LensAction::Delete {
                scope: CategoryScope::Project,
                ..
            } => Change,
            _ => Refused(INSTANCE_WIDE),
        },
        Command::Tool { action } => match action {
            ToolAction::List | ToolAction::ProvisionStatus => Read,
            ToolAction::Register { .. } | ToolAction::Bind { .. } | ToolAction::Unbind { .. } => {
                Change
            }
            ToolAction::ProvisionConsent { .. } | ToolAction::ProvisionMode { .. } => {
                Refused("only a human consents to licenses and provisioning")
            }
            _ => Refused("managed provisioning and shared tool generations stay with the owner"),
        },
        Command::Agent { action } => match action {
            AgentAction::List | AgentAction::Show { .. } => Read,
            _ => Refused("a fix does not start, stop or reconfigure agent runs"),
        },
        Command::Done { .. }
        | Command::Cancel { .. }
        | Command::Supersede { .. }
        | Command::Reject { .. }
        | Command::Copy { .. }
        | Command::Breakdown { .. } => Refused(OWNER_JUDGMENT),
        Command::Import { .. } | Command::Export { .. } => {
            Refused("importing and exporting task files is not part of a fix")
        }
        Command::SettingsExport { .. } | Command::SettingsImport { .. } => {
            Refused("exporting and importing settings, shared ones included, is not part of a fix")
        }
        Command::ProjectDelete { .. } | Command::ProjectMove { .. } => {
            Refused("a fix never deletes or moves a project")
        }
        Command::Projects => Refused("a fix works on its own project only"),
        Command::Link { .. } => Refused("linking hosts is the owner's setup"),
        Command::Shutdown | Command::Pause { all: true } => {
            Refused("a fix never shuts ktask down or touches other projects")
        }
        Command::Storage { .. } | Command::Retention { .. } | Command::Cache { .. } => {
            Refused("evidence retention and build caches are not part of a fix")
        }
        Command::Evidence { .. } => Refused("run ktask evidence directly, not through the fix"),
        Command::Mcp
        | Command::Screen { .. }
        | Command::FollowUp { .. }
        | Command::DependencyRequest { .. } => Refused("it is not a command of a fix"),
        Command::InFix { .. } => Refused("fix commands do not nest"),
    }
}

fn words(text: &str) -> Vec<String> {
    text.to_lowercase()
        .replace('’', "'")
        .split(|c: char| !(c.is_alphanumeric() || matches!(c, '#' | '\'' | '_' | '-')))
        .map(|word| word.trim_matches(['\'', '-']).to_owned())
        .filter(|word| !word.is_empty())
        .collect()
}

/// Whether the instruction itself says to approve the draft of task `id` (or all drafts).
fn instructs_approval(instruction: &str, id: Option<i64>) -> bool {
    let words = words(instruction);
    words.iter().enumerate().any(|(at, word)| {
        let negated = words[at.saturating_sub(3)..at].iter().any(|word| {
            matches!(
                word.as_str(),
                "not" | "never" | "don't" | "dont" | "without"
            )
        });
        if word != "approve" || negated {
            return false;
        }
        let mut rest = words[at + 1..]
            .iter()
            .map(String::as_str)
            .skip_while(|word| matches!(*word, "the" | "task" | "draft" | "of" | "revision"));
        match rest.next() {
            Some("all") => rest.find(|word| *word != "the") == Some("drafts"),
            Some(named) => {
                id.is_some_and(|id| named.strip_prefix('#').unwrap_or(named) == id.to_string())
            }
            None => false,
        }
    })
}

/// Whether the instruction gives this answer to a question.
fn instructs_answer(instruction: &str, answer: &str) -> bool {
    let (said, answer) = (words(instruction), words(answer));
    !answer.is_empty()
        && said
            .iter()
            .any(|word| matches!(word.as_str(), "answer" | "question"))
        && said.windows(answer.len()).any(|part| part == answer)
}

/// The project settings a command may change, as comparable text.
fn settings(service: &Service, paths: &Paths) -> Result<BTreeMap<String, String>> {
    let mut found = BTreeMap::new();
    for check in service.checks()? {
        found.insert(
            format!("check {}", check.name),
            format!("{} (timeout {} ms)", check.command, check.timeout_ms),
        );
    }
    for row in service.effective_models()? {
        if let Some(model) = row.model {
            let effort = model
                .effort
                .as_ref()
                .map_or_else(String::new, |effort| format!(" · effort {effort}"));
            found.insert(
                format!("model {}", row.role),
                format!("{}{effort}", model.identifier()),
            );
        }
    }
    let delivery = service.delivery_config()?;
    found.insert(
        "delivery".into(),
        format!(
            "{} via {}/{}",
            delivery.mode, delivery.remote, delivery.base_branch
        ),
    );
    found.insert("agent network".into(), service.agent_network()?.to_string());
    found.insert(
        "check before task".into(),
        service.check_before_task()?.to_string(),
    );
    let preferences = service.preferences(&paths.config)?;
    found.insert("run mode".into(), preferences.continuation.name().into());
    found.insert(
        "planner approval".into(),
        preferences.planner_approval.name().into(),
    );
    found.insert("lens defaults".into(), service.lens_defaults()?.join(", "));
    for setting in service.resource_limits()? {
        found.insert(
            format!("limit {}", setting.limit),
            format!("{} (ceiling {})", setting.value, setting.ceiling),
        );
    }
    for tool in service.tools()?.into_iter().filter(|tool| tool.bound) {
        found.insert(
            format!("tool {}", tool.name),
            format!("{} at {}", tool.version, tool.path.display()),
        );
    }
    Ok(found)
}

fn differences(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<(String, String)> {
    let shown = |value: Option<&String>| value.map_or("unset", String::as_str).to_owned();
    before
        .keys()
        .chain(after.keys().filter(|key| !before.contains_key(*key)))
        .filter(|key| before.get(*key) != after.get(*key))
        .map(|key| {
            (
                key.clone(),
                format!("{} -> {}", shown(before.get(key)), shown(after.get(key))),
            )
        })
        .collect()
}

/// Parses and applies one command line (without the program name) for a fix.
pub(crate) fn run(
    service: &mut Service,
    paths: &Paths,
    authority: &Authority<'_>,
    args: &[String],
) -> Answer {
    let line = std::iter::once("ktask").chain(args.iter().map(String::as_str));
    let cli = match Cli::try_parse_from(line) {
        Ok(cli) => cli,
        Err(error) => return Answer::Failed(error.to_string()),
    };
    if cli.project.is_some()
        || cli.project_name.is_some()
        || cli.actor.is_some()
        || cli.actor_kind.is_some()
        || cli.worker_token.is_some()
        || cli.host.is_some()
        || cli.from_parent.is_some()
        || cli.paths
        || cli.demo
    {
        return Answer::Refused(
            "a fix works on its own project as itself; omit project, actor, host and instance options"
                .into(),
        );
    }
    let Some(mut command) = cli.command else {
        return Answer::Failed("Name a ktask command".into());
    };
    let change = match scope(&command) {
        Scope::Refused(why) => return Answer::Refused(why.into()),
        Scope::Read => false,
        Scope::Change => true,
    };
    let text = |reply: Reply| {
        if cli.json {
            reply.data.to_string()
        } else {
            reply.text
        }
    };
    if !change {
        let reply = match &command {
            Command::Tool { action } => execute::tool(service, paths, action),
            Command::Agent { action } => {
                super::agent::run(service, paths, crate::application::Actor::default(), action)
            }
            command => execute::command(service, &paths.config, &paths.cache, paths, command),
        };
        return match reply {
            Ok(reply) => Answer::Read(text(reply)),
            Err(failure) => Answer::Failed(failure.message),
        };
    }
    let before = match settings(service, paths) {
        Ok(before) => before,
        Err(error) => return Answer::Failed(error.to_string()),
    };
    let answer = apply(service, paths, authority, &mut command, cli.json);
    let after = settings(service, paths);
    match (answer, after) {
        (Answer::Applied { result, .. }, Ok(after)) => Answer::Applied {
            result,
            changes: differences(&before, &after),
        },
        // A command that saved part of its change before failing still reports it.
        (Answer::Failed(error), Ok(after)) if before != after => Answer::Applied {
            result: error,
            changes: differences(&before, &after),
        },
        (answer, _) => answer,
    }
}

fn applied(result: impl Into<String>) -> Answer {
    Answer::Applied {
        result: result.into(),
        changes: Vec::new(),
    }
}

fn failed(error: Error) -> Answer {
    Answer::Failed(match error {
        Error::Conflict(message) => message,
        other => other.to_string(),
    })
}

fn versioned(service: &mut Service, target: &Target) -> Result<(Task, VersionedTask)> {
    let task = service.task(target.id)?.ok_or_else(|| {
        Error::Conflict(format!(
            "Task #{} does not exist in this project",
            target.id
        ))
    })?;
    if target
        .if_version
        .is_some_and(|version| version != task.version)
    {
        return Err(Error::Conflict(format!(
            "Task #{} is version {}; read it again before retrying",
            task.id, task.version
        )));
    }
    let versioned = VersionedTask {
        id: task.id,
        version: task.version,
    };
    Ok((task, versioned))
}

const RUNS_LATER: &str =
    "no worker starts while the fix works; it runs when the queue continues after the fix";

fn apply(
    service: &mut Service,
    paths: &Paths,
    authority: &Authority<'_>,
    command: &mut Command,
    json: bool,
) -> Answer {
    let result = special(service, paths, authority, command);
    match result {
        Ok(Some(answer)) => answer,
        Err(error) => failed(error),
        Ok(None) => {
            let reply = match command {
                Command::Tool { action } => execute::tool(service, paths, action),
                command => execute::command(service, &paths.config, &paths.cache, paths, command),
            };
            match reply {
                Ok(reply) => applied(if json {
                    reply.data.to_string()
                } else {
                    reply.text
                }),
                Err(failure) => Answer::Failed(failure.message),
            }
        }
    }
}

/// Commands whose effect or permission differs inside a fix. None leaves `command`, as
/// adjusted here, to the ordinary dispatch.
fn special(
    service: &mut Service,
    paths: &Paths,
    authority: &Authority<'_>,
    command: &mut Command,
) -> Result<Option<Answer>> {
    let by = format!("agent fix #{}", authority.fix);
    let reason_or = |reason: &str, verb: &str| {
        if reason.trim().is_empty() {
            format!("{verb} by {by}")
        } else {
            format!("{by}: {reason}")
        }
    };
    let drafts_only =
        service.preferences(&paths.config)?.planner_approval == PlannerApproval::ReviewRequired;
    let may_approve = |id: Option<i64>| {
        authority
            .instruction
            .is_some_and(|instruction| instructs_approval(instruction, id))
    };
    Ok(match command {
        Command::Pause { .. } => Some(Answer::Read(
            "The runner is already paused for this fix".into(),
        )),
        Command::Stop { target, reason } => {
            let (task, target) = versioned(service, target)?;
            let saved = control::stop_held(
                service,
                authority.root,
                target,
                &reason_or(reason, "Stopped"),
            )?;
            Some(applied(format!(
                "stopped: #{}{}",
                task.id,
                saved.stash.map_or_else(String::new, |stash| format!(
                    " · uncommitted changes saved as git stash \"{stash}\""
                ))
            )))
        }
        Command::Restart { target, reason, .. } => {
            let (task, target) = versioned(service, target)?;
            let current = service.current_task()?.map(|task| task.id);
            crate::application::require(
                current == Some(task.id),
                "Only the current task can be restarted",
            )?;
            service.require_restart(target)?;
            let saved = control::stop_held(
                service,
                authority.root,
                target,
                &reason_or(reason, "Restart requested"),
            )?;
            Some(applied(format!(
                "restart: #{} is Pending{} · {RUNS_LATER}",
                task.id,
                saved.stash.map_or_else(String::new, |stash| format!(
                    " · changes saved as git stash \"{stash}\""
                ))
            )))
        }
        Command::Run { raise: Some(_), .. } => Some(Answer::Refused(
            "raise a limit with limits; the queue resumes after the fix".into(),
        )),
        Command::Run { .. } => {
            let current = service
                .current_task()?
                .ok_or_else(|| Error::Conflict("Queue is empty".into()))?;
            let state = current.state();
            if !matches!(
                state.forward(),
                Some(Action::Run | Action::Resume | Action::Retry)
            ) {
                return Err(Error::Conflict(state.refusal(Action::Run)));
            }
            Some(applied(format!(
                "#{} is ready to run: {RUNS_LATER} (agent fix --resume, or ktask run)",
                current.id
            )))
        }
        Command::Answer { target, text } => {
            if !authority
                .instruction
                .is_some_and(|instruction| instructs_answer(instruction, text))
            {
                return Ok(Some(Answer::Refused(
                    "the instruction does not give this answer to a question".into(),
                )));
            }
            let (task, target) = versioned(service, target)?;
            service.answer(target, text)?;
            Some(applied(format!("answered: #{} · {RUNS_LATER}", task.id)))
        }
        Command::Approve { target } => {
            if !may_approve(Some(target.id)) {
                return Ok(Some(Answer::Refused(
                    "the instruction does not say to approve this draft".into(),
                )));
            }
            let (task, target) = versioned(service, target)?;
            service.decide(target, true, &format!("Approved by {by} as instructed"))?;
            Some(applied(format!("approved: #{}", task.id)))
        }
        Command::Add(add) => {
            if drafts_only && !may_approve(None) {
                add.approval = Approval::Draft;
            }
            None
        }
        Command::Edit {
            target, changes, ..
        } => {
            let (task, _) = versioned(service, target)?;
            let draft = task.definition().lifecycle != "ready"
                || task
                    .revisions
                    .last()
                    .is_some_and(|r| r.lifecycle == "draft");
            let approves = matches!(changes.approval, Some(Approval::Ready));
            if draft && approves && !may_approve(Some(task.id)) {
                return Ok(Some(Answer::Refused(
                    "making a draft ready approves it, and the instruction does not say so".into(),
                )));
            }
            if drafts_only && !may_approve(Some(task.id)) {
                changes.approval = Some(Approval::Draft);
            }
            None
        }
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(line: &[&str]) -> bool {
        let cli = Cli::try_parse_from(std::iter::once(&"ktask").chain(line)).unwrap();
        matches!(scope(&cli.command.unwrap()), Scope::Refused(_))
    }

    #[test]
    fn nothing_that_reaches_beyond_the_project_or_judges_work_is_in_scope() {
        for line in [
            &["project-delete", "1", "--confirm", "x"][..],
            &["project-move", "1", "/tmp"],
            &["projects"],
            &["shutdown"],
            &["pause", "--all"],
            &["done", "1"],
            &["cancel", "1"],
            &[
                "model-default",
                "execution",
                "--provider",
                "claude",
                "--new-projects",
            ],
            &["defaults", "--run-mode", "one"],
            &["retention", "0"],
            &["storage", "--apply"],
            &["profile", "delete", "x"],
            &["category", "delete", "x"],
            &["tool", "provision-consent", "x", "1"],
            &["agent", "fix", "again"],
            &["agent", "stop", "1"],
            &["limits", "output_rows", "--value", "9", "--instance"],
            &["limits", "output_rows", "--reset", "--instance"],
        ] {
            assert!(refused(line), "{line:?}");
        }
        for line in [
            &["check-set", "build", "--command", "true"][..],
            &["model-default", "execution", "--provider", "claude"],
            &["delivery", "--mode", "local"],
            &["edit", "1", "--title", "x"],
            &["move", "1"],
            &["stop", "1"],
            &["restart", "1"],
            &["retry"],
            &["answer", "1", "--text", "x"],
            &["category", "delete", "x", "--scope", "project"],
            &["limits"],
            &["limits", "output_rows", "--value", "9"],
        ] {
            assert!(!refused(line), "{line:?}");
        }
    }

    #[test]
    fn only_the_instruction_permits_approving_a_draft_or_answering() {
        assert!(instructs_approval("Please approve #7.", Some(7)));
        assert!(instructs_approval("approve the draft of task 7", Some(7)));
        assert!(instructs_approval("Approve all the drafts", Some(3)));
        for text in [
            "Do not approve #7",
            "never approve all drafts",
            "approve #8",
            "the owner must approve it",
            "fix the check",
        ] {
            assert!(!instructs_approval(text, Some(7)), "{text}");
        }
        assert!(!instructs_approval("approve #7", None));
        assert!(instructs_answer(
            "Answer the question with: use SQLite.",
            "Use SQLite"
        ));
        assert!(!instructs_answer("Answer the question", "Use SQLite"));
        assert!(!instructs_answer("use SQLite", "use SQLite"));
    }
}
