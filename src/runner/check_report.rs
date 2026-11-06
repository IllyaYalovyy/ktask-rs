//! Check-owned structured results supplement, never override, process status.
use super::test_summary;
use serde::{Deserialize, Serialize};
const PREFIX: &str = "KTASK_CHECK_RESULT ";
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Suite {
    pub name: String,
    pub status: Status,
    pub passed: Option<u64>,
    pub failed: Option<u64>,
    pub skipped: Option<u64>,
    pub evidence: Option<String>,
}
#[derive(Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(super) enum Status {
    Passed,
    Failed,
    Crashed,
    Incomplete,
    Unknown,
}
#[derive(Debug, Serialize)]
pub(super) struct Report {
    pub suites: Vec<Suite>,
    pub errors: Vec<String>,
}
impl Report {
    pub fn blocks(&self) -> bool {
        !self.errors.is_empty()
            || self
                .suites
                .iter()
                .any(|s| s.status != Status::Passed || s.failed.is_some_and(|n| n > 0))
    }
}
pub(super) fn parse(text: &str) -> Report {
    let mut report = Report {
        suites: Vec::new(),
        errors: Vec::new(),
    };
    let mut game: Option<Suite> = None;
    let lines: Vec<_> = text.lines().map(test_summary::plain).collect();
    for (index, line) in lines.iter().enumerate() {
        let plain = test_summary::plain(line);
        if let Some(payload) = plain.strip_prefix(PREFIX) {
            match serde_json::from_str::<Suite>(payload) {
                Ok(suite) if !suite.name.trim().is_empty() => report.suites.push(suite),
                _ => report.errors.push(format!(
                    "Invalid structured suite result at line {}",
                    index + 1
                )),
            }
            continue;
        }
        if plain.contains("[minecraft/GameTestServer]") {
            let starts = plain.contains("Started game test server")
                || plain.contains("tests are now running at position");
            if starts
                && game.as_ref().is_some_and(|s| {
                    plain.contains("Started game test server") || s.status != Status::Incomplete
                })
                && let Some(previous) = game.take()
            {
                report.suites.push(previous);
            }
            let suite = game.get_or_insert_with(|| Suite {
                name: "Minecraft GameTests".into(),
                status: Status::Incomplete,
                passed: None,
                failed: None,
                skipped: None,
                evidence: Some(format!("transcript line {}", index + 1)),
            });
            if plain.contains("Game test server crashed") {
                suite.status = Status::Crashed;
            } else if plain.contains(" required tests failed") {
                if suite.status != Status::Crashed {
                    suite.status = Status::Failed;
                }
                suite.evidence = Some(format!("transcript line {}", index + 1));
            } else if !matches!(suite.status, Status::Crashed | Status::Failed)
                && let Some((_, tail)) = plain.split_once("All ")
                && let Some((count, _)) = tail.split_once(" required tests passed")
                && let Ok(n) = count.parse::<u64>()
            {
                suite.status = Status::Passed;
                suite.passed = Some(n);
                suite.failed = Some(0);
                suite.evidence = Some(format!("transcript line {}", index + 1));
            }
        }
        // Framework summary counts are separate from GameTests, never their completion proof.
        if let Some(counts) = test_summary::summary(&plain, &lines[index + 1..]) {
            report.suites.push(Suite {
                name: format!("framework summary at line {}", index + 1),
                status: if counts.failed > 0 {
                    Status::Failed
                } else {
                    Status::Passed
                },
                passed: Some(counts.passed as u64),
                failed: Some(counts.failed as u64),
                skipped: Some(counts.ignored as u64),
                evidence: Some(format!("transcript line {}", index + 1)),
            });
        }
    }
    if let Some(game) = game {
        report.suites.push(game);
    }
    report
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn passing_python_cannot_hide_game_server_crash() {
        let r = parse(
            "Ran 36 tests in 1s\n\nOK\n[12] [Server thread/ERROR] [minecraft/GameTestServer]: Game test server crashed\n",
        );
        assert!(r.blocks());
        assert_eq!(r.suites.len(), 2);
        assert_eq!(r.suites[0].passed, Some(36));
        assert_eq!(r.suites[1].status, Status::Crashed);
    }
    #[test]
    fn later_game_run_cannot_inherit_earlier_success() {
        let passed = "[minecraft/GameTestServer]: All 5 required tests passed :)\n";
        for later in [
            "Started game test server",
            "5 tests are now running at position 0,0,0!",
            "1 required tests failed :(",
        ] {
            let report = parse(&format!("{passed}[minecraft/GameTestServer]: {later}\n"));
            assert!(report.blocks(), "{later}: {report:?}");
        }
        assert!(parse("[minecraft/GameTestServer]: Started game test server\n[minecraft/GameTestServer]: Started game test server\n[minecraft/GameTestServer]: All 5 required tests passed :)\n").blocks());
        let report = parse(&format!(
            "{passed}[minecraft/GameTestServer]: Started game test server\n{passed}"
        ));
        assert!(!report.blocks());
        assert_eq!(report.suites.len(), 2);
        assert!(
            parse(&format!(
                "[minecraft/GameTestServer]: 1 required tests failed :(\n{passed}"
            ))
            .blocks()
        );
    }
    #[test]
    fn structured_unknown_malformed_and_failed_results_do_not_pass() {
        for state in ["failed", "crashed", "incomplete", "unknown"] {
            let r = parse(&format!(
                "KTASK_CHECK_RESULT {{\"name\":\"suite\",\"status\":\"{state}\"}}"
            ));
            assert!(r.blocks());
        }
        assert!(parse("KTASK_CHECK_RESULT {bad}").blocks());
        assert!(!parse("KTASK_CHECK_RESULT {\"name\":\"suite\",\"status\":\"passed\",\"passed\":5,\"failed\":0}").blocks());
        assert!(parse("[minecraft/GameTestServer]: starting").blocks());
        assert!(!parse("[minecraft/GameTestServer]: All 655 required tests passed :)").blocks());
    }
}
