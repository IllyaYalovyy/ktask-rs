//! Claude Code and OpenCode adapters, driven through the real binary with fake CLIs that
//! emit each provider's own event format. The fake reads its scenario from $HOME/agent-mode.
use super::*;

const CLAUDE: &str = r#"#!/bin/sh
mode=$(cat "$HOME/agent-mode" 2>/dev/null || echo normal)
model=unknown
session=""
args="$*"
while [ "$#" -gt 0 ]; do
 case "$1" in --model) shift; model="$1";; --resume) shift; session="$1";; esac
 shift
done
prompt=$(cat)
printf '%s|%s|%s\n' "$model" "$session" "$args" >> "$HOME/claude-calls"
[ -n "$session" ] || session="claude-$model-$$"
printf '{"type":"system","subtype":"init","session_id":"%s"}\n' "$session"
case "$mode" in
 rate) printf '%s\n' '{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":4102444800}}' '{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"result":"You have hit your limit"}'; exit 1;;
 fail) echo 'API Error: 500 upstream' >&2; exit 1;;
 slow) if [ "$model" = execution ]; then sleep 30; fi;;
esac
reply() {
 printf '{"type":"result","subtype":"success","is_error":false,"result":"","structured_output":%s,"total_cost_usd":0.0123,"usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":7,"output_tokens":10}}\n' "$1"
 exit 0
}
case "$prompt" in *"You are the failure judge"*)
 reply '{"judgment":{"action":"repair","plan":"Write verified to result.txt","reason":"The boundary was missing"}}';;
esac
if [ "$mode" = question ]; then
 case "$prompt" in *"Human answer"*) :;; *) reply '{"outcome":"blocked","summary":"Need direction","question":"Which content?","criteria":[]}';; esac
fi
if [ "$mode" = review-once ] && [ "$model" = review ] && [ ! -f "$HOME/review-failed" ]; then
 touch "$HOME/review-failed"
 reply '{"outcome":"changes_requested","summary":"Fix the boundary","question":null,"criteria":[{"ordinal":1,"passed":false,"evidence":"Boundary fails"}],"findings":[{"severity":"critical","description":"result.txt:1: missing boundary; write verified"}]}'
fi
case "$model" in execution|remediation)
 printf '%s\n' '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"printf verified > result.txt"}}]}}'
 printf 'verified\n' > result.txt
 printf '%s\n' '{"type":"user","message":{"content":[{"type":"tool_result","content":"wrote result.txt"}]}}';;
esac
reply '{"outcome":"complete","summary":"Verified result","question":null,"criteria":[{"ordinal":1,"passed":true,"evidence":"Inspected result.txt"}]}'
"#;

const OPENCODE: &str = r#"#!/bin/sh
mode=$(cat "$HOME/agent-mode" 2>/dev/null || echo normal)
model=unknown
session=""
auto=no
while [ "$#" -gt 0 ]; do
 case "$1" in -m) shift; model="${1#*/}";; --session) shift; session="$1";; --auto) auto=yes;; esac
 shift
done
prompt=$(cat)
printf '%s|%s|auto=%s|%s\n' "$model" "$session" "$auto" "${OPENCODE_PERMISSION:-none}" >> "$HOME/opencode-calls"
[ -n "$session" ] || session="ses_$model$$"
ev() { printf '{"type":"%s","timestamp":1,"sessionID":"%s","part":%s}\n' "$1" "$session" "$2"; }
step() { ev step_finish "{\"reason\":\"$1\",\"cost\":0.01,\"tokens\":{\"input\":10,\"output\":5,\"reasoning\":1,\"cache\":{\"read\":100,\"write\":20}}}"; }
case "$mode" in rate)
 printf '{"type":"error","timestamp":1,"sessionID":"%s","error":{"name":"APIError","data":{"message":"Rate limit exceeded","statusCode":429,"responseHeaders":{"retry-after":"600"}}}}\n' "$session"
 exit 1;;
esac
case "$prompt" in *"JSON Schema"*) :;; *) echo 'schema missing from prompt' >&2; exit 3;; esac
ev step_start '{}'
case "$model" in execution|remediation)
 ev tool_use '{"tool":"bash","state":{"status":"completed","input":{"command":"printf verified > result.txt"},"output":"wrote result.txt"}}'
 printf 'verified\n' > result.txt
 step tool-calls
 ev step_start '{}';;
esac
ev text '{"text":"```json\n{\"outcome\":\"complete\",\"summary\":\"Verified result\",\"question\":null,\"criteria\":[{\"ordinal\":1,\"passed\":true,\"evidence\":\"Inspected result.txt\"}]}\n```"}'
step stop
"#;

impl Fixture {
    fn agent(&self, provider: &str, script: &str, mode: &str) -> Result {
        let path = self.scratch.path().join("bin").join(provider);
        fs::write(&path, script)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        self.mode(mode)?;
        for role in ["execution", "review", "validation", "remediation"] {
            let model = if provider == "opencode" {
                format!("test/{role}")
            } else {
                role.into()
            };
            self.ok(&[
                "model-default",
                role,
                "--provider",
                provider,
                "--model",
                &model,
            ])?;
        }
        Ok(())
    }

    fn mode(&self, mode: &str) -> Result {
        fs::write(self.scratch.path().join("agent-mode"), mode)?;
        Ok(())
    }

    /// (model, resumed session, rest) for each recorded provider call.
    fn calls(&self, provider: &str) -> Result<Vec<(String, String, String)>> {
        let path = self.scratch.path().join(format!("{provider}-calls"));
        Ok(fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut parts = line.splitn(3, '|').map(String::from);
                let mut next = || parts.next().unwrap_or_default();
                (next(), next(), next())
            })
            .collect())
    }

    fn usage(&self) -> Result<Vec<(String, [i64; 5])>> {
        let db = self.db()?;
        let mut stmt = db.connection().prepare(
            "SELECT role,input_tokens,cache_read_tokens,cache_write_tokens,output_tokens,cost_microusd
             FROM model_calls WHERE state='succeeded' ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    [r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?],
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

fn read_only(args: &str) -> bool {
    args.contains("--permission-mode dontAsk")
        && args.contains("--tools Read,Grep,Glob,Bash")
        && !args.contains("bypassPermissions")
}

#[test]
fn claude_runs_every_role_with_read_only_judges_session_reuse_and_uncached_usage() -> Result {
    let f = Fixture::new()?;
    f.agent("claude", CLAUDE, "review-once")?;
    f.add("Claude task")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded", "{task}");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    let calls = f.calls("claude")?;
    let roles: Vec<_> = calls.iter().map(|(model, ..)| model.as_str()).collect();
    assert_eq!(
        roles,
        [
            "execution",
            "review",
            "remediation",
            "remediation",
            "review",
            "validation"
        ]
    );
    for (model, _, args) in &calls {
        assert!(args.contains("--output-format stream-json") && args.contains("--json-schema"));
        assert!(args.contains(&format!("--model {model}")));
    }
    // Reviewers and the failure judge are read-only; workers never ask for permission.
    let [execution, review, judge, repair, review_again, validation] = &calls[..] else {
        unreachable!()
    };
    for (_, _, args) in [review, judge, review_again, validation] {
        assert!(read_only(args), "{args}");
    }
    for (_, _, args) in [execution, repair] {
        assert!(
            args.contains("--permission-mode bypassPermissions"),
            "{args}"
        );
    }
    // The second review resumes the first review's session.
    assert_eq!(review.1, "");
    assert!(review_again.1.starts_with("claude-review-"), "{calls:?}");
    let usage = f.usage()?;
    assert_eq!(usage.len(), 6);
    for (role, counts) in usage {
        assert_eq!(counts, [100, 40, 7, 10, 12_300], "{role}");
    }
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT count(*) FROM ai_sessions WHERE provider='claude' AND provider_session_id LIKE 'claude-%'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        5,
        "execution, review, judge, repair and validation sessions"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn claude_question_pause_and_resume_keep_one_attempt_and_its_session() -> Result {
    let f = Fixture::new()?;
    f.agent("claude", CLAUDE, "question")?;
    f.add("Ask then resume")?;
    f.ok(&["run"])?;
    f.state("waiting")?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["attempts"][0]["question"], "Which content?");
    assert_eq!(task["attempts_used"], 0);
    f.ok(&["answer", "1", "--text", "Human answer: verified"])?;
    f.state("succeeded")?;
    let calls = f.calls("claude")?;
    // Asked, nudged once in the same session, then resumed after the answer.
    let session = &calls[1].1;
    assert!(session.starts_with("claude-execution-"), "{calls:?}");
    assert_eq!(&calls[2].1, session, "{calls:?}");

    let f = Fixture::new()?;
    f.agent("claude", CLAUDE, "slow")?;
    f.add("Pause me")?;
    let mut run = Running(f.process().arg("run").stdout(Stdio::null()).spawn()?);
    f.state("running")?;
    thread::sleep(Duration::from_millis(200));
    f.ok(&["pause", "1"])?;
    run.0.wait()?;
    f.state("paused")?;
    f.mode("normal")?;
    f.ok(&["resume"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded");
    assert_eq!(task["attempts"].as_array().unwrap().len(), 1);
    let calls = f.calls("claude")?;
    assert!(
        calls[1].1.starts_with("claude-execution-"),
        "resume reuses the paused execution session: {calls:?}"
    );
    f.no_scratch()?;
    Ok(())
}

#[test]
fn claude_limits_wait_for_the_reset_and_failures_spend_no_budget() -> Result {
    let f = Fixture::new()?;
    f.agent("claude", CLAUDE, "rate")?;
    f.add("Limited")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    assert_eq!(task["attempts_used"], 0);
    assert_eq!(task["attempts"][0]["wait_reason"], "rate_limit");
    assert_eq!(
        task["attempts"][0]["resume_at_ms"],
        4_102_444_800_000_i64 + 60_000
    );

    let f = Fixture::new()?;
    f.agent("claude", CLAUDE, "fail")?;
    f.add("Broken provider")?;
    assert_eq!(
        f.process().args(["run", "--one"]).output()?.status.code(),
        Some(4)
    );
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "failed");
    assert_eq!(task["attempts_used"], 0);
    assert!(
        task["attempts"][0]["outcome"]
            .as_str()
            .unwrap()
            .contains("Claude Code exited 1"),
        "{task}"
    );
    assert_eq!(f.calls("claude")?.len(), 3, "two automatic retries");
    f.no_scratch()?;
    Ok(())
}

#[test]
fn opencode_runs_every_role_read_only_where_required_and_sums_step_usage() -> Result {
    let f = Fixture::new()?;
    f.agent("opencode", OPENCODE, "normal")?;
    f.add("OpenCode task")?;
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["lifecycle"], "succeeded", "{task}");
    let calls = f.calls("opencode")?;
    let roles: Vec<_> = calls.iter().map(|(model, ..)| model.as_str()).collect();
    assert_eq!(roles, ["execution", "review", "validation"]);
    let (_, _, execution) = &calls[0];
    assert!(execution.starts_with("auto=yes|none"), "{execution}");
    for (_, _, reader) in &calls[1..] {
        let policy: Value = serde_json::from_str(reader.strip_prefix("auto=no|").unwrap())?;
        assert_eq!(policy["edit"], "deny");
        assert_eq!(policy["bash"]["*"], "deny");
    }
    // Workers report two steps, reviewers one; input stays uncached and cache counts disjoint.
    let usage = f.usage()?;
    assert_eq!(usage[0].1, [20, 200, 40, 12, 20_000]);
    for (_, counts) in &usage[1..] {
        assert_eq!(*counts, [10, 100, 20, 6, 10_000]);
    }
    assert_eq!(
        f.db()?.connection().query_row(
            "SELECT count(*) FROM ai_sessions WHERE provider='opencode' AND provider_session_id LIKE 'ses_%'",
            [],
            |r| r.get::<_, i64>(0)
        )?,
        3
    );

    let f = Fixture::new()?;
    f.agent("opencode", OPENCODE, "rate")?;
    f.add("Limited")?;
    let before = support_now();
    f.ok(&["run", "--one"])?;
    let task = f.ok(&["show", "1"])?;
    assert_eq!(task["status"], "waiting");
    let resume = task["attempts"][0]["resume_at_ms"].as_i64().unwrap();
    assert!(
        resume >= before + 660_000 && resume < before + 700_000,
        "retry-after 600s plus a minute"
    );
    f.no_scratch()?;
    Ok(())
}

fn support_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

#[test]
fn real_tui_runs_a_claude_task_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = Fixture::new()?;
        f.agent("claude", CLAUDE, "normal")?;
        f.add("Claude in the TUI")?;
        let mut t = Terminal::open_project(BIN, f.scratch.path(), &f.root, 120, 36)?;
        t.screen("claude current", |s| {
            s.contains("Claude in the TUI") && s.contains("claude/execution")
        })?;
        if mouse {
            t.click("u Readiness")?;
        } else {
            t.send("u")?;
        }
        let ready = t.screen("claude readiness", |s| {
            s.contains("OK · Claude Code CLI") && s.contains("Can run")
        })?;
        println!("CLAUDE READINESS mouse={mouse}\n{ready}");
        if mouse {
            t.click("r Run")?;
        } else {
            t.send("r")?;
        }
        let done = t.screen("claude complete", |s| s.contains("Queue complete."))?;
        println!("CLAUDE COMPLETE mouse={mouse}\n{done}");
        let task = f.ok(&["show", "1"])?;
        assert_eq!(task["lifecycle"], "succeeded");
        assert_eq!(
            f.calls("claude")?
                .iter()
                .map(|(model, ..)| model.as_str())
                .collect::<Vec<_>>(),
            ["execution", "review", "validation"]
        );
        t.send("q")?;
        t.finish()?;
        f.no_scratch()?;
    }
    Ok(())
}
