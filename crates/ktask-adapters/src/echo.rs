//! The `echo` provider: a built-in [`Provider`] value that uses no tokens. It runs the first
//! fenced `bash` code block of a prompt with `bash`, passing the token, attempt number, step
//! name, a session to resume (and its transcript's path) and model as positional arguments —
//! and it keeps real session transcripts, so `retry --same-session` can be tested with no
//! model and no network. A script reports the session it ran in by printing a line
//! `KTASK_SESSION: <id>`; a script that never prints one gets no session recorded at all,
//! exactly as before this existed.

use ktask_core::{Output, Provider, ProviderCommand, StepCall};

/// The name the `echo` provider is known by.
pub const NAME: &str = "echo";

/// The line prefix a script's own standard output reports its session with: the rest of the
/// line, trimmed, is the session id.
const SESSION_PREFIX: &str = "KTASK_SESSION: ";

/// The `echo` provider. It supports resuming a session: a resumed invocation is told which
/// one, and where its transcript lives, as positional arguments; it reads the session an
/// invocation ran in back from its own standard output.
pub const PROVIDER: Provider = Provider {
    name: NAME,
    command,
    supports_resume: true,
    read_session,
};

/// The session a script reported running in, when its standard output has a line `KTASK_
/// SESSION: <id>` — the first one, when there is more than one. `None` when it reported none.
fn read_session(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().find_map(|line| {
        line.strip_prefix(SESSION_PREFIX)
            .map(|session| session.trim().to_owned())
    })
}

/// Turns `prompt` into the command that runs its first fenced `bash` code block with `bash`,
/// passing `call.token` as `$1`, `call.attempt` as `$2`, `call.step` as `$3`, the session
/// `call.resume` names to continue (empty when it is `None`) as `$4`, the path of that
/// session's transcript (empty when `call.resume` is `None`) as `$5`, and `call.model` (empty
/// when it is `None`) as `$6` — so a scripted prompt can prove to a test which session and
/// model it was run with, and read back what an earlier invocation under the same session
/// wrote.
///
/// # Errors
///
/// Fails when `prompt` has no fenced `bash` code block.
fn command(prompt: &str, call: StepCall<'_>) -> Result<ProviderCommand, String> {
    let block = first_bash_block(prompt).ok_or_else(|| {
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
        ],
        stdin: block.into_bytes(),
    })
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
    use super::*;

    fn call<'a>(token: &'a str, attempt: u32, step: &'a str) -> StepCall<'a> {
        StepCall {
            token,
            attempt,
            step,
            model: None,
            resume: None,
        }
    }

    #[test]
    fn a_prompt_with_no_bash_block_is_an_error() {
        let result = command(
            "just some text\n```python\nprint(1)\n```\n",
            call("tok", 1, "implementation"),
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
            vec!["-s", "the-token", "3", "implementation", "", "", ""]
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
            },
        )
        .unwrap();
        assert_eq!(
            built.args,
            vec!["-s", "the-token", "3", "implementation", "", "", "opus"]
        );
    }

    #[test]
    fn a_resumed_invocation_gets_the_session_and_transcript_path_as_the_fourth_and_fifth_args() {
        use std::path::Path;

        let prompt = "```bash\necho hi\n```\n";
        let built = command(
            prompt,
            StepCall {
                token: "the-token",
                attempt: 3,
                step: "implementation",
                model: None,
                resume: Some(ktask_core::Resume {
                    session: "the-session",
                    transcript_path: Path::new("/state/sessions/the-session.log"),
                }),
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
            ]
        );
    }

    #[test]
    fn only_the_first_of_several_bash_blocks_is_run() {
        let prompt = "```bash\nfirst\n```\n```bash\nsecond\n```\n";
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
        assert_eq!(PROVIDER.name, NAME);
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
}
