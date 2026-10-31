//! Real Git fixtures; only provider and gh responses are deterministic mocks.
use super::*;

pub(super) fn mock_pr_destination(f: &Fixture, remote: &str) -> Result {
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        format!(
            r#"#!/bin/sh
case " $* " in
  *" remote get-url "*) printf 'https://github.com/owner/repo.git\n'; exit 0;;
esac
exec /usr/bin/git "$@"
# configured fixture remote: {remote}
"#
        ),
    )?;
    Ok(())
}
fn remote(f: &Fixture) -> Result<PathBuf> {
    f.git(&["branch", "-M", "main"])?;
    let bare = f.scratch.path().join("remote.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "-q"])
            .arg(&bare)
            .status()?
            .success()
    );
    f.git(&["remote", "add", "origin", bare.to_str().unwrap()])?;
    f.git(&["push", "origin", "main"])?;
    Ok(bare)
}
fn one_attempt(f: &Fixture) -> Result {
    let count: i64 = f.db()?.connection().query_row(
        "SELECT count(*) FROM attempts WHERE task_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(count, 1);
    Ok(())
}
#[test]
fn preflight_fetch_retries_without_starting_another_attempt() -> Result {
    let f = Fixture::new()?;
    remote(&f)?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in *" fetch "*) if [ ! -f "$HOME/fetch-failed" ]; then touch "$HOME/fetch-failed"; echo transient >&2; exit 1; fi;; esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.add("Retry preparation fetch")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    one_attempt(&f)
}
#[test]
fn real_delivery_preflight_pause_and_resume_both_inputs() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        remote(&f)?;
        support::write_executable(
            &f.scratch.path().join("bin/git"),
            r#"#!/bin/sh
case " $* " in *" fetch "*) if [ ! -f "$HOME/release-fetch" ]; then touch "$HOME/fetch-started"; sleep 30; fi;; esac
exec /usr/bin/git "$@"
"#,
        )?;
        f.ok(&["delivery", "--mode", "direct"])?;
        f.add("Pause Git preparation")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
        t.screen("run available", |s| s.contains("r Run queue"))?;
        if mouse {
            t.click("r Run queue")?;
        } else {
            t.send("r")?;
        }
        let preparing = t.screen("preparing", |s| s.contains("Git delivery · preparing"))?;
        assert!(!preparing.contains("Run blocked"), "{preparing}");
        println!("DELIVERY PREPARING mouse={mouse}\n{preparing}");
        if mouse {
            t.click("p Pause")?;
        } else {
            t.send("p")?;
        }
        println!(
            "DELIVERY PAUSED mouse={mouse}\n{}",
            t.screen("paused", |s| s.contains("PAUSED · runner stopped")
                && s.contains("r Resume"))?
        );
        fs::write(f.scratch.path().join("release-fetch"), "")?;
        if mouse {
            t.click("r Resume")?;
        } else {
            t.send("r")?;
        }
        t.screen("queue completed", |s| s.contains("No current task"))?;
        if mouse {
            t.click("3 History")?;
        } else {
            t.send("3")?;
        }
        println!(
            "DELIVERY HISTORY mouse={mouse}\n{}",
            t.screen("delivery history", |s| s.contains("Pause Git preparation")
                && s.contains("DONE"))?
        );
        one_attempt(&f)?;
        t.send("\x03")?;
        t.finish()?;
    }
    Ok(())
}
fn gh(f: &Fixture, lose_creation_ack: bool) -> Result {
    support::write_executable(
        &f.scratch.path().join("bin/gh"),
        format!(
            r#"#!/bin/sh
printf '%s\n' "$*" >> "$HOME/gh-calls"
case "$*" in
 repo\ view*) printf '{{"nameWithOwner":"owner/repo","url":"https://github.com/owner/repo"}}\n';;
 pr\ list*) if [ -f "$HOME/pr-created" ]; then printf '[{{"url":"https://github.com/owner/repo/pull/1","headRefOid":"%s"}}]\n' "$(git rev-parse HEAD)"; else printf '[]\n'; fi;;
 pr\ create*) touch "$HOME/pr-created"; if [ '{lose_creation_ack}' = true ]; then echo 'response lost' >&2; exit 1; fi; printf 'https://github.com/owner/repo/pull/1\n';;
 pr\ edit*) :;;
 *) exit 9;;
esac
"#
        ),
    )?;
    Ok(())
}
fn pr_fixture() -> Result<Fixture> {
    let f = Fixture::new()?;
    remote(&f)?;
    mock_pr_destination(&f, "origin")?;
    gh(&f, false)?;
    f.provider("sequential-pr")?;
    f.ok(&["delivery", "--mode", "pr"])?;
    Ok(f)
}
#[test]
fn pr_creation_ack_loss_is_reconciled_without_duplicate_and_url_is_visible() -> Result {
    let f = pr_fixture()?;
    gh(&f, true)?;
    f.add("Publish useful PR description")?;
    f.ok(&["run", "--one"])?;
    let calls = fs::read_to_string(f.scratch.path().join("gh-calls"))?;
    assert_eq!(calls.matches("pr create").count(), 1, "{calls}");
    assert!(calls.contains("--repo github.com/owner/repo"), "{calls}");
    assert!(calls.contains("Publish useful PR description"), "{calls}");
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "succeeded");
    assert!(
        task.to_string()
            .contains("https://github.com/owner/repo/pull/1")
    );
    one_attempt(&f)?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 140, 36)?;
    t.screen("no current", |s| s.contains("No current task"))?;
    t.send("3")?;
    t.screen("history", |s| s.contains("Publish useful PR description"))?;
    t.send("\r")?;
    println!(
        "PULL REQUEST HISTORY\n{}",
        t.screen("PR URL", |s| s.contains("github.com/owner/repo/pull/1"))?
    );
    t.send("\x1b\x03")?;
    t.finish()?;
    Ok(())
}
fn peer_commit(f: &Fixture, branch: &str, filename: &str) -> Result {
    let peer = f.scratch.path().join("peer");
    if !peer.exists() {
        f.git(&[
            "clone",
            "-q",
            f.scratch.path().join("remote.git").to_str().unwrap(),
            peer.to_str().unwrap(),
        ])?;
    }
    f.git_in(&peer, &["switch", branch])?;
    fs::write(peer.join(filename), "remote edit\n")?;
    f.git_in(&peer, &["add", filename])?;
    f.git_in(
        &peer,
        &[
            "-c",
            "user.name=Peer",
            "-c",
            "user.email=peer@example.com",
            "commit",
            "-qm",
            "peer",
        ],
    )?;
    f.git_in(&peer, &["push", "origin", branch])?;
    Ok(())
}
#[test]
fn successful_pr_push_interrupted_before_ack_reconciles_before_new_remote_base() -> Result {
    let f = pr_fixture()?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in
 *" remote get-url "*) printf 'https://github.com/owner/repo.git\n'; exit 0;;
 *" push "*) if [ ! -f "$HOME/push-done" ]; then /usr/bin/git "$@" || exit $?; touch "$HOME/push-done"; sleep 30; exit 0; fi;;
esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.add("Reconcile pushed checkpoint")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("push completed before ack", || {
        Ok(f.scratch.path().join("push-done").exists().then_some(()))
    })?;
    f.ok(&["pause", "1"])?;
    assert!(runner.0.wait()?.success());
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    peer_commit(&f, "main", "remote.txt")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert!(f.root.join("remote.txt").exists());
    one_attempt(&f)
}
#[test]
fn external_pr_branch_tip_is_never_overwritten() -> Result {
    let f = pr_fixture()?;
    f.add("First verified task")?;
    f.ok(&["run", "--one"])?;
    let branch = f.git(&["branch", "--show-current"])?;
    peer_commit(&f, branch.trim(), "external.txt")?;
    let external = f.git_in(&f.scratch.path().join("peer"), &["rev-parse", "HEAD"])?;
    f.add("Must retain foreign PR tip")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "2"])?["status"], "paused");
    let actual = f.git(&[
        "ls-remote",
        "origin",
        &format!("refs/heads/{}", branch.trim()),
    ])?;
    assert!(actual.starts_with(external.trim()));
    Ok(())
}
#[test]
fn merged_deleted_pr_branch_is_safely_recreated_for_later_work() -> Result {
    let f = pr_fixture()?;
    f.add("First PR")?;
    f.ok(&["run", "--one"])?;
    let branch = f.git(&["branch", "--show-current"])?;
    f.git(&["push", "origin", "HEAD:refs/heads/main"])?;
    f.git(&["push", "origin", "--delete", branch.trim()])?;
    fs::remove_file(f.scratch.path().join("pr-created"))?;
    f.add("Changes after merged PR")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
    let calls = fs::read_to_string(f.scratch.path().join("gh-calls"))?;
    assert_eq!(calls.matches("pr create").count(), 2, "{calls}");
    Ok(())
}
#[test]
fn delivery_config_rejects_invalid_refs_stale_edits_and_live_changes() -> Result {
    let f = Fixture::new()?;
    let initial = f.ok(&["delivery"])?;
    for name in [".hidden", "foo/.hidden", "foo.lock/bar", "foo..bar"] {
        let out = f
            .process()
            .args(["--json", "delivery", "--base", name])
            .output()?;
        assert!(!out.status.success(), "{name}");
        assert_eq!(f.ok(&["delivery"])?, initial);
    }
    let changed = f.ok(&["delivery", "--base", "trunk", "--if-version", "0"])?;
    let out = f
        .process()
        .args(["--json", "delivery", "--base", "main", "--if-version", "0"])
        .output()?;
    assert!(!out.status.success());
    assert_eq!(f.ok(&["delivery"])?, changed);
    f.provider("slow")?;
    f.add("Lock configuration")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("live attempt", || {
        Ok((f.ok(&["show", "1"])?["status"] == "running").then_some(()))
    })?;
    let out = f
        .process()
        .args(["--json", "delivery", "--mode", "direct"])
        .output()?;
    assert!(!out.status.success());
    f.ok(&["pause", "1"])?;
    runner.0.wait()?;
    Ok(())
}
fn advance_during_check(f: &Fixture, conflict: bool) -> Result {
    let file = if conflict { "result.txt" } else { "remote.txt" };
    f.ok(&[
        "check-set",
        "build",
        "--command",
        &format!(
            r#"test -f result.txt || exit 1
if [ ! -f "$HOME/remote-advanced" ]; then
 git clone -q "$HOME/remote.git" "$TMPDIR/peer" || exit 2
 git -C "$TMPDIR/peer" switch -q main || exit 3
 printf 'remote\n' > "$TMPDIR/peer/{file}"
 git -C "$TMPDIR/peer" add {file}
 git -C "$TMPDIR/peer" -c user.name=Peer -c user.email=peer@example.com commit -qm peer
 git -C "$TMPDIR/peer" push -q origin main || exit 4
 touch "$HOME/remote-advanced"
fi"#
        ),
    ])?;
    Ok(())
}
#[test]
fn completed_rebase_interrupted_before_state_save_regenerates_proof() -> Result {
    let f = Fixture::new()?;
    remote(&f)?;
    advance_during_check(&f, false)?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in *" rebase "*) if [ ! -f "$HOME/rebase-done" ]; then /usr/bin/git "$@" || exit $?; touch "$HOME/rebase-done"; sleep 30; exit 0; fi;; esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.add("Reconcile completed rebase")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("rebase completed before save", || {
        Ok(f.scratch.path().join("rebase-done").exists().then_some(()))
    })?;
    // A real supervisor crash, followed by guardian lifeline cleanup and explicit recovery.
    runner.0.kill()?;
    runner.0.wait()?;
    f.guardian_finished()?;
    f.ok(&["recover"])?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let calls = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert!(calls.matches("review").count() >= 2, "{calls}");
    assert!(calls.matches("validation").count() >= 2, "{calls}");
    one_attempt(&f)
}
#[test]
fn conflict_judge_revise_and_insert_transfer_integration_ownership() -> Result {
    for mode in ["judge-revise", "judge-insert"] {
        let f = Fixture::new()?;
        remote(&f)?;
        advance_during_check(&f, true)?;
        f.provider(mode)?;
        let path = f.scratch.path().join("bin/codex");
        let script = fs::read_to_string(&path)?.replace(
            "else printf 'verified\\n' > result.txt; fi\nelif",
            "else printf 'remote\\nverified\\n' > result.txt; fi\nelif",
        );
        support::write_executable(&path, script)?;
        f.ok(&["delivery", "--mode", "direct"])?;
        f.add("Preserve conflict through judge replan")?;
        f.ok(&["run"])?;
        assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded", "{mode}");
        assert_eq!(
            fs::read_to_string(f.root.join("result.txt"))?,
            "remote\nverified\n"
        );
        let db = f.db()?;
        let handed: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_delivery WHERE handoff_claimed_by IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        assert_eq!(handed, 1, "{mode}");
        let rounds: i64 = db.connection().query_row(
            "SELECT min(repair_round) FROM attempt_progress WHERE attempt_id>1",
            [],
            |r| r.get(0),
        )?;
        assert!(rounds >= 1, "{mode}");
    }
    Ok(())
}
#[test]
fn delivery_deadline_can_be_raised_and_retry_retains_intent() -> Result {
    let f = Fixture::new()?;
    remote(&f)?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in *" fetch "*) if [ ! -f "$HOME/fetch-released" ]; then sleep 30; fi;; esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.add("Adjust delivery deadline")?;
    f.ok(&["edit", "1", "--timeout-ms", "500"])?;
    let out = f.process().args(["run", "--one"]).output()?;
    assert!(!out.status.success());
    assert_eq!(f.ok(&["show", "1"])?["status"], "failed");
    fs::write(f.scratch.path().join("fetch-released"), "")?;
    f.ok(&["edit", "1", "--timeout-ms", "30000"])?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    Ok(())
}
#[test]
fn changed_delivery_policy_never_publishes_a_failed_local_tasks_unreviewed_baseline() -> Result {
    use ktask_rs_mk3::application::{Service, VersionedTask, progress::Progress};
    for (mode, branch) in [("direct", "feature"), ("direct", "main"), ("pr", "feature")] {
        let f = Fixture::new()?;
        remote(&f)?;
        let remote_base = f.git(&["rev-parse", "HEAD"])?;
        if branch == "feature" {
            f.git(&["switch", "-c", "feature"])?;
        }
        fs::write(
            f.root.join("unreviewed.txt"),
            "unrelated unpublished baseline\n",
        )?;
        f.git(&["add", "unreviewed.txt"])?;
        f.git(&[
            "-c",
            "user.name=User",
            "-c",
            "user.email=user@example.com",
            "commit",
            "-qm",
            "unreviewed baseline",
        ])?;
        let baseline = f.git(&["rev-parse", "HEAD"])?;
        f.add("Retain failed local implementation")?;
        let mut service = Service::open(f.db()?, &f.root, None)?;
        let task = service.current_task()?.unwrap();
        let run = service.begin_run(
            VersionedTask {
                id: task.id,
                version: task.version,
            },
            "fixture-local-failure",
            baseline.trim(),
        )?;
        service.ensure_attempt_delivery(&run, &Default::default(), None, "")?;
        service.save_progress(
            &run,
            &Progress {
                round: 0,
                phase: "implement".into(),
                context: "Retained local task".into(),
                evidence: String::new(),
                verified_tree: String::new(),
            },
        )?;
        fs::write(f.root.join("result.txt"), "useful retained edit\n")?;
        service.finish_run(
            &run,
            "failed",
            Some("agent"),
            "Fixture local executor failed",
            None,
        )?;
        drop(service);
        if mode == "pr" {
            mock_pr_destination(&f, "origin")?;
            gh(&f, false)?;
        }
        f.ok(&["delivery", "--mode", mode])?;
        f.ok(&["run", "--one"])?;
        assert_eq!(f.ok(&["show", "1"])?["status"], "paused", "{mode}/{branch}");
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, baseline);
        assert_eq!(
            fs::read_to_string(f.root.join("result.txt"))?,
            "useful retained edit\n"
        );
        assert!(
            f.git(&["ls-remote", "origin", "refs/heads/main"])?
                .starts_with(remote_base.trim())
        );
        assert!(!f.scratch.path().join("provider-invocations").exists());
    }
    Ok(())
}
#[test]
fn repeated_remote_churn_has_a_durable_bound_and_explicit_restart() -> Result {
    let f = Fixture::new()?;
    remote(&f)?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.ok(&[
        "check-set",
        "build",
        "--command",
        r#"test -f result.txt || exit 1
if [ ! -f "$HOME/remote-settled" ]; then
 git clone -q "$HOME/remote.git" "$TMPDIR/peer" || exit 2
 git -C "$TMPDIR/peer" switch -q main || exit 3
 printf 'remote advancement\n' >> "$TMPDIR/peer/remote.txt"
 git -C "$TMPDIR/peer" add remote.txt
 git -C "$TMPDIR/peer" -c user.name=Peer -c user.email=peer@example.com commit -qm peer
 git -C "$TMPDIR/peer" push -q origin main || exit 4
fi"#,
    ])?;
    f.add("Bound remote synchronization")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "paused");
    assert!(task.to_string().contains("six synchronization cycles"));
    let cycles: i64 = f.db()?.connection().query_row(
        "SELECT sync_cycles FROM attempt_delivery WHERE attempt_id=1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(cycles, 6);
    one_attempt(&f)?;
    let head = f.git(&["rev-parse", "HEAD"])?;
    let calls = fs::read_to_string(f.scratch.path().join("provider-invocations"))?;
    assert_eq!(calls.matches("review").count(), 7, "{calls}");
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    assert_eq!(f.git(&["rev-parse", "HEAD"])?, head);
    assert_eq!(
        fs::read_to_string(f.scratch.path().join("provider-invocations"))?,
        calls
    );
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 130, 36)?;
    println!(
        "BOUNDED REMOTE SYNC\n{}",
        t.screen("retained synchronization", |s| s.contains("PAUSED")
            && s.contains("six synchronization cycles"))?
    );
    t.send("\x03")?;
    t.finish()?;
    fs::write(f.scratch.path().join("remote-settled"), "")?;
    f.ok(&[
        "abandon",
        "1",
        "--attempt",
        "--reason",
        "Remote settled; explicitly restart bounded sync",
    ])?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    let counts: Vec<i64> = f
        .db()?
        .connection()
        .prepare("SELECT sync_cycles FROM attempt_delivery ORDER BY attempt_id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(counts, vec![6, 1]);
    Ok(())
}
