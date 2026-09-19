//! Decision request parsing from NEEDS_INPUT report bodies.

use crate::{DecisionRequest, Error};

/// Parse a `NEEDS_INPUT` report body into a structured `DecisionRequest`.
///
/// Expects sections in the body:
/// - `Question`: the main question
/// - `Options`: newline-separated list of options
/// - `Trade-offs`: trade-offs explanation
/// - `Impact`: impact statement
/// - `Recommended`: (optional) recommended option
///
/// # Errors
///
/// Returns an error if any required section is missing, with a message naming which sections
/// are missing.
pub fn parse_decision_request(body: &str) -> crate::Result<DecisionRequest> {
    let mut question: Option<String> = None;
    let mut options: Option<Vec<String>> = None;
    let mut tradeoffs: Option<String> = None;
    let mut impact: Option<String> = None;
    let mut recommended: Option<String> = None;

    let mut current_section: Option<&str> = None;
    let mut current_content = String::new();

    for line in body.lines() {
        let trimmed = line.trim();

        // Check if this line starts a new section
        if trimmed.ends_with(':') && trimmed.len() > 1 {
            // Save previous section if any
            if let Some(section) = current_section {
                let content = current_content.trim().to_string();
                match section {
                    "Question" => question = Some(content),
                    "Options" => {
                        options = Some(
                            content
                                .lines()
                                .map(|s| s.trim())
                                .filter(|s| !s.is_empty())
                                .map(|s| {
                                    if s.starts_with("- ") {
                                        s[2..].to_string()
                                    } else if s.starts_with("* ") {
                                        s[2..].to_string()
                                    } else {
                                        s.to_string()
                                    }
                                })
                                .collect(),
                        );
                    }
                    "Trade-offs" => tradeoffs = Some(content),
                    "Impact" => impact = Some(content),
                    "Recommended" => recommended = Some(content),
                    _ => {}
                }
            }

            // Start new section
            let section_name = trimmed.trim_end_matches(':');
            current_section = Some(section_name);
            current_content.clear();
        } else if current_section.is_some() {
            // Accumulate content for current section
            if !current_content.is_empty() {
                current_content.push('\n');
            }
            current_content.push_str(line);
        }
    }

    // Save last section
    if let Some(section) = current_section {
        let content = current_content.trim().to_string();
        match section {
            "Question" => question = Some(content),
            "Options" => {
                options = Some(
                    content
                        .lines()
                        .map(|s| s.trim())
                        .filter(|s| !s.is_empty())
                        .map(|s| {
                            if s.starts_with("- ") {
                                s[2..].to_string()
                            } else if s.starts_with("* ") {
                                s[2..].to_string()
                            } else {
                                s.to_string()
                            }
                        })
                        .collect(),
                );
            }
            "Trade-offs" => tradeoffs = Some(content),
            "Impact" => impact = Some(content),
            "Recommended" => recommended = Some(content),
            _ => {}
        }
    }

    // Check for missing required sections
    let mut missing = Vec::new();
    if question.is_none() {
        missing.push("Question");
    }
    if options.is_none() {
        missing.push("Options");
    }
    if tradeoffs.is_none() {
        missing.push("Trade-offs");
    }
    if impact.is_none() {
        missing.push("Impact");
    }

    if !missing.is_empty() {
        return Err(Error::Deserialize {
            detail: format!(
                "Malformed decision request: missing required sections: {}",
                missing.join(", ")
            ),
        });
    }

    Ok(DecisionRequest {
        question: question.unwrap(),
        options: options.unwrap_or_default(),
        tradeoffs: tradeoffs.unwrap(),
        impact: impact.unwrap(),
        recommended,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_decision_request_with_all_sections() {
        let body = r#"Question:
Should we refactor the auth module?

Options:
- Yes, refactor now
- No, defer to next quarter

Trade-offs:
Refactoring now improves code quality but delays other work.
Deferring spreads effort but risks technical debt.

Impact:
Affects auth system reliability and team velocity.

Recommended:
Yes, refactor now
"#;

        let result = parse_decision_request(body).expect("should parse");
        assert_eq!(result.question, "Should we refactor the auth module?");
        assert_eq!(result.options.len(), 2);
        assert_eq!(result.options[0], "Yes, refactor now");
        assert_eq!(result.options[1], "No, defer to next quarter");
        assert!(result.tradeoffs.contains("Refactoring now"));
        assert!(result.impact.contains("reliability"));
        assert_eq!(result.recommended, Some("Yes, refactor now".to_string()));
    }

    #[test]
    fn parse_decision_request_without_recommended() {
        let body = r#"Question:
Should we use Rust?

Options:
- Yes
- No

Trade-offs:
Rust is safer but has a steep learning curve.

Impact:
Affects codebase quality and team productivity.
"#;

        let result = parse_decision_request(body).expect("should parse");
        assert_eq!(result.question, "Should we use Rust?");
        assert_eq!(result.options.len(), 2);
        assert!(result.tradeoffs.contains("safer"));
        assert_eq!(result.recommended, None);
    }

    #[test]
    fn parse_decision_request_missing_question() {
        let body = r#"Options:
- A
- B

Trade-offs:
Some trade-offs.

Impact:
Some impact.
"#;

        let err = parse_decision_request(body).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Question"));
        assert!(msg.contains("missing required sections"));
    }

    #[test]
    fn parse_decision_request_missing_options() {
        let body = r#"Question:
Should we do X?

Trade-offs:
Some trade-offs.

Impact:
Some impact.
"#;

        let err = parse_decision_request(body).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Options"));
    }

    #[test]
    fn parse_decision_request_missing_tradeoffs() {
        let body = r#"Question:
Should we do X?

Options:
- Yes
- No

Impact:
Some impact.
"#;

        let err = parse_decision_request(body).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Trade-offs"));
    }

    #[test]
    fn parse_decision_request_missing_impact() {
        let body = r#"Question:
Should we do X?

Options:
- Yes
- No

Trade-offs:
Some trade-offs.
"#;

        let err = parse_decision_request(body).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Impact"));
    }

    #[test]
    fn parse_decision_request_multiple_missing_sections() {
        let body = r#"Question:
Should we do X?

Trade-offs:
Some trade-offs.
"#;

        let err = parse_decision_request(body).expect_err("should error");
        let msg = err.to_string();
        assert!(msg.contains("Options"));
        assert!(msg.contains("Impact"));
    }

    #[test]
    fn parse_decision_request_empty_options_filtered() {
        let body = r#"Question:
Should we do X?

Options:
- Yes

- No

Trade-offs:
Some trade-offs.

Impact:
Some impact.
"#;

        let result = parse_decision_request(body).expect("should parse");
        assert_eq!(result.options.len(), 2);
        assert_eq!(result.options[0], "Yes");
        assert_eq!(result.options[1], "No");
    }

    #[test]
    fn parse_decision_request_multiline_content() {
        let body = r#"Question:
This is a multi-line
question spanning
multiple lines.

Options:
- First option
- Second option
- Third option

Trade-offs:
First paragraph about trade-offs.

Second paragraph about trade-offs.

Impact:
First paragraph about impact.

Second paragraph about impact.
"#;

        let result = parse_decision_request(body).expect("should parse");
        assert!(result.question.contains("multi-line"));
        assert!(result.tradeoffs.contains("First paragraph"));
        assert!(result.tradeoffs.contains("Second paragraph"));
        assert_eq!(result.options.len(), 3);
    }

    #[test]
    fn parse_decision_request_roundtrips_through_json() {
        let request = DecisionRequest {
            question: "Test question".to_string(),
            options: vec!["Option A".to_string(), "Option B".to_string()],
            tradeoffs: "Test tradeoffs".to_string(),
            impact: "Test impact".to_string(),
            recommended: Some("Option A".to_string()),
        };

        let json = serde_json::to_string(&request).expect("serialize");
        let deserialized: DecisionRequest = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(request, deserialized);
    }
}
