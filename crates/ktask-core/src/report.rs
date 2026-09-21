//! What an agent's report claims, read as a claim and nothing more.
//!
//! Every attempt ends with a report the agent wrote about its own work, and the
//! prompt template's finishing rule tells the agent that its first line is
//! exactly one of three words. [`parse_report`] is that rule read back: it turns
//! those words into a [`ReportResult`], so what an agent *said* becomes a typed
//! fact that the journal, a screen and `--json` output can each render instead
//! of each inventing a vocabulary for it.
//!
//! What a [`ReportResult`] deliberately is not: evidence that the work is
//! finished. VISION.md §3's invariant 4 — a task is never done based only on an
//! agent's exit code or statement — makes a report an input to *reporting* and
//! never an input to the verdict, which invariant 7 gives to local verification,
//! clean publication and fetched remote equality instead. The split is kept
//! structural rather than promised: nothing in this file reads a gate, a SHA or
//! a journal, so a `DONE` parsed out of text has no path into a decision the
//! gates did not make. One test pins the pair anyway — a report claiming `DONE`
//! above a refused gate is read as [`ReportResult::Done`] and classified as a
//! verification failure in the same breath.
//!
//! Two readings had to be chosen, and ADR-0076 records both. The header is the
//! first line with anything on it and *only* that line, so that prose about the
//! protocol — a session quoting its own prompt, which is what
//! [`mod@crate::classify`] guards against the same way — cannot be mistaken for a use
//! of it. And the three spellings are matched exactly, because a header that
//! needs fixing is cheap to refuse loudly and expensive to guess at: the refusal
//! names what was found beside the three lines that would have been read.

use crate::{Error, Result};

/// The three lines a header may be, and the result each one says.
///
/// This table is the whole accepted grammar, and the refusal below is built from
/// it, so what the parser rejects and what it says it wanted cannot drift apart.
const CLAIMS: [(&str, ReportResult); 3] = [
    ("KTASK_RESULT: DONE", ReportResult::Done),
    ("KTASK_RESULT: FAILED", ReportResult::Failed),
    ("KTASK_RESULT: NEEDS_INPUT", ReportResult::NeedsInput),
];

/// What an attempt's own report says the attempt achieved.
///
/// Three words, because the reporting contract offers three answers and a fourth
/// would be a claim nobody knows how to act on. The value is what the agent
/// asserted, never what was proved: it is recorded beside the gates, the push and
/// the fetched remote, and it loses to all three of them (VISION.md §3's
/// invariants 4 and 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportResult {
    /// The agent asserted it finished: `KTASK_RESULT: DONE`. Whether it did is
    /// the gates' answer, and a refused gate outranks this claim every time.
    Done,
    /// The agent stopped short and says so: `KTASK_RESULT: FAILED`. A usable
    /// result rather than an accident — the reason belongs in the failure record
    /// the class is read from.
    Failed,
    /// The agent stopped at a decision it is not authorised to make:
    /// `KTASK_RESULT: NEEDS_INPUT`. VISION.md §3's invariant 8 makes that a
    /// pause state for a human rather than a retry, so nothing loops on it.
    NeedsInput,
}

/// Read the header of an agent's report.
///
/// The header is the first line that holds anything, with the whitespace a line
/// ending or an indent leaves at either end trimmed off; everything after that
/// line is the report's prose and is not read here. On that line sits one of the
/// three headers the `CLAIMS` table above lists, spelled exactly as it spells them.
///
/// # Errors
///
/// [`Error::Corrupt`] when the report holds no line at all, or when its first
/// line with content is not one of the three headers. The message names the line
/// that was found, if there was one, beside the three that were expected.
/// ADR-0076 records why a report that cannot be read is corrupt data rather than
/// a provider's fault: it is durable text filed in the project's state directory,
/// and the only alternative is to guess at a claim the agent did not make.
pub fn parse_report(text: &str) -> Result<ReportResult> {
    let header = header_line(text).ok_or_else(|| refused("nothing"))?;
    CLAIMS
        .iter()
        .find_map(|&(expected, result)| (header == expected).then_some(result))
        .ok_or_else(|| refused(&format!("`{header}`")))
}

/// The first line with anything on it, with the whitespace at either end gone.
///
/// Both ends are trimmed because the two things that get in the way are the line
/// ending the report was written with and the indent a bullet put there, and
/// neither is part of the claim. A line of pure whitespace holds no claim either,
/// so the search walks past it: the header is the first line with content, which
/// is the line an agent was told to open its report with.
fn header_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// The refusal of a report whose header cannot be read.
///
/// `found` is the line that was read, quoted, or the word `nothing` when the
/// report held no line to read.
fn refused(found: &str) -> Error {
    Error::Corrupt {
        detail: format!(
            "report opens with {found}; its first non-empty line must be exactly \
             one of {expected}",
            expected = expected_headers(),
        ),
        seq: None,
    }
}

/// The three accepted headers, spelled out for whoever has to fix the report.
///
/// Built from [`CLAIMS`] rather than written out again, so a refusal cannot
/// advertise a header the parser would not have accepted.
fn expected_headers() -> String {
    CLAIMS
        .iter()
        .map(|&(header, _)| format!("`{header}`"))
        .collect::<Vec<_>>()
        .join(" or ")
}

#[cfg(test)]
mod tests {
    use super::ReportResult;
    use crate::{
        Error, FailureClass, GateKind, GateResult, Outcome, Result, classify, parse_report,
    };
    use proptest::prelude::*;

    /// The three headers, spelled here rather than read from the implementation.
    ///
    /// A test that asked the parser's own table what it accepted would pass
    /// against a table that had been changed to be wrong, which is the whole
    /// failure this suite exists to prevent.
    const HEADERS: [(ReportResult, &str); 3] = [
        (ReportResult::Done, "KTASK_RESULT: DONE"),
        (ReportResult::Failed, "KTASK_RESULT: FAILED"),
        (ReportResult::NeedsInput, "KTASK_RESULT: NEEDS_INPUT"),
    ];

    /// A full report as an agent is asked to write one: the header, then prose.
    fn report(header: &str) -> String {
        format!("{header}\nSummary: the work is where the task asked for it.\n")
    }

    /// Whether a refusal spelled out all three lines it wanted.
    fn names_every_expectation(message: &str) -> bool {
        HEADERS.iter().all(|(_, header)| message.contains(header))
    }

    /// The message of the error a report was refused for.
    fn refusal(claim: Result<ReportResult>) -> String {
        claim
            .err()
            .unwrap_or_else(|| panic!("a report that was read cannot be the answer here"))
            .to_string()
    }

    #[test]
    fn a_header_claiming_done_is_read_as_done() {
        let claim = parse_report(&report("KTASK_RESULT: DONE")).ok();
        assert_eq!(claim, Some(ReportResult::Done));
    }

    #[test]
    fn a_header_claiming_failed_is_read_as_failed() {
        let claim = parse_report(&report("KTASK_RESULT: FAILED")).ok();
        assert_eq!(claim, Some(ReportResult::Failed));
    }

    #[test]
    fn a_header_claiming_needs_input_is_read_as_needs_input() {
        let claim = parse_report(&report("KTASK_RESULT: NEEDS_INPUT")).ok();
        assert_eq!(claim, Some(ReportResult::NeedsInput));
    }

    #[test]
    fn a_header_may_be_the_first_line_without_a_trailing_newline() {
        let claim = parse_report("KTASK_RESULT: NEEDS_INPUT").ok();
        assert_eq!(
            claim,
            Some(ReportResult::NeedsInput),
            "a report whose last byte is still part of the header was written",
        );
    }

    #[test]
    fn blank_lines_above_the_header_do_not_hide_it() {
        let claim = parse_report("\n  \n\t\nKTASK_RESULT: FAILED\nSummary: blocked.\n").ok();
        assert_eq!(
            claim,
            Some(ReportResult::Failed),
            "an opening blank line carries no claim, so it cannot be the header",
        );
    }

    #[test]
    fn an_indent_does_not_stop_a_line_being_the_header() {
        let claim = parse_report("   KTASK_RESULT: DONE\n").ok();
        assert_eq!(claim, Some(ReportResult::Done));
    }

    #[test]
    fn a_windows_line_ending_does_not_hide_the_header() {
        let claim =
            parse_report("KTASK_RESULT: DONE\r\nSummary: written on another host.\r\n").ok();
        assert_eq!(
            claim,
            Some(ReportResult::Done),
            "a carriage return is line-ending noise, not part of the claim",
        );
    }

    #[test]
    fn the_first_header_wins_when_a_report_holds_two() {
        let claim = parse_report("KTASK_RESULT: DONE\nSummary: also KTASK_RESULT: FAILED\n").ok();
        assert_eq!(
            claim,
            Some(ReportResult::Done),
            "only the first line with content is read, so a report cannot claim two answers",
        );
    }

    #[test]
    fn a_header_below_the_opening_prose_is_not_a_header() {
        let claim = parse_report(
            "Notes from the run.\n\nA line saying KTASK_RESULT: DONE appears further down.\n",
        );
        assert!(
            claim.is_err(),
            "prose that quotes the protocol is not a use of it: {claim:?}"
        );
        let message = refusal(claim);
        assert!(
            names_every_expectation(&message),
            "a refusal has to say what it wanted: {message}"
        );
    }

    #[test]
    fn an_empty_report_is_refused_and_names_what_was_expected() {
        let message = refusal(parse_report(""));
        assert!(
            names_every_expectation(&message),
            "the refusal is the only clue an operator gets: {message}"
        );
    }

    #[test]
    fn a_report_of_nothing_but_blank_lines_is_refused() {
        let message = refusal(parse_report("\n \n\t\n   \n"));
        assert!(
            names_every_expectation(&message),
            "whitespace is not a header, however many lines of it there are: {message}"
        );
    }

    #[test]
    fn a_malformed_header_is_refused_naming_the_line_that_was_found() {
        for found in [
            "ktask_result: done",
            "KTASK_RESULT: done",
            "KTASK_RESULT:DONE",
            "KTASK_RESULT:  DONE",
            "KTASK_RESULT: DONE, mostly",
            "KTASK_RESULT: SUCCEEDED",
            "KTASK_RESULT:",
            "RESULT: DONE",
            "- KTASK_RESULT: DONE",
            "KTASK_RESULT: NEEDS-INPUT",
        ] {
            let claim = parse_report(&report(found));
            assert!(
                claim.is_err(),
                "`{found}` is not one of the three headers and was read as one: {claim:?}"
            );
            let message = refusal(claim);
            assert!(
                message.contains(found),
                "a refusal that hides the line it read cannot be acted on: {message}"
            );
            assert!(names_every_expectation(&message), "{message}");
        }
    }

    #[test]
    fn an_unreadable_header_is_corrupt_data_with_no_journal_position() {
        let claim = parse_report("KTASK_RESULT: MAYBE\n");
        assert!(
            matches!(claim, Err(Error::Corrupt { seq: None, .. })),
            "a report is durable text that could not be trusted, not a provider \
             that was unreachable: {claim:?}"
        );
    }

    #[test]
    fn a_claim_of_done_does_not_overrule_a_refused_gate() {
        let text = report("KTASK_RESULT: DONE");
        let claim = parse_report(&text).expect("the header is the one the prompt asked for");
        let outcome = Outcome {
            exit_code: 0,
            stdout: text.clone(),
            stderr: String::new(),
            usage: None,
            session_id: None,
            model_reported: None,
        };
        let refused = GateResult {
            kind: GateKind::Verify,
            passed: false,
            exit_code: Some(101),
            signal: None,
            duration_ms: 31_472,
            stdout: "test result: FAILED. 247 passed; 2 failed\n".to_owned(),
            stderr: String::new(),
            timed_out: false,
        };

        let class = classify(&outcome, &[refused], None);

        assert_eq!(claim, ReportResult::Done, "the agent did claim it finished");
        assert_eq!(
            class,
            FailureClass::VerificationFailure,
            "a report claiming DONE is read beside a refused gate, never above it \
             (VISION.md §3's invariant 4)",
        );
    }

    proptest! {
        /// Whatever the body holds, and however many blank or indented lines
        /// precede it, a header is read as the one answer it names — nothing the
        /// body says afterwards moves the answer.
        #[test]
        fn a_header_above_any_body_is_read_as_the_answer_it_named(
            choice in 0usize..3,
            blanks in "[ \t\n\r]{0,12}",
            body in any::<String>(),
        ) {
            let (expected, header) = HEADERS[choice];
            let claim = parse_report(&format!("{blanks}{header}\n{body}")).ok();
            prop_assert_eq!(
                claim,
                Some(expected),
                "the answer moved off the header: {}",
                header,
            );
        }

        /// A report that opens with anything else is refused however hard a
        /// header appears later, and every refusal names the three lines it
        /// wanted rather than only that something was wrong.
        #[test]
        fn anything_that_does_not_open_with_the_header_is_refused(
            prose in "[A-Za-z0-9 .:,;'`-]{0,40}",
            later in 0usize..3,
            tail in any::<String>(),
        ) {
            let (expected, header) = HEADERS[later];
            let text = format!("# {prose}\n{header}\n{tail}");
            let claim = parse_report(&text);
            prop_assert!(
                claim.is_err(),
                "a header below the opening prose was read as {expected:?}",
            );
            prop_assert!(
                names_every_expectation(&refusal(claim)),
                "the refusal did not name what it wanted",
            );
        }
    }
}
