//! The full acceptance run (T160): the whole product exercised in one
//! reproducible journey, through the compiled `ktask-rs` binary only.
//!
//! [`support::build`] starts from a clean state — a scratch project with a
//! bare `origin`, `init`, and a four-task plan imported with `add --file` —
//! and the test then drains the queue the way an operator would:
//!
//! 1. `run`: task 1 is done; task 2's verify gate fails, one fresh-session
//!    remediation fixes it and it is done; task 3 asks a question and the run
//!    stops with exit 5.
//! 2. The TUI attaches mid-flight, over the same journal the run wrote.
//! 3. `resolve` answers the question into an ADR, which the operator commits
//!    (preflight refuses an uncommitted tree); `run --task 3` finishes it.
//! 4. `resume` runs task 4, whose formatter changes a tracked file, so its
//!    commit is the one that moves the remote tip; the queue drains.
//! 5. The journal is asserted event for event, the task states, the ADR and
//!    the remote; the TUI, still attached, follows the rest of the journal.
//!
//! Nothing here is timing-dependent: every provider step is scripted, every
//! commit id is read back from git rather than assumed, and the TUI is driven
//! through ratatui's `TestBackend`.

mod support;

use std::io;
use std::path::{Path, PathBuf};
use std::process::Output;

use ktask_core::{
    AttemptId, DecisionRequest, EventKind, FailureClass, Journal, Phase, Stream, TaskId, TaskState,
    apply, journal_path,
};
use ktask_tui::testing::Harness;
use ktask_tui::{App, JournalTail};

const TASKS: u32 = 4;

/// What the journey's steps return: any failure ends the test, with its own
/// message.
type Res<T = ()> = Result<T, Box<dyn std::error::Error>>;

/// The ADR the `resolve` in step 3 writes, relative to the repository root.
const ADR: &str = "docs/adr/0001-which-store.md";

/// Task `n` of the plan, valid under `docs/CONTRACT.md` section 3.
fn plan_task(n: u32) -> String {
    format!(
        "## Task {n}\n\n**Outcome:** thing {n} exists.\n\n\
         **Done-when:** thing {n} is visible.\n\n**Verify:** `true`\n\n**Refs:** none\n\n"
    )
}

/// A four-task plan.
fn plan() -> String {
    (1..=TASKS).map(plan_task).collect::<Vec<_>>().concat()
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The seven-character form of `sha` the run's result lines print.
fn short(sha: &str) -> &str {
    &sha[..7]
}

/// Runs `git` in `dir` and returns its stdout without the trailing newline.
fn git(dir: &Path, args: &[&str]) -> io::Result<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "git {args:?} failed: {}",
            stderr_of(&output)
        )));
    }
    Ok(stdout_of(&output).trim_end().to_string())
}

/// Asserts `output` exited with `code`, showing both streams if it did not.
fn assert_exit(output: &Output, code: i32) {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        stdout_of(output),
        stderr_of(output)
    );
}

/// Where the runner expects `task`'s `attempt` report.
fn report_path(scenario: &support::Scenario, task: u32, attempt: u32) -> String {
    scenario
        .state_dir()
        .join("attempts")
        .join(task.to_string())
        .join(attempt.to_string())
        .join("report.md")
        .display()
        .to_string()
}

/// One scripted provider step that succeeds, prints `stdout` and writes
/// `report` to `task`'s `attempt` report path.
fn agent_step(
    scenario: &support::Scenario,
    task: u32,
    attempt: u32,
    stdout: &str,
    report: &str,
) -> String {
    format!(
        "[[steps]]\noutcome = \"success\"\nstdout = \"{stdout}\\n\"\n\n\
         [[steps.files]]\npath = \"{}\"\ncontent = \"{report}\"\n\n",
        report_path(scenario, task, attempt)
    )
}

/// The step every run consumes before an attempt: the provider probe.
const PROBE: &str = "[[steps]]\noutcome = \"success\"\n\n";

const DONE_REPORT: &str = "KTASK_RESULT: DONE\\nSummary: it worked.\\n";

const QUESTION_REPORT: &str = "KTASK_RESULT: NEEDS_INPUT\\nQuestion: Which store?\\n\
                               Options:\\n- Postgres\\n- SQLite\\n\
                               Trade-offs: one scales, one is a file.\\nImpact: durability.\\n";

/// The project config: the `dummy` provider on this scenario, and a verify
/// gate that logs each run to `log` and fails on exactly the second — task
/// 2's first attempt — and passes otherwise. `format` is appended verbatim.
fn config(scenario: &support::Scenario, log: &Path, format: &str) -> String {
    format!(
        "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\n\
         verify_command = [\"sh\", \"-c\", \
         \"echo run >> \\\"$1\\\"; test \\\"$(wc -l < \\\"$1\\\")\\\" -ne 2\", \"_\", \"{}\"]\n{format}",
        scenario.state_dir().join("scenario.toml").display(),
        log.display()
    )
}

/// A formatter that appends a line to the tracked `SEED.md`. It runs as part
/// of the completion set, so the change is committed with the task.
const FORMATTER: &str = "format_command = [\"sh\", \"-c\", \"echo formatted >> SEED.md\"]\n";

/// A journal row: the task it belongs to, if any, and what happened.
type Row = (Option<TaskId>, EventKind);

/// The four events preflight journals: the task's own, around the
/// project-level probe.
fn preflight(task: u32, base: &str) -> Vec<Row> {
    let id = Some(TaskId::new(task));
    let passed = || EventKind::PreflightPassed {
        base_sha: base.to_string(),
    };
    vec![
        (id, EventKind::PreflightStarted),
        (None, EventKind::PreflightStarted),
        (None, passed()),
        (id, passed()),
    ]
}

/// An attempt starting from `base`, up to the start of its implement phase.
/// The pid is normalised to 0 by the caller.
fn begin(task: u32, attempt: u32, base: &str) -> Vec<Row> {
    let attempt = AttemptId::new(attempt);
    vec![
        (
            Some(TaskId::new(task)),
            EventKind::AttemptStarted {
                attempt,
                protocol: "direct".to_string(),
                pid: 0,
                base_sha: base.to_string(),
            },
        ),
        phase(task, attempt.get(), Phase::Implement),
    ]
}

fn phase(task: u32, attempt: u32, phase: Phase) -> Row {
    (
        Some(TaskId::new(task)),
        EventKind::PhaseEntered {
            attempt: AttemptId::new(attempt),
            phase,
        },
    )
}

/// The agent printing `text` and exiting 0.
fn implement(task: u32, attempt: u32, text: &str) -> Vec<Row> {
    let id = Some(TaskId::new(task));
    let attempt = AttemptId::new(attempt);
    vec![
        (
            id,
            EventKind::AgentOutput {
                attempt,
                stream: Stream::Stdout,
                text: text.to_string(),
            },
        ),
        (
            id,
            EventKind::AttemptFinished {
                attempt,
                exit_code: 0,
                usage: None,
                session_id: None,
                model_reported: None,
            },
        ),
    ]
}

/// The gates passing and `candidate` being published; `tip` is the commit
/// the remote holds afterwards.
fn publish(task: u32, attempt: u32, candidate: &str, tip: &str) -> Vec<Row> {
    let id = Some(TaskId::new(task));
    vec![
        (
            id,
            EventKind::VerifyPassed {
                attempt: AttemptId::new(attempt),
            },
        ),
        (
            id,
            EventKind::PublishStarted {
                attempt: AttemptId::new(attempt),
                candidate_sha: candidate.to_string(),
            },
        ),
        (
            id,
            EventKind::PublishVerified {
                commit: tip.to_string(),
                remote_sha: tip.to_string(),
            },
        ),
        (
            id,
            EventKind::TaskDone {
                commit: tip.to_string(),
            },
        ),
    ]
}

/// Every event the journey journals, in order. `seed` is the commit the
/// project started from, `adr` the operator's commit of the ADR on top of it,
/// and `tip` task 4's commit, on top of that.
fn expected_journal(seed: &str, adr: &str, tip: &str) -> Vec<Row> {
    let mut rows = Vec::new();

    // Task 1: straight through. Nothing changes a file, so the candidate is
    // the commit the project started from, which the remote already holds.
    rows.extend(preflight(1, seed));
    rows.extend(begin(1, 1, seed));
    rows.extend(implement(1, 1, "work on 1/1\n"));
    rows.push(phase(1, 1, Phase::Verify));
    rows.extend(publish(1, 1, seed, seed));

    // Task 2: the verify gate fails, then a remediation round — a fresh
    // agent session under the same attempt record — passes it.
    rows.extend(preflight(2, seed));
    rows.extend(begin(2, 1, seed));
    rows.extend(implement(2, 1, "work on 2/1\n"));
    rows.push(phase(2, 1, Phase::Verify));
    rows.push((
        Some(TaskId::new(2)),
        EventKind::VerifyFailed {
            attempt: AttemptId::new(1),
            class: FailureClass::VerificationFailure,
            detail: "completion gate Verify failed".to_string(),
        },
    ));
    rows.push(phase(2, 2, Phase::Implement));
    rows.extend(implement(2, 2, "remediation\n"));
    rows.push(phase(2, 2, Phase::Verify));
    rows.extend(publish(2, 2, seed, seed));

    // Task 3: asks a question instead of finishing, and is answered.
    rows.extend(preflight(3, seed));
    rows.extend(begin(3, 1, seed));
    rows.extend(implement(3, 1, "work on 3/1\n"));
    rows.push((
        Some(TaskId::new(3)),
        EventKind::DecisionRaised {
            request: DecisionRequest {
                question: "Which store?".to_string(),
                options: vec!["Postgres".to_string(), "SQLite".to_string()],
                tradeoffs: "one scales, one is a file.".to_string(),
                impact: "durability.".to_string(),
                recommended: None,
            },
        },
    ));
    rows.push((
        Some(TaskId::new(3)),
        EventKind::DecisionResolved {
            adr_path: ADR.into(),
            answer: "Use SQLite.".to_string(),
        },
    ));

    // Task 3 again, from the commit that holds the ADR, as attempt 2.
    rows.extend(preflight(3, adr));
    rows.extend(begin(3, 2, adr));
    rows.extend(implement(3, 2, "work on 3/2\n"));
    rows.push(phase(3, 2, Phase::Verify));
    rows.extend(publish(3, 2, adr, adr));

    // Task 4: its formatter changed a tracked file, so it publishes a new
    // commit on top of the ADR's.
    rows.extend(preflight(4, adr));
    rows.extend(begin(4, 1, adr));
    rows.extend(implement(4, 1, "work on 4/1\n"));
    rows.push(phase(4, 1, Phase::Verify));
    rows.extend(publish(4, 1, tip, tip));

    rows
}

/// The journal as rows, with every attempt's pid — a real process id, never
/// zero — normalised to 0 so it can be compared.
fn journal_rows(journal: &Journal) -> ktask_core::Result<Vec<Row>> {
    let mut rows: Vec<Row> = journal
        .events()?
        .into_iter()
        .map(|event| (event.task_id, event.kind))
        .collect();
    for (_, kind) in &mut rows {
        if let EventKind::AttemptStarted { pid, .. } = kind {
            assert_ne!(*pid, 0, "an attempt records the agent's real pid");
            *pid = 0;
        }
    }
    Ok(rows)
}

/// What the state machine says `task` is in after replaying its events.
fn replayed_state(journal: &Journal, task: TaskId) -> ktask_core::Result<TaskState> {
    journal
        .events_for(task)?
        .iter()
        .try_fold(TaskState::Queued, |state, event| apply(&state, &event.kind))
}

/// Sends every event `tail` has not delivered yet to `harness`, returning how
/// many there were.
fn deliver(tail: &mut JournalTail, harness: &mut Harness) -> ktask_core::Result<usize> {
    let events = tail.poll()?;
    let count = events.len();
    for event in events {
        harness.send(event);
    }
    Ok(count)
}

/// The interface after pressing `key` on a screen: its full text.
fn screen(harness: &mut Harness, key: char) -> String {
    harness.key(key);
    harness.text()
}

/// Asserts `text` contains every one of `needles`, showing the screen if not.
fn assert_shows(text: &str, needles: &[&str]) {
    for needle in needles {
        assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
    }
}

/// The `summary` of `status --json`: how many tasks are in each state.
fn status_summary(scenario: &support::Scenario) -> Res<serde_json::Value> {
    let status = scenario.run(&["status", "--json"])?;
    assert_exit(&status, 0);
    let status: serde_json::Value = serde_json::from_str(stdout_of(&status).trim())?;
    status
        .get("summary")
        .cloned()
        .ok_or_else(|| "status --json has no summary".into())
}

/// The journey's fixture: the scenario, the remote, and the commit the
/// project started from.
struct Journey {
    scenario: support::Scenario,
    origin: PathBuf,
    seed: String,
    verify_log: PathBuf,
    journal: Journal,
}

impl Journey {
    /// A clean state: `init` and `add --file` were done by the harness, and
    /// nothing has run.
    fn start() -> Res<Self> {
        let scenario = support::build(PROBE, &plan())?;
        let project = scenario.project_dir();
        let origin = PathBuf::from(git(project, &["remote", "get-url", "origin"])?);
        let seed = git(project, &["rev-parse", "HEAD"])?;
        assert_eq!(git(&origin, &["rev-parse", "HEAD"])?, seed);

        assert_eq!(
            status_summary(&scenario)?,
            serde_json::json!({"Queued": TASKS})
        );
        assert!(!project.join("docs").exists(), "no ADR exists yet");

        let journal = Journal::open(&journal_path(scenario.state_dir()))?;
        assert_eq!(journal.events()?, [], "a clean journal");
        assert_eq!(
            journal
                .tasks()?
                .iter()
                .map(|task| task.id.get())
                .collect::<Vec<_>>(),
            (1..=TASKS).collect::<Vec<_>>(),
            "add --file queued the whole plan, in order"
        );

        let verify_log = scenario.state_dir().join("verify-runs.log");
        std::fs::write(&verify_log, "")?;
        let journey = Self {
            scenario,
            origin,
            seed,
            verify_log,
            journal,
        };
        journey.write_config("")?;
        Ok(journey)
    }

    fn project(&self) -> &Path {
        self.scenario.project_dir()
    }

    fn write_config(&self, format: &str) -> Res {
        std::fs::write(
            self.scenario.state_dir().join("config.toml"),
            config(&self.scenario, &self.verify_log, format),
        )?;
        Ok(())
    }

    fn origin_tip(&self) -> Res<String> {
        Ok(git(&self.origin, &["rev-parse", "HEAD"])?)
    }

    fn event_count(&self) -> Res<usize> {
        Ok(self.journal.events()?.len())
    }

    /// `run`: task 1 done, task 2 remediated and done, task 3 asks a
    /// question and stops the run with exit 5.
    fn run_until_the_question(&self) -> Res {
        let scenario = &self.scenario;
        scenario.set_scenario(&format!(
            "{PROBE}{}{PROBE}{}{}{PROBE}{}",
            agent_step(scenario, 1, 1, "work on 1/1", DONE_REPORT),
            agent_step(scenario, 2, 1, "work on 2/1", DONE_REPORT),
            agent_step(scenario, 2, 2, "remediation", DONE_REPORT),
            agent_step(scenario, 3, 1, "work on 3/1", QUESTION_REPORT),
        ))?;
        let run = scenario.run(&["run"])?;
        assert_exit(&run, 5);
        let lines: Vec<String> = stdout_of(&run).lines().map(str::to_string).collect();
        assert_eq!(
            lines,
            [
                format!("task 1: done (commit {})", short(&self.seed)),
                format!("task 2: done (commit {})", short(&self.seed)),
            ],
            "tasks 1 and 2 are reported"
        );
        assert!(
            stderr_of(&run).contains("run: stopped: task 3 needs input"),
            "{}",
            stderr_of(&run)
        );
        Ok(())
    }

    /// `resolve` writes the ADR; the operator commits it. Returns the ADR's
    /// text and the commit that holds it.
    fn resolve_and_commit(&self) -> Res<(String, String)> {
        let resolve = self
            .scenario
            .run(&["resolve", "--task", "3", "--note", "Use SQLite."])?;
        assert_exit(&resolve, 0);
        assert_eq!(
            stdout_of(&resolve).trim(),
            format!("task 3 resolved: {ADR}")
        );
        assert!(
            stderr_of(&resolve).contains("commit the ADR before `ktask-rs resume`"),
            "{}",
            stderr_of(&resolve)
        );
        let adr = std::fs::read_to_string(self.project().join(ADR))?;
        assert!(adr.starts_with("# 0001. Which store\n"), "{adr}");
        assert!(adr.contains("## Decision\n\nUse SQLite.\n"), "{adr}");
        assert!(adr.contains("Postgres") && adr.contains("SQLite"), "{adr}");

        git(self.project(), &["add", "docs/adr"])?;
        git(
            self.project(),
            &[
                "-c",
                "user.name=Operator",
                "-c",
                "user.email=operator@example.com",
                "commit",
                "--quiet",
                "-m",
                "Record the store decision",
            ],
        )?;
        let commit = git(self.project(), &["rev-parse", "HEAD"])?;
        assert_eq!(
            self.origin_tip()?,
            self.seed,
            "the operator's commit is local until a task publishes it"
        );
        Ok((adr, commit))
    }

    /// Task 3 again, on its own so that only task 4 runs with a formatter.
    fn finish_task_3(&self, adr_commit: &str) -> Res {
        self.scenario.set_scenario(&format!(
            "{PROBE}{}",
            agent_step(&self.scenario, 3, 2, "work on 3/2", DONE_REPORT)
        ))?;
        let third = self.scenario.run(&["run", "--task", "3"])?;
        assert_exit(&third, 0);
        assert_eq!(
            stdout_of(&third).trim_end(),
            format!("task 3: done (commit {})", short(adr_commit))
        );
        assert_eq!(
            self.origin_tip()?,
            adr_commit,
            "publishing task 3 put the ADR on the remote"
        );
        Ok(())
    }

    /// `resume` drains the rest of the queue; returns the remote's tip.
    fn drain(&self) -> Res<String> {
        self.write_config(FORMATTER)?;
        self.scenario.set_scenario(&format!(
            "{PROBE}{}",
            agent_step(&self.scenario, 4, 1, "work on 4/1", DONE_REPORT)
        ))?;
        let resume = self.scenario.run(&["resume"])?;
        assert_exit(&resume, 0);
        let tip = self.origin_tip()?;
        assert_eq!(
            stdout_of(&resume).trim_end(),
            format!("task 4: done (commit {})", short(&tip))
        );
        assert!(
            stderr_of(&resume).contains("run: queue drained"),
            "{}",
            stderr_of(&resume)
        );
        let again = self.scenario.run(&["run"])?;
        assert_exit(&again, 0);
        assert_eq!(stdout_of(&again), "", "nothing is left to run");
        Ok(tip)
    }

    /// The journal, event for event, and the state each task ends in.
    fn assert_journal(&self, adr_commit: &str, tip: &str) -> Res {
        assert_eq!(
            journal_rows(&self.journal)?,
            expected_journal(&self.seed, adr_commit, tip),
            "the journal, in full"
        );
        let events = self.journal.events()?;
        let expected_seqs: Vec<u64> = (1..).take(events.len()).collect();
        assert_eq!(
            events
                .iter()
                .map(|event| event.seq.get())
                .collect::<Vec<_>>(),
            expected_seqs,
            "sequence numbers are gapless"
        );

        let mut replayed = Vec::new();
        for task in self.journal.tasks()? {
            replayed.push(replayed_state(&self.journal, task.id)?.name().to_string());
        }
        assert_eq!(replayed, vec!["Done"; TASKS as usize]);
        assert_eq!(
            status_summary(&self.scenario)?,
            serde_json::json!({"Done": TASKS})
        );
        Ok(())
    }

    /// Five attempts ran the gates, the second failing, and each attempt and
    /// the remediation left its evidence.
    fn assert_gates_and_evidence(&self) -> Res {
        assert_eq!(
            std::fs::read_to_string(&self.verify_log)?.lines().count(),
            5,
            "the verify gate is rerun from scratch for every attempt and remediation"
        );
        let attempts = |task: u32| -> io::Result<usize> {
            let dir = self.scenario.state_dir().join("attempts");
            Ok(std::fs::read_dir(dir.join(task.to_string()))?.count())
        };
        assert_eq!(
            (attempts(1)?, attempts(2)?, attempts(3)?, attempts(4)?),
            (1, 2, 2, 1),
            "every attempt and the remediation left evidence"
        );
        Ok(())
    }

    /// The remote holds the seed, the ADR and task 4, and the ADR on it is
    /// the one `resolve` wrote.
    fn assert_remote(&self, adr: &str, adr_commit: &str, tip: &str) -> Res {
        let show = |path: &str| git(&self.origin, &["show", &format!("{tip}:{path}")]);
        assert_eq!(
            show(ADR)?,
            adr.trim_end(),
            "the ADR on the remote is the one `resolve` wrote"
        );
        assert_eq!(
            git(&self.origin, &["log", "--format=%H %s", tip])?,
            format!(
                "{tip} Task 4\n{adr_commit} Record the store decision\n{} seed",
                self.seed
            ),
            "the remote's history is the seed, the ADR, and task 4"
        );
        assert_eq!(
            git(&self.origin, &["rev-parse", &format!("{tip}^")])?,
            adr_commit
        );
        assert_eq!(
            show("SEED.md")?,
            "ktask scratch repo seed\nformatted",
            "task 4's commit carries the formatter's change"
        );
        Ok(())
    }
}

/// What the interface shows while task 3 waits for an answer.
fn assert_midflight_screens(tui: &mut Harness, events: usize) {
    assert_shows(
        &screen(tui, '6'),
        &[
            "Pending decisions: 1",
            "Task 3 asks",
            "Which store?",
            "- Postgres",
            "- SQLite",
            "one scales, one is a file.",
            "durability.",
        ],
    );
    assert_shows(
        &screen(tui, '4'),
        &[
            "verification_failure",
            "task 2",
            "recovered",
            "completion gate Verify failed",
        ],
    );
    assert_shows(
        &screen(tui, '2'),
        &[
            "task 3 · attempt 1 · phase Implement",
            "work on 1/1",
            "work on 2/1",
            "remediation",
            "work on 3/1",
        ],
    );
    assert_shows(
        &screen(tui, '3'),
        &[
            &format!("{events}/{events}"),
            "verify failed (VerificationFailure): completion gate Verify failed",
            "decision raised: Which store?",
        ],
    );
}

/// What the interface shows once the queue has drained.
fn assert_final_screens(tui: &mut Harness, events: usize, tip: &str) {
    assert_shows(
        &screen(tui, '6'),
        &["Pending decisions: 0", "No decisions are waiting."],
    );
    assert_shows(
        &screen(tui, '2'),
        &[
            "task 4 · attempt 1 · phase Verify",
            "ended",
            "work on 3/2",
            "work on 4/1",
        ],
    );
    assert_shows(
        &screen(tui, '3'),
        &[
            &format!("{events}/{events}"),
            "decision resolved: docs/adr/0001-which-store.md",
            &format!("task done: {tip}"),
        ],
    );
    assert_shows(&screen(tui, '4'), &["verification_failure", "recovered"]);
    tui.key('?');
    assert_shows(&tui.text(), &["Key map"]);
}

/// The whole product, from a clean state to a drained queue.
#[test]
fn acceptance_full_run_journals_every_step_and_publishes_the_remote_tip() -> Res {
    let journey = Journey::start()?;

    // 1. `run` stops at task 3's question.
    journey.run_until_the_question()?;

    // 2. The TUI attaches mid-flight, over the same journal.
    let midflight = journey.event_count()?;
    let mut tail = JournalTail::open(&journal_path(journey.scenario.state_dir()))?;
    let mut tui = Harness::from_app(App::new((120, 30)));
    assert_eq!(
        deliver(&mut tail, &mut tui)?,
        midflight,
        "attaching delivers the whole history, once"
    );
    assert_eq!(deliver(&mut tail, &mut tui)?, 0);
    assert_midflight_screens(&mut tui, midflight);
    assert_eq!(
        journey.event_count()?,
        midflight,
        "looking at a run writes nothing to its journal"
    );

    // 3. and 4. Answer, commit the ADR, finish task 3, drain the queue.
    let (adr, adr_commit) = journey.resolve_and_commit()?;
    journey.finish_task_3(&adr_commit)?;
    let tip = journey.drain()?;

    // 5. Everything the journey did, checked.
    journey.assert_journal(&adr_commit, &tip)?;
    journey.assert_gates_and_evidence()?;
    journey.assert_remote(&adr, &adr_commit, &tip)?;

    // The TUI, still attached, follows the rest of the journal.
    let total = journey.event_count()?;
    assert_eq!(
        deliver(&mut tail, &mut tui)?,
        total - midflight,
        "only what is new is delivered"
    );
    assert_eq!(
        tail.last_seq().get(),
        u64::try_from(total)?,
        "the tail has consumed the whole journal"
    );
    assert_final_screens(&mut tui, total, &tip);
    assert_eq!(
        journey.event_count()?,
        total,
        "the TUI never wrote to the journal"
    );
    Ok(())
}
