//! Specialist review: one independent, read-only review turn per selected lens, all
//! finished before the supervisor asks for any repair.
//!
//! Every lens reads the same review scope and evidence in a session of its own. A finished
//! review is kept with the source state it judged, so pausing and resuming never repeats
//! a lens for an unchanged tree. The reviews are aggregated into the one outcome the
//! ordinary review would have produced; the repair budget and the failure judge are the
//! attempt's own, never the lens's.
use super::{Context, Outcome, agent, turn::ai_lens};
use crate::{
    application::{
        execution::{Run, Step},
        lenses::{Finding, Lens, Review},
    },
    storage::Result,
};
use serde_json::json;

/// One lens review of one source state.
pub(super) struct LensTurn<'a> {
    /// Position of the lens in the revision's selection, from 1.
    pub ordinal: i64,
    pub lens: &'a Lens,
    pub tree: &'a str,
    /// Repair rounds already spent.
    pub round: i64,
}

impl LensTurn<'_> {
    /// The record of the turn's decided assessment: its findings, with an unmet criterion
    /// or a failed verdict that names none made explicit as a critical finding.
    pub(super) fn review(&self, step: &Step, assessment: &agent::Assessment) -> Review {
        let mut findings: Vec<Finding> = assessment
            .findings
            .iter()
            .map(|f| Finding {
                critical: f.severity == agent::Severity::Critical,
                description: f.description.clone(),
            })
            .collect();
        for criterion in assessment.criteria.iter().filter(|c| !c.passed) {
            findings.push(Finding {
                critical: true,
                description: format!(
                    "Criterion {} not met: {}",
                    criterion.ordinal, criterion.evidence
                ),
            });
        }
        if assessment.outcome == "failed" && !findings.iter().any(|f| f.critical) {
            findings.push(Finding {
                critical: true,
                description: assessment.summary.clone(),
            });
        }
        Review {
            ordinal: self.ordinal,
            lens: self.lens.name.clone(),
            step: step.id,
            round: self.round,
            critical: findings.iter().any(|f| f.critical),
            summary: assessment.summary.clone(),
            findings,
        }
    }
}

/// Runs every selected lens against `tree` that has no recorded review yet, then
/// aggregates all of them. Anything but a decided review (a pause, a limit wait, a
/// failure) ends the phase at once; the finished lenses are kept for the resume.
pub(super) fn review(
    service: &mut crate::application::Service,
    run: &Run,
    ctx: &mut Context<'_>,
    evidence: &str,
    tree: &str,
    round: i64,
) -> Result<Outcome> {
    let mut reviews = Vec::new();
    for (index, lens) in run.lenses.iter().enumerate() {
        let ordinal = index as i64 + 1;
        if let Some(done) = service.lens_review(run, round, tree, ordinal)? {
            reviews.push(done);
            continue;
        }
        if ctx.interrupted(service, run)? {
            return Ok(Outcome::Paused);
        }
        let turn = LensTurn {
            ordinal,
            lens,
            tree,
            round,
        };
        match ai_lens(service, run, ctx, evidence, &turn)? {
            Outcome::Passed(_) | Outcome::Changes { .. } => {}
            other => return Ok(other),
        }
        reviews.push(
            service
                .lens_review(run, round, tree, ordinal)?
                .expect("a decided lens review is recorded by its turn"),
        );
    }
    Ok(aggregate(run, &reviews))
}

/// The one outcome of all lens reviews of a source state: changes requested when any lens
/// found a critical problem, otherwise a pass that keeps every minor finding.
fn aggregate(run: &Run, reviews: &[Review]) -> Outcome {
    let critical: Vec<&Review> = reviews.iter().filter(|r| r.critical).collect();
    let count = |critical: bool| {
        reviews
            .iter()
            .flat_map(|r| &r.findings)
            .filter(|f| f.critical == critical)
            .count()
    };
    let names = |list: &[&Review]| {
        list.iter()
            .map(|r| r.lens.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let all: Vec<&Review> = reviews.iter().collect();
    let headline = if critical.is_empty() {
        format!(
            "Specialist reviews ({}) found no critical problem; {} minor finding(s)",
            names(&all),
            count(false)
        )
    } else {
        format!(
            "Specialist reviews found {} critical finding(s): {}",
            count(true),
            names(&critical)
        )
    };
    let per_lens: String = reviews
        .iter()
        .map(|r| {
            format!(
                "\n{} lens: {}",
                r.lens,
                r.summary.lines().next().unwrap_or_default()
            )
        })
        .collect();
    let findings: Vec<_> = reviews
        .iter()
        .flat_map(|r| {
            r.findings.iter().map(|f| {
                json!({
                    "severity": if f.critical { "critical" } else { "minor" },
                    "lens": r.lens,
                    "description": format!("[{} lens] {}", r.lens, f.description),
                })
            })
        })
        .collect();
    let failed = !critical.is_empty();
    let report = json!({
        "task_id": run.task, "attempt_id": run.attempt, "role": "review",
        "lenses": reviews.iter().map(|r| r.lens.as_str()).collect::<Vec<_>>(),
        "assessment": {
            "outcome": if failed { "changes_requested" } else { "complete" },
            "summary": format!("{headline}{per_lens}"),
            "criteria": [],
            "findings": findings,
        },
    })
    .to_string();
    if failed {
        Outcome::Changes {
            report,
            critical: true,
        }
    } else {
        Outcome::Passed(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{Definition, Kind, categories::Scope};

    fn assessment(
        outcome: &str,
        findings: &[(agent::Severity, &str)],
        passed: bool,
    ) -> agent::Assessment {
        agent::Assessment {
            outcome: outcome.into(),
            summary: "Summary line\nmore".into(),
            question: None,
            criteria: vec![agent::Evidence {
                ordinal: 1,
                passed,
                evidence: "proof".into(),
            }],
            findings: findings
                .iter()
                .map(|(severity, text)| agent::Finding {
                    severity: match severity {
                        agent::Severity::Critical => agent::Severity::Critical,
                        agent::Severity::Minor => agent::Severity::Minor,
                    },
                    description: (*text).into(),
                })
                .collect(),
        }
    }

    fn turn(lens: &Lens) -> LensTurn<'_> {
        LensTurn {
            ordinal: 2,
            lens,
            tree: "t",
            round: 3,
        }
    }

    fn lens() -> Lens {
        Lens {
            name: "security".into(),
            description: String::new(),
            instructions: "x".into(),
            scope: Scope::User,
        }
    }

    fn run() -> Run {
        Run {
            attempt: 7,
            task: 4,
            revision: 1,
            token: "t".into(),
            definition: Definition {
                title: "T".into(),
                goal: "G".into(),
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["C".into()],
                models: [None, None, None, None],
                attempt_limit: None,
                timeout_ms: None,
                stage_timeout_ms: None,
            },
            category: None,
            lenses: Vec::new(),
            active_ms: 0,
        }
    }

    fn step() -> Step {
        Step {
            id: 9,
            session: Some(1),
            provider_session: None,
        }
    }

    #[test]
    fn an_unmet_criterion_or_a_failed_verdict_is_a_critical_finding_of_its_lens() {
        let lens = lens();
        let review = turn(&lens).review(
            &step(),
            &assessment(
                "changes_requested",
                &[(agent::Severity::Minor, "polish")],
                false,
            ),
        );
        assert!(review.critical);
        assert_eq!((review.ordinal, review.round, review.step), (2, 3, 9));
        assert_eq!(review.findings.len(), 2);
        assert!(!review.findings[0].critical);
        assert!(review.findings[1].critical);
        assert!(
            review.findings[1]
                .description
                .starts_with("Criterion 1 not met: proof")
        );
        let failed = turn(&lens).review(&step(), &assessment("failed", &[], true));
        assert!(failed.critical && failed.findings[0].description.starts_with("Summary line"));
        let minor = turn(&lens).review(
            &step(),
            &assessment("complete", &[(agent::Severity::Minor, "polish")], true),
        );
        assert!(!minor.critical);
    }

    fn review(lens: &str, critical: bool, text: &str) -> Review {
        Review {
            ordinal: 1,
            lens: lens.into(),
            step: 1,
            round: 0,
            critical,
            summary: format!("{lens} summary\nsecond line"),
            findings: vec![Finding {
                critical,
                description: text.into(),
            }],
        }
    }

    #[test]
    fn one_critical_lens_requests_changes_and_every_finding_keeps_its_lens() {
        let Outcome::Changes { report, critical } = aggregate(
            &run(),
            &[
                review("concurrency", true, "race at a.rs:1"),
                review("security", true, "injection at b.rs:2"),
                review("simplicity", false, "polish"),
            ],
        ) else {
            panic!("critical lenses request changes");
        };
        assert!(critical);
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        let assessment = &value["assessment"];
        let summary = assessment["summary"].as_str().unwrap();
        assert_eq!(
            summary.lines().next().unwrap(),
            "Specialist reviews found 2 critical finding(s): concurrency, security"
        );
        assert!(summary.contains("\nsimplicity lens: simplicity summary"));
        let findings: Vec<_> = assessment["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["lens"].as_str().unwrap(),
                    f["severity"].as_str().unwrap(),
                    f["description"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            findings,
            [
                (
                    "concurrency",
                    "critical",
                    "[concurrency lens] race at a.rs:1"
                ),
                (
                    "security",
                    "critical",
                    "[security lens] injection at b.rs:2"
                ),
                ("simplicity", "minor", "[simplicity lens] polish"),
            ]
        );
    }

    #[test]
    fn only_minor_findings_pass_and_are_kept() {
        let Outcome::Passed(report) = aggregate(&run(), &[review("security", false, "polish")])
        else {
            panic!("minor findings pass");
        };
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(value["assessment"]["outcome"], "complete");
        assert!(
            value["assessment"]["summary"]
                .as_str()
                .unwrap()
                .starts_with("Specialist reviews (security) found no critical problem; 1 minor")
        );
        assert_eq!(value["assessment"]["findings"][0]["severity"], "minor");
    }
}
