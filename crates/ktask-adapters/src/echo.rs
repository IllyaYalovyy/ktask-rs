//! The `echo` provider: a built-in [`Provider`] value that uses no tokens. It runs the first
//! fenced `bash` code block of a prompt with `bash`, passing the token, attempt number, step
//! name, a session to resume (and its transcript's path) and model as positional arguments —
//! and it keeps real session transcripts, so `retry --same-session` can be tested with no
//! model and no network. A script reports the session it ran in by printing a line
//! `KTASK_SESSION: <id>`; a script that never prints one gets no session recorded at all,
//! exactly as before this existed.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ktask_core::{LimitSignal, Output, Provider, ProviderCommand, Resume, StepCall};

/// The name the `echo` provider is known by.
pub const NAME: &str = "echo";

/// The line prefix a script's own standard output reports its session with: the rest of the
/// line, trimmed, is the session id.
const SESSION_PREFIX: &str = "KTASK_SESSION: ";

/// The line prefix a script's own standard output reports hitting its usage limit with: the
/// rest of the line, trimmed, is the limit's reset time as whole seconds since the epoch, or
/// empty when the script names no reset time at all. A test script prints this fixed line to
/// make the resolve role's own limit handling run with no model and no network.
const LIMIT_PREFIX: &str = "KTASK_LIMIT: ";

/// The `echo` provider. It supports resuming a session: a resumed invocation is told which
/// one, and where its transcript lives, as positional arguments; it reads the session an
/// invocation ran in back from its own standard output, and whether that output says its
/// usage limit was hit.
pub fn provider() -> Provider {
    Provider {
        name: NAME.to_owned(),
        command: Arc::new(command),
        supports_resume: true,
        read_session: Arc::new(read_session),
        detect_limit: Arc::new(detect_limit),
        read_usage: Arc::new(|_| ktask_core::ProviderUsage::default()),
        parse_output: Arc::new(|output| output),
        model_aliases: std::collections::BTreeMap::new(),
    }
}

/// The session a script reported running in, when its standard output has a line `KTASK_
/// SESSION: <id>` — the first one, when there is more than one. `None` when it reported none.
fn read_session(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        line.strip_prefix(SESSION_PREFIX)
            .map(|session| session.trim().to_owned())
    })
}

/// Whether a script reported hitting its usage limit, with a line `KTASK_LIMIT: <reset>` —
/// the first one, when there is more than one — `<reset>` the whole seconds since the epoch
/// its limit resets at, or empty when it names no reset time. `None` when it reported no such
/// line at all.
fn detect_limit(output: &Output) -> Option<LimitSignal> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(LIMIT_PREFIX))?;
    let reset_at = line
        .trim()
        .parse::<u64>()
        .ok()
        .map(|seconds| SystemTime::UNIX_EPOCH + Duration::from_secs(seconds));
    Some(LimitSignal { reset_at })
}

/// Turns `prompt` into the command that runs its first fenced `bash` code block with `bash`,
/// passing `call.token` as `$1`, `call.attempt` as `$2`, `call.step` as `$3`, the session
/// `call.resume` names to continue (empty when it is `None`) as `$4`, the path of that
/// session's transcript (empty when `call.resume` is `None`) as `$5`, `call.model` (empty when
/// it is `None`) as `$6`, and the path of `call.prompt_path` — the whole prompt this call runs,
/// not only the block run here — as `$7`, so a scripted prompt can prove to a test which
/// session and model it was run with, read back what an earlier invocation under the same
/// session wrote, and read everything its own prompt said, not only the block chosen to run.
///
/// A nudge's own prompt carries no block of its own — only the sentence and the exact report
/// commands a real agent would be asked with no script to run at all — so a resumed call with
/// none is run with the block its own session's earlier call ran instead, read back from its
/// own transcript: the same script, now able to see on `$4` that it is the one being resumed.
///
/// # Errors
///
/// Fails when `prompt` has no fenced `bash` code block, and `call.resume` names no session
/// whose own transcript has one either.
fn command(prompt: &str, call: StepCall<'_>) -> Result<ProviderCommand, String> {
    let block = first_bash_block(prompt)
        .or_else(|| resumed_block(call.resume))
        .ok_or_else(|| {
            "the prompt has no fenced bash code block for the echo provider to run".to_owned()
        })?;
    let (resume_session, resume_transcript) = match call.resume {
        Some(resume) => (
            resume.session.to_owned(),
            resume.transcript_path.display().to_string(),
        ),
        None => (String::new(), String::new()),
    };
    Ok(ProviderCommand {
        program: "bash".to_owned(),
        args: vec![
            "-s".to_owned(),
            call.token.to_owned(),
            call.attempt.to_string(),
            call.step.to_owned(),
            resume_session,
            resume_transcript,
            call.model.unwrap_or_default().to_owned(),
            call.prompt_path.display().to_string(),
        ],
        stdin: block.into_bytes(),
    })
}

/// The first fenced `bash` code block in the transcript of `resume`'s own session, when it
/// names one — the block an earlier call in that same session ran, kept under
/// `=== prompt ===` at the top of its own transcript file. `None` when `resume` is `None`, or
/// its transcript cannot be read or has no such block.
fn resumed_block(resume: Option<Resume<'_>>) -> Option<String> {
    let transcript = std::fs::read_to_string(resume?.transcript_path).ok()?;
    first_bash_block(&transcript)
}

/// The content of the first fenced `bash` code block in `prompt`, or `None` when it has none.
/// When the block's closing fence is missing, everything to the end of `prompt` is taken as
/// the block.
fn first_bash_block(prompt: &str) -> Option<String> {
    let mut lines = prompt.lines();
    for line in lines.by_ref() {
        if line.trim() != "```bash" {
            continue;
        }
        let mut block = Vec::new();
        for line in lines.by_ref() {
            if line.trim() == "```" {
                break;
            }
            block.push(line);
        }
        block.push("");
        return Some(block.join("\n"));
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn call<'a>(token: &'a str, attempt: u32, step: &'a str) -> StepCall<'a> {
        StepCall {
            token,
            attempt,
            step,
            model: None,
            resume: None,
            prompt_path: Path::new("/state/prompts/the-prompt.prompt"),
            project_dir: Path::new("/work/app"),
        }
    }

    #[test]
    fn a_prompt_with_no_bash_block_is_an_error() {
        let result = command(
            "just some text\n```ruby\nputs 1\n```\n",
            call("tok", 1, "implementation"),
        );
        assert_eq!(
            result,
            Err("the prompt has no fenced bash code block for the echo provider to run".to_owned())
        );
    }

    #[test]
    fn a_prompt_with_no_block_but_a_resume_runs_the_block_from_its_own_transcript() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let transcript_path = dir.path().join("the-session.log");
        std::fs::write(
            &transcript_path,
            "=== prompt ===\n# Review\n```bash\necho from-transcript\n```\n\n=== output ===\nhi\n",
        )
        .expect("a written transcript");
        let built = command(
            "You ended without running the report command. Run exactly one of these now:\n",
            StepCall {
                token: "tok",
                attempt: 1,
                step: "review",
                model: None,
                resume: Some(Resume {
                    session: "the-session",
                    transcript_path: &transcript_path,
                }),
                prompt_path: Path::new("/state/prompts/the-prompt.prompt"),
                project_dir: Path::new("/work/app"),
            },
        )
        .unwrap();
        assert_eq!(built.stdin, b"echo from-transcript\n");
    }

    #[test]
    fn a_prompt_with_no_block_and_an_unreadable_transcript_is_still_an_error() {
        let result = command(
            "nothing to run here\n",
            StepCall {
                token: "tok",
                attempt: 1,
                step: "review",
                model: None,
                resume: Some(Resume {
                    session: "missing-session",
                    transcript_path: Path::new("/does/not/exist.log"),
                }),
                prompt_path: Path::new("/state/prompts/the-prompt.prompt"),
                project_dir: Path::new("/work/app"),
            },
        );
        assert_eq!(
            result,
            Err("the prompt has no fenced bash code block for the echo provider to run".to_owned())
        );
    }

    #[test]
    fn the_first_bash_block_is_run_with_bash_and_the_token_attempt_and_step_as_positional_args() {
        let prompt = "before\n```bash\necho hi\n```\nafter\n";
        let built = command(prompt, call("the-token", 3, "implementation")).unwrap();
        assert_eq!(built.program, "bash");
        assert_eq!(
            built.args,
            vec![
                "-s",
                "the-token",
                "3",
                "implementation",
                "",
                "",
                "",
                "/state/prompts/the-prompt.prompt",
            ]
        );
        assert_eq!(built.stdin, b"echo hi\n");
    }

    #[test]
    fn the_model_is_passed_as_a_sixth_positional_arg_when_there_is_one() {
        let prompt = "```bash\necho hi\n```\n";
        let built = command(
            prompt,
            StepCall {
                token: "the-token",
                attempt: 3,
                step: "implementation",
                model: Some("opus"),
                resume: None,
                prompt_path: Path::new("/state/prompts/the-prompt.prompt"),
                project_dir: Path::new("/work/app"),
            },
        )
        .unwrap();
        assert_eq!(
            built.args,
            vec![
                "-s",
                "the-token",
                "3",
                "implementation",
                "",
                "",
                "opus",
                "/state/prompts/the-prompt.prompt",
            ]
        );
    }

    #[test]
    fn a_resumed_invocation_gets_the_session_and_transcript_path_as_the_fourth_and_fifth_args() {
        let prompt = "```bash\necho hi\n```\n";
        let built = command(
            prompt,
            StepCall {
                token: "the-token",
                attempt: 3,
                step: "implementation",
                model: None,
                resume: Some(Resume {
                    session: "the-session",
                    transcript_path: Path::new("/state/sessions/the-session.log"),
                }),
                prompt_path: Path::new("/state/prompts/the-prompt.prompt"),
                project_dir: Path::new("/work/app"),
            },
        )
        .unwrap();
        assert_eq!(
            built.args,
            vec![
                "-s",
                "the-token",
                "3",
                "implementation",
                "the-session",
                "/state/sessions/the-session.log",
                "",
                "/state/prompts/the-prompt.prompt",
            ]
        );
    }

    #[test]
    fn only_the_first_bash_block_among_several_languages_is_run() {
        let prompt = "```ruby\nputs 1\n```\n```bash\nfirst\n```\n```perl\nprint 2\n```\n```bash\nsecond\n```\n";
        let built = command(prompt, call("t", 1, "implementation")).unwrap();
        assert_eq!(built.stdin, b"first\n");
    }

    #[test]
    fn a_block_with_no_closing_fence_runs_to_the_end_of_the_prompt() {
        let prompt = "```bash\necho a\necho b";
        let built = command(prompt, call("t", 1, "implementation")).unwrap();
        assert_eq!(built.stdin, b"echo a\necho b\n");
    }

    #[test]
    fn the_provider_is_named_echo() {
        assert_eq!(provider().name, NAME);
        assert_eq!(NAME, "echo");
    }

    fn output(stdout: &[u8]) -> Output {
        Output {
            stdout: stdout.to_vec(),
            stderr: Vec::new(),
            exit: ktask_core::Exit::Code(0),
        }
    }

    #[test]
    fn a_script_that_never_reports_a_session_has_none_read_back() {
        assert_eq!(read_session(&output(b"just some output\n")), None);
        assert_eq!(read_session(&output(b"")), None);
    }

    #[test]
    fn a_script_that_reports_a_session_has_it_read_back() {
        assert_eq!(
            read_session(&output(b"before\nKTASK_SESSION: abc-123\nafter\n")),
            Some("abc-123".to_owned())
        );
    }

    #[test]
    fn the_first_reported_session_wins_when_there_is_more_than_one() {
        assert_eq!(
            read_session(&output(b"KTASK_SESSION: first\nKTASK_SESSION: second\n")),
            Some("first".to_owned())
        );
    }

    #[test]
    fn a_script_that_never_reports_a_limit_has_none_detected() {
        assert_eq!(detect_limit(&output(b"just some output\n")), None);
        assert_eq!(detect_limit(&output(b"")), None);
    }

    #[test]
    fn a_script_that_reports_a_limit_with_a_reset_time_has_it_read_back() {
        assert_eq!(
            detect_limit(&output(b"before\nKTASK_LIMIT: 1700000000\nafter\n")),
            Some(LimitSignal {
                reset_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
            })
        );
    }

    #[test]
    fn a_script_that_reports_a_limit_with_no_reset_time_names_none() {
        assert_eq!(
            detect_limit(&output(b"KTASK_LIMIT: \n")),
            Some(LimitSignal { reset_at: None })
        );
    }
}
