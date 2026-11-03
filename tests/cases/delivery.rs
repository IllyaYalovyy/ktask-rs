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
fn stopped_checkpoint(f: &Fixture) -> Result<String> {
    remote(f)?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in
 *" push "*) touch "$HOME/push-waiting"; sleep 30;;
esac
exec /usr/bin/git "$@"
"#,
    )?;
    f.ok(&["delivery", "--mode", "direct"])?;
    f.add("Retain verified checkpoint")?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("checkpoint before push", || {
        Ok(f.scratch.path().join("push-waiting").exists().then_some(()))
    })?;
    let checkpoint = f.git(&["rev-parse", "HEAD"])?;
    f.ok(&["pause"])?;
    assert!(runner.0.wait()?.success());
    f.ok(&["stop", "1", "--reason", "Change delivery settings"])?;
    fs::remove_file(f.scratch.path().join("bin/git"))?;
    Ok(checkpoint)
}

#[test]
fn stopped_checkpoint_uses_current_delivery_settings_in_real_remotes() -> Result {
    for change in ["local", "remote", "base", "unchanged"] {
        let f = Fixture::new()?;
        let checkpoint = stopped_checkpoint(&f)?;
        let old_remote = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
        let target = match change {
            "local" => {
                f.ok(&["delivery", "--mode", "local"])?;
                None
            }
            "remote" => {
                let second = f.scratch.path().join("second.git");
                assert!(
                    Command::new("git")
                        .args(["init", "--bare", "-q"])
                        .arg(&second)
                        .status()?
                        .success()
                );
                f.git(&["remote", "add", "second", second.to_str().unwrap()])?;
                f.git(&["push", "second", "HEAD^:refs/heads/main"])?;
                f.ok(&["delivery", "--remote", "second"])?;
                Some(("second", "main"))
            }
            "base" => {
                f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
                f.ok(&["delivery", "--base", "alternate"])?;
                Some(("origin", "alternate"))
            }
            _ => Some(("origin", "main")),
        };
        f.ok(&["run", "--one"])?;
        assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded", "{change}");
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, checkpoint, "{change}");
        if let Some((remote, branch)) = target {
            assert!(
                f.git(&["ls-remote", remote, &format!("refs/heads/{branch}")])?
                    .starts_with(checkpoint.trim()),
                "{change}"
            );
        }
        if change != "unchanged" {
            assert_eq!(
                f.git(&["ls-remote", "origin", "refs/heads/main"])?,
                old_remote,
                "{change}"
            );
        }
        let rows: Vec<(String,String,String,i64)> = f.db()?.connection().prepare("SELECT mode,remote,base_branch,retargeted FROM attempt_delivery ORDER BY attempt_id")?.query_map([], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?)))?.collect::<rusqlite::Result<_>>()?;
        assert_eq!(rows.len(), 2, "{change}");
        assert_eq!(
            rows[1].3, 0,
            "adoption must finish before delivery: {change}: {rows:?}"
        );
        if let Some(dir) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            fs::write(
                PathBuf::from(dir).join(format!("stopped-delivery-{change}.txt")),
                format!(
                    "change={change}\ncheckpoint={}\nold_origin_main={}\nnew_origin_main={}\nnew_attempt={:?}\n",
                    checkpoint.trim(),
                    old_remote.trim(),
                    f.git(&["ls-remote", "origin", "refs/heads/main"])?.trim(),
                    rows[1]
                ),
            )?;
        }
    }
    Ok(())
}

#[test]
fn stopped_direct_checkpoint_can_change_to_pull_request_delivery() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    let main_before = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
    mock_pr_destination(&f, "origin")?;
    gh(&f, false)?;
    f.ok(&["delivery", "--mode", "pr"])?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(
        f.git(&["ls-remote", "origin", "refs/heads/main"])?,
        main_before
    );
    let branch = f.git(&["branch", "--show-current"])?;
    assert!(branch.starts_with("ktask/project-"), "{branch}");
    assert!(
        f.git(&[
            "ls-remote",
            "origin",
            &format!("refs/heads/{}", branch.trim())
        ])?
        .starts_with(checkpoint.trim())
    );
    assert!(f.scratch.path().join("pr-created").exists());
    Ok(())
}

#[test]
fn stopped_retarget_keeps_its_branch_intent_on_an_unchanged_next_run() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
    f.ok(&["delivery", "--base", "alternate"])?;
    fs::remove_file(f.scratch.path().join("push-waiting"))?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in *" push "*) touch "$HOME/push-waiting"; sleep 30;; esac
exec /usr/bin/git "$@"
"#,
    )?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("retargeted checkpoint before push", || {
        Ok(f.scratch.path().join("push-waiting").exists().then_some(()))
    })?;
    f.ok(&["pause"])?;
    assert!(runner.0.wait()?.success());
    f.ok(&["stop", "1"])?;
    fs::remove_file(f.scratch.path().join("bin/git"))?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert!(
        f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(checkpoint.trim())
    );
    let retargeted: Vec<i64> = f
        .db()?
        .connection()
        .prepare("SELECT retargeted FROM attempt_delivery ORDER BY attempt_id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    assert_eq!(retargeted, vec![0, 0, 0]);
    Ok(())
}

fn remote_alternate_conflict(f: &Fixture) -> Result<String> {
    f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
    let main = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
    let peer = f.scratch.path().join("alternate-peer");
    f.git(&[
        "clone",
        "-q",
        f.scratch.path().join("remote.git").to_str().unwrap(),
        peer.to_str().unwrap(),
    ])?;
    f.git_in(&peer, &["switch", "alternate"])?;
    fs::write(peer.join("result.txt"), "remote\n")?;
    f.git_in(&peer, &["add", "result.txt"])?;
    f.git_in(
        &peer,
        &[
            "-c",
            "user.name=Peer",
            "-c",
            "user.email=peer@example.com",
            "commit",
            "-qm",
            "alternate edit",
        ],
    )?;
    f.git_in(&peer, &["push", "origin", "alternate"])?;
    f.ok(&["delivery", "--base", "alternate"])?;
    Ok(main)
}

#[test]
fn retargeted_direct_conflict_repairs_on_the_new_branch() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    let main = remote_alternate_conflict(&f)?;
    f.provider("resolve-conflict")?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(f.git(&["branch", "--show-current"])?.trim(), "alternate");
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?,
        "remote\nverified\n"
    );
    assert_eq!(f.git(&["ls-remote", "origin", "refs/heads/main"])?, main);
    let head = f.git(&["rev-parse", "HEAD"])?;
    assert_ne!(head, checkpoint);
    assert!(
        f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(head.trim())
    );
    let db = f.db()?;
    for role in ["check", "review", "validation"] {
        let count: i64 = db.connection().query_row("SELECT count(*) FROM attempt_steps WHERE attempt_id=(SELECT max(id) FROM attempts) AND role=?1", [role], |r| r.get(0))?;
        assert!(count >= 2, "{role}: {count}");
    }
    let repairs: i64 = db.connection().query_row("SELECT count(*) FROM attempt_steps WHERE attempt_id=(SELECT max(id) FROM attempts) AND role='remediation'", [], |r|r.get(0))?;
    assert!(repairs >= 1);
    Ok(())
}

#[test]
fn retargeted_direct_branch_serves_the_next_queued_task_in_real_pty() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    let main = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
    f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
    f.git(&["branch", "alternate", "HEAD^"])?;
    f.ok(&["delivery", "--base", "alternate"])?;
    f.provider("sequential-pr")?;
    f.add("Second queued task uses alternate")?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 130, 36)?;
    let ready = t.screen("two queued tasks", |s| {
        s.contains("Queue 2") && s.contains("r Run queue")
    })?;
    t.send("r")?;
    let done = t.screen("queue completed", |s| {
        s.contains("No current task") && s.contains("History 2")
    })?;
    t.send("3")?;
    let history = t.screen("both delivered tasks", |s| {
        s.contains("Retain verified checkpoint") && s.contains("Second queued task uses alternate")
    })?;
    assert_eq!(f.git(&["branch", "--show-current"])?.trim(), "alternate");
    assert_eq!(f.git(&["rev-parse", "main"])?, checkpoint);
    assert_eq!(f.git(&["ls-remote", "origin", "refs/heads/main"])?, main);
    let head = f.git(&["rev-parse", "HEAD"])?;
    assert!(
        f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(head.trim())
    );
    assert_eq!(f.ok(&["show", "2"])?["status"], "succeeded");
    if let Some(dir) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        fs::write(
            PathBuf::from(dir).join("retargeted-queue-pty.txt"),
            format!(
                "Before Run:\n{ready}\nAfter Run:\n{done}\nHistory:\n{history}\norigin_main={main}\nalternate_head={head}"
            ),
        )?;
    }
    t.send("\x03")?;
    t.finish()?;
    Ok(())
}

#[test]
fn retargeted_direct_repair_edits_survive_pause_and_resume_in_real_pty() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    let main = remote_alternate_conflict(&f)?;
    f.provider("resolve-conflict-pause")?;
    let script = f.scratch.path().join("bin/codex");
    let source = fs::read_to_string(&script)?;
    let before = "touch \"$HOME/conflict-repair-started\"\n sleep 30";
    assert!(source.contains(before));
    support::write_executable(&script, source.replace(before, "printf 'remote\\nverified\\n' > result.txt\n touch \"$HOME/conflict-repair-started\"\n sleep 30"))?;
    let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 130, 36)?;
    t.screen("retargeted task ready", |s| s.contains("r Run queue"))?;
    t.send("r")?;
    support::eventually("edited conflict repair", || {
        Ok((f.scratch.path().join("conflict-repair-started").exists()
            && fs::read_to_string(f.root.join("result.txt"))
                .ok()
                .as_deref()
                == Some("remote\nverified\n"))
        .then_some(()))
    })?;
    let running = t.screen("repair running", |s| {
        s.contains("p Pause") && s.contains("Step: Repair")
    })?;
    t.send("p")?;
    let paused = t.screen("repair paused", |s| {
        s.contains("Paused · runner stopped") && s.contains("r Resume")
    })?;
    let attempt: i64 = f
        .db()?
        .connection()
        .query_row("SELECT max(id) FROM attempts", [], |r| r.get(0))?;
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?,
        "remote\nverified\n"
    );
    f.provider("resolve-conflict")?;
    t.send("r")?;
    let done = t.screen("repair completed", |s| s.contains("No current task"))?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(
        f.db()?
            .connection()
            .query_row("SELECT max(id) FROM attempts", [], |r| r.get::<_, i64>(0))?,
        attempt
    );
    assert_ne!(f.git(&["rev-parse", "HEAD"])?, checkpoint);
    assert_eq!(f.git(&["ls-remote", "origin", "refs/heads/main"])?, main);
    assert_eq!(f.git(&["branch", "--show-current"])?.trim(), "alternate");
    assert_eq!(
        fs::read_to_string(f.root.join("result.txt"))?,
        "remote\nverified\n"
    );
    assert!(
        f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(f.git(&["rev-parse", "HEAD"])?.trim())
    );
    let db = f.db()?;
    for role in ["check", "review", "validation"] {
        let count: i64 = db.connection().query_row(
            "SELECT count(*) FROM attempt_steps WHERE attempt_id=?1 AND role=?2",
            rusqlite::params![attempt, role],
            |r| r.get(0),
        )?;
        assert!(count >= 2, "{role}: {count}");
    }
    if let Some(dir) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        fs::write(
            PathBuf::from(dir).join("retargeted-repair-pty.txt"),
            format!(
                "Running:\n{running}\nPaused:\n{paused}\nResumed:\n{done}\nattempt={attempt}\n"
            ),
        )?;
    }
    t.send("\x03")?;
    t.finish()?;
    Ok(())
}

#[test]
fn divergent_local_destination_branch_is_preserved() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
    f.git(&["branch", "alternate", "HEAD^"])?;
    f.git(&["switch", "alternate"])?;
    fs::write(f.root.join("divergent.txt"), "local branch work\n")?;
    f.git(&["add", "divergent.txt"])?;
    f.git(&["commit", "-qm", "local branch work"])?;
    let divergent = f.git(&["rev-parse", "HEAD"])?;
    f.git(&["switch", "main"])?;
    f.ok(&["delivery", "--base", "alternate"])?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "paused");
    assert_eq!(f.git(&["rev-parse", "HEAD"])?, checkpoint);
    assert_eq!(f.git(&["rev-parse", "alternate"])?, divergent);
    assert!(
        !f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(checkpoint.trim())
    );
    Ok(())
}

#[test]
fn interrupted_branch_adoption_reconciles_its_completed_switch() -> Result {
    let f = Fixture::new()?;
    let checkpoint = stopped_checkpoint(&f)?;
    let main = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
    f.git(&["push", "origin", "HEAD^:refs/heads/alternate"])?;
    f.ok(&["delivery", "--base", "alternate"])?;
    support::write_executable(
        &f.scratch.path().join("bin/git"),
        r#"#!/bin/sh
case " $* " in
 *" switch -c alternate "*) /usr/bin/git "$@" || exit $?; touch "$HOME/switch-completed"; sleep 30; exit 0;;
esac
exec /usr/bin/git "$@"
"#,
    )?;
    let mut runner = Running(f.process().args(["run", "--one"]).spawn()?);
    support::eventually("branch switched before acknowledgement", || {
        Ok(f.scratch
            .path()
            .join("switch-completed")
            .exists()
            .then_some(()))
    })?;
    f.ok(&["pause"])?;
    assert!(runner.0.wait()?.success());
    assert_eq!(f.git(&["branch", "--show-current"])?.trim(), "alternate");
    let pending: i64 = f.db()?.connection().query_row(
        "SELECT retargeted FROM attempt_delivery ORDER BY attempt_id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(pending, 1);
    fs::remove_file(f.scratch.path().join("bin/git"))?;
    f.ok(&["run", "--one"])?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "succeeded");
    assert_eq!(f.git(&["rev-parse", "HEAD"])?, checkpoint);
    assert_eq!(f.git(&["ls-remote", "origin", "refs/heads/main"])?, main);
    assert!(
        f.git(&["ls-remote", "origin", "refs/heads/alternate"])?
            .starts_with(checkpoint.trim())
    );
    let pending: i64 = f.db()?.connection().query_row(
        "SELECT retargeted FROM attempt_delivery ORDER BY attempt_id DESC LIMIT 1",
        [],
        |r| r.get(0),
    )?;
    assert_eq!(pending, 0);
    Ok(())
}

#[test]
fn stopped_checkpoint_delivery_change_runs_from_real_pty_with_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        let checkpoint = stopped_checkpoint(&f)?;
        let remote_before = f.git(&["ls-remote", "origin", "refs/heads/main"])?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 35)?;
        t.screen("stopped current", |s| {
            s.contains("Retain verified checkpoint") && s.contains("r Run queue")
        })?;
        if mouse {
            t.click("4 Config")?;
            t.screen("configuration navigation", |s| s.contains("g Git delivery"))?;
            t.click("g Git delivery")?;
        } else {
            t.send("4g")?;
        }
        t.screen("direct delivery form", |s| {
            s.contains("╭ Git delivery") && s.contains("Direct push")
        })?;
        if mouse {
            t.click("< Direct push >")?;
            t.screen("mode changed to pull request", |s| {
                s.contains("< Pull request >")
            })?;
            t.click("< Pull request >")?;
            t.screen("mode changed to local", |s| s.contains("< Local >"))?;
            t.click("Apply")?;
        } else {
            t.send("  \x13")?;
        }
        let saved = t.screen("local delivery saved", |s| s.contains("Delivery: local"))?;
        if mouse {
            t.click("1 Current")?;
            t.screen("current navigation", |s| s.contains("r Run queue"))?;
            t.click("r Run queue")?;
        } else {
            t.send("1r")?;
        }
        let done = t.screen("local completion", |s| s.contains("No current task"))?;
        println!("RECONFIGURED DELIVERY mouse={mouse}\n{done}");
        assert_eq!(f.git(&["rev-parse", "HEAD"])?, checkpoint);
        assert_eq!(
            f.git(&["ls-remote", "origin", "refs/heads/main"])?,
            remote_before
        );
        if let Some(dir) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            fs::write(
                PathBuf::from(dir).join(format!("stopped-delivery-pty-{mouse}.txt")),
                format!(
                    "Configuration after save:\n{saved}\nCurrent Task after Run:\n{done}\ncheckpoint={}\norigin_main={}\n",
                    checkpoint.trim(),
                    remote_before.trim()
                ),
            )?;
        }
        t.send("\x03")?;
        t.finish()?;
    }
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
            t.screen("paused", |s| s.contains("Paused · runner stopped")
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
                && s.contains(" Done "))?
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
    f.ok(&["pause"])?;
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
    f.ok(&["pause"])?;
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
    // A real supervisor crash, followed by guardian lifeline cleanup. Run settles the
    // orphaned attempt to Paused and resumes it.
    runner.0.kill()?;
    runner.0.wait()?;
    f.guardian_finished()?;
    assert_eq!(f.ok(&["show", "1"])?["status"], "running");
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
    use ktask_rs_mk3::application::{
        Service, VersionedTask,
        progress::{Phase, Progress},
    };
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
            false,
        )?;
        service.ensure_attempt_delivery(&run, &Default::default(), None, "")?;
        service.save_progress(
            &run,
            &Progress {
                round: 0,
                phase: Phase::Implement,
                context: "Retained local task".into(),
                evidence: String::new(),
                verified_tree: String::new(),
                why: String::new(),
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
        t.screen("retained synchronization", |s| s
            .contains("Current #1 · Paused")
            && s.contains("six synchronization cycles"))?
    );
    t.send("\x03")?;
    t.finish()?;
    fs::write(f.scratch.path().join("remote-settled"), "")?;
    f.ok(&[
        "stop",
        "1",
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
