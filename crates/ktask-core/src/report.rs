//! Where an agent's report goes, and what reading it back can say.
//!
//! Every attempt ends with a report the agent wrote about its own work, and the
//! prompt template's finishing rule tells the agent that its first line is
//! exactly one of three words. [`parse_report`] is that rule read back: it turns
//! those words into a [`ReportResult`], so what an agent *said* becomes a typed
//! fact that the journal, a screen and `--json` output can each render instead
//! of each inventing a vocabulary for it.
//!
//! A claim can only be read back if it was written somewhere the supervisor
//! knows, so this module owns that somewhere too: [`report_path`] is the one
//! spelling of the file an agent is told to write, and [`read_report`] is the
//! read that happens after the provider exits. The round trip is the point: a
//! prompt that names a path nobody resolves, and a file nobody reads, leave an
//! attempt's account of itself in nobody's hands. Reading it answers two
//! questions and only two — what the agent claimed ([`ReportClaim::Claimed`]),
//! and that the agent wrote nothing ([`ReportClaim::Missing`], which carries the
//! class the run records beside the path the agent was told to use).
//!
//! What a [`ReportResult`] deliberately is not: evidence that the work is
//! finished. VISION.md §3's invariant 4 — a task is never done based only on an
//! agent's exit code or statement — makes a report an input to *reporting* and
//! never an input to the verdict, which invariant 7 gives to local verification,
//! clean publication and fetched remote equality instead. The split is kept
//! structural rather than promised: the only file this module opens is the report
//! itself — never a gate, a SHA or a journal — so a `DONE` read out of text has
//! no path into a decision the gates did not make. What a reading can do is
//! refuse: an absent report is a failure with a class beside it, and that is the
//! one direction invariant 4 leaves open to an agent's account of its own work.
//! One test pins the pair anyway — a report claiming `DONE` above a refused gate
//! is read as [`ReportResult::Done`] and classified as a verification failure in
//! the same breath.
//!
//! Two readings had to be chosen, and ADR-0076 records both. The header is the
//! first line with anything on it and *only* that line, so that prose about the
//! protocol — a session quoting its own prompt, which is what
//! [`mod@crate::classify`] guards against the same way — cannot be mistaken for a use
//! of it. And the three spellings are matched exactly, because a header that
//! needs fixing is cheap to refuse loudly and expensive to guess at: the refusal
//! names what was found beside the three lines that would have been read.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::{AttemptId, Error, FailureClass, Project, Result, TaskId, evidence_dir};

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

/// Everything an agent wrote below the header of its report.
///
/// The header is the first line with content on it, which is the line
/// [`header_line`] reads the claim from and the line this stops after; what
/// follows is the prose the sections of a [`NEEDS_INPUT`](ReportResult::NeedsInput)
/// report are read out of by [`mod@crate::decision`]. A report that held only
/// its header has an empty body, which is an answer rather than a refusal here:
/// deciding whether an empty body is enough is what reads the body.
///
/// The reader counts bytes and slices once, rather than re-reading the first
/// line of whatever is left each time. A walker that advanced by what it had
/// just read would sit still on a read that came back empty, and a supervisor
/// that never finishes reading a report is worse than one that reads it wrong.
pub(crate) fn body_after_header(text: &str) -> &str {
    let mut past_header = 0;
    for line in text.split_inclusive('\n') {
        past_header += line.len();
        if !line.trim().is_empty() {
            break;
        }
    }
    slice_after(text, past_header)
}

/// `text` from `bytes` onward, which is every length this module computes.
///
/// A slice rather than an index expression because a report is text an agent
/// wrote: a boundary that landed mid-character is a report to refuse, not a
/// panic in the supervisor reading it.
fn slice_after(text: &str, bytes: usize) -> &str {
    text.get(bytes..).unwrap_or("")
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

/// The file an attempt's own report is written to, inside its evidence directory.
///
/// `pub(crate)` rather than private because the assembled prompt names it
/// (ADR-0075): an agent told a path it cannot find writes its account nowhere the
/// run will look for it.
///
/// It is *not* `report.md`. That name belongs to the record the runner generates
/// from the gates and SHAs it watched (ADR-0065), and VISION.md §3's invariant 4
/// is why the two accounts of one attempt stay two files: an agent's word must
/// never come to stand in for the mechanical record, least of all by occupying its
/// filename and being overwritten by it.
pub(crate) const AGENT_REPORT_FILE: &str = "agent-report.md";

/// The class a run records when an agent wrote no report at all.
///
/// `agent_failure` is "the agent could not complete the implementation"
/// (VISION.md §7), and a session that ran, was asked for a report and left none is
/// that: the absence proves nothing about completion, and the recovery §7 gives it
/// is a fresh session asked again. It is therefore not `policy_failure` (nothing
/// forbidden was touched), not `verification_failure` (no gate refused) and not a
/// pause for a human (nothing is undecided).
const MISSING_REPORT: FailureClass = FailureClass::AgentFailure;

/// Where one attempt's own report is written, and read back from.
///
/// `<state_dir>/attempts/<task>/<attempt>/agent-report.md`: the file inside
/// [`crate::evidence_dir`]'s directory, so an agent's account joins the directory
/// of the attempt it describes rather than a third layout nobody reads, and a
/// retry adds a directory instead of replacing a file (VISION.md §7). The path is
/// below the project's state directory and never below its working copy, because
/// an operational artifact inside the repository is what the privacy gate exists
/// to catch (VISION.md §3's invariant 6, §11).
///
/// This is *the* spelling of that path: the prompt header an agent is handed
/// names exactly this path, and [`read_report`] reads exactly this path back, so
/// "written where the prompt says, and read back" is one fact rather than two that
/// can drift apart.
#[must_use]
pub fn report_path(project: &Project, task: TaskId, attempt: AttemptId) -> PathBuf {
    evidence_dir(project, task, attempt).join(AGENT_REPORT_FILE)
}

/// What reading one attempt's report back can say.
///
/// Two answers, because when a provider exits the file is either there or not
/// there. A third answer — assume it went well — is what VISION.md §3's invariant
/// 4 forbids, and the type leaves it unspellable: an absent report carries no
/// [`ReportResult`] at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportClaim {
    /// The file was there, and its header named one of the three claims.
    Claimed {
        /// The file the claim was read out of: [`report_path`]'s path.
        path: PathBuf,
        /// What the agent said it achieved. A claim, never a verdict.
        result: ReportResult,
        /// The whole text as it stands. The body of a
        /// [`NEEDS_INPUT`](ReportResult::NeedsInput) report is what
        /// [`crate::decision_request`] reads, and opening the file again for it
        /// would be a second read of a file a fast session may already have
        /// replaced.
        text: String,
    },
    /// The file was not there, which is a failure the run acts on.
    Missing {
        /// The path that was expected — the one the prompt named — because a
        /// refusal nobody can locate is a refusal nobody can act on.
        path: PathBuf,
        /// The class VISION.md §7's recovery policy is read from.
        class: FailureClass,
        /// The one-line account of the refusal, naming `path`.
        detail: String,
    },
}

/// Read one attempt's report, and parse what its header claims.
///
/// This is what happens after the provider exits: the file the prompt named is
/// opened once, its header is read with [`parse_report`], and the answer is a
/// [`ReportClaim`] rather than an assumption. Absence is an answer and not an
/// error — a session that wrote nothing produced a real outcome, and classifying
/// it is the run's job — while a file that is there and cannot be read stays an
/// error, because durable text that cannot be trusted is not a claim in a
/// different costume (ADR-0076).
///
/// # Errors
///
/// [`Error::Corrupt`] when the file is there and its header is none of the three
/// claims; the message names the file. [`Error::Io`] when the filesystem refused a
/// read that was not plain absence — a report occupied by a directory, say.
pub fn read_report(project: &Project, task: TaskId, attempt: AttemptId) -> Result<ReportClaim> {
    let path = report_path(project, task, attempt);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(why) if absent(&why) => {
            let detail = missing_words(&path);
            return Ok(ReportClaim::Missing {
                detail,
                path,
                class: MISSING_REPORT,
            });
        }
        Err(why) => return Err(Error::Io(why)),
    };
    let result = parse_report(&text).map_err(|why| unreadable(&path, &why))?;
    Ok(ReportClaim::Claimed { path, result, text })
}

/// Whether the filesystem says the report simply is not there.
///
/// Absence is the ordinary answer for a session that died before it wrote, so it
/// is recognised from the one code that means absence rather than by matching an
/// error message for phrases.
fn absent(why: &io::Error) -> bool {
    why.kind() == io::ErrorKind::NotFound
}

/// The refusal of a report that is there and cannot be read, naming the file.
///
/// [`parse_report`] answers about text and cannot know where the text came from, so
/// the reader holding the path adds it: "the report is malformed" is not
/// actionable until somebody is told which report.
fn unreadable(path: &Path, why: &Error) -> Error {
    Error::Corrupt {
        detail: format!("report `{}`: {why}", path.display()),
        seq: None,
    }
}

/// The line an operator, a screen and the journal all read for an absent report.
fn missing_words(path: &Path) -> String {
    format!(
        "the agent wrote no report: `{}` is not there, and an attempt with no report of \
         its own is a failure rather than a success nobody objected to",
        path.display()
    )
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

    /// What is left of a report once its header is read is what a decision is
    /// read out of, so its shape is a contract in its own right: everything
    /// below the header line, none of the header line, and nothing invented.
    ///
    /// Asserted here as well as through [`crate::decision_request`], because a
    /// header line can never open one of a decision's five sections — so a body
    /// that still held the header, or had lost a few of its bytes, would read
    /// exactly like one that did not. Only this comparison tells the three
    /// apart, and the two surviving mutants `scripts/review-tests.sh` reported
    /// for the first version of this function are exactly that difference.
    #[test]
    fn the_body_is_everything_below_the_header_line() {
        const BODIES: [(&str, &str, &str); 5] = [
            (
                "KTASK_RESULT: DONE\n",
                "",
                "a report of nothing but its header holds no body",
            ),
            (
                "KTASK_RESULT: DONE",
                "",
                "a header with no line ending after it holds no body",
            ),
            (
                "KTASK_RESULT: DONE\nSummary: all gates passed.\n",
                "Summary: all gates passed.\n",
                "the body is what the agent wrote, newline included",
            ),
            (
                "\n   \nKTASK_RESULT: NEEDS_INPUT\nQuestion: which?\n",
                "Question: which?\n",
                "blank lines above the header are not the header",
            ),
            (
                "KTASK_RESULT: NEEDS_INPUT\r\nImpact: every replay.\r\n",
                "Impact: every replay.\r\n",
                "a Windows line ending ends the header, not the body",
            ),
        ];
        for (report, body, why) in BODIES {
            assert_eq!(super::body_after_header(report), body, "{why}: {report:?}");
        }
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

#[cfg(test)]
mod round_trip {
    //! Where an agent's report is written, and what reading it back can say.
    //!
    //! Named after what it tests — the path the prompt names and the round trip
    //! out of it — the way `runner.rs` names `new` and `prepare`, because the
    //! task that asked for this fixed `test(/report::/)` as its Verify command.

    use super::{ReportClaim, ReportResult, read_report, report_path};
    use crate::testing::{ScratchRepo, scratch_repo};
    use crate::{AttemptId, Error, FailureClass, Project, TaskId, evidence_dir};
    use std::fs;
    use std::path::PathBuf;

    /// The identity every fixture gives its registered project.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// The task number the fixtures use.
    const TASK: u32 = 12;

    /// A report as an agent leaves it on the path it was told to write.
    const AGENT_REPORT: &str = "KTASK_RESULT: DONE\nSummary: the round trip is proved.\n";

    /// A project of its own: a repository, a state directory below it, and an
    /// empty `attempts` tree that nothing has filed yet.
    struct Fixture {
        repo: ScratchRepo,
        project: Project,
    }

    impl Fixture {
        fn new() -> Self {
            let repo = scratch_repo().expect("a scratch repository is buildable");
            let project = Project {
                root: repo.work().to_path_buf(),
                id: PROJECT_ID.to_owned(),
                state_dir: repo.path().join("state").join(PROJECT_ID),
            };
            fs::create_dir_all(&project.state_dir).expect("a state directory is creatable");
            Self { repo, project }
        }

        /// File `text` where `task`/`attempt`'s agent would have written it, and
        /// return the path that now holds it.
        ///
        /// The parent directory is made here because this is the agent's side of
        /// the round trip: the runner is what guarantees the directory, and a
        /// report the runner never gave a home is the case tested separately.
        fn write(&self, task: u32, attempt: u32, text: &str) -> PathBuf {
            let path = report_path(&self.project, TaskId::new(task), AttemptId::new(attempt));
            let parent = path
                .parent()
                .expect("a report path has a directory to hold it");
            fs::create_dir_all(parent).expect("an agent's report directory is creatable");
            fs::write(&path, text).expect("an agent's report is writable");
            path
        }

        /// Read one attempt's report back.
        fn read(&self, task: u32, attempt: u32) -> Result<ReportClaim, Error> {
            read_report(&self.project, TaskId::new(task), AttemptId::new(attempt))
        }
    }

    /// The path a test expects, spelled out rather than asked of the code under test.
    ///
    /// Every assertion about where a report goes is worth nothing if it asks
    /// `report_path` where the report goes.
    fn expected(project: &Project, task: &str, attempt: &str) -> PathBuf {
        project
            .state_dir
            .join("attempts")
            .join(task)
            .join(attempt)
            .join("agent-report.md")
    }

    /// The claim a reading came back with, or a failing test that says what did.
    fn claim(reading: &Result<ReportClaim, Error>) -> ReportResult {
        claimed(reading).0
    }

    /// The three facts of a reading that found a report, or a failing test.
    fn claimed(reading: &Result<ReportClaim, Error>) -> (ReportResult, PathBuf, String) {
        match reading.as_ref() {
            Ok(ReportClaim::Claimed { path, result, text }) => {
                (*result, path.clone(), text.clone())
            }
            other => panic!("expected a claim, got {other:?}"),
        }
    }

    /// The path and class a missing report was refused for, or a failing test.
    fn missing(reading: &Result<ReportClaim, Error>) -> (PathBuf, FailureClass, String) {
        match reading.as_ref() {
            Ok(ReportClaim::Missing {
                path,
                class,
                detail,
            }) => (path.clone(), *class, detail.clone()),
            other => panic!("expected a missing report, got {other:?}"),
        }
    }

    #[test]
    fn the_report_path_names_the_agents_file_in_its_own_attempt_directory() {
        let fixture = Fixture::new();

        let path = report_path(&fixture.project, TaskId::new(TASK), AttemptId::new(2));

        assert_eq!(
            path,
            expected(&fixture.project, "12", "2"),
            "the prompt tells an agent to write below its project's state directory at \
             `<state_dir>/attempts/<task>/<attempt>/`, and this is the path it names",
        );
    }

    #[test]
    fn the_report_path_is_the_file_an_agent_writes_and_not_the_one_the_runner_generates() {
        let fixture = Fixture::new();
        let work = TaskId::new(TASK);
        let attempt = AttemptId::new(1);

        let path = report_path(&fixture.project, work, attempt);
        let generated = evidence_dir(&fixture.project, work, attempt).join("report.md");

        assert_eq!(
            path.parent(),
            Some(evidence_dir(&fixture.project, work, attempt).as_path())
        );
        assert_ne!(
            path, generated,
            "`report.md` is the record the runner generated from the gates it watched \
             (ADR-0065); an agent's own account is a different file, because VISION.md §3's \
             invariant 4 refuses to let the second stand in for the first",
        );
    }

    #[test]
    fn each_attempt_of_a_task_is_named_by_its_own_report_path() {
        let fixture = Fixture::new();
        let work = TaskId::new(TASK);

        let first = report_path(&fixture.project, work, AttemptId::new(1));
        let second = report_path(&fixture.project, work, AttemptId::new(2));

        assert_eq!(first, expected(&fixture.project, "12", "1"));
        assert_eq!(second, expected(&fixture.project, "12", "2"));
        assert_ne!(
            first, second,
            "a retry adds a directory rather than replacing a file, so two attempts can \
             never read each other's report by accident",
        );
    }

    #[test]
    fn a_report_written_where_the_path_points_is_read_back_as_its_claim() {
        let fixture = Fixture::new();
        fixture.write(TASK, 1, AGENT_REPORT);

        let reading = fixture.read(TASK, 1);

        assert_eq!(
            claim(&reading),
            ReportResult::Done,
            "the header the prompt asked for is read as the claim it is",
        );
        let (_, path, text) = claimed(&reading);
        assert_eq!(path, expected(&fixture.project, "12", "1"));
        assert_eq!(text, AGENT_REPORT, "the body is read whole, not re-derived");
    }

    #[test]
    fn every_claim_the_contract_offers_is_read_back_from_where_it_was_written() {
        let fixture = Fixture::new();
        let claims = [
            (ReportResult::Done, "KTASK_RESULT: DONE\n"),
            (
                ReportResult::Failed,
                "KTASK_RESULT: FAILED\nSummary: blocked.\n",
            ),
            (
                ReportResult::NeedsInput,
                "KTASK_RESULT: NEEDS_INPUT\nQuestion: which shape?\n",
            ),
        ];
        for (index, (_, text)) in claims.iter().enumerate() {
            let attempt = u32::try_from(index + 1).expect("three attempts are numberable");
            fixture.write(TASK, attempt, text);
        }
        for (index, (expected, _)) in claims.iter().enumerate() {
            let attempt = u32::try_from(index + 1).expect("three attempts are numberable");
            let reading = fixture.read(TASK, attempt);
            assert_eq!(
                claim(&reading),
                *expected,
                "attempt {attempt}'s report was read as something else",
            );
        }
    }

    #[test]
    fn a_missing_report_is_a_classified_failure_naming_the_path_it_expected() {
        let fixture = Fixture::new();

        let reading = fixture.read(TASK, 1);

        let (path, class, detail) = missing(&reading);
        assert_eq!(
            path,
            expected(&fixture.project, "12", "1"),
            "a refusal that does not name the path the agent was told to write cannot be \
             acted on by whoever reads the failure",
        );
        assert_eq!(
            class,
            FailureClass::AgentFailure,
            "an agent that wrote nothing completed the work as far as the run can see it \
             and did not deliver its account: that is the agent's own failure to finish, \
             remediable by asking again, not a gate that refused or a machine that is wrong",
        );
        assert!(
            detail.contains(&path.display().to_string()),
            "the line an operator or a screen reads has to name the expected path: {detail}",
        );
    }

    #[test]
    fn a_missing_report_is_never_read_as_a_claim_of_done() {
        let fixture = Fixture::new();

        let reading = fixture.read(TASK, 1);

        assert!(
            !matches!(reading, Ok(ReportClaim::Claimed { .. })),
            "VISION.md §3's invariant 4: a task is never done on an agent's statement — \
             and it is least of all done on a statement that was never made: {reading:?}",
        );
        assert!(
            matches!(reading, Ok(ReportClaim::Missing { .. })),
            "an absent report is an answer the run acts on, not an error that stops it: \
             {reading:?}",
        );
    }

    #[test]
    fn a_report_left_by_an_earlier_attempt_is_not_the_current_attempt_s_report() {
        let fixture = Fixture::new();
        let stale = fixture.write(
            TASK,
            1,
            "KTASK_RESULT: DONE\nSummary: attempt one's work.\n",
        );

        let reading = fixture.read(TASK, 2);

        let (path, class, detail) = missing(&reading);
        assert_eq!(
            path,
            expected(&fixture.project, "12", "2"),
            "attempt 2 was asked for and attempt 1's file was found; the retry's own path \
             is what the refusal has to name",
        );
        assert_eq!(class, FailureClass::AgentFailure);
        assert!(
            !detail.contains("attempt one's work")
                && !detail.contains(&stale.display().to_string()),
            "a refusal that quotes the previous attempt's report or path reads as if that \
             attempt had answered: {detail}",
        );
    }

    #[test]
    fn two_attempts_of_a_task_each_read_their_own_report() {
        let fixture = Fixture::new();
        fixture.write(TASK, 1, "KTASK_RESULT: DONE\nSummary: attempt one.\n");
        fixture.write(
            TASK,
            2,
            "KTASK_RESULT: FAILED\nSummary: attempt two stopped short.\n",
        );

        let first = fixture.read(TASK, 1);
        let second = fixture.read(TASK, 2);

        assert_eq!(
            claim(&first),
            ReportResult::Done,
            "the first attempt's account is still readable after the retry",
        );
        assert_eq!(
            claim(&second),
            ReportResult::Failed,
            "and the retry's own account is what a reader of attempt 2 is told",
        );
    }

    #[test]
    fn another_task_s_report_is_not_this_task_s_report() {
        let fixture = Fixture::new();
        fixture.write(TASK + 1, 1, AGENT_REPORT);

        let reading = fixture.read(TASK, 1);

        let (path, _, _) = missing(&reading);
        assert_eq!(
            path,
            expected(&fixture.project, "12", "1"),
            "task 13's report cannot answer for task 12: the directory level is the task's \
             own",
        );
    }

    #[test]
    fn a_report_the_agent_wrote_but_cannot_be_read_is_refused_naming_its_path() {
        let fixture = Fixture::new();
        fixture.write(
            TASK,
            1,
            "KTASK_RESULT: MAYBE\nSummary: a header nobody defined.\n",
        );

        let reading = fixture.read(TASK, 1);

        assert!(
            matches!(reading, Err(Error::Corrupt { .. })),
            "a report that is there and cannot be trusted is corrupt data (ADR-0076), not a \
             missing one and not a claim: {reading:?}"
        );
        let message = reading.expect_err("matched above").to_string();
        assert!(
            message.contains(&expected(&fixture.project, "12", "1").display().to_string()),
            "the refusal of an unreadable report has to say which file it could not read: \
             {message}",
        );
    }

    #[test]
    fn a_project_with_no_state_directory_has_no_report_to_read() {
        let fixture = Fixture::new();
        let unregistered = Project {
            root: fixture.repo.work().to_path_buf(),
            id: "ffffffffffffffff".to_owned(),
            state_dir: fixture.repo.path().join("never-registered"),
        };

        let reading = read_report(&unregistered, TaskId::new(TASK), AttemptId::new(1));

        let (path, class, _) = missing(&reading);
        assert_eq!(class, FailureClass::AgentFailure);
        assert_eq!(
            path,
            expected(&unregistered, "12", "1"),
            "a run that never got a state directory is answered the same way an absent \
             report is, and the path it names is the one an agent was told to write",
        );
    }

    #[test]
    fn a_report_replaced_on_disk_is_read_as_what_is_there_now() {
        let fixture = Fixture::new();
        fixture.write(TASK, 1, AGENT_REPORT);
        fixture.write(
            TASK,
            1,
            "KTASK_RESULT: FAILED\nSummary: the agent came back.\n",
        );

        let reading = fixture.read(TASK, 1);

        assert_eq!(
            claim(&reading),
            ReportResult::Failed,
            "the report is read when the provider exits, so whatever is on disk then is \
             the account of the attempt — a stale read of an earlier write would report \
             work the run has no evidence of",
        );
    }

    #[test]
    fn a_report_directory_is_a_directory_and_is_not_read_as_a_report() {
        let fixture = Fixture::new();
        let path = expected(&fixture.project, "12", "1");
        fs::create_dir_all(&path).expect("a path occupied by a directory is creatable");

        let reading = fixture.read(TASK, 1);

        assert!(
            matches!(reading, Err(Error::Io { .. })),
            "the place the report should be is held by a directory: that is the filesystem \
             refusing the read, not an absent report and not a corrupt one: {reading:?}"
        );
    }

    #[test]
    fn the_report_path_is_under_the_state_directory_and_not_the_repository() {
        let fixture = Fixture::new();

        let path = report_path(&fixture.project, TaskId::new(TASK), AttemptId::new(1));
        let repo = fixture.repo.work();

        assert!(
            path.starts_with(&fixture.project.state_dir),
            "the report is operational state (VISION.md §3's invariant 6): {path:?} is not \
             below the project's state directory",
        );
        assert!(
            !path.starts_with(repo),
            "a report path below the working copy would be caught by the privacy gate as an \
             artifact the run wrote on purpose: {path:?} is inside {repo:?}"
        );
        assert_eq!(
            path.strip_prefix(&fixture.project.state_dir)
                .ok()
                .and_then(|rest| rest.iter().next())
                .and_then(|first| first.to_str())
                .map(str::to_owned),
            Some("attempts".to_owned()),
            "the report joins the state directory through the `attempts` root rather than \
             being spelled onto it: {path:?}"
        );
    }
}
