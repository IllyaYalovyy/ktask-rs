use crate::{
    cli::{DEV, json_result, process},
    support::{self, Result, Terminal},
};
use serde_json::{Value, json};
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

fn cli(root: &Path, project: &Path, arguments: &[&str]) -> Result<Value> {
    let output = process(DEV, root)
        .arg("--json")
        .arg("--project")
        .arg(project)
        .args(arguments)
        .output()?;
    json_result(output, 0)
}

fn server(root: &Path, project: &Path) -> Command {
    let mut command = process(DEV, root);
    command.args(["--actor", "test-client", "--project"]);
    command.arg(project).arg("mcp");
    command
}

fn call(id: i64, name: &str, arguments: Value) -> Value {
    json!({"jsonrpc":"2.0", "id":id, "method":"tools/call", "params":{"name":name,"arguments":arguments}})
}

fn exchange(root: &Path, project: &Path, foreign_id: i64, previous_id: i64) -> Result<Vec<Value>> {
    let mut child = server(root, project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let mut input = child.stdin.take().ok_or("MCP stdin")?;
    let draft = json!({"title":"Draft from MCP","goal":"Preserve read position","criteria":["The last row remains visible"]});
    let mut forged = draft.clone();
    forged["approval"] = json!("ready");
    let requests = [
        json!({"jsonrpc":"2.0","id":"handshake","method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "current", json!({})),
        call(3, "show", json!({"id":foreign_id})),
        call(4, "add_draft", forged),
        json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"add_draft","arguments":draft}}),
        call(5, "add_draft", draft),
        call(6, "current", json!({})),
        call(7, "list", json!({"scope":"queue","limit":1})),
        call(8, "events", json!({"limit":1})),
        call(9, "list", json!({"scope":"all","limit":1})),
        call(10, "list", json!({"scope":"all","offset":1,"limit":1})),
        call(11, "show", json!({"id":previous_id})),
    ];
    writeln!(input, "{{malformed")?;
    for request in requests {
        writeln!(input, "{request}")?;
    }
    drop(input);
    let output = child.wait_with_output()?;
    assert!(output.status.success(), "{:?}", output.status);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    let replies: Vec<Value> = String::from_utf8(output.stdout)?
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    Ok(replies)
}

fn file_draft(root: &Path, project: &Path, foreign_id: i64, previous_id: i64) -> Result<i64> {
    let replies = exchange(root, project, foreign_id, previous_id)?;
    assert_eq!(replies.len(), 13, "{replies:?}");
    assert_eq!(replies[0]["error"]["code"], -32700);
    assert_eq!(replies[1]["id"], "handshake");
    assert_eq!(replies[1]["result"]["protocolVersion"], "2025-11-25");
    let reply = |id| replies.iter().find(|reply| reply["id"] == id).unwrap();
    let names: Vec<_> = reply(1)["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert_eq!(names.len(), 5);
    for name in ["current", "list", "show", "events", "add_draft"] {
        assert!(names.contains(&name), "{names:?}");
    }
    let structured = |id| &reply(id)["result"]["structuredContent"];
    assert_eq!(*structured(2), json!({"task":null}));
    assert_eq!(reply(3)["result"]["isError"], true);
    assert_eq!(reply(4)["result"]["isError"], true);
    let id = structured(5)["tasks"][0]["id"].as_i64().unwrap();
    assert_eq!(structured(5)["tasks"][0]["version"], 1);
    let current = &structured(6)["task"];
    assert_eq!(current["id"], id);
    assert_eq!(current["state"], "needs_approval");
    assert_eq!(current["actions"][0], "Approve");
    assert_eq!(
        current["definition"]["criteria"][0],
        "The last row remains visible"
    );
    assert_eq!(structured(7)["tasks"][0]["id"], id);
    assert_eq!(structured(7)["count"], 1);
    assert_eq!(structured(8)["events"].as_array().unwrap().len(), 1);
    assert_eq!(
        structured(8)["next_after"],
        structured(8)["events"][0]["id"]
    );
    assert_eq!(structured(9)["tasks"][0]["id"], id);
    assert_eq!(structured(9)["next_offset"], 1);
    assert_eq!(structured(10)["tasks"][0]["id"], previous_id);
    assert!(structured(10)["next_offset"].is_null());
    assert_eq!(structured(11)["task"]["state"], "done");
    Ok(id)
}

#[test]
fn mcp_reads_one_project_and_files_a_draft_the_owner_can_review() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = root.join("project");
    let other = root.join("other");
    for path in [&project, &other] {
        std::fs::create_dir(path)?;
        support::empty_repository(path)?;
    }
    let foreign = cli(
        root,
        &other,
        &[
            "add",
            "--title",
            "Other project",
            "--goal",
            "Separate queue",
            "--criterion",
            "Project stays separate",
        ],
    )?;
    let foreign_id = foreign["data"]["tasks"][0]["id"].as_i64().unwrap();
    let previous = cli(
        root,
        &project,
        &[
            "add",
            "--title",
            "Completed task",
            "--goal",
            "Record prior work",
            "--criterion",
            "History remains visible",
        ],
    )?;
    let previous_id = previous["data"]["tasks"][0]["id"].as_i64().unwrap();
    cli(
        root,
        &project,
        &[
            "done",
            &previous_id.to_string(),
            "--reason",
            "Prior work verified",
        ],
    )?;
    let mut tui = Terminal::open_project(DEV, root, &project, 100, 26)?;
    tui.screen("empty registered project", |screen| {
        screen.contains("Queue complete.")
    })?;

    let id = file_draft(root, &project, foreign_id, previous_id)?;
    let shown = cli(root, &project, &["show", &id.to_string()])?;
    assert_eq!(shown["data"]["revisions"][0]["actor"], "ai: test-client");

    let draft_screen = tui.screen("MCP draft appears live", |screen| {
        screen.contains("Draft from MCP") && screen.contains("Needs approval")
    })?;
    tui.click("2 Queue")?;
    let queue_screen = tui.screen("queue shows MCP draft", |screen| {
        screen.contains("Queue · 1 shown") && screen.contains("Draft from MCP")
    })?;
    tui.send("1v")?;
    let approved_screen = tui.screen("owner approves by keyboard", |screen| {
        screen.contains("approved by human: owner") && screen.contains("Pending")
    })?;
    println!(
        "MCP DRAFT\n{draft_screen}\nMCP QUEUE\n{queue_screen}\nMCP APPROVED\n{approved_screen}"
    );
    tui.send("\x03")?;
    tui.finish()?;
    let approved = cli(root, &project, &["show", &id.to_string()])?;
    assert_eq!(approved["data"]["state"], "pending");
    assert_eq!(cli(root, &other, &["list"])?["data"]["count"], 1);
    Ok(())
}

#[test]
fn mcp_startup_never_registers_a_project_or_bypasses_worker_isolation() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    for output in [
        process(DEV, root).arg("mcp").output()?,
        server(root, Path::new(".")).output()?,
        server(root, root).output()?,
    ] {
        assert!(!output.status.success(), "{:?}", output.status);
        assert!(output.stdout.is_empty(), "{:?}", output.stdout);
        assert!(!output.stderr.is_empty());
        assert!(!root.join("state").exists());
    }
    let project = root.join("registered");
    std::fs::create_dir(&project)?;
    support::empty_repository(&project)?;
    cli(
        root,
        &project,
        &["check-set", "fixture", "--command", "true"],
    )?;
    let unknown = server(root, root).output()?;
    assert_eq!(unknown.status.code(), Some(3));
    assert!(unknown.stdout.is_empty());
    let worker = server(root, &project)
        .env("KTASK_WORKER_CALLBACK", "1")
        .output()?;
    assert_eq!(worker.status.code(), Some(2));
    assert!(worker.stdout.is_empty());
    assert!(String::from_utf8(worker.stderr)?.contains("Workers may only"));
    Ok(())
}
