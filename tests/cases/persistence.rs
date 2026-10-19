use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{Definition, Kind, ModelConfig, Service, VersionedTask, transfer},
    storage::Database,
};
use std::path::Path;
use std::{fs, os::unix::fs::PermissionsExt};

fn definition(title: &str) -> Definition {
    Definition {
        title: title.into(),
        goal: "Persist a real task".into(),
        criteria: vec!["Survives restart".into()],
        kind: Kind::Ai,
        models: std::array::from_fn(|_| None),
        attempt_limit: Some(3),
        timeout_ms: Some(600_000),
    }
}
fn service(root: &Path) -> Result<Service> {
    Ok(Service::open(
        Database::open_in(&root.join("state/ktask-dev"))?,
        root,
        None,
    )?)
}
fn target(service: &mut Service, id: i64) -> Result<VersionedTask> {
    let snapshot = service.snapshot()?;
    let task = snapshot
        .queue
        .iter()
        .chain(&snapshot.history)
        .find(|t| t.id == id)
        .unwrap();
    Ok(VersionedTask {
        id,
        version: task.version,
    })
}
fn terminal(root: &Path) -> Result<Terminal> {
    Terminal::open_real(env!("CARGO_BIN_EXE_ktask-dev"), root, 70, 22)
}
fn key(t: &mut Terminal, keys: &str, text: &str) -> Result<String> {
    t.send(keys)?;
    t.screen(text, |s| s.contains(text))
}
fn quit(t: &mut Terminal) -> Result {
    t.send("\x03")?;
    t.finish()
}

fn table_contents(root: &Path, table: &str) -> Result<Vec<String>> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let mut stmt = db.connection().prepare(&format!("SELECT * FROM {table}"))?;
    let count = stmt.column_count();
    Ok(stmt
        .query_map([], |row| {
            (0..count)
                .map(|i| {
                    row.get::<_, rusqlite::types::Value>(i)
                        .map(|v| format!("{v:?}"))
                })
                .collect::<rusqlite::Result<Vec<_>>>()
                .map(|v| v.join("|"))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}
fn attempts_contents(root: &Path) -> Result<Vec<String>> {
    table_contents(root, "attempts")
}
fn storage_contents(root: &Path) -> Result<Vec<Vec<String>>> {
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let mut stmt = db
        .connection()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")?;
    let tables = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tables
        .iter()
        .map(|name| table_contents(root, name))
        .collect()
}

fn failed_task(root: &Path) -> Result<(i64, i64)> {
    let mut service = service(root)?;
    let mut d = definition("Failed current");
    for role in 0..4 {
        d.models[role] = Some(ModelConfig {
            provider: "codex".into(),
            model: "fixture-model".into(),
            effort: Some("high".into()),
        });
    }
    let id = service.create(&[d], None, true)?[0];
    let target = target(&mut service, id)?;
    let run = service.begin_run(target, "fixture-token", "fixture-base")?;
    service.output(&run, "stderr", "Check failed: transcript evidence")?;
    for role in ["review", "validation"] {
        let model = ModelConfig {
            provider: "codex".into(),
            model: "fixture-model".into(),
            effort: None,
        };
        let step = service.start_ai_step(&run, role, &model, "fixture", false)?;
        service.criterion(&run, &step, 1, false, &format!("{role} criterion evidence"))?;
        service.finish_step(&step, false, &format!("{role} structured finding"), None)?;
    }
    service.finish_run(
        &run,
        "failed",
        Some("check"),
        "Failed check with review finding",
        None,
    )?;
    Ok((id, run.attempt))
}

fn provider_fixture(root: &Path, proposal: serde_json::Value, fail: bool) -> Result<()> {
    let bin = root.join("bin");
    fs::create_dir_all(&bin)?;
    let fixture = root.join("provider-events.jsonl");
    let body = if fail {
        "not-json\n".to_owned()
    } else {
        let message = if proposal.get("action").is_some() {
            serde_json::json!({"proposal": proposal}).to_string()
        } else {
            proposal.to_string()
        };
        [serde_json::json!({"type":"thread.started","thread_id":"fixture"}),
         serde_json::json!({"type":"item.completed","item":{"type":"agent_message","text":message}}),
         serde_json::json!({"type":"turn.completed","usage":{"input_tokens":1,"cached_input_tokens":0,"output_tokens":1}})]
            .iter().map(serde_json::Value::to_string).collect::<Vec<_>>().join("\n") + "\n"
    };
    fs::write(&fixture, body)?;
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}/provider-args'\ncat > '{}/provider-prompt'\nwhile [ \"$#\" -gt 0 ]; do if [ \"$1\" = --output-schema ]; then shift; cat \"$1\" > '{}/provider-schema'; break; fi; shift; done\ncat '{}'\n",
        root.display(),
        root.display(),
        root.display(),
        fixture.display()
    );
    let executable = bin.join("codex");
    fs::write(&executable, script)?;
    fs::set_permissions(executable, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

fn breakdown_proposal() -> serde_json::Value {
    serde_json::json!({"tasks":[
        {"title":"Plan the implementation","goal":"Implement the first part","criteria":["First part works"]},
        {"title":"Verify the result","goal":"Verify the second part","criteria":["All checks pass"]}
    ],"reason":"Work is easier to verify in two steps."})
}

#[test]
fn breakdown_pty_keyboard_mouse_pending_failed_approval_cancel() -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval, Preferences};
    for mouse in [false, true] {
        for failed in [false, true] {
            for approval in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
                for cancel in [false, true] {
                    let scratch = support::sandbox()?;
                    let root = scratch.path();
                    let id = if failed {
                        failed_task(root)?.0
                    } else {
                        service(root)?.create(&[definition("Pending original")], None, true)?[0]
                    };
                    let mut service = service(root)?;
                    let mut models = service.project_defaults()?;
                    let old = models.clone();
                    models[0] = Some(ModelConfig {
                        provider: "codex".into(),
                        model: "planning-fixture".into(),
                        effort: Some("high".into()),
                    });
                    service.save_defaults(&old, &models)?;
                    if approval == PlannerApproval::ReviewRequired {
                        let config = root.join("config/ktask-dev");
                        let previous = service.preferences(&config)?;
                        service.save_preferences(
                            &config,
                            &previous,
                            &Preferences {
                                version: 0,
                                mouse: true,
                                planner_approval: approval,
                                continuation: Continuation::Single,
                            },
                        )?;
                    }
                    let before = storage_contents(root)?;
                    let attempts = attempts_contents(root)?;
                    provider_fixture(root, breakdown_proposal(), false)?;
                    let mut terminal = terminal(root)?;
                    terminal.screen("original", |s| {
                        s.contains(if failed {
                            "Failed current"
                        } else {
                            "Pending original"
                        })
                    })?;
                    if mouse {
                        terminal.click("b Break down")?;
                    } else {
                        terminal.send("b")?;
                    }
                    let screen = terminal.screen("Breakdown proposal", |s| {
                        s.contains("Breakdown proposal · replace task")
                    })?;
                    println!(
                        "BREAKDOWN mouse={mouse} failed={failed} approval={approval:?} cancel={cancel} PROPOSAL\n{screen}"
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-prompt"))?
                            .contains("Acceptance checks:")
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-args"))?
                            .contains("planning-fixture")
                    );
                    assert!(
                        fs::read_to_string(root.join("provider-args"))?
                            .contains("sandbox_mode=\"read-only\"")
                    );
                    if cancel {
                        if mouse {
                            terminal.click("Cancel")?;
                        } else {
                            terminal.send("\x1b")?;
                        }
                        println!(
                            "BREAKDOWN CANCELLED\n{}",
                            terminal.screen("cancelled", |s| !s.contains("Breakdown proposal"))?
                        );
                        assert_eq!(storage_contents(root)?, before);
                    } else {
                        if mouse {
                            terminal.click("Apply")?;
                        } else {
                            terminal.send("\x13")?;
                        }
                        let screen =
                            terminal.screen("applied", |s| s.contains("Saved to SQLite"))?;
                        println!("BREAKDOWN APPLIED\n{screen}");
                        let snapshot = service.snapshot()?;
                        assert_eq!(snapshot.queue.len(), 2);
                        assert_eq!(
                            snapshot.queue[0].definition().definition.title,
                            "Plan the implementation"
                        );
                        assert_eq!(
                            snapshot.queue[1].definition().definition.title,
                            "Verify the result"
                        );
                        assert_eq!(
                            snapshot.queue[0].revisions[0].actor.as_deref(),
                            Some("ai: planner")
                        );
                        assert_eq!(
                            snapshot.queue[0].definition().lifecycle,
                            if approval == PlannerApproval::Automatic {
                                "ready"
                            } else {
                                "draft"
                            }
                        );
                        assert_eq!(snapshot.history[0].id, id);
                        assert_eq!(snapshot.history[0].lifecycle, "superseded");
                    }
                    assert_eq!(attempts_contents(root)?, attempts);
                    quit(&mut terminal)?;
                }
            }
        }
    }
    Ok(())
}

#[test]
fn breakdown_pty_uses_approval_preference_at_apply() -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    for mouse in [false, true] {
        for updated in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            let mut service = service(root)?;
            let id = service.create(&[definition("Preference change target")], None, true)?[0];
            let old = service.project_defaults()?;
            let mut models = old.clone();
            models[0] = Some(ModelConfig {
                provider: "codex".into(),
                model: "planning-fixture".into(),
                effort: None,
            });
            service.save_defaults(&old, &models)?;
            let config = root.join("config/ktask-dev");
            let previous = service.preferences(&config)?;
            let initial = if updated == PlannerApproval::Automatic {
                PlannerApproval::ReviewRequired
            } else {
                PlannerApproval::Automatic
            };
            service.save_preferences(
                &config,
                &previous,
                &ktask_rs_mk3::application::preferences::Preferences {
                    planner_approval: initial,
                    ..previous
                },
            )?;
            provider_fixture(root, breakdown_proposal(), false)?;
            let mut terminal = terminal(root)?;
            terminal.screen("original", |s| s.contains("Preference change target"))?;
            if mouse {
                terminal.click("b Break down")?;
            } else {
                terminal.send("b")?;
            }
            println!(
                "BREAKDOWN PREFERENCE mouse={mouse} initial={initial:?} PROPOSAL\n{}",
                terminal.screen("proposal", |s| s
                    .contains("Breakdown proposal · replace task"))?
            );
            let previous = service.preferences(&config)?;
            service.save_preferences(
                &config,
                &previous,
                &ktask_rs_mk3::application::preferences::Preferences {
                    planner_approval: updated,
                    ..previous
                },
            )?;
            if mouse {
                terminal.click("Apply")?;
            } else {
                terminal.send("\x13")?;
            }
            println!(
                "BREAKDOWN PREFERENCE mouse={mouse} updated={updated:?} APPLIED\n{}",
                terminal.screen("applied", |s| s.contains("Saved to SQLite"))?
            );
            let snapshot = service.snapshot()?;
            assert_eq!(snapshot.queue.len(), 2);
            assert!(snapshot.queue.iter().all(|task| task.definition().lifecycle
                == if updated == PlannerApproval::Automatic {
                    "ready"
                } else {
                    "draft"
                }));
            assert_eq!(snapshot.history[0].id, id);
            quit(&mut terminal)?;
        }
    }
    Ok(())
}

#[test]
fn breakdown_cli_preview_apply_and_version_guard() -> Result {
    use ktask_rs_mk3::application::preferences::{PlannerApproval, Preferences};
    use std::process::Command;
    for failed in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let id = if failed {
            failed_task(root)?.0
        } else {
            service(root)?.create(&[definition("CLI original")], None, true)?[0]
        };
        let mut service = service(root)?;
        let old = service.project_defaults()?;
        let mut models = old.clone();
        models[0] = Some(ModelConfig {
            provider: "codex".into(),
            model: "planning-fixture".into(),
            effort: None,
        });
        service.save_defaults(&old, &models)?;
        let config = root.join("config/ktask-dev");
        let previous = service.preferences(&config)?;
        let initial = if failed {
            PlannerApproval::Automatic
        } else {
            PlannerApproval::ReviewRequired
        };
        service.save_preferences(
            &config,
            &previous,
            &Preferences {
                planner_approval: initial,
                ..previous
            },
        )?;
        provider_fixture(root, breakdown_proposal(), false)?;
        let invoke = |args: &[&str]| -> Result<serde_json::Value> {
            let output = Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
                .env_clear()
                .env("HOME", root)
                .env("XDG_STATE_HOME", root.join("state"))
                .env("XDG_CONFIG_HOME", root.join("config"))
                .env("XDG_CACHE_HOME", root.join("cache"))
                .env("XDG_RUNTIME_DIR", root.join("runtime"))
                .env("TMPDIR", root)
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", root.join("bin").display()),
                )
                .current_dir(root)
                .arg("--json")
                .args(args)
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            Ok(serde_json::from_slice(&output.stdout)?)
        };
        let id_string = id.to_string();
        let before = storage_contents(root)?;
        let preview = invoke(&["breakdown", &id_string])?;
        assert_eq!(preview["data"]["applied"], false);
        assert_eq!(
            preview["data"]["proposal"]["tasks"][0]["title"],
            "Plan the implementation"
        );
        assert_eq!(storage_contents(root)?, before);
        let previous = service.preferences(&config)?;
        let updated = if failed {
            PlannerApproval::ReviewRequired
        } else {
            PlannerApproval::Automatic
        };
        service.save_preferences(
            &config,
            &previous,
            &Preferences {
                planner_approval: updated,
                ..previous
            },
        )?;
        let version = service.task(id)?.unwrap().version.to_string();
        let applied = invoke(&["breakdown", &id_string, "--if-version", &version, "--apply"])?;
        assert_eq!(applied["data"]["receipts"].as_array().unwrap().len(), 2);
        let snapshot = service.snapshot()?;
        assert_eq!(
            snapshot
                .queue
                .iter()
                .map(|t| t.definition().definition.title.as_str())
                .collect::<Vec<_>>(),
            vec!["Plan the implementation", "Verify the result"]
        );
        assert_eq!(snapshot.history[0].id, id);
        assert_eq!(snapshot.history[0].lifecycle, "superseded");
        assert!(snapshot.queue.iter().all(|task| task.definition().lifecycle
            == if updated == PlannerApproval::Automatic {
                "ready"
            } else {
                "draft"
            }));
        assert_eq!(
            snapshot.history[0].attempts.len(),
            if failed { 1 } else { 0 }
        );
        let output = Command::new(env!("CARGO_BIN_EXE_ktask-dev"))
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .current_dir(root)
            .args([
                "--json",
                "breakdown",
                &id_string,
                "--if-version",
                &version,
                "--apply",
            ])
            .output()?;
        assert_eq!(output.status.code(), Some(4));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output.stderr)?["error"]["code"],
            "conflict"
        );
    }
    Ok(())
}

#[test]
fn breakdown_pending_queue_row_keeps_failed_current_in_place() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (failed_id, _) = failed_task(root)?;
        let mut service = service(root)?;
        let pending_id = service.create(&[definition("Pending after failure")], None, true)?[0];
        let old = service.project_defaults()?;
        let mut models = old.clone();
        models[0] = Some(ModelConfig {
            provider: "codex".into(),
            model: "planning-fixture".into(),
            effort: None,
        });
        service.save_defaults(&old, &models)?;
        provider_fixture(root, breakdown_proposal(), false)?;
        let mut t = terminal(root)?;
        t.screen("failed current", |s| s.contains("Failed current"))?;
        if mouse {
            t.click("2 Queue")?;
        } else {
            t.send("2")?;
        }
        t.screen("queue", |s| s.contains("Pending after failure"))?;
        if mouse {
            t.click("#2   PENDING")?;
        } else {
            t.send("j")?;
        }
        let selected = t.screen("selected", |s| s.contains("› #2"))?;
        println!("BREAKDOWN QUEUE mouse={mouse} SELECTED\n{selected}");
        if mouse {
            t.click("b Break down")?;
        } else {
            t.send("b")?;
        }
        println!(
            "BREAKDOWN QUEUE PROPOSAL\n{}",
            t.screen("proposal", |s| s
                .contains("Breakdown proposal · replace task"))?
        );
        if mouse {
            t.click("Apply")?;
        } else {
            t.send("\x13")?;
        }
        println!(
            "BREAKDOWN QUEUE APPLIED\n{}",
            t.screen("applied", |s| s.contains("Saved to SQLite"))?
        );
        let snapshot = service.snapshot()?;
        assert_eq!(
            snapshot.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![failed_id, pending_id + 1, pending_id + 2]
        );
        assert_eq!(snapshot.queue[0].status, "failed");
        assert_eq!(snapshot.history[0].id, pending_id);
        quit(&mut t)?;
    }
    Ok(())
}

#[test]
fn failed_current_remediation_proposals_use_real_pty_apply_cancel_and_provider_errors() -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    for mouse in [false, true] {
        for approval in [PlannerApproval::Automatic, PlannerApproval::ReviewRequired] {
            for (name, proposal, expected) in [
                (
                    "escalate",
                    serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"fixture-strong","effort":"max"},"reason":"The failed check needs a stronger execution model."}),
                    "Remediation proposal · escalate model",
                ),
                (
                    "insert",
                    serde_json::json!({"action":"insert","tasks":[{"title":"Repair the check boundary","goal":"Fix the failing boundary before retrying current.","criteria":["The check passes"]},{"title":"Verify repaired boundary","goal":"Confirm the repair.","criteria":["Regression passes"]}],"reason":"Isolate the failing boundary first."}),
                    "Remediation proposal · insert repair tasks",
                ),
                (
                    "cancel",
                    serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"cancelled-model","effort":"high"},"reason":"Cancel must preserve state."}),
                    "Remediation proposal · escalate model",
                ),
            ] {
                exercise_remediation(name, proposal, expected, mouse, approval)?;
            }
        }
    }

    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, _) = failed_task(root)?;
    let before = service(root)?.task(id)?.unwrap();
    let storage_before = storage_contents(root)?;
    let events_before = service(root)?.events_after(0, 1000)?.len();
    provider_fixture(root, serde_json::Value::Null, true)?;
    let mut terminal = terminal(root)?;
    terminal.send("d")?;
    let screen = terminal.screen("provider error", |s| s.contains("Invalid Codex event"))?;
    println!("REMEDIATION INVALID PROVIDER\n{screen}");
    assert_eq!(storage_contents(root)?, storage_before);
    let after = service(root)?.task(id)?.unwrap();
    assert_eq!(after.revisions.len(), 1);
    assert_eq!(after.version, before.version);
    assert_eq!(service(root)?.events_after(0, 1000)?.len(), events_before);
    quit(&mut terminal)?;
    Ok(())
}

fn exercise_remediation(
    name: &str,
    proposal: serde_json::Value,
    expected: &str,
    mouse: bool,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
) -> Result {
    use ktask_rs_mk3::application::preferences::{Continuation, PlannerApproval, Preferences};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let mut service = service(root)?;
    let before = service.task(id)?.unwrap();
    let event_count = service.events_after(0, 1000)?.len();
    if approval == PlannerApproval::ReviewRequired {
        let config = root.join("config/ktask-dev");
        let expected_preferences = service.preferences(&config)?;
        service.save_preferences(
            &config,
            &expected_preferences,
            &Preferences {
                version: 0,
                mouse: true,
                planner_approval: approval,
                continuation: Continuation::Single,
            },
        )?;
    }
    let storage_before = storage_contents(root)?;
    let attempts_before = attempts_contents(root)?;
    provider_fixture(root, proposal, false)?;
    let mut terminal = terminal(root)?;
    terminal.screen("failed current", |s| s.contains("Failed current"))?;
    if mouse {
        terminal.click("d Remediate")?;
    } else {
        terminal.send("d")?;
    }
    let screen = terminal.screen(expected, |s| s.contains(expected))?;
    println!("REMEDIATION {name} mouse={mouse} approval={approval:?} PROPOSAL\n{screen}");
    assert!(fs::read_to_string(root.join("provider-args"))?.contains("sandbox_mode=\"read-only\""));
    let schema: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("provider-schema"))?)?;
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["required"], serde_json::json!(["proposal"]));
    assert!(schema["properties"]["proposal"]["anyOf"].is_array());
    assert!(fs::read_to_string(root.join("provider-prompt"))?.contains("transcript evidence"));
    if name == "cancel" {
        if mouse {
            terminal.click("Cancel")?;
        } else {
            terminal.send("\x1b")?;
        }
    } else if mouse {
        terminal.click("Apply")?;
    } else {
        terminal.send("\x13")?;
    }
    if name != "cancel" {
        println!(
            "REMEDIATION {name} APPLIED\n{}",
            terminal.screen("applied", |s| s.contains("Saved to SQLite"))?
        );
    } else {
        println!(
            "REMEDIATION CANCELLED\n{}",
            terminal.screen("cancelled", |s| !s.contains("Remediation proposal"))?
        );
    }
    let after = service.task(id)?.unwrap();
    assert_eq!(attempts_contents(root)?, attempts_before);
    assert_eq!(after.attempts[0].id, attempt);
    assert_eq!(after.attempts[0].state, "failed");
    if name == "cancel" {
        assert_eq!(storage_contents(root)?, storage_before);
        assert_eq!(after.revisions.len(), before.revisions.len());
        assert_eq!(after.version, before.version);
        assert_eq!(service.events_after(0, 1000)?.len(), event_count);
    } else {
        verify_applied_remediation(root, id, name, approval)?;
    }
    quit(&mut terminal)?;
    Ok(())
}

fn verify_applied_remediation(
    root: &Path,
    id: i64,
    name: &str,
    approval: ktask_rs_mk3::application::preferences::PlannerApproval,
) -> Result {
    use ktask_rs_mk3::application::preferences::PlannerApproval;
    let mut service = service(root)?;
    let after = service.task(id)?.unwrap();
    match name {
        "escalate" => {
            assert_eq!(after.revisions.len(), 2);
            let revision = after.revisions.last().unwrap();
            assert_eq!(revision.lifecycle, "ready");
            assert_eq!(after.definition().number, revision.number);
            assert_eq!(revision.actor.as_deref(), Some("ai: remediator"));
            assert_eq!(
                revision.definition.models[0].as_ref().unwrap().model,
                "fixture-strong"
            );
            let db = Database::open_in(&root.join("state/ktask-dev"))?;
            let reason: String = db.connection().query_row(
                "SELECT change_reason FROM task_revisions WHERE task_id=?1 AND revision=?2",
                rusqlite::params![id, revision.number],
                |row| row.get(0),
            )?;
            assert_eq!(reason, "The failed check needs a stronger execution model.");
        }
        "insert" => {
            let snapshot = service.snapshot()?;
            assert_eq!(snapshot.queue.len(), 3);
            assert_eq!(
                snapshot.queue[1].definition().definition.title,
                "Verify repaired boundary"
            );
            assert_eq!(snapshot.queue[2].id, id);
            assert_eq!(
                snapshot.queue[0].definition().definition.title,
                "Repair the check boundary"
            );
            assert_eq!(
                snapshot.queue[0].definition().lifecycle,
                if approval == PlannerApproval::Automatic {
                    "ready"
                } else {
                    "draft"
                }
            );
            assert_eq!(
                snapshot.queue[0].revisions[0].actor.as_deref(),
                Some("ai: remediator")
            );
        }
        _ => unreachable!(),
    }
    Ok(())
}

#[test]
fn remediation_stalled_provider_can_cancel_and_quit_without_leaking_children() -> Result {
    for input in ["keyboard", "mouse", "quit"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        let (_, attempt) = failed_task(root)?;
        // Fill stdin beyond pipe capacity, while the fixture never reads its prompt.
        let db = Database::open_in(&root.join("state/ktask-dev"))?;
        db.connection().execute(
            "INSERT INTO attempt_output(attempt_id,sequence,recorded_at_ms,stream,content) VALUES (?1,2,1,'stderr',?2)",
            rusqlite::params![attempt, "界".repeat(100_000)],
        )?;
        provider_fixture(root, serde_json::Value::Null, false)?;
        fs::write(
            root.join("bin/codex"),
            format!(
                "#!/bin/sh\necho $$ > '{0}/provider-pid'\nsleep 60 &\necho $! > '{0}/descendant-pid'\nwait\n",
                root.display()
            ),
        )?;
        let before = storage_contents(root)?;
        let mut terminal = terminal(root)?;
        terminal.send("d")?;
        println!(
            "REMEDIATION STALLED {input}\n{}",
            terminal.screen("pending proposal", |s| s
                .contains("Requesting remediation proposal"))?
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !root.join("descendant-pid").exists() {
            assert!(std::time::Instant::now() < deadline);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        terminal.send("\r")?;
        terminal.screen("pending Enter is inert", |s| {
            s.contains("Requesting remediation proposal") && !s.contains("Simulation")
        })?;
        match input {
            "mouse" => terminal.click("Cancel")?,
            "quit" => quit(&mut terminal)?,
            _ => terminal.send("\x1b")?,
        }
        if input != "quit" {
            println!(
                "REMEDIATION REQUEST CANCELLED {input}\n{}",
                terminal.screen("cancelled pending request", |s| !s
                    .contains("Requesting remediation proposal"))?
            );
            terminal.send("2")?;
            println!(
                "RESPONSIVE QUEUE {input}\n{}",
                terminal.screen("responsive queue", |s| s.contains("Queue · 1 shown"))?
            );
            quit(&mut terminal)?;
        }
        for file in ["provider-pid", "descendant-pid"] {
            let pid = fs::read_to_string(root.join(file))?;
            assert!(
                !Path::new("/proc").join(pid.trim()).exists(),
                "leaked {file}: {pid}"
            );
        }
        assert_eq!(storage_contents(root)?, before);
        let workers = root.join("cache/ktask-dev/workers");
        assert!(
            !fs::read_dir(workers)?.any(|e| e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("run-"))
        );
    }
    Ok(())
}

#[test]
fn remediation_reaps_pipe_holding_descendants_and_reports_provider_failure() -> Result {
    for fail in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        failed_task(root)?;
        provider_fixture(
            root,
            serde_json::json!({"action":"escalate","model":{"provider":"codex","model":"stronger","effort":"high"},"reason":"Repair the failed boundary."}),
            false,
        )?;
        fs::write(
            root.join("bin/codex"),
            format!(
                "#!/bin/sh\ncat >/dev/null\nsleep 60 &\necho $! > '{0}/descendant-pid'\n{1}\n",
                root.display(),
                if fail {
                    "echo fixture-provider-failed >&2\nexit 9"
                } else {
                    "cat provider-events.jsonl"
                }
            ),
        )?;
        let before = storage_contents(root)?;
        let mut terminal = terminal(root)?;
        terminal.send("d")?;
        let expected = if fail {
            "fixture-provider-failed"
        } else {
            "Remediation proposal · escalate model"
        };
        println!(
            "REMEDIATION PIPE HOLDER fail={fail}\n{}",
            terminal.screen(expected, |s| s.contains(expected))?
        );
        quit(&mut terminal)?;
        let pid = fs::read_to_string(root.join("descendant-pid"))?;
        assert!(!Path::new("/proc").join(pid.trim()).exists());
        assert_eq!(storage_contents(root)?, before);
    }
    Ok(())
}

#[test]
fn remediation_large_unicode_evidence_retains_latest_output_transcripts_and_findings() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (id, attempt) = failed_task(root)?;
    let db = Database::open_in(&root.join("state/ktask-dev"))?;
    let output = format!("{}LATEST_OUTPUT", "界".repeat(1_500_000));
    db.connection().execute(
        "INSERT INTO attempt_output(attempt_id,sequence,recorded_at_ms,stream,content) VALUES (?1,2,1,'stderr',?2)",
        rusqlite::params![attempt, output],
    )?;
    let transcript = format!("{}FAILED_TRANSCRIPT", "界".repeat(400_000));
    fs::write(root.join("state/ktask-dev/check.log"), &transcript)?;
    db.connection().execute("INSERT INTO artifacts(attempt_id,name,media_type,relative_path,size_bytes,created_at_ms) VALUES (?1,'failed check','text/plain','check.log',?2,1)", rusqlite::params![attempt,transcript.len() as i64])?;
    let evidence = service(root)?.remediation_evidence(id, &root.join("state/ktask-dev"))?;
    assert!(evidence.len() <= 4 * 1024 * 1024);
    for expected in [
        "LATEST_OUTPUT",
        "FAILED_TRANSCRIPT",
        "review structured finding",
        "validation structured finding",
        "review criterion evidence",
        "validation criterion evidence",
    ] {
        assert!(evidence.contains(expected), "missing {expected}");
    }
    Ok(())
}

#[test]
fn commands_are_atomic_versioned_relational_and_preserve_usable_revisions() -> Result {
    let scratch = support::sandbox()?;
    let mut a = service(scratch.path())?;
    let mut b = service(scratch.path())?;
    let id = a.create(&[definition("Approved")], None, true)?[0];
    let old = target(&mut a, id)?;
    let mut draft = definition("Draft escalation");
    draft.models[0] = Some(ModelConfig {
        provider: "test-provider".into(),
        model: "test-model".into(),
        effort: None,
    });
    b.revise(old, &draft, false)?;
    assert!(a.revise(old, &definition("Stale overwrite"), true).is_err());
    assert!(a.delete(old).is_err());
    assert_eq!(
        a.snapshot()?.queue[0].definition().definition.title,
        "Approved"
    );
    let current = target(&mut a, id)?;
    a.decide(current, false, "Keep approved inputs")?;
    assert_eq!(
        a.snapshot()?.queue[0].definition().definition.title,
        "Approved"
    );
    let current = target(&mut a, id)?;
    a.revise(current, &draft, false)?;
    let current = target(&mut a, id)?;
    a.decide(current, true, "Reviewed")?;
    let snapshot = a.snapshot()?;
    assert_eq!(
        snapshot.queue[0].definition().definition.models[0],
        draft.models[0]
    );
    assert_eq!(snapshot.queue[0].revisions.len(), 3);
    let mut invalid = definition("Invalid");
    invalid.criteria.clear();
    assert!(
        a.create(&[definition("Must roll back"), invalid], None, true)
            .is_err()
    );
    assert_eq!(a.snapshot()?.queue.len(), 1);
    let db = Database::open_in(&scratch.path().join("state/ktask-dev"))?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM tasks", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.connection().query_row(
            "SELECT count(*) FROM revision_models WHERE effort IS NULL",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        2
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM acceptance_criteria", [], |r| r
                .get::<_, i64>(0))?,
        3
    );
    Ok(())
}

#[test]
fn commands_preserve_order_history_deleted_rows_and_human_evidence() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut human = definition("Human checkpoint");
    human.kind = Kind::Human;
    let ids = s.create(&[human, definition("Last")], None, true)?;
    let inserted = s.create(
        &(0..10)
            .map(|n| definition(&format!("Inserted {n}")))
            .collect::<Vec<_>>(),
        Some(ids[1]),
        true,
    )?;
    assert_eq!(
        s.snapshot()?.queue.iter().map(|t| t.id).collect::<Vec<_>>(),
        [vec![ids[0]], inserted, vec![ids[1]]].concat()
    );
    let first = target(&mut s, ids[0])?;
    let last = target(&mut s, ids[1])?;
    assert!(s.acknowledge(last).is_err());
    s.acknowledge(first)?;
    assert!(s.revise(first, &definition("Bad"), true).is_err());
    let snapshot = s.snapshot()?;
    assert_eq!(snapshot.history[0].lifecycle, "succeeded");
    assert!(snapshot.history[0].attempts.is_empty());
    s.delete(last)?;
    s.restore(VersionedTask {
        version: last.version + 1,
        ..last
    })?;
    let last = target(&mut s, ids[1])?;
    let current = s.snapshot()?.queue[0].id;
    s.move_before(last, Some(current))?;
    assert_eq!(s.snapshot()?.queue[0].id, ids[1]);
    drop(s);
    assert_eq!(service(scratch.path())?.snapshot()?.queue[0].id, ids[1]);
    Ok(())
}

#[test]
fn project_identity_and_definition_transfer_are_lossless_and_safe() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let nested = scratch.path().join("nested");
    std::fs::create_dir(&nested)?;
    assert!(
        Service::open(
            Database::open_in(&scratch.path().join("state/ktask-dev"))?,
            &nested,
            Some(&s.project_name)
        )
        .is_err()
    );
    let mut other = Service::open(
        Database::open_in(&scratch.path().join("state/ktask-dev"))?,
        &nested,
        Some("Other"),
    )?;
    s.create(&[definition("Private to first")], None, true)?;
    assert!(other.snapshot()?.queue.is_empty());
    let mut d = definition("Unicode 雪\tand\nnewlines\\");
    d.criteria.push("Second\rcriterion".into());
    d.models[3] = Some(ModelConfig {
        provider: "test".into(),
        model: "family/model".into(),
        effort: Some("custom".into()),
    });
    let text = transfer::encode(&[d.clone()])?;
    assert_eq!(transfer::decode(&text)?, vec![d]);
    let file = scratch.path().join("tasks.tsv");
    transfer::export(&file, &text, None)?;
    assert!(transfer::export(&file, "overwrite", None).is_err());
    assert!(transfer::export(&file, "overwrite", Some(b"outdated")).is_err());
    assert_eq!(std::fs::read_to_string(&file)?, text);
    transfer::export(&file, &text, Some(text.as_bytes()))?;
    assert_eq!(transfer::import(&file)?, transfer::decode(&text)?);
    assert!(transfer::decode("ktask-tasks-v1\nbad").is_err());
    Ok(())
}

#[test]
fn real_tui_saves_across_kill_restart_and_never_simulates_real_tasks() -> Result {
    let scratch = support::sandbox()?;
    let mut t = terminal(scratch.path())?;
    println!(
        "REAL EMPTY\n{}",
        t.screen("empty", |s| s.contains("No current task."))?
    );
    key(&mut t, "a", "Add task")?;
    println!(
        "REAL ADD\n{}",
        key(
            &mut t,
            "Real durable task\tGoal\tCheck\r",
            "Real durable task"
        )?
    );
    key(&mut t, "c", "Execution provider/model")?;
    key(
        &mut t,
        "test-provider/worker-v2\tcustom\r",
        "Saved to SQLite",
    )?;
    t.send("1r")?;
    println!(
        "REAL RUN PREFLIGHT\n{}",
        t.screen("non-project workspace cannot run", |s| {
            s.contains("Project must be the Git repository root") || s.contains("not a git")
        })?
    );
    t.kill()?;
    let mut t = terminal(scratch.path())?;
    println!(
        "REAL RESTART\n{}",
        t.screen("persisted", |s| s.contains("Real durable task")
            && s.contains("PENDING"))?
    );
    key(&mut t, "c", "test-provider/worker-v2")?;
    t.screen("custom effort persists", |s| s.contains("custom"))?;
    key(&mut t, "\x1b", "Real durable task")?;
    key(
        &mut t,
        "e\x01Renamed durable task\r",
        "Renamed durable task",
    )?;
    quit(&mut t)?;
    let snapshot = service(scratch.path())?.snapshot()?;
    assert_eq!(snapshot.queue.len(), 1);
    assert_eq!(snapshot.queue[0].revisions.len(), 3);
    assert_eq!(
        snapshot.queue[0].definition().definition.title,
        "Renamed durable task"
    );
    assert!(snapshot.queue[0].attempts.is_empty());
    Ok(())
}

#[test]
fn real_tui_mouse_approval_history_delete_undo_and_all_views() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut human = definition("Human approval");
    human.kind = Kind::Human;
    s.create(&[human, definition("Delete and restore")], None, false)?;
    let mut t = terminal(scratch.path())?;
    t.screen("draft", |s| s.contains("Current #1 · NEW"))?;
    t.click("v Approve")?;
    t.screen("ready", |s| s.contains("Current #1 · PENDING"))?;
    t.click("y Acknowledge")?;
    t.screen("next", |s| s.contains("Current #2 · NEW"))?;
    t.click("v Approve")?;
    t.screen("ready second", |s| s.contains("Current #2 · PENDING"))?;
    t.click("2 Queue")?;
    t.screen("queue", |s| s.contains("d Delete"))?;
    t.click("d Delete")?;
    t.screen("confirm", |s| s.contains("Delete pending task?"))?;
    t.click("Apply")?;
    t.screen("deleted", |s| s.contains("Queue 0"))?;
    t.click("u Undo")?;
    t.screen("restored", |s| s.contains("Queue 1"))?;
    for (tab, text) in [
        ("1 Current", "No attempts yet"),
        ("2 Queue", "Delete and restore"),
        ("3 History", "Human approval"),
        ("4 Config", "Storage: SQLite"),
        ("5 About", "relational SQLite"),
    ] {
        t.click(tab)?;
        println!("REAL {tab}\n{}", t.screen(tab, |s| s.contains(text))?);
    }
    key(&mut t, "4e", "Project preferences")?;
    t.click("< Enabled >")?;
    t.screen("disabled", |s| s.contains("< Disabled >"))?;
    key(&mut t, "\r", "Mouse: Disabled")?;
    key(&mut t, "e \r", "Mouse: Enabled")?;
    println!(
        "SQLITE STATS\n{}",
        key(&mut t, "s", "Project statistics · SQLite")?
    );
    key(&mut t, "\x1b", "Mouse: Enabled")?;
    println!("SQLITE EVENTS\n{}", key(&mut t, "l", "Project events")?);
    key(&mut t, "\x1b", "Mouse: Enabled")?;
    key(&mut t, "?", "Keyboard help")?;
    println!(
        "SQLITE HELP\n{}",
        key(&mut t, "\x1b[F", "Changes save immediately")?
    );
    quit(&mut t)?;
    let snapshot = s.snapshot()?;
    assert_eq!(snapshot.history[0].lifecycle, "succeeded");
    assert!(snapshot.history[0].attempts.is_empty());
    Ok(())
}

#[test]
fn two_real_windows_reject_stale_forms_and_refresh_external_changes() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Original")], None, true)?;
    let mut a = terminal(scratch.path())?;
    let mut b = terminal(scratch.path())?;
    for t in [&mut a, &mut b] {
        t.screen("original", |s| s.contains("Original"))?;
        key(t, "e", "Edit task")?;
    }
    key(&mut a, "\x01First writer\r", "First writer")?;
    println!(
        "CONFLICT\n{}",
        key(&mut b, "\x01Stale writer\r", "Task changed elsewhere")?
    );
    key(&mut b, "\x1b", "First writer")?;
    quit(&mut a)?;
    quit(&mut b)?;
    assert_eq!(
        service(scratch.path())?.snapshot()?.queue[0]
            .definition()
            .definition
            .title,
        "First writer"
    );
    Ok(())
}

#[test]
fn real_tui_disk_import_export_preview_overwrite_and_atomic_error() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Transfer me")], None, true)?;
    let mut t = terminal(scratch.path())?;
    t.screen("startup", |s| s.contains("Transfer me"))?;
    key(&mut t, "2x", "Export task definitions")?;
    key(&mut t, "\r", "Exported to disk")?;
    assert!(scratch.path().join("tasks.tsv").is_file());
    key(&mut t, "x\r", "Overwrite file?")?;
    t.click("Cancel")?;
    t.screen("cancelled", |s| !s.contains("Overwrite file?"))?;
    key(&mut t, "x\r", "Overwrite file?")?;
    t.click("Apply")?;
    t.screen("exported", |s| s.contains("Exported to disk"))?;
    t.click("o Import")?;
    t.screen("import", |s| s.contains("Import task definitions"))?;
    t.click("Apply")?;
    println!(
        "IMPORT PREVIEW\n{}",
        t.screen("preview", |s| s.contains("Import preview")
            && s.contains("Transfer me"))?
    );
    key(&mut t, "999\r", "insertion target is not in this queue")?;
    assert_eq!(service(scratch.path())?.snapshot()?.queue.len(), 1);
    key(&mut t, "\x011\r", "Queue 2")?;
    key(&mut t, "1", "Current #2")?;
    quit(&mut t)?;
    Ok(())
}

#[test]
fn real_instances_and_demo_never_share_task_data() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Development only")], None, true)?;
    let mut prod = Terminal::open_real(env!("CARGO_BIN_EXE_ktask"), scratch.path(), 70, 22)?;
    prod.screen("empty production", |s| {
        s.contains("No current task.") && !s.contains("Development only")
    })?;
    let mut demo = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), scratch.path(), 70, 22)?;
    demo.screen("demo", |s| {
        s.contains("MOCK DATA") && s.contains("Queue 120")
    })?;
    key(&mut demo, "aDemo only\tGoal\tCheck\r", "Demo only")?;
    quit(&mut demo)?;
    quit(&mut prod)?;
    assert_eq!(service(scratch.path())?.snapshot()?.queue.len(), 1);
    Ok(())
}

#[test]
fn running_guards_and_real_failure_resolution_preserve_attempt_evidence() -> Result {
    use ktask_rs_mk3::storage::execution::StartAttempt;
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let mut d = definition("Stored failure");
    d.models = std::array::from_fn(|_| {
        Some(ModelConfig {
            provider: "test".into(),
            model: "worker".into(),
            effort: Some("medium".into()),
        })
    });
    let id = s.create(&[d.clone()], None, true)?[0];
    let mut db = Database::open_in(&scratch.path().join("state/ktask-dev"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let attempt = db.start_attempt(StartAttempt {
        task_id: id,
        expected_version: 1,
        execution_token: "fixture-not-real-execution",
        now_ms: now,
    })?;
    assert!(
        s.create(&[definition("Cannot precede running")], Some(id), true)
            .is_err()
    );
    let running = target(&mut s, id)?;
    assert!(s.revise(running, &d, true).is_err());
    assert!(s.delete(running).is_err());
    assert!(s.move_before(running, None).is_err());
    db.connection().execute("UPDATE attempts SET state = 'failed', finished_at_ms = started_at_ms, outcome = 'Recorded fixture failure', failure_category = 'check' WHERE id = ?1", [attempt])?;
    db.connection().execute("INSERT INTO attempt_output(attempt_id, sequence, stream, recorded_at_ms, content) VALUES (?1, 1, 'system', ?2, 'Original evidence retained')", rusqlite::params![attempt, now])?;
    let mut t = terminal(scratch.path())?;
    t.screen("failed", |s| s.contains("Current #1 · FAILED"))?;
    println!(
        "STORED FAILURE\n{}",
        key(&mut t, "\r", "Recorded fixture failure")?
    );
    key(&mut t, "\x1b", "Current #1 · FAILED")?;
    key(&mut t, "z", "Resolve failed current")?;
    key(&mut t, "\tAccepted evidence\r", "History 1")?;
    key(&mut t, "3\r", "Original evidence retained")?;
    println!(
        "STORED RESOLUTION\n{}",
        key(&mut t, "\x1b[F", "Accepted evidence")?
    );
    quit(&mut t)?;
    let history = s.snapshot()?.history.remove(0);
    assert_eq!(history.lifecycle, "succeeded_override");
    assert_eq!(history.attempts[0].state, "failed");
    assert_eq!(
        history.attempts[0].outcome.as_deref(),
        Some("Recorded fixture failure")
    );
    Ok(())
}

#[test]
fn real_forms_save_drafts_all_model_roles_limits_and_reject_without_blocking() -> Result {
    let scratch = support::sandbox()?;
    service(scratch.path())?.create(&[definition("Keep ready")], None, true)?;
    let mut t = terminal(scratch.path())?;
    t.screen("ready", |s| s.contains("Keep ready"))?;
    key(
        &mut t,
        "e\x01Proposed title\t\t\t\t \r",
        "Pending revision 2",
    )?;
    key(&mut t, "nKeep old inputs\r", "Saved to SQLite")?;
    t.screen("ready survived", |s| {
        s.contains("Keep ready") && !s.contains("Pending revision")
    })?;
    t.click("c Models")?;
    println!(
        "STORED MODEL FORM\n{}",
        t.screen("models", |s| s.contains("Execution provider/model"))?
    );
    key(&mut t, "malformed\r", "Use provider/model")?;
    key(
        &mut t,
        "\x01test/executor\tlow\ttest/reviewer\tmedium\ttest/validator\thigh\ttest/remediator\tmax\t\x017\t\x012000",
        "2000",
    )?;
    println!(
        "STORED MODEL LIMITS\n{}",
        t.screen("limits", |s| s.contains("Attempt limit"))?
    );
    t.click("Apply")?;
    t.screen("saved", |s| s.contains("Saved to SQLite"))?;
    key(&mut t, "2i", "Insert task")?;
    key(&mut t, "Earlier task\tGoal\tCheck\t\t1\r", "Earlier task")?;
    key(&mut t, "m1\r", "Saved to SQLite")?;
    key(&mut t, "1", "Current #2")?;
    quit(&mut t)?;
    let snapshot = service(scratch.path())?.snapshot()?;
    let d = &snapshot.queue[1].definition().definition;
    assert_eq!(d.title, "Keep ready");
    assert_eq!(d.attempt_limit, Some(7));
    assert_eq!(d.timeout_ms, Some(2000));
    for (i, role) in ["executor", "reviewer", "validator", "remediator"]
        .iter()
        .enumerate()
    {
        assert_eq!(d.models[i].as_ref().unwrap().model, *role);
    }
    Ok(())
}

#[test]
fn sqlite_queue_and_history_handle_240_tasks_in_execution_order() -> Result {
    let scratch = support::sandbox()?;
    let mut s = service(scratch.path())?;
    let definitions = (1..=240)
        .map(|n| {
            let mut d = definition(&format!("Durable task {n}"));
            if n <= 120 {
                d.kind = Kind::Human;
            }
            d
        })
        .collect::<Vec<_>>();
    let ids = s.create(&definitions, None, true)?;
    for id in &ids[..120] {
        s.acknowledge(VersionedTask {
            id: *id,
            version: 1,
        })?;
    }
    let mut t = terminal(scratch.path())?;
    t.screen("large stored queue", |s| {
        s.contains("Queue 120 · History 120") && s.contains("Current #121")
    })?;
    key(&mut t, "2\x1b[F", "Durable task 240")?;
    println!(
        "LARGE SQLITE QUEUE\n{}",
        t.screen("last page", |s| s.contains("120 / 120"))?
    );
    key(&mut t, "3\x1b[H", "Durable task 120")?;
    key(&mut t, "/Durable task 1\r", "Search:")?;
    key(&mut t, "2", "Durable task 240")?;
    key(&mut t, "1", "Current #121")?;
    quit(&mut t)?;
    assert_eq!(s.snapshot()?.queue.len(), 120);
    Ok(())
}
