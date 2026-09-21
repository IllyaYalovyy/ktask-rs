//! The question a `NEEDS_INPUT` report asks, read as the decision it is.
//!
//! VISION.md §3's invariant 8 makes an unresolved product or technical decision
//! a first-class pause state rather than something an agent guesses its way
//! past, and §6 says what the pause carries: "a structured decision request
//! (question, options, trade-offs, impact)". [`DecisionRequest`] is that
//! sentence given a type, and it is what the `DecisionRaised` journal record
//! that opens the pause holds — so the ask a human answers with
//! `ktask-rs resolve` is the ask the agent wrote, field for field, and not a
//! paraphrase of it.
//!
//! A pause with no question in it is worth nothing, which is what the two
//! readings below are shaped around. A report that claims `NEEDS_INPUT` and
//! never says what is being asked is a *malformed* report, refused with a
//! message naming the section that is missing; it is not a pause, because a
//! human handed an empty inbox has nothing to resolve and no way to say so. And
//! a report that claims something else asks for nothing: a `DONE` whose prose
//! happens to contain the word `Question:` must not stop a queue.
//!
//! The header itself is [`crate::parse_report`]'s business and is read by it, so
//! the three spellings and the strictness behind them are stated once, in
//! ADR-0076. What this module adds is the grammar of the body under that header,
//! which no prompt template prescribes yet — ADR-0079 records why these five
//! labels, why four of them are required, and why the rule differs from the
//! `**Label:**` sections [`crate::task`] reads out of a task block.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::event::EventKind;
use crate::report::body_after_header;
use crate::task::missing_phrase;
use crate::{ReportResult, parse_report};

/// The five sections a report's body is read for, in the order the format names
/// them.
///
/// One table for the labels the parser opens, the message refuses with, and a
/// report has to write, so a refusal cannot name a label the parser would not
/// have read.
const SECTIONS: [&str; 5] = ["Question", "Options", "Trade-offs", "Impact", "Recommended"];

/// The [`SECTIONS`] a request cannot be read out of a body without, each with
/// the test that says the field it fills was actually filled.
///
/// Four of the five, in the order they are written. The [`DecisionRequest`]
/// fields those four fill are neither `Option` nor empty-able: a decision a
/// human can actually answer states what is asked, what the choices are, what
/// they cost, and what answering one does to the work after this task.
/// `Recommended` is the fifth, and is absent on purpose — an agent at a genuine
/// fork is not obliged to have picked an answer already.
///
/// One table for the label a refusal names and the field that earned it, so a
/// message cannot complain about a section whose text was in fact read: the
/// test asks the field rather than the raw section, because `Options` written as
/// a bullet with nothing after the `-` names no choice even though its text is
/// not empty.
/// Whether the field one section fills came out of the body with something in it.
type Filled = fn(&DecisionRequest) -> bool;

const REQUIRED: [(&str, Filled); 4] = [
    ("Question", |request| !request.question.is_empty()),
    ("Options", |request| !request.options.is_empty()),
    ("Trade-offs", |request| !request.tradeoffs.is_empty()),
    ("Impact", |request| !request.impact.is_empty()),
];

/// The whitespace a section's text is trimmed of at both ends.
///
/// Carriage return is one of them because a report written on a host that ends
/// its lines with `\r\n` says nothing different from one that does not.
const SECTION_PADDING: [char; 4] = ['\n', '\r', ' ', '\t'];

/// The list markers an option may be written with, and which are not part of it.
const BULLETS: [char; 3] = ['-', '*', '+'];

/// The characters that end a number used to mark an option: `1.` and `1)` are
/// the two an agent writes, and both say how the list was written rather than
/// what was chosen.
const NUMBER_ENDS: [char; 2] = ['.', ')'];

/// What an agent is asking a human to decide, in the parts a decision is made
/// from.
///
/// The five fields are the ones `docs/DESIGN.md` fixes for the `DecisionRaised`
/// payload, spelled as it spells them, because this is durable data: the struct
/// is what the journal's `payload` column holds inside a `DecisionRaised` record
/// and what `--json` output prints, so a renamed field silently changes every
/// journal written before it.
///
/// It is the *ask*, never the answer. The answer is a `DecisionResolved` record
/// plus an ADR under `docs/adr/`, which is what later tasks are handed as
/// context (VISION.md §6); nothing here is written by a human, and nothing here
/// decides anything on one's behalf.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRequest {
    /// What is being asked, in the words the agent wrote it in.
    pub question: String,
    /// The choices the agent saw, in the order it wrote them, and never empty:
    /// a decision with no named choice in it is a statement, not a fork.
    pub options: Vec<String>,
    /// What each choice costs, as the agent judged it. Read by the human, and
    /// never re-derived from the diff afterwards.
    pub tradeoffs: String,
    /// What answering changes for the work after this task — the queue, the
    /// schema, the interface. The part an inbox leads with, because it is what
    /// makes a decision about one task a decision about several.
    pub impact: String,
    /// The choice the agent would make, if it named one. `None` is the absence
    /// of a recommendation, not a recommendation to decide nothing.
    pub recommended: Option<String>,
}

/// The decision a report asks for, when its header claims `NEEDS_INPUT`.
///
/// [`None`] means the report asked for nothing: its header claims `DONE` or
/// `FAILED`, so there is no question and nothing written below it stops a queue.
///
/// # Errors
///
/// [`Error::Corrupt`] when the header cannot be read — which is
/// [`crate::parse_report`]'s refusal, quoted unchanged — or when the header
/// claims `NEEDS_INPUT` and the body is short of a required section. The message
/// names every missing section, in the order the format names them.
pub fn decision_request(text: &str) -> Result<Option<DecisionRequest>> {
    if parse_report(text)? != ReportResult::NeedsInput {
        return Ok(None);
    }
    let sections = sections_in(body_after_header(text));
    let request = DecisionRequest {
        question: text_of(&sections, "Question"),
        options: options_of(&text_of(&sections, "Options")),
        tradeoffs: text_of(&sections, "Trade-offs"),
        impact: text_of(&sections, "Impact"),
        recommended: answered(text_of(&sections, "Recommended")),
    };
    let missing = short_of(&request);
    if !missing.is_empty() {
        return Err(refused(&missing));
    }
    Ok(Some(request))
}

/// The journal record a report asks for, when its header claims `NEEDS_INPUT`.
///
/// [`decision_request`] reads the ask; this puts it in the catalog entry that
/// opens the wait, so a runner has one call for the three answers a report can
/// give: the record to journal, nothing at all because the report claimed `DONE`
/// or `FAILED`, or a refusal of the pause it claimed without asking. Journaling
/// what comes back parks the attempt that wrote it ([`crate::apply`], and
/// ADR-0079 for why the record is the question rather than a note about it).
///
/// # Errors
///
/// Whatever [`decision_request`] refuses: the report's unreadable header, or a
/// `NEEDS_INPUT` whose body is short of a required section.
pub fn decision_event(text: &str) -> Result<Option<EventKind>> {
    Ok(decision_request(text)?.map(|request| EventKind::DecisionRaised { request }))
}

/// The required sections a body did not fill, in the order the format names them.
///
/// A label written with nothing under it fills nothing, which is the same
/// shortage as never writing the label at all: either way the human asked to
/// decide is missing one of the four parts of an answer.
fn short_of(request: &DecisionRequest) -> Vec<&'static str> {
    REQUIRED
        .iter()
        .filter_map(|(label, was_filled)| (!was_filled(request)).then_some(*label))
        .collect()
}

/// The section a line opens, with the text written on the label's own line.
///
/// The label has to be the whole start of the line, colon and all: `Impact of
/// either choice:` is prose *about* a section rather than the section, so the
/// section stays missing and the refusal says so. A label in another spelling
/// (`question:`, `Tradeoffs:`, `Recommend:`) opens nothing either, because the
/// format is five words a report has to write and a near miss cannot be guessed
/// at without reading one section's text into another field.
fn opens_section(line: &str) -> Option<(&'static str, &str)> {
    let trimmed = line.trim();
    SECTIONS.into_iter().find_map(|label| {
        let after_label = trimmed.strip_prefix(label)?.strip_prefix(':')?;
        Some((label, after_label.trim()))
    })
}

/// The text each section of a report's body holds, keyed by its label.
///
/// One pass, because a section runs to the next label and nothing else decides
/// where it ends: text below the first label belongs to the section it sits
/// under, and prose above that first label belongs to none of them. A label
/// written twice keeps the text written under it first and drops the second
/// copy, as a task block's repeated section does in [`mod@crate::task`] — the
/// alternative is to join two answers into one field.
fn sections_in(body: &str) -> BTreeMap<&'static str, String> {
    let mut held: BTreeMap<&'static str, Vec<&str>> = BTreeMap::new();
    let mut open: Option<&'static str> = None;
    for line in body.lines() {
        match opens_section(line) {
            Some((label, on_the_label_line)) => {
                open = if held.contains_key(&label) {
                    None
                } else {
                    held.entry(label).or_default().push(on_the_label_line);
                    Some(label)
                };
            }
            None => {
                if let Some(label) = open.filter(|label| held.contains_key(label)) {
                    held.entry(label).or_default().push(line);
                }
            }
        }
    }
    held.into_iter()
        .map(|(label, lines)| {
            (
                label,
                lines.join("\n").trim_matches(SECTION_PADDING).to_owned(),
            )
        })
        .collect()
}

/// What one section holds, or nothing when the report never wrote its label.
fn text_of(sections: &BTreeMap<&'static str, String>, label: &str) -> String {
    sections.get(label).cloned().unwrap_or_default()
}

/// A section's text as a field that may be absent: nothing written there is
/// nothing answered, which is the same as never writing the label.
fn answered(text: String) -> Option<String> {
    (!text.is_empty()).then_some(text)
}

/// The choices written in the `Options:` section, one per line that holds text.
///
/// A bullet or a number says how the list was written rather than what was
/// chosen, so either is stripped — and only one marker per line, so an option
/// whose own text starts with `- ` keeps what the agent wrote. A blank line
/// separates two options rather than becoming a third, empty one.
fn options_of(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| option_line(line.trim()).to_owned())
        .filter(|option| !option.is_empty())
        .collect()
}

/// One option line with the list marker it opened with gone.
fn option_line(line: &str) -> &str {
    let line = line.trim_start_matches([' ', '\t']);
    BULLETS
        .into_iter()
        .filter_map(|bullet| line.strip_prefix(bullet))
        .find_map(|tail| tail.strip_prefix([' ', '\t']))
        .or_else(|| numbered_line(line))
        .unwrap_or(line)
}

/// Whether a character is one a list number is written with.
fn is_digit(ch: char) -> bool {
    ch.is_ascii_digit()
}

/// The text after a `1.` or `1)` marker, for the one line of an option list that
/// opens with a number.
///
/// The number has to be followed by a space, because `1.x` is a version rather
/// than a numbered choice, and stripping a marker off it would rewrite an option
/// a human is being asked to choose.
fn numbered_line(line: &str) -> Option<&str> {
    if !line.starts_with(is_digit) {
        return None;
    }
    let after_digits = line.trim_start_matches(is_digit);
    let after_marker = NUMBER_ENDS
        .into_iter()
        .find_map(|end| after_digits.strip_prefix(end))?;
    after_marker.strip_prefix([' ', '\t'])
}

/// Why a `NEEDS_INPUT` report cannot be read as a decision request: the sections
/// it is short of, named as the format writes them.
fn refused(missing: &[&str]) -> Error {
    Error::Corrupt {
        detail: format!(
            "a `KTASK_RESULT: NEEDS_INPUT` report short of {missing} is a malformed report, \
             not a pause: a wait has to carry the question and what deciding it is for, or \
             the human it waits for has nothing to answer",
            missing = missing_phrase(missing),
        ),
        seq: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{DecisionRequest, SECTIONS};
    use crate::{Error, EventKind, Result, parse_report};
    use proptest::prelude::*;

    /// The three headers [`crate::parse_report`] accepts, spelled here so a
    /// refusal can be checked against them without asking the parser what it
    /// would have accepted.
    const HEADERS: [&str; 3] = [
        "KTASK_RESULT: DONE",
        "KTASK_RESULT: FAILED",
        "KTASK_RESULT: NEEDS_INPUT",
    ];

    /// The five field names `docs/DESIGN.md` gives the payload, spelled
    /// alphabetically because that is the order the assertions below compare them
    /// in.
    const FIELDS: [&str; 5] = ["impact", "options", "question", "recommended", "tradeoffs"];

    /// Text a proptest may put inside a section: no colon, so nothing it writes
    /// can open a section of its own, and never a leading space, so a section it
    /// fills is never one that was written empty.
    const PHRASE: &str = r"[A-Za-z0-9.,'-][A-Za-z0-9 ,.'-]{0,39}";

    /// A `NEEDS_INPUT` report in the shape this module reads: the header, prose
    /// above the sections, then all five of them — options written as bullets,
    /// and the trade-offs wrapped over two lines.
    const REQUEST: &str = concat!(
        "KTASK_RESULT: NEEDS_INPUT\n",
        "Summary: stopped at a fork the task did not decide.\n",
        "Question: Should the journal keep its own sequence or adopt SQLite's rowid?\n",
        "Options:\n",
        "- Keep the explicit sequence, and repair it when a rebuild renumbers rows\n",
        "- Adopt rowid, and accept a gap after an append the transaction refused\n",
        "Trade-offs: an explicit sequence is readable in a raw dump of the table;\n",
        "rowid cannot be repaired once an append has been refused.\n",
        "Impact: every replay, and the order the History screen prints.\n",
        "Recommended: keep the explicit sequence.\n",
    );

    /// A complete request in the fewest lines, for tests that vary one part of it.
    const MINIMAL: &str = concat!(
        "KTASK_RESULT: NEEDS_INPUT\n",
        "Question: Which sequence?\n",
        "Options:\n- keep the sequence\n- adopt rowid\n",
        "Trade-offs: a dump stays readable.\n",
        "Impact: every replay.\n",
    );

    /// The message a report was refused for, whichever of the two readings
    /// refused it: the request alone, or the record it would have become.
    fn refusal<T>(asked: Result<Option<T>>) -> String {
        asked
            .err()
            .unwrap_or_else(|| panic!("a report that was read cannot be the answer here"))
            .to_string()
    }

    /// The request a report asked for, or the panic its absence deserves.
    fn asked(text: &str) -> DecisionRequest {
        super::decision_request(text)
            .expect("a well-formed request is not a refusal")
            .unwrap_or_else(|| panic!("a NEEDS_INPUT report has to raise its question"))
    }

    #[test]
    fn a_needs_input_report_raises_the_question_it_asked() {
        let request = asked(REQUEST);
        assert_eq!(
            request.question, "Should the journal keep its own sequence or adopt SQLite's rowid?",
            "the question is the sentence a human has to answer, so it is read verbatim"
        );
    }

    #[test]
    fn every_other_field_is_read_from_its_own_section() {
        let request = asked(REQUEST);
        assert_eq!(
            request.options,
            vec![
                "Keep the explicit sequence, and repair it when a rebuild renumbers rows",
                "Adopt rowid, and accept a gap after an append the transaction refused",
            ],
            "the options are what the human chooses between, in the order they were written"
        );
        assert_eq!(
            request.tradeoffs,
            "an explicit sequence is readable in a raw dump of the table;\n\
             rowid cannot be repaired once an append has been refused.",
            "a section wrapped over two lines is one section, with both lines in it"
        );
        assert_eq!(
            request.impact, "every replay, and the order the History screen prints.",
            "the impact is what makes a decision about one task a decision about several"
        );
        assert_eq!(
            request.recommended.as_deref(),
            Some("keep the explicit sequence."),
            "a recommendation is read as the agent's own opinion, not as the answer"
        );
    }

    #[test]
    fn every_field_round_trips_through_the_encoding_the_journal_stores() {
        let request = asked(REQUEST);
        let encoded = serde_json::to_value(&request).expect("a request encodes as JSON");
        let object = encoded.as_object().expect("a request is a JSON object");
        let mut fields: Vec<&str> = object.keys().map(String::as_str).collect();
        fields.sort_unstable();
        assert_eq!(
            fields, FIELDS,
            "the payload holds exactly the five fields docs/DESIGN.md lists, and no others"
        );

        let text = serde_json::to_string(&request).expect("a request encodes as a string");
        let decoded: DecisionRequest =
            serde_json::from_str(&text).expect("what the journal writes is read back");
        assert_eq!(
            decoded, request,
            "a decision read back out of the journal has to be the decision that was asked"
        );
    }

    #[test]
    fn the_section_labels_are_the_payload_fields_written_with_their_labels() {
        let mut labels: Vec<String> = SECTIONS
            .iter()
            .map(|label| label.to_lowercase().replace('-', ""))
            .collect();
        labels.sort();
        let fields: Vec<String> = FIELDS.iter().map(|field| (*field).to_owned()).collect();
        assert_eq!(
            labels, fields,
            "the labels a report writes and the fields the payload stores are one list, so no \
             section can be read into a field that goes nowhere"
        );
    }

    #[test]
    fn a_recommendation_is_optional_and_its_absence_is_not_a_malformed_report() {
        let request = asked(MINIMAL);
        assert_eq!(
            request.recommended, None,
            "an agent at a genuine fork is not obliged to have picked an answer already"
        );
    }

    #[test]
    fn a_recommendation_written_with_nothing_under_it_is_the_absence_of_one() {
        let text = REQUEST.replace(
            "Recommended: keep the explicit sequence.\n",
            "Recommended:\n",
        );
        let request = asked(&text);
        assert_eq!(
            request.recommended, None,
            "an empty `Recommended:` says nothing, which is the same answer as not writing it"
        );
    }

    #[test]
    fn a_report_without_a_question_is_malformed_and_names_the_question_section() {
        let text = MINIMAL.replace("Question: Which sequence?\n", "");
        let message = refusal(super::decision_request(&text));
        assert!(
            message.contains("`Question:`"),
            "the refusal is the only clue whoever fixes the report gets: {message}"
        );
    }

    #[test]
    fn a_report_short_of_every_required_section_names_every_one_of_them() {
        let message = refusal(super::decision_request(
            "KTASK_RESULT: NEEDS_INPUT\nSummary: stuck, for reasons.\n",
        ));
        for label in ["`Question:`", "`Options:`", "`Trade-offs:`", "`Impact:`"] {
            assert!(
                message.contains(label),
                "{label} is missing and the refusal hid it: {message}"
            );
        }
        assert!(
            !message.contains("`Recommended:`"),
            "a recommendation is never a missing section: {message}"
        );
    }

    #[test]
    fn a_required_label_written_with_nothing_under_it_is_a_missing_one() {
        let text = REQUEST.replace(
            "Impact: every replay, and the order the History screen prints.\n",
            "Impact:\n",
        );
        let message = refusal(super::decision_request(&text));
        assert!(
            message.contains("`Impact:`"),
            "an empty section names no impact to decide from: {message}"
        );
    }

    #[test]
    fn a_done_report_asks_for_no_decision_however_many_questions_it_mentions() {
        let body = REQUEST.replace("KTASK_RESULT: NEEDS_INPUT\n", "");
        let text = format!("KTASK_RESULT: DONE\n{body}");
        assert_eq!(
            super::decision_request(&text).ok().flatten(),
            None,
            "a report that claims the work is finished cannot stop the queue on a question"
        );
    }

    #[test]
    fn a_failed_report_asks_for_no_decision() {
        let body = REQUEST.replace("KTASK_RESULT: NEEDS_INPUT\n", "");
        let text = format!("KTASK_RESULT: FAILED\n{body}");
        assert_eq!(
            super::decision_request(&text).ok().flatten(),
            None,
            "a failure is a failure bundle, not a question, and the two go to different screens"
        );
    }

    #[test]
    fn a_header_that_cannot_be_read_is_refused_before_any_section_is() {
        let body = REQUEST.replace("KTASK_RESULT: NEEDS_INPUT\n", "");
        let text = format!("KTASK_RESULT: MAYBE\n{body}");
        let message = refusal(super::decision_request(&text));
        for header in HEADERS {
            assert!(
                message.contains(header),
                "the refusal of an unreadable header names {header}: {message}"
            );
        }
        assert!(
            !message.contains("`Question:`"),
            "the header is what was wrong, so the body's sections are not the complaint: {message}"
        );
        let claim = parse_report(&text);
        assert!(
            matches!(claim, Err(Error::Corrupt { seq: None, .. })),
            "a report nobody can read is corrupt data, not a provider's fault: {claim:?}"
        );
    }

    #[test]
    fn a_label_written_twice_keeps_the_text_written_under_it_first() {
        let text = format!("{REQUEST}Question: a different question entirely?\n");
        let request = asked(&text);
        assert_eq!(
            request.question, "Should the journal keep its own sequence or adopt SQLite's rowid?",
            "a question written twice is a broken report; the first one wins, as it does in a \
             task block"
        );
    }

    #[test]
    fn prose_above_the_first_label_is_not_read_as_the_question() {
        let text = REQUEST.replace(
            "Summary: stopped at a fork the task did not decide.\n",
            "There is no question in this part of the report at all.\n",
        );
        let request = asked(&text);
        assert!(
            request.question.starts_with("Should the journal"),
            "the prose above the first label was read as the question: {:?}",
            request.question
        );
    }

    #[test]
    fn an_indent_does_not_stop_a_line_opening_a_section() {
        let text = REQUEST.replace("Impact:", "  Impact:");
        let request = asked(&text);
        assert_eq!(
            request.impact, "every replay, and the order the History screen prints.",
            "a section indented under a bullet is still the section"
        );
    }

    #[test]
    fn a_label_in_another_spelling_opens_no_section() {
        for spelling in [
            "question:",
            "QUESTION:",
            "Tradeoffs:",
            "Trade offs:",
            "Recommend:",
            "Question -",
        ] {
            let text = format!(
                "KTASK_RESULT: NEEDS_INPUT\n{spelling} not the label this format uses\n\
                 Options:\n- keep the sequence\n- adopt rowid\n\
                 Trade-offs: a dump stays readable.\nImpact: every replay.\n"
            );
            let asked = super::decision_request(&text);
            assert!(
                asked.is_err(),
                "`{spelling}` is not one of the five labels and was read as one: {asked:?}"
            );
        }
    }

    #[test]
    fn a_numbered_list_is_read_as_options_too() {
        let text = REQUEST.replace(
            "- Keep the explicit sequence, and repair it when a rebuild renumbers rows\n\
             - Adopt rowid, and accept a gap after an append the transaction refused\n",
            "1. keep the sequence\n2. adopt rowid\n",
        );
        let request = asked(&text);
        assert_eq!(
            request.options,
            vec!["keep the sequence", "adopt rowid"],
            "the marker says how the list was written, and is not part of the option"
        );
    }

    #[test]
    fn an_option_line_with_no_bullet_is_still_one_option() {
        let text = REQUEST.replace(
            "- Keep the explicit sequence, and repair it when a rebuild renumbers rows\n\
             - Adopt rowid, and accept a gap after an append the transaction refused\n",
            "keep the sequence\n\nadopt rowid\n\n",
        );
        let request = asked(&text);
        assert_eq!(
            request.options,
            vec!["keep the sequence", "adopt rowid"],
            "a blank line separates two options, it does not become a third one"
        );
    }

    #[test]
    fn a_line_that_merely_starts_like_a_label_is_text_not_a_section() {
        let text = REQUEST.replace(
            "Impact: every replay, and the order the History screen prints.\n",
            "Impact of either choice: every replay, and the order the History screen prints.\n",
        );
        let message = refusal(super::decision_request(&text));
        assert!(
            message.contains("`Impact:`"),
            "`Impact of either choice:` is prose, so the section really is missing: {message}"
        );
    }

    #[test]
    fn the_header_is_never_part_of_a_field() {
        let request = asked(REQUEST);
        assert!(
            !request.question.contains("KTASK_RESULT"),
            "the header is the claim, not part of the question: {:?}",
            request.question
        );
        assert!(
            !request.tradeoffs.contains("Summary:"),
            "the prose above the first label belongs to no section: {:?}",
            request.tradeoffs
        );
    }

    #[test]
    fn a_report_in_the_shape_the_prompt_asks_for_is_refused_naming_the_question() {
        let text = concat!(
            "KTASK_RESULT: NEEDS_INPUT\n",
            "Summary: the queue cannot proceed without a product decision.\n",
            "Action: a human has to choose which layout the journal keeps.\n",
            "Reason: the two layouts are not interchangeable once rows exist.\n",
            "Evidence: docs/DESIGN.md and the schema in crates/ktask-core/src/journal.rs.\n",
        );
        let message = refusal(super::decision_request(text));
        assert!(
            message.contains("`Question:`"),
            "prose that explains a blocker without asking a question is not a pause: {message}"
        );
    }

    /// The record a report asks for, or the panic its absence deserves.
    fn raised(text: &str) -> EventKind {
        super::decision_event(text)
            .expect("a well-formed request is not a refusal")
            .unwrap_or_else(|| panic!("a NEEDS_INPUT report has to raise its question"))
    }

    #[test]
    fn a_needs_input_report_raises_the_record_that_opens_the_wait() {
        let event = raised(REQUEST);
        let EventKind::DecisionRaised { request } = &event else {
            panic!(
                "a pause for input is journaled as the ask, not as another event: {:?}",
                event.discriminant()
            );
        };
        assert_eq!(
            request.question, "Should the journal keep its own sequence or adopt SQLite's rowid?",
            "the record a human reads has to hold the sentence the agent wrote"
        );
        assert_eq!(
            event.discriminant(),
            "DecisionRaised",
            "the kind column the journal indexes on is the entry docs/DESIGN.md names"
        );
    }

    #[test]
    fn a_report_that_asks_for_nothing_raises_no_record() {
        for header in ["DONE", "FAILED"] {
            let body = REQUEST.replace("KTASK_RESULT: NEEDS_INPUT\n", "");
            let text = format!("KTASK_RESULT: {header}\n{body}");
            assert_eq!(
                super::decision_event(&text).ok().flatten(),
                None,
                "a {header} report cannot stop the queue on a question it did not ask"
            );
        }
    }

    #[test]
    fn a_pause_claimed_without_a_question_raises_no_record_and_says_which_section() {
        let text = MINIMAL.replace("Question: Which sequence?\n", "");
        let message = refusal(super::decision_event(&text));
        assert!(
            message.contains("`Question:`"),
            "the runner is told which section to send back for: {message}"
        );
    }

    #[test]
    fn a_raised_record_survives_the_encoding_the_journal_stores() {
        let event = raised(REQUEST);
        let text = serde_json::to_string(&event).expect("a raised record encodes as JSON");
        let decoded: EventKind =
            serde_json::from_str(&text).expect("what the journal writes is read back");
        assert_eq!(
            decoded, event,
            "the question read out of the journal has to be the question that was asked"
        );
        assert!(
            text.contains("Should the journal keep its own sequence"),
            "a record that stored the ask as anything but its own text would lose it: {text}"
        );
    }

    proptest! {
        /// A report written in the shape the format prescribes reads back as the
        /// request it wrote, field for field, whatever the text inside them says.
        #[test]
        fn a_request_read_back_from_a_report_is_the_request_that_was_written(
            question in PHRASE,
            options in prop::collection::vec(PHRASE, 1..4),
            tradeoffs in PHRASE,
            impact in PHRASE,
            has_advice in proptest::bool::ANY,
            advice in PHRASE,
        ) {
            let bullets: String = options
                .iter()
                .flat_map(|option| ["- ", option.as_str(), "\n"])
                .collect();
            let advice_line = if has_advice {
                format!("Recommended: {advice}\n")
            } else {
                String::new()
            };
            let fields = format!("Trade-offs: {tradeoffs}\nImpact: {impact}\n{advice_line}");
            let text = format!(
                "KTASK_RESULT: NEEDS_INPUT\nQuestion: {question}\nOptions:\n{bullets}{fields}"
            );
            let request = super::decision_request(&text)
                .expect("a report with all four required sections is not a refusal")
                .expect("a NEEDS_INPUT report raises its question");
            prop_assert_eq!(&request.question, question.trim_matches([' ', '\t', '\r']));
            let written: Vec<String> = options
                .iter()
                .map(|option| option.trim_matches([' ', '\t', '\r']).to_owned())
                .collect();
            prop_assert_eq!(&request.options, &written);
            prop_assert_eq!(&request.tradeoffs, tradeoffs.trim_matches([' ', '\t', '\r']));
            prop_assert_eq!(&request.impact, impact.trim_matches([' ', '\t', '\r']));
            prop_assert_eq!(
                &request.recommended,
                &has_advice.then(|| advice.trim_matches([' ', '\t', '\r']).to_owned())
            );
        }

        /// A body that never writes the `Question:` label is refused whatever
        /// else it says, and the refusal always names the section it is short of.
        #[test]
        fn a_body_without_the_question_label_is_refused_naming_the_question_section(
            prose in r"[A-Za-z0-9 ,.'-]{0,40}",
            lines in prop::collection::vec(r"[A-Za-z0-9 ,.'-]{0,40}", 0..4),
        ) {
            let mut text = format!("KTASK_RESULT: NEEDS_INPUT\n{prose}\n");
            for line in &lines {
                text.push_str(line);
                text.push('\n');
            }
            let asked = super::decision_request(&text);
            prop_assert!(
                asked.is_err(),
                "a report with no `Question:` section paused the run anyway: {text:?}"
            );
            let message = asked
                .expect_err("the refusal above is the error")
                .to_string();
            prop_assert!(
                message.contains("`Question:`"),
                "the refusal did not name the missing section: {message}"
            );
        }
    }
}
