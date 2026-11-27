use crate::{
    cli::{DEV, json_result, process},
    support::{self, Result, Terminal},
};
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
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
    assert_eq!(names.len(), 6);
    for name in ["projects", "current", "list", "show", "events", "add_draft"] {
        assert!(names.contains(&name), "{names:?}");
    }
    let structured = |id| &reply(id)["result"]["structuredContent"];
    assert_eq!(structured(2)["task"], Value::Null);
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

    let needs_approval = |screen: &str| screen.contains("Projects: 1 need you · 0 running");
    let draft_screen = tui.screen("MCP draft appears live", |screen| {
        screen.contains("Draft from MCP")
            && screen.contains("Needs approval")
            && needs_approval(screen)
    })?;
    tui.click("2 Queue")?;
    let queue_screen = tui.screen("queue shows MCP draft", |screen| {
        screen.contains("Queue · 1 shown")
            && screen.contains("Draft from MCP")
            && needs_approval(screen)
    })?;
    tui.send("1v")?;
    let approved_screen = tui.screen("owner approves by keyboard", |screen| {
        screen.contains("approved by human: owner")
            && screen.contains("Pending")
            && screen.contains("Projects: 0 need you · 0 running")
    })?;
    crate::support::record("MCP DRAFT", false, &draft_screen);
    crate::support::record("MCP QUEUE", true, &queue_screen);
    crate::support::record("MCP APPROVED", true, &approved_screen);
    tui.send("\x03")?;
    tui.finish()?;
    let approved = cli(root, &project, &["show", &id.to_string()])?;
    assert_eq!(approved["data"]["state"], "pending");
    assert_eq!(cli(root, &other, &["list"])?["data"]["count"], 1);
    Ok(())
}

/// One server process answering one request at a time, so a test can change the host between calls.
struct Session {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    next: i64,
}

impl Session {
    fn start(mut command: Command) -> Result<Self> {
        let mut child = command
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let input = child.stdin.take().ok_or("MCP stdin")?;
        let output = BufReader::new(child.stdout.take().ok_or("MCP stdout")?);
        let mut session = Self {
            child,
            input,
            output,
            next: 1,
        };
        let reply = session.request(
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"fixture","version":"1"}}),
        )?;
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        writeln!(
            session.input,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )?;
        Ok(session)
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next;
        self.next += 1;
        writeln!(
            self.input,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
        )?;
        self.input.flush()?;
        let mut line = String::new();
        assert_ne!(self.output.read_line(&mut line)?, 0, "server exited");
        let reply: Value = serde_json::from_str(&line)?;
        assert_eq!(reply["id"], id);
        Ok(reply)
    }

    /// The structured result of a successful tool call.
    fn ok(&mut self, name: &str, arguments: Value) -> Result<Value> {
        let reply = self.request("tools/call", json!({"name":name,"arguments":arguments}))?;
        assert_ne!(reply["result"]["isError"], true, "{name}: {reply}");
        Ok(reply["result"]["structuredContent"].clone())
    }

    /// The message of a tool call that failed.
    fn error(&mut self, name: &str, arguments: Value) -> Result<String> {
        let reply = self.request("tools/call", json!({"name":name,"arguments":arguments}))?;
        assert_eq!(reply["result"]["isError"], true, "{name}: {reply}");
        Ok(reply["result"]["content"][0]["text"]
            .as_str()
            .ok_or("error text")?
            .to_owned())
    }

    fn finish(mut self) -> Result {
        drop(self.input);
        let mut rest = String::new();
        std::io::Read::read_to_string(&mut self.output, &mut rest)?;
        assert!(rest.is_empty(), "{rest}");
        let mut stderr = String::new();
        std::io::Read::read_to_string(&mut self.child.stderr.take().ok_or("stderr")?, &mut stderr)?;
        assert!(stderr.is_empty(), "{stderr}");
        assert!(self.child.wait()?.success());
        Ok(())
    }
}

fn host(root: &Path, directory: &Path, arguments: &[&Path]) -> Command {
    let mut command = process(DEV, root);
    command
        .current_dir(directory)
        .args(["--actor", "test-client"]);
    for argument in arguments {
        command.arg("--project").arg(argument);
    }
    command
}

fn register(root: &Path, name: &str, title: &str) -> Result<(std::path::PathBuf, i64)> {
    let folder = root.join(name);
    std::fs::create_dir(&folder)?;
    support::empty_repository(&folder)?;
    let added = cli(
        root,
        &folder,
        &[
            "add",
            "--title",
            title,
            "--goal",
            "Be listed",
            "--criterion",
            "Shown",
        ],
    )?;
    let task = added["data"]["tasks"][0]["id"].as_i64().ok_or("task id")?;
    Ok((folder, task))
}

fn project_id(root: &Path, name: &str) -> Result<i64> {
    let listed = json_result(process(DEV, root).args(["--json", "projects"]).output()?, 0)?;
    listed["data"]["projects"]
        .as_array()
        .and_then(|projects| projects.iter().find(|project| project["name"] == name))
        .and_then(|project| project["id"].as_i64())
        .ok_or_else(|| format!("{name} is not registered").into())
}

fn tasks(root: &Path, folder: &Path) -> Result<Value> {
    Ok(cli(root, folder, &["list", "--scope", "all"])?["data"]["tasks"].clone())
}

struct Fleet<'a> {
    root: &'a Path,
    alpha: std::path::PathBuf,
    beta: std::path::PathBuf,
    gamma: std::path::PathBuf,
    alpha_id: i64,
    beta_id: i64,
    gamma_id: i64,
    alpha_task: i64,
    beta_task: i64,
    /// Every project's tasks before the session changed anything.
    before: Vec<Value>,
}

fn read_by_id_folder_and_default(server: &mut Session, fleet: &Fleet) -> Result {
    let Fleet {
        root,
        alpha,
        beta,
        alpha_id,
        beta_id,
        gamma_id,
        alpha_task,
        beta_task,
        ..
    } = fleet;
    let (alpha_id, beta_id, gamma_id, alpha_task, beta_task) =
        (*alpha_id, *beta_id, *gamma_id, *alpha_task, *beta_task);
    let listed = server.ok("projects", json!({}))?;
    let cli_listed = json_result(process(DEV, root).args(["--json", "projects"]).output()?, 0)?;
    assert_eq!(listed, cli_listed["data"]);
    assert_eq!(listed["count"], 3);
    for field in [
        "id",
        "name",
        "root",
        "state",
        "runner_alive",
        "queued",
        "finished",
        "current",
    ] {
        assert!(listed["projects"][0].get(field).is_some(), "{field}");
    }

    let by_default = server.ok("current", json!({}))?;
    assert_eq!(by_default["project"], json!({"id":gamma_id,"name":"gamma"}));
    assert_eq!(by_default["task"]["title"], "Gamma work");
    let by_id = server.ok("current", json!({"project":alpha_id}))?;
    assert_eq!(by_id["project"], json!({"id":alpha_id,"name":"alpha"}));
    assert_eq!(by_id["task"]["title"], "Alpha work");
    let by_folder = server.ok("current", json!({"project":beta}))?;
    assert_eq!(by_folder["project"], json!({"id":beta_id,"name":"beta"}));
    assert_eq!(by_folder["task"]["title"], "Beta work");
    let shown = server.ok(
        "show",
        json!({"id":beta_task,"project":beta_id.to_string()}),
    )?;
    assert_eq!(shown["task"]["title"], "Beta work");
    assert_eq!(shown["project"]["name"], "beta");
    let shown = server.ok("show", json!({"id":alpha_task,"project":alpha}))?;
    assert_eq!(shown["task"]["title"], "Alpha work");
    let listed = server.ok("list", json!({"project":alpha_id}))?;
    assert_eq!(listed["project"]["id"], alpha_id);
    assert_eq!(listed["count"], 1);
    let events = server.ok("events", json!({"project":beta_id}))?;
    assert_eq!(events["project"]["id"], beta_id);
    assert!(!events["events"].as_array().unwrap().is_empty());

    Ok(())
}

fn failures_change_nothing(server: &mut Session, fleet: &Fleet) -> Result {
    let Fleet {
        root,
        alpha,
        beta,
        gamma,
        alpha_id,
        beta_task,
        before,
        ..
    } = fleet;
    let (alpha_id, beta_task) = (*alpha_id, *beta_task);
    let unknown = server.error("current", json!({"project":9999}))?;
    assert!(unknown.contains("not a registered project"), "{unknown}");
    let unregistered = server.error(
        "add_draft",
        json!({"project":root,"title":"T","goal":"G","criteria":["C"]}),
    )?;
    assert!(unregistered.contains("projects tool"), "{unregistered}");
    let relative = server.error("current", json!({"project":"alpha"}))?;
    assert!(relative.contains("absolute folder"), "{relative}");
    assert!(
        server
            .error("show", json!({"id":beta_task,"project":alpha_id}))?
            .contains("does not exist")
    );
    for (folder, expected) in [alpha, beta, gamma].into_iter().zip(before) {
        assert_eq!(tasks(root, folder)?, *expected);
    }

    Ok(())
}

fn draft_into_named_project(server: &mut Session, fleet: &Fleet) -> Result {
    let Fleet {
        root,
        alpha,
        beta,
        gamma,
        alpha_id,
        before,
        ..
    } = fleet;
    let alpha_id = *alpha_id;
    // A draft goes to the named project, not the working directory's, as the AI actor.
    let draft =
        json!({"title":"Filed over MCP","goal":"Arrive in alpha","criteria":["Only alpha has it"]});
    let mut named = draft.clone();
    named["project"] = json!(alpha_id);
    let filed = server.ok("add_draft", named)?;
    assert_eq!(filed["project"], json!({"id":alpha_id,"name":"alpha"}));
    let draft_id = filed["tasks"][0]["id"].as_i64().ok_or("draft id")?;
    let shown = cli(root, alpha, &["show", &draft_id.to_string()])?;
    assert_eq!(shown["data"]["revisions"][0]["actor"], "ai: test-client");
    assert_eq!(shown["data"]["state"], "needs_approval");
    assert_eq!(tasks(root, beta)?, before[1]);
    assert_eq!(tasks(root, gamma)?, before[2]);
    let via_server = server.ok("show", json!({"id":draft_id,"project":alpha_id}))?;
    assert_eq!(via_server["task"]["revision_actor"], "ai: test-client");
    let by_folder = server.ok(
        "add_draft",
        json!({"project":gamma,"title":"Folder draft","goal":"G","criteria":["C"]}),
    )?;
    assert_eq!(by_folder["project"]["name"], "gamma");

    Ok(())
}

fn missing_and_moved_folders(server: &mut Session, fleet: &Fleet) -> Result {
    let Fleet {
        root,
        beta,
        beta_id,
        ..
    } = fleet;
    let beta_id = *beta_id;
    // A missing folder is still readable and still takes drafts.
    let moved = root.join("beta-elsewhere");
    std::fs::rename(beta, &moved)?;
    let missing = server.ok("current", json!({"project":beta_id}))?;
    assert_eq!(missing["task"]["title"], "Beta work");
    let state = server.ok("projects", json!({}))?;
    let beta_row = state["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["id"] == beta_id)
        .unwrap();
    assert_eq!(beta_row["state"], "missing");
    server.ok(
        "add_draft",
        json!({"project":beta,"title":"Into a missing folder","goal":"G","criteria":["C"]}),
    )?;
    // Moving it mid-session: the new folder resolves, the old one no longer does.
    let relocated = json_result(
        process(DEV, root)
            .args(["--json", "project-move", &beta_id.to_string()])
            .arg(&moved)
            .output()?,
        0,
    )?;
    assert_eq!(relocated["data"]["id"], beta_id);
    let after_move = server.ok("list", json!({"project":moved}))?;
    assert_eq!(after_move["count"], 2);
    assert!(
        server
            .error("list", json!({"project":beta}))?
            .contains("not a registered project")
    );
    assert_eq!(server.ok("list", json!({"project":beta_id}))?["count"], 2);

    Ok(())
}

fn deleted_projects(server: &mut Session, fleet: &Fleet) -> Result {
    let Fleet {
        root,
        alpha,
        alpha_id,
        beta_id,
        gamma_id,
        ..
    } = fleet;
    let (alpha_id, beta_id, gamma_id) = (*alpha_id, *beta_id, *gamma_id);
    // Deleting a project mid-session fails only the calls that name it.
    let removed = process(DEV, root)
        .args([
            "--json",
            "project-delete",
            &alpha_id.to_string(),
            "--confirm",
            "alpha",
        ])
        .output()?;
    json_result(removed, 0)?;
    assert!(
        server
            .error("current", json!({"project":alpha_id}))?
            .contains("not a registered project")
    );
    assert!(
        server
            .error("list", json!({"project":alpha}))?
            .contains("projects tool")
    );
    assert_eq!(server.ok("current", json!({}))?["project"]["name"], "gamma");
    let remaining = server.ok("projects", json!({}))?;
    assert_eq!(remaining["count"], 2);
    assert!(
        remaining["projects"]
            .as_array()
            .unwrap()
            .iter()
            .all(|p| p["id"] != alpha_id)
    );
    // The folder the server started in is deleted too: no project applies any more.
    json_result(
        process(DEV, root)
            .args([
                "--json",
                "project-delete",
                &gamma_id.to_string(),
                "--confirm",
                "gamma",
            ])
            .output()?,
        0,
    )?;
    assert!(
        server
            .error("current", json!({}))?
            .contains("projects tool")
    );
    assert_eq!(
        server.ok("current", json!({"project":beta_id}))?["project"]["name"],
        "beta"
    );

    Ok(())
}

#[test]
fn one_host_wide_server_reads_and_drafts_for_every_project_in_one_session() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (alpha, alpha_task) = register(root, "alpha", "Alpha work")?;
    let (beta, beta_task) = register(root, "beta", "Beta work")?;
    let (gamma, _) = register(root, "gamma", "Gamma work")?;
    let inside = gamma.join("src");
    std::fs::create_dir(&inside)?;
    let (alpha_id, beta_id, gamma_id) = (
        project_id(root, "alpha")?,
        project_id(root, "beta")?,
        project_id(root, "gamma")?,
    );
    let before: Vec<Value> = [&alpha, &beta, &gamma]
        .iter()
        .map(|folder| tasks(root, folder))
        .collect::<Result<_>>()?;
    let fleet = Fleet {
        root,
        alpha,
        beta,
        gamma,
        alpha_id,
        beta_id,
        gamma_id,
        alpha_task,
        beta_task,
        before,
    };

    // The working directory is inside gamma; every call may still name another project.
    let mut server = Session::start(host(root, &inside, &[]))?;
    let names: Vec<_> = server.request("tools/list", json!({}))?["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        names,
        ["projects", "current", "list", "show", "events", "add_draft"]
    );

    read_by_id_folder_and_default(&mut server, &fleet)?;
    failures_change_nothing(&mut server, &fleet)?;
    draft_into_named_project(&mut server, &fleet)?;
    missing_and_moved_folders(&mut server, &fleet)?;
    deleted_projects(&mut server, &fleet)?;
    server.finish()
}

#[test]
fn a_pinned_server_refuses_every_other_project() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let (alpha, _) = register(root, "alpha", "Alpha work")?;
    let (beta, _) = register(root, "beta", "Beta work")?;
    let (alpha_id, beta_id) = (project_id(root, "alpha")?, project_id(root, "beta")?);
    let before = tasks(root, &beta)?;

    let mut server = Session::start(host(root, root, &[&alpha]))?;
    assert_eq!(server.ok("current", json!({}))?["project"]["id"], alpha_id);
    assert_eq!(
        server.ok("current", json!({"project":alpha_id}))?["project"]["id"],
        alpha_id
    );
    assert_eq!(
        server.ok("current", json!({"project":alpha}))?["project"]["id"],
        alpha_id
    );
    for other in [json!(beta_id), json!(beta)] {
        let refusal = server.error("current", json!({"project":other}))?;
        assert!(refusal.contains("pinned to one project"), "{refusal}");
        let refusal = server.error(
            "add_draft",
            json!({"project":other,"title":"T","goal":"G","criteria":["C"]}),
        )?;
        assert!(refusal.contains("pinned to one project"), "{refusal}");
    }
    assert_eq!(tasks(root, &beta)?, before);
    assert_eq!(server.ok("projects", json!({}))?["count"], 2);
    json_result(
        process(DEV, root)
            .args([
                "--json",
                "project-delete",
                &alpha_id.to_string(),
                "--confirm",
                "alpha",
            ])
            .output()?,
        0,
    )?;
    assert!(
        server
            .error("current", json!({}))?
            .contains("not a registered project")
    );
    assert_eq!(server.ok("projects", json!({}))?["count"], 1);
    server.finish()
}

#[test]
fn mcp_startup_creates_nothing_and_registers_nothing() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let nowhere = root.join("nowhere");
    std::fs::create_dir(&nowhere)?;

    // No database yet, and the working directory is no project.
    let mut server = Session::start(host(root, &nowhere, &[]))?;
    assert_eq!(
        server.ok("projects", json!({}))?,
        json!({"projects":[],"count":0})
    );
    for tool in ["current", "list", "events"] {
        let message = server.error(tool, json!({}))?;
        assert!(message.contains("projects tool"), "{message}");
    }
    let message = server.error(
        "add_draft",
        json!({"project":nowhere,"title":"T","goal":"G","criteria":["C"]}),
    )?;
    assert!(message.contains("projects tool"), "{message}");
    server.finish()?;
    assert!(!root.join("state").exists(), "mcp created storage");

    // A pin must name a registered project, a human actor is refused, and workers cannot serve.
    for output in [
        host(root, &nowhere, &[&nowhere]).arg("mcp").output()?,
        host(root, &nowhere, &[Path::new("relative")])
            .arg("mcp")
            .output()?,
        host(root, &nowhere, &[])
            .args(["--actor-kind", "human", "mcp"])
            .output()?,
        host(root, &nowhere, &[])
            .args(["--project-name", "x", "mcp"])
            .output()?,
    ] {
        assert!(!output.status.success(), "{:?}", output.status);
        assert!(output.stdout.is_empty(), "{:?}", output.stdout);
        assert!(!output.stderr.is_empty());
        assert!(!root.join("state").exists(), "mcp created storage");
    }
    let worker = host(root, &nowhere, &[])
        .arg("mcp")
        .env("KTASK_WORKER_CALLBACK", "1")
        .output()?;
    assert_eq!(worker.status.code(), Some(2));
    assert!(String::from_utf8(worker.stderr)?.contains("Workers may only"));

    // With projects registered, an unregistered directory still registers nothing.
    let (project, _) = register(root, "registered", "Registered work")?;
    let unknown = host(root, &nowhere, &[&nowhere]).arg("mcp").output()?;
    assert_eq!(unknown.status.code(), Some(3));
    assert!(unknown.stdout.is_empty());
    let mut server = Session::start(host(root, &nowhere, &[]))?;
    assert!(
        server
            .error("current", json!({}))?
            .contains("projects tool")
    );
    assert_eq!(server.ok("projects", json!({}))?["count"], 1);
    server.finish()?;
    assert_eq!(
        json_result(process(DEV, root).args(["--json", "projects"]).output()?, 0)?["data"]["count"],
        1
    );
    assert_eq!(tasks(root, &project)?.as_array().map(Vec::len), Some(1));
    Ok(())
}
