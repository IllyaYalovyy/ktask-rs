//! `DecisionRequest`: the structured question behind `waiting_input`
//! (`VISION.md` §6, invariant 8): "the agent (or a gate) surfaces a
//! structured decision request (question, options, trade-offs, impact); the
//! queue pauses; `ktask-rs resolve` records the answer as an ADR."
//!
//! [`parse_decision_request`] turns a `KTASK_RESULT: NEEDS_INPUT` report's
//! body into that structure. The report names its sections `Question:`,
//! `Options:`, `Trade-offs:`, `Impact:` and, optionally, `Recommended:`,
//! matching `AGENTS.md`'s reporting contract. A section starts at a line
//! whose trimmed text opens with one of those labels and runs until the
//! next recognized label or the end of the body — the same convention
//! [`crate::task::validate`] uses for a task's own required sections.
//!
//! `Question`, `Options`, `Trade-offs` and `Impact` are mandatory: a report
//! that claims `NEEDS_INPUT` without a real question is a malformed report,
//! not a pause a human can act on, per this crate's reporting contract.

use crate::{Error, Result};
use serde::{Deserialize, Serialize};

/// The labels recognized as decision-request sections, in the order they
/// are expected to appear.
const LABELS: [&str; 5] = ["Question", "Options", "Trade-offs", "Impact", "Recommended"];

/// The labels whose section must be present and non-empty.
const REQUIRED_LABELS: [&str; 4] = ["Question", "Options", "Trade-offs", "Impact"];

/// A structured decision request: what a `NEEDS_INPUT` report asks a human
/// to resolve, per `docs/DESIGN.md`'s event catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// The question that needs a human answer.
    pub question: String,
    /// The candidate answers under consideration.
    pub options: Vec<String>,
    /// What each option costs or risks relative to the others.
    pub tradeoffs: String,
    /// What the decision affects.
    pub impact: String,
    /// The agent's suggested answer, if it named one.
    pub recommended: Option<String>,
}

/// Parses a `NEEDS_INPUT` report's body (everything after the
/// `KTASK_RESULT: NEEDS_INPUT` header line) into a [`DecisionRequest`].
///
/// # Errors
///
/// Returns [`Error::Report`] naming every missing or empty required section
/// — `Question`, `Options`, `Trade-offs`, `Impact` — found in `body`.
pub fn parse_decision_request(body: &str) -> Result<DecisionRequest> {
    let sections = extract_sections(body);
    let content = |label: &str| {
        sections
            .iter()
            .find(|(found, _)| *found == label)
            .map(|(_, content)| content.as_str())
    };

    let options = content("Options").map(parse_options).unwrap_or_default();

    let missing: Vec<&str> = REQUIRED_LABELS
        .into_iter()
        .filter(|label| {
            if *label == "Options" {
                options.is_empty()
            } else {
                content(label).is_none_or(|value| value.trim().is_empty())
            }
        })
        .collect();

    if !missing.is_empty() {
        return Err(Error::Report {
            detail: format!(
                "decision request missing required section(s): {}",
                missing.join(", ")
            ),
        });
    }

    let recommended = content("Recommended")
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    Ok(DecisionRequest {
        question: content("Question").unwrap_or_default().trim().to_string(),
        options,
        tradeoffs: content("Trade-offs").unwrap_or_default().trim().to_string(),
        impact: content("Impact").unwrap_or_default().trim().to_string(),
        recommended,
    })
}

/// Splits `body` into `(label, content)` pairs at recognized [`LABELS`].
///
/// A section starts at a line whose trimmed text opens with `Label:` and
/// runs until the next recognized label or the end of `body`, mirroring
/// [`crate::task`]'s `extract_sections` for `**Label:**` task sections.
fn extract_sections(body: &str) -> Vec<(&'static str, String)> {
    let mut sections: Vec<(&'static str, Vec<&str>)> = Vec::new();

    for line in body.lines() {
        if let Some((label, rest)) = label_prefix(line) {
            sections.push((label, vec![rest]));
        } else if let Some((_, content)) = sections.last_mut() {
            content.push(line);
        }
    }

    sections
        .into_iter()
        .map(|(label, content)| (label, content.join("\n").trim().to_string()))
        .collect()
}

/// Recognizes a line opening with one of [`LABELS`], such as `Question: is
/// this correct?`. The label must sit at the very start of the (trimmed)
/// line, so a colon-terminated phrase mid-sentence is never mistaken for a
/// section.
fn label_prefix(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim_start();
    LABELS.into_iter().find_map(|label| {
        let rest = trimmed.strip_prefix(label)?.strip_prefix(':')?;
        Some((label, rest.trim_start()))
    })
}

/// Splits an `Options:` section's content into individual options, one per
/// line, with an optional leading `-` or `*` bullet stripped.
fn parse_options(content: &str) -> Vec<String> {
    content
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            trimmed
                .strip_prefix('-')
                .or_else(|| trimmed.strip_prefix('*'))
                .map_or(trimmed, str::trim_start)
                .to_string()
        })
        .filter(|option| !option.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_body() -> &'static str {
        "Question: Postgres or SQLite for the journal?\n\
         Options:\n\
         - Postgres\n\
         - SQLite\n\
         Trade-offs: Postgres scales better; SQLite is simpler to run.\n\
         Impact: Journal durability and operational overhead.\n\
         Recommended: SQLite"
    }

    #[test]
    fn every_field_round_trips_from_a_well_formed_body() {
        let request = parse_decision_request(full_body()).expect("well-formed body parses");
        assert_eq!(
            request,
            DecisionRequest {
                question: "Postgres or SQLite for the journal?".to_string(),
                options: vec!["Postgres".to_string(), "SQLite".to_string()],
                tradeoffs: "Postgres scales better; SQLite is simpler to run.".to_string(),
                impact: "Journal durability and operational overhead.".to_string(),
                recommended: Some("SQLite".to_string()),
            }
        );
    }

    #[test]
    fn recommended_is_none_when_the_section_is_absent() {
        let body = "Question: q?\nOptions:\n- a\n- b\nTrade-offs: t\nImpact: i";
        let request = parse_decision_request(body).expect("no Recommended section is legal");
        assert_eq!(request.recommended, None);
    }

    #[test]
    fn recommended_is_none_when_the_section_is_present_but_empty() {
        let body = "Question: q?\nOptions:\n- a\nTrade-offs: t\nImpact: i\nRecommended:\n";
        let request = parse_decision_request(body).expect("empty Recommended is still legal");
        assert_eq!(request.recommended, None);
    }

    #[test]
    fn a_body_with_no_question_is_rejected_naming_the_missing_section() {
        let body = "Options:\n- a\n- b\nTrade-offs: t\nImpact: i";
        let err = parse_decision_request(body).expect_err("missing Question is malformed");
        match err {
            Error::Report { detail } => assert!(
                detail.contains("Question"),
                "detail should name Question: {detail}"
            ),
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn a_body_with_an_empty_question_is_rejected() {
        let body = "Question:\nOptions:\n- a\nTrade-offs: t\nImpact: i";
        let err = parse_decision_request(body).expect_err("empty Question is malformed");
        match err {
            Error::Report { detail } => assert!(detail.contains("Question")),
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn a_body_with_no_options_is_rejected_naming_the_missing_section() {
        let body = "Question: q?\nTrade-offs: t\nImpact: i";
        let err = parse_decision_request(body).expect_err("missing Options is malformed");
        match err {
            Error::Report { detail } => assert!(
                detail.contains("Options"),
                "detail should name Options: {detail}"
            ),
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn a_body_missing_every_required_section_names_all_of_them() {
        let err = parse_decision_request("").expect_err("empty body is malformed");
        match err {
            Error::Report { detail } => {
                assert!(detail.contains("Question"));
                assert!(detail.contains("Options"));
                assert!(detail.contains("Trade-offs"));
                assert!(detail.contains("Impact"));
            }
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn a_body_with_no_trade_offs_is_rejected() {
        let body = "Question: q?\nOptions:\n- a\nImpact: i";
        let err = parse_decision_request(body).expect_err("missing Trade-offs is malformed");
        match err {
            Error::Report { detail } => assert!(detail.contains("Trade-offs")),
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn a_body_with_no_impact_is_rejected() {
        let body = "Question: q?\nOptions:\n- a\nTrade-offs: t";
        let err = parse_decision_request(body).expect_err("missing Impact is malformed");
        match err {
            Error::Report { detail } => assert!(detail.contains("Impact")),
            other => panic!("expected Error::Report, got {other:?}"),
        }
    }

    #[test]
    fn options_accepts_a_star_bullet_and_a_plain_line_alike() {
        let body = "Question: q?\nOptions:\n* star option\nplain option\n- dash option\nTrade-offs: t\nImpact: i";
        let request = parse_decision_request(body).expect("mixed bullet styles parse");
        assert_eq!(
            request.options,
            vec![
                "star option".to_string(),
                "plain option".to_string(),
                "dash option".to_string(),
            ]
        );
    }

    #[test]
    fn a_multiline_section_is_joined_and_trimmed() {
        let body = "Question: q?\nOptions:\n- a\nTrade-offs: first line\nsecond line\nImpact: i";
        let request = parse_decision_request(body).expect("multiline trade-offs parses");
        assert_eq!(request.tradeoffs, "first line\nsecond line");
    }

    #[test]
    fn a_label_like_phrase_mid_sentence_does_not_open_a_new_section() {
        let body = "Question: what about Impact: on cost?\nOptions:\n- a\nTrade-offs: t\nImpact: i";
        let request = parse_decision_request(body).expect("mid-line label is not a section");
        assert_eq!(request.question, "what about Impact: on cost?");
    }
}
