//! Verified Git delivery. Every network command runs under the worker guardian.
use super::{
    Context, Outcome,
    process::{Invocation, Message, Worker},
    workspace::{git, is_ancestor},
};
use crate::{
    application::{Service, delivery::DeliveryAttempt, execution::Run, progress::REPAIR_ROUNDS},
    storage::{Error, Result},
};
use std::{
    path::PathBuf,
    sync::atomic::Ordering,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(super) enum DeliveryError {
    Retry(String),
    Deadline,
    Safety(Error),
}
impl std::fmt::Display for DeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Retry(message) => message.fmt(f),
            Self::Deadline => f.write_str(
                "Active time limit reached during Git delivery; Retry restarts the limit",
            ),
            Self::Safety(error) => error.fmt(f),
        }
    }
}
impl From<Error> for DeliveryError {
    fn from(error: Error) -> Self {
        Self::Safety(error)
    }
}
impl From<std::io::Error> for DeliveryError {
    fn from(error: std::io::Error) -> Self {
        Self::Safety(error.into())
    }
}
type DeliveryResult<T> = std::result::Result<T, DeliveryError>;

pub(super) fn retry(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    error: &DeliveryError,
    retries: &mut u32,
) -> Result<Option<Outcome>> {
    if matches!(error, DeliveryError::Deadline) {
        return Ok(Some(Outcome::Failed("agent", error.to_string())));
    }
    if matches!(error, DeliveryError::Retry(_)) && *retries < 3 {
        service.output(run, "system", &format!("Git delivery retry: {error}"))?;
        let outcome = super::operational_backoff(service, run, ctx, *retries)?;
        *retries += 1;
        Ok(outcome)
    } else {
        Ok(Some(Outcome::DeliveryBlocked(error.to_string())))
    }
}

enum CommandResult {
    Exit(i32, String, String),
    Paused,
}

fn command(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    executable: &str,
    args: &[&str],
) -> DeliveryResult<CommandResult> {
    let identity = super::workspace::git_identity(&ctx.workspace.root)?;
    let invocation = Invocation {
        executable: PathBuf::from(executable),
        args: args.iter().map(|s| (*s).to_owned()).collect(),
        env: vec![
            ("GIT_TERMINAL_PROMPT".into(), "0".into()),
            ("GCM_INTERACTIVE".into(), "never".into()),
            ("GIT_EDITOR".into(), "true".into()),
            ("GH_PROMPT_DISABLED".into(), "1".into()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("GIT_CONFIG_COUNT".into(), "2".into()),
            ("GIT_CONFIG_KEY_0".into(), "core.hooksPath".into()),
            ("GIT_CONFIG_VALUE_0".into(), "/dev/null".into()),
            ("GIT_CONFIG_KEY_1".into(), "commit.gpgsign".into()),
            ("GIT_CONFIG_VALUE_1".into(), "false".into()),
            ("GIT_COMMITTER_NAME".into(), identity.name),
            ("GIT_COMMITTER_EMAIL".into(), identity.email),
        ],
        cwd: ctx.workspace.root.clone(),
        prompt: String::new(),
        schema: None,
        scratch_root: ctx.paths.cache.join("workers"),
        artifact_target: None,
        worker_lock: ctx.workspace.worker_lock.clone(),
        callback: None,
        sandbox: None,
    };
    if ctx.deadline_outcome(run).is_some() {
        return Err(DeliveryError::Deadline);
    }
    let mut worker = Worker::spawn(&std::env::current_exe()?, &invocation)
        .map_err(|e| DeliveryError::Retry(e.to_string()))?;
    let start = Instant::now();
    let mut last_active = Instant::now();
    let mut out = Vec::new();
    let mut err = Vec::new();
    loop {
        if last_active.elapsed() >= Duration::from_secs(1) {
            ctx.persist_active_time(service, run)?;
            last_active = Instant::now();
        }
        if ctx.stop.load(Ordering::Relaxed) || service.control_cursor(run)? > ctx.cursor {
            worker.join()?;
            return Ok(CommandResult::Paused);
        }
        if ctx.deadline_outcome(run).is_some() {
            worker.join()?;
            ctx.persist_active_time(service, run)?;
            return Err(DeliveryError::Deadline);
        }
        if start.elapsed() > Duration::from_secs(120) {
            worker.join()?;
            return Err(DeliveryError::Retry(format!(
                "{} timed out after 120 seconds",
                executable
            )));
        }
        match worker.messages.recv_timeout(Duration::from_millis(50)) {
            Ok(Message::Artifact { .. }) => {
                return Err(DeliveryError::Retry("Unexpected delivery artifact".into()));
            }
            Ok(Message::Output { stream, bytes }) => {
                let buffer = if stream == "stdout" {
                    &mut out
                } else {
                    &mut err
                };
                if buffer.len().saturating_add(bytes.len()) > 4 * 1024 * 1024 {
                    worker.join()?;
                    return Err(DeliveryError::Retry(
                        "Delivery command output exceeded 4 MiB".into(),
                    ));
                }
                buffer.extend(bytes);
            }
            Ok(Message::Exit { code }) => {
                worker.join()?;
                ctx.persist_active_time(service, run)?;
                if ctx.deadline_outcome(run).is_some() {
                    return Err(DeliveryError::Deadline);
                }
                return Ok(CommandResult::Exit(
                    code,
                    String::from_utf8_lossy(&out).trim().into(),
                    String::from_utf8_lossy(&err).trim().into(),
                ));
            }
            Ok(Message::Error(error)) => {
                worker.join()?;
                return Err(DeliveryError::Retry(error));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => {
                worker.join()?;
                return Err(DeliveryError::Retry("Delivery worker disconnected".into()));
            }
        }
    }
}
fn git_network(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    args: &[&str],
) -> DeliveryResult<CommandResult> {
    command(service, run, ctx, "git", args)
}
fn require_ok(result: CommandResult, label: &str) -> DeliveryResult<Option<String>> {
    match result {
        CommandResult::Paused => Ok(None),
        CommandResult::Exit(0, out, _) => Ok(Some(out)),
        CommandResult::Exit(_, _, err) => Err(DeliveryError::Retry(format!("{label}: {err}"))),
    }
}
fn remote_ref(remote: &str, branch: &str) -> String {
    format!("refs/remotes/{remote}/{branch}")
}
fn fetch_refspec(remote: &str, branch: &str) -> String {
    format!("+refs/heads/{branch}:{}", remote_ref(remote, branch))
}
fn remote_head(root: &std::path::Path, remote: &str, branch: &str) -> Result<String> {
    git(
        root,
        &["rev-parse", "--verify", &remote_ref(remote, branch)],
    )
}
fn current_branch(root: &std::path::Path) -> Result<String> {
    git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
}

pub(super) fn reconcile_resume(service: &mut Service, run: &Run, ctx: &Context<'_>) -> Result<()> {
    let delivery = service.attempt_delivery(run)?;
    if delivery.stage == "work" {
        return Ok(());
    }
    let expected = if delivery.mode == "pr" {
        delivery.branch.as_deref()
    } else {
        Some(delivery.base_branch.as_str())
    };
    let git_dir = git(&ctx.workspace.root, &["rev-parse", "--absolute-git-dir"])?;
    let rebasing = ["rebase-merge", "rebase-apply"]
        .iter()
        .any(|name| std::path::Path::new(&git_dir).join(name).exists());
    if rebasing {
        if delivery.stage != "integrating" {
            return Err(Error::Conflict(
                "Unexpected Git rebase during delivery".into(),
            ));
        }
        let head_name = ["rebase-merge/head-name", "rebase-apply/head-name"]
            .iter()
            .find_map(|name| {
                std::fs::read_to_string(std::path::Path::new(&git_dir).join(name)).ok()
            });
        if delivery.mode != "local"
            && head_name.as_deref().map(str::trim)
                != expected
                    .map(|branch| format!("refs/heads/{branch}"))
                    .as_deref()
        {
            return Err(Error::Conflict(
                "Rebase belongs to a different branch".into(),
            ));
        }
    } else if delivery.mode != "local"
        && current_branch(&ctx.workspace.root)?.as_str() != expected.unwrap_or_default()
    {
        return Err(Error::Conflict(
            "Delivery branch changed while attempt was paused".into(),
        ));
    }
    if delivery.stage == "checkpointing" {
        let saved = delivery
            .checkpoint_head
            .as_deref()
            .ok_or_else(|| Error::Conflict("Missing checkpoint intent".into()))?;
        let actual = ctx.workspace.head()?;
        if actual != saved {
            let verified = service
                .progress(run)?
                .ok_or_else(|| Error::Conflict("Missing checkpoint proof".into()))?
                .verified_tree;
            let tree = git(&ctx.workspace.root, &["rev-parse", "HEAD^{tree}"])?;
            let parent = git(&ctx.workspace.root, &["rev-parse", "HEAD^"])?;
            let prior_parent = git(&ctx.workspace.root, &["rev-parse", &format!("{saved}^")]).ok();
            let subject = git(&ctx.workspace.root, &["log", "-1", "--format=%s"])?;
            if tree != verified
                || !ctx.workspace.clean()?
                || subject != format!("ktask: task #{}, attempt #{}", run.task, run.attempt)
                || (parent != saved && prior_parent.as_deref() != Some(parent.as_str()))
            {
                return Err(Error::Conflict(
                    "Checkpoint commit differs from saved intent; publishing stopped".into(),
                ));
            }
            service.delivery_stage(run, "checkpointed", Some(&actual), None, None)?;
        }
    }
    if delivery.stage == "integrating" {
        let progress = service
            .progress(run)?
            .ok_or_else(|| Error::Conflict("Missing delivery progress".into()))?;
        if rebasing {
            if matches!(
                progress.phase.as_str(),
                "complete" | "check" | "review" | "validate"
            ) {
                if has_conflicts(&ctx.workspace.root)? {
                    conflict(
                        service,
                        run,
                        "Rebase was interrupted; inspect the retained conflict and continue repair",
                    )?;
                } else {
                    let mut progress = progress;
                    progress.phase = "integrate".into();
                    service.save_progress(run, &progress)?;
                }
            }
        } else {
            let head = ctx.workspace.head()?;
            if delivery.checkpoint_head.as_deref() != Some(&head) {
                let original = git(&ctx.workspace.root, &["rev-parse", "ORIG_HEAD"])?;
                let action = git(
                    &ctx.workspace.root,
                    &["reflog", "-1", "--format=%gs", "HEAD"],
                )?;
                let base = delivery
                    .fetched_base
                    .as_deref()
                    .ok_or_else(|| Error::Conflict("Missing integration target".into()))?;
                if delivery.checkpoint_head.as_deref() != Some(&original)
                    || !action.starts_with("rebase (finish):")
                    || !is_ancestor(&ctx.workspace.root, base, &head)?
                {
                    return Err(Error::Conflict(
                        "HEAD changed outside the recorded rebase; publishing stopped".into(),
                    ));
                }
            }
            reset_verification(
                service,
                run,
                &head,
                "Rebase completed before supervisor handoff; regenerate checks, review and validation",
            )?;
        }
    }
    Ok(())
}

/// Preparation belongs to the live attempt, so pause, retries and crash recovery retain it.
pub(super) fn prepare(service: &mut Service, run: &Run, ctx: &Context<'_>) -> DeliveryResult<bool> {
    let delivery = service.attempt_delivery(run)?;
    if delivery.stage != "preparing" {
        return Ok(true);
    }
    service.delivery_stage(run, "preparing", None, None, None)?;
    check_destination(&ctx.workspace.root, &delivery)?;
    if delivery.mode == "pr" {
        super::executable("gh")?;
    }
    let refspec = fetch_refspec(&delivery.remote, &delivery.base_branch);
    if require_ok(
        git_network(
            service,
            run,
            ctx,
            &["fetch", "--no-tags", "--", &delivery.remote, &refspec],
        )?,
        "Git fetch",
    )?
    .is_none()
    {
        return Ok(false);
    }
    let base = remote_head(&ctx.workspace.root, &delivery.remote, &delivery.base_branch)?;
    let branch = current_branch(&ctx.workspace.root)?;
    let head = ctx.workspace.head()?;
    let prior = service
        .attempt_base(run.task)?
        .ok_or_else(|| Error::Conflict("Missing preparation baseline".into()))?;
    if !ctx.workspace.clean()? {
        // A prior attempt owns its edits, not permission to publish its baseline under
        // a newly selected delivery policy. Validate destination/branch ownership first.
        if delivery.mode == "direct" {
            if branch != delivery.base_branch || !is_ancestor(&ctx.workspace.root, &head, &base)? {
                return Err(Error::Conflict("Retained work is not based on the configured remote base branch; unpublished baseline will not be delivered".into()).into());
            }
        } else {
            let owned = service.delivery_owned_tip()?;
            if delivery.branch.as_deref() != Some(&branch)
                || head != owned.as_deref().unwrap_or(&base)
            {
                return Err(Error::Conflict(
                    "Retained work is not on the recorded owned PR branch and baseline".into(),
                )
                .into());
            }
        }
        // Retry retained work without changing its baseline. Integration follows verification.
        if head == prior && service.prior_progress(run)?.is_some() {
            service.finish_delivery_prepare(run, &head)?;
            return Ok(true);
        }
        return Err(
            Error::Conflict("Delivery preparation requires a clean workspace".into()).into(),
        );
    }
    if delivery.mode == "direct" {
        if branch != delivery.base_branch {
            return Err(Error::Conflict(format!(
                "Direct delivery requires checkout of {}",
                delivery.base_branch
            ))
            .into());
        }
        if head != base {
            if !is_ancestor(&ctx.workspace.root, &head, &base)? {
                return Err(Error::Conflict("Local branch contains unpublished commits; direct delivery will not publish an unverified baseline".into()).into());
            }
            // Persist the target before the side effect; a resumed fetch may advance further.
            service.delivery_stage(run, "preparing", Some(&head), Some(&base), None)?;
            if require_ok(
                git_network(service, run, ctx, &["merge", "--ff-only", &base])?,
                "Git fast-forward",
            )?
            .is_none()
            {
                return Ok(false);
            }
        }
    } else {
        let project_branch = delivery
            .branch
            .as_deref()
            .ok_or_else(|| Error::Conflict("Missing PR branch".into()))?;
        let owned = service.delivery_owned_tip()?;
        let head = if owned.is_none()
            && branch == delivery.base_branch
            && head != base
            && is_ancestor(&ctx.workspace.root, &head, &base)?
        {
            service.delivery_stage(run, "preparing", Some(&head), Some(&base), None)?;
            if require_ok(
                git_network(service, run, ctx, &["merge", "--ff-only", &base])?,
                "Git fast-forward",
            )?
            .is_none()
            {
                return Ok(false);
            }
            ctx.workspace.head()?
        } else {
            head
        };
        if branch != project_branch {
            if let Ok(tip) = git(
                &ctx.workspace.root,
                &[
                    "rev-parse",
                    "--verify",
                    &format!("refs/heads/{project_branch}"),
                ],
            ) {
                if tip != owned.as_deref().unwrap_or(&base) {
                    return Err(Error::Conflict(
                        "Existing local PR branch is not the recorded owned tip".into(),
                    )
                    .into());
                }
                if require_ok(
                    git_network(service, run, ctx, &["switch", project_branch])?,
                    "Git switch",
                )?
                .is_none()
                {
                    return Ok(false);
                }
            } else {
                if branch != delivery.base_branch || head != base {
                    return Err(Error::Conflict(
                        "Pull request branch must start from a clean remote base branch".into(),
                    )
                    .into());
                }
                if require_ok(
                    git_network(service, run, ctx, &["switch", "-c", project_branch])?,
                    "Git branch creation",
                )?
                .is_none()
                {
                    return Ok(false);
                }
            }
        }
        if ctx.workspace.head()? != owned.as_deref().unwrap_or(&base) {
            return Err(Error::Conflict(
                "PR branch contains unpublished local commits outside the current attempt".into(),
            )
            .into());
        }
    }
    service.finish_delivery_prepare(run, &ctx.workspace.head()?)?;
    Ok(true)
}

fn check_destination(root: &std::path::Path, delivery: &DeliveryAttempt) -> Result<()> {
    let fetch = git(root, &["remote", "get-url", &delivery.remote])?;
    let push = git(root, &["remote", "get-url", "--push", &delivery.remote])?;
    if fetch != delivery.destination_url || push != delivery.destination_url {
        return Err(Error::Conflict(
            "Delivery remote URL changed or fetch/push destinations differ".into(),
        ));
    }
    Ok(())
}

pub(super) enum DeliveryOutcome {
    Done(String),
    Retry,
    Paused,
    Exhausted(String),
}
pub(super) fn publish(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
    summary: &str,
    head: &str,
) -> DeliveryResult<DeliveryOutcome> {
    let delivery = service.attempt_delivery(run)?;
    if delivery.mode == "local" {
        return Ok(DeliveryOutcome::Done(summary.into()));
    }
    if delivery.mode == "pr" {
        repository_identity(&delivery.destination_url)?;
    }
    let fetch_url = git(
        &ctx.workspace.root,
        &["remote", "get-url", &delivery.remote],
    )?;
    let push_url = git(
        &ctx.workspace.root,
        &["remote", "get-url", "--push", &delivery.remote],
    )?;
    if fetch_url != delivery.destination_url || push_url != delivery.destination_url {
        return Err(
            Error::Conflict("Delivery remote URL changed during the attempt".into()).into(),
        );
    }
    if delivery.mode == "pr"
        && let Some((pending, expected)) = service.pending_delivery_push(run)?
    {
        let Some(remote) = remote_tip(service, run, ctx, &delivery)? else {
            return Ok(DeliveryOutcome::Paused);
        };
        if remote == pending {
            service.record_delivery_tip(run, service.delivery_owned_tip()?.as_deref(), &pending)?;
        } else if remote != expected && !remote.is_empty() {
            return Err(Error::Conflict(
                "PR branch changed outside the recorded push intent".into(),
            )
            .into());
        }
    }
    service.delivery_stage(run, "checkpointed", Some(head), None, None)?;
    let refspec = fetch_refspec(&delivery.remote, &delivery.base_branch);
    let fetched = require_ok(
        git_network(
            service,
            run,
            ctx,
            &["fetch", "--no-tags", "--", &delivery.remote, &refspec],
        )?,
        "Git fetch",
    )?;
    if fetched.is_none() {
        return Ok(DeliveryOutcome::Paused);
    }
    let base = remote_head(&ctx.workspace.root, &delivery.remote, &delivery.base_branch)?;
    let ancestor = std::process::Command::new("git")
        .arg("-C")
        .arg(&ctx.workspace.root)
        .args(["merge-base", "--is-ancestor", &base, head])
        .status()?;
    if !ancestor.success() {
        if !service.begin_delivery_integration(run, &base)? {
            return Err(Error::Conflict("Remote advanced beyond six synchronization cycles. Work and proof are retained; Resume preserves this bound. After remote writers settle, s Stop, then r Run for a fresh bounded synchronization batch.".into()).into());
        }
        match git_network(service, run, ctx, &["rebase", &base])? {
            CommandResult::Paused => return Ok(DeliveryOutcome::Paused),
            CommandResult::Exit(0, _, _) => {
                reset_verification(
                    service,
                    run,
                    &ctx.workspace.head()?,
                    "Remote base advanced; rebased checkpoint requires new checks and independent review",
                )?;
                return Ok(DeliveryOutcome::Retry);
            }
            CommandResult::Exit(_, _, err) => {
                if !has_conflicts(&ctx.workspace.root)? {
                    return Err(DeliveryError::Retry(format!("Git rebase failed: {err}")));
                }
                if service
                    .progress(run)?
                    .is_some_and(|p| p.round >= REPAIR_ROUNDS)
                {
                    return Ok(DeliveryOutcome::Exhausted(err));
                }
                conflict(service, run, &err)?;
                return Ok(DeliveryOutcome::Retry);
            }
        }
    }
    service.delivery_stage(run, "checkpointed", None, Some(&base), None)?;
    let current = ctx.workspace.head()?;
    if current != head {
        return Err(Error::Conflict(
            "HEAD changed before delivery; verification retained and push stopped".into(),
        )
        .into());
    }
    let refspec = if delivery.mode == "direct" {
        format!("HEAD:refs/heads/{}", delivery.base_branch)
    } else {
        format!(
            "HEAD:refs/heads/{}",
            delivery
                .branch
                .as_deref()
                .ok_or_else(|| Error::Conflict("Missing PR branch".into()))?
        )
    };
    let remote_branch = if delivery.mode == "direct" {
        base.clone()
    } else {
        match remote_tip(service, run, ctx, &delivery)? {
            Some(tip) => tip,
            None => return Ok(DeliveryOutcome::Paused),
        }
    };
    if delivery.mode == "pr" {
        let owned = service.delivery_owned_tip()?;
        if !remote_branch.is_empty()
            && remote_branch != owned.clone().unwrap_or_default()
            && remote_branch != current
        {
            return Err(Error::Conflict(
                "PR branch has external commits; refusing to overwrite an unowned remote tip"
                    .into(),
            )
            .into());
        }
    }
    if remote_branch != current {
        let lease = if delivery.mode == "pr" {
            Some(format!(
                "--force-with-lease=refs/heads/{}:{}",
                delivery.branch.as_deref().unwrap(),
                remote_branch
            ))
        } else {
            None
        };
        let mut args = vec!["push"];
        if let Some(ref lease) = lease {
            args.push(lease)
        }
        args.extend(["--", &delivery.remote, &refspec]);
        service.delivery_push_intent(run, &current, &remote_branch)?;
        match git_network(service, run, ctx, &args)? {
            CommandResult::Paused => return Ok(DeliveryOutcome::Paused),
            CommandResult::Exit(0, _, _) => {}
            CommandResult::Exit(_, _, err) => {
                return Err(DeliveryError::Retry(format!("Git push failed: {err}")));
            }
        }
    }
    if delivery.mode == "pr" {
        service.record_delivery_tip(run, service.delivery_owned_tip()?.as_deref(), &current)?;
    } else {
        service.delivery_stage(run, "pushed", None, None, None)?;
    }
    if delivery.mode == "pr"
        && git(&ctx.workspace.root, &["rev-parse", "HEAD^{tree}"])?
            == git(
                &ctx.workspace.root,
                &["rev-parse", &format!("{base}^{{tree}}")],
            )?
    {
        return Ok(DeliveryOutcome::Done(format!(
            "{summary}; project branch matches the base, no pull request needed"
        )));
    }
    if delivery.mode == "pr" {
        let Some(url) = ensure_pr(service, run, ctx, &delivery)? else {
            return Ok(DeliveryOutcome::Paused);
        };
        service.delivery_stage(run, "pr_ready", None, None, Some(&url))?;
        return Ok(DeliveryOutcome::Done(format!(
            "{summary}; pull request {url}"
        )));
    }
    Ok(DeliveryOutcome::Done(format!(
        "{summary}; published {} to {}/{}",
        current,
        delivery.remote,
        if delivery.mode == "direct" {
            &delivery.base_branch
        } else {
            delivery.branch.as_deref().unwrap()
        }
    )))
}

fn remote_tip(
    service: &Service,
    run: &Run,
    ctx: &Context<'_>,
    delivery: &DeliveryAttempt,
) -> DeliveryResult<Option<String>> {
    let refname = format!("refs/heads/{}", delivery.branch.as_deref().unwrap());
    match require_ok(
        git_network(
            service,
            run,
            ctx,
            &["ls-remote", "--heads", "--", &delivery.remote, &refname],
        )?,
        "Git ls-remote",
    )? {
        None => Ok(None),
        Some(out) => Ok(Some(out.split_whitespace().next().unwrap_or("").into())),
    }
}
fn reset_verification(service: &mut Service, run: &Run, head: &str, reason: &str) -> Result<()> {
    service.output(run, "system", reason)?;
    service.reset_delivery_verification(run, head, reason)
}
fn has_conflicts(root: &std::path::Path) -> Result<bool> {
    Ok(!git(root, &["diff", "--name-only", "--diff-filter=U"])?.is_empty())
}
fn conflict(service: &mut Service, run: &Run, reason: &str) -> Result<()> {
    let mut progress = service
        .progress(run)?
        .ok_or_else(|| Error::Conflict("Missing verification progress".into()))?;
    if progress.round >= REPAIR_ROUNDS {
        return Err(Error::Conflict(format!(
            "Remote conflict remains after {REPAIR_ROUNDS} repair rounds: {reason}"
        )));
    }
    super::cycle::request_fixes(
        service,
        run,
        &mut progress,
        &format!(
            "Remote rebase conflict. Preserve work and resolve conflicting files without committing. Supervisor will continue the rebase. Git reported: {reason}"
        ),
    )?;
    progress.verified_tree.clear();
    service.save_progress(run, &progress)
}
pub(super) fn continue_integration(
    service: &mut Service,
    run: &Run,
    ctx: &mut Context<'_>,
) -> Result<Outcome> {
    let mut retries = 0;
    loop {
        git(&ctx.workspace.root, &["add", "--all"])?;
        let result = git_network(
            service,
            run,
            ctx,
            &["-c", "core.editor=true", "rebase", "--continue"],
        );
        let error = match result {
            Ok(CommandResult::Paused) => return Ok(Outcome::Paused),
            Ok(CommandResult::Exit(0, _, _)) => {
                reset_verification(
                    service,
                    run,
                    &ctx.workspace.head()?,
                    "Rebase completed; regenerating proof",
                )?;
                return Ok(Outcome::Passed(
                    "Rebase completed; regenerating proof".into(),
                ));
            }
            Ok(CommandResult::Exit(_, _, err)) if has_conflicts(&ctx.workspace.root)? => {
                return Ok(Outcome::Changes {
                    report: format!("Rebase still needs conflict resolution: {err}"),
                    critical: true,
                });
            }
            Ok(CommandResult::Exit(_, _, err)) => {
                DeliveryError::Retry(format!("Git rebase continuation: {err}"))
            }
            Err(error) => error,
        };
        if let Some(outcome) = retry(service, run, ctx, &error, &mut retries)? {
            return Ok(outcome);
        }
    }
}

fn repository_identity(destination: &str) -> Result<String> {
    let normalized = if let Some(rest) = destination
        .strip_prefix("https://")
        .or_else(|| destination.strip_prefix("http://"))
        .or_else(|| destination.strip_prefix("ssh://"))
    {
        rest.rsplit_once('@')
            .map_or(rest, |(_, suffix)| suffix)
            .to_owned()
    } else if let Some((host, path)) = destination.split_once(':') {
        format!("{}/{}", host.rsplit('@').next().unwrap_or(host), path)
    } else {
        return Err(Error::Conflict(
            "PR delivery requires a host-qualified Git remote URL".into(),
        ));
    };
    let normalized = normalized.trim_end_matches('/').trim_end_matches(".git");
    let parts: Vec<_> = normalized.split('/').collect();
    if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) || normalized.contains(['?', '#']) {
        return Err(Error::Conflict(
            "Cannot identify the PR repository from its Git destination".into(),
        ));
    }
    Ok(normalized.to_ascii_lowercase())
}
fn ensure_pr(
    service: &mut Service,
    run: &Run,
    ctx: &Context<'_>,
    delivery: &DeliveryAttempt,
) -> DeliveryResult<Option<String>> {
    check_destination(&ctx.workspace.root, delivery)?;
    let repo = repository_identity(&delivery.destination_url)?;
    let Some(identity) = require_ok(
        command(
            service,
            run,
            ctx,
            "gh",
            &["repo", "view", &repo, "--json", "nameWithOwner,url"],
        )?,
        "gh repo view",
    )?
    else {
        return Ok(None);
    };
    let identity: serde_json::Value = serde_json::from_str(&identity)
        .map_err(|e| Error::Conflict(format!("Invalid gh repository identity: {e}")))?;
    let canonical = identity["url"]
        .as_str()
        .ok_or_else(|| Error::Conflict("gh repository URL missing".into()))?;
    let expected_name = repo
        .split_once('/')
        .map(|(_, name)| name)
        .unwrap_or_default();
    if repository_identity(canonical)? != repo
        || identity["nameWithOwner"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .as_deref()
            != Some(expected_name)
    {
        return Err(Error::Conflict(
            "GitHub repository identity differs from the pinned Git destination".into(),
        )
        .into());
    }
    let branch = delivery
        .branch
        .as_deref()
        .ok_or_else(|| Error::Conflict("Missing PR branch".into()))?;
    let (title, body) = service.delivery_pr_description(run)?;
    // Always query the exact repository and branch again after creation. This reconciles
    // successful requests whose response was lost, and verifies the returned head.
    for created in [false, true] {
        let Some(listed) = require_ok(
            command(
                service,
                run,
                ctx,
                "gh",
                &[
                    "pr",
                    "list",
                    "--repo",
                    &repo,
                    "--head",
                    branch,
                    "--base",
                    &delivery.base_branch,
                    "--state",
                    "open",
                    "--json",
                    "url,headRefOid",
                ],
            )?,
            "gh pr list",
        )?
        else {
            return Ok(None);
        };
        let prs: Vec<serde_json::Value> = serde_json::from_str(&listed)
            .map_err(|e| Error::Conflict(format!("Invalid gh PR list: {e}")))?;
        if prs.len() > 1 {
            return Err(
                Error::Conflict("Multiple open PRs match the delivery branch".into()).into(),
            );
        }
        if let Some(pr) = prs.first() {
            if pr["headRefOid"].as_str() != Some(ctx.workspace.head()?.as_str()) {
                return Err(DeliveryError::Retry(
                    "PR head has not caught up with the verified pushed commit".into(),
                ));
            }
            let url = pr["url"]
                .as_str()
                .ok_or_else(|| Error::Conflict("PR URL missing".into()))?;
            if !url.starts_with(&format!("https://{repo}/pull/")) {
                return Err(
                    Error::Conflict("PR URL differs from the pinned repository".into()).into(),
                );
            }
            if require_ok(
                command(
                    service,
                    run,
                    ctx,
                    "gh",
                    &[
                        "pr", "edit", url, "--repo", &repo, "--title", &title, "--body", &body,
                    ],
                )?,
                "gh pr edit",
            )?
            .is_none()
            {
                return Ok(None);
            }
            return Ok(Some(url.into()));
        }
        if created {
            return Err(DeliveryError::Retry(
                "Created PR is not visible yet; reconciling before retry".into(),
            ));
        }
        if require_ok(
            command(
                service,
                run,
                ctx,
                "gh",
                &[
                    "pr",
                    "create",
                    "--repo",
                    &repo,
                    "--head",
                    branch,
                    "--base",
                    &delivery.base_branch,
                    "--title",
                    &title,
                    "--body",
                    &body,
                ],
            )?,
            "gh pr create",
        )?
        .is_none()
        {
            return Ok(None);
        }
    }
    unreachable!("creation is followed by reconciliation")
}
