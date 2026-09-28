//! The `echo` provider: a built-in [`Provider`] value that uses no model and no tokens. It
//! runs the first fenced `bash` code block of a prompt with `bash`, passing the token and
//! attempt number as positional arguments.

use ktask_core::{Provider, ProviderCommand};

/// The name the `echo` provider is known by.
pub const NAME: &str = "echo";

/// The `echo` provider.
pub const PROVIDER: Provider = Provider {
    name: NAME,
    command,
};

/// Turns `prompt` into the command that runs its first fenced `bash` code block with `bash`,
/// passing `token` as `$1` and `attempt` as `$2`.
///
/// # Errors
///
/// Fails when `prompt` has no fenced `bash` code block.
fn command(prompt: &str, token: &str, attempt: u32) -> Result<ProviderCommand, String> {
    let block = first_bash_block(prompt).ok_or_else(|| {
        "the prompt has no fenced bash code block for the echo provider to run".to_owned()
    })?;
    Ok(ProviderCommand {
        program: "bash".to_owned(),
        args: vec!["-s".to_owned(), token.to_owned(), attempt.to_string()],
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

    #[test]
    fn a_prompt_with_no_bash_block_is_an_error() {
        let result = command("just some text\n```python\nprint(1)\n```\n", "tok", 1);
        assert_eq!(
            result,
            Err("the prompt has no fenced bash code block for the echo provider to run".to_owned())
        );
    }

    #[test]
    fn the_first_bash_block_is_run_with_bash_and_the_token_and_attempt_as_positional_args() {
        let prompt = "before\n```bash\necho hi\n```\nafter\n";
        let built = command(prompt, "the-token", 3).unwrap();
        assert_eq!(built.program, "bash");
        assert_eq!(built.args, vec!["-s", "the-token", "3"]);
        assert_eq!(built.stdin, b"echo hi\n");
    }

    #[test]
    fn only_the_first_of_several_bash_blocks_is_run() {
        let prompt = "```bash\nfirst\n```\n```bash\nsecond\n```\n";
        let built = command(prompt, "t", 1).unwrap();
        assert_eq!(built.stdin, b"first\n");
    }

    #[test]
    fn a_block_with_no_closing_fence_runs_to_the_end_of_the_prompt() {
        let prompt = "```bash\necho a\necho b";
        let built = command(prompt, "t", 1).unwrap();
        assert_eq!(built.stdin, b"echo a\necho b\n");
    }

    #[test]
    fn the_provider_is_named_echo() {
        assert_eq!(PROVIDER.name, NAME);
        assert_eq!(NAME, "echo");
    }
}
