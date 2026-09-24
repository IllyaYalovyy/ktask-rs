//! The getting-started guide (`docs/GUIDE.md`, T157) is executable
//! documentation: the tests here run its commands, in order, in a scratch
//! shell and compare what they print with what the guide says they print, and
//! check that every command and flag it names exists.
//!
//! A ```` ```console ```` block is a terminal transcript. A line starting with
//! `$ ` is a command typed at the prompt (`> ` lines continue it, as a shell
//! prints them for a here-document), and the lines up to the next command are
//! its expected output, standard output and standard error together, as a
//! terminal shows them. The blocks share one shell, so the variables and the
//! working directory one command sets are there for the next, exactly as they
//! are for a reader following along.
//!
//! Three things differ from one machine, or one run, to the next and are
//! compared as placeholders on both sides: project ids, commit ids and
//! timestamps (see [`guide::normalize`]). Nothing else is matched loosely.

mod docs {
    mod guide {
        use std::io::{BufRead, BufReader, Write};
        use std::path::{Path, PathBuf};
        use std::process::{Child, ChildStdin, Command, Stdio};
        use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
        use std::time::Duration;

        const GUIDE: &str = include_str!("../../../docs/GUIDE.md");
        const README: &str = include_str!("../../../README.md");

        /// The command the task, and the guide's own introduction, name for
        /// running these tests.
        const VERIFY: &str = "cargo nextest run -p ktask-cli -E 'test(/docs::guide/)'";

        /// Printed by the scratch shell after each command, so the reader
        /// knows where that command's output ends.
        const DONE: &str = "__KTASK_DOCS_DONE__";

        /// The longest a single command in the guide may take.
        const COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

        /// The directory holding the compiled `ktask-rs`, put first on the
        /// scratch shell's `PATH` so `ktask-rs` in the guide is this build.
        fn bin_dir() -> PathBuf {
            Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_default()
        }

        // ---- reading the guide -------------------------------------------

        /// One fenced block: its info string (`console`, `sh`, ...) and the
        /// lines between the fences.
        struct Block<'a> {
            info: &'a str,
            lines: Vec<&'a str>,
        }

        fn blocks(text: &str) -> Vec<Block<'_>> {
            let mut found = Vec::new();
            let mut open: Option<Block<'_>> = None;
            for line in text.lines() {
                match (line.strip_prefix("```"), open.take()) {
                    (Some(_), Some(block)) => found.push(block),
                    (Some(info), None) => {
                        open = Some(Block {
                            info: info.trim(),
                            lines: Vec::new(),
                        });
                    }
                    (None, Some(mut block)) => {
                        block.lines.push(line);
                        open = Some(block);
                    }
                    (None, None) => {}
                }
            }
            found
        }

        /// One command of a transcript and the output shown beneath it.
        struct Step {
            /// The command's lines as a shell reads them, prompts removed.
            command: Vec<String>,
            expected: Vec<String>,
        }

        fn steps(block: &Block<'_>) -> Result<Vec<Step>, String> {
            let mut steps: Vec<Step> = Vec::new();
            for line in &block.lines {
                if let Some(command) = line.strip_prefix("$ ") {
                    steps.push(Step {
                        command: vec![command.to_string()],
                        expected: Vec::new(),
                    });
                    continue;
                }
                let Some(step) = steps.last_mut() else {
                    return Err(format!("output before any command: {line:?}"));
                };
                let continuation = line
                    .strip_prefix("> ")
                    .or_else(|| (*line == ">").then_some(""));
                match continuation {
                    Some(text) if step.expected.is_empty() => step.command.push(text.to_string()),
                    _ => step.expected.push((*line).to_string()),
                }
            }
            Ok(steps)
        }

        // ---- comparing output --------------------------------------------

        /// True for a run of characters that can make up a timestamp.
        fn is_time_char(c: char) -> bool {
            c.is_ascii_digit() || matches!(c, '-' | ':' | '.' | 'T' | 'Z')
        }

        fn is_lower_hex(word: &str) -> bool {
            word.chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        }

        /// Replaces every RFC 3339 UTC timestamp with `<timestamp>`.
        fn mask_timestamps(line: &str) -> String {
            let mut out = String::new();
            let mut run = String::new();
            let flush = |run: &mut String, out: &mut String| {
                let stamp = run.len() >= 20
                    && run.ends_with('Z')
                    && run.contains('T')
                    && run.starts_with(|c: char| c.is_ascii_digit());
                out.push_str(if stamp { "<timestamp>" } else { run });
                run.clear();
            };
            for c in line.chars() {
                if is_time_char(c) {
                    run.push(c);
                } else {
                    flush(&mut run, &mut out);
                    out.push(c);
                }
            }
            flush(&mut run, &mut out);
            out
        }

        /// Replaces project ids (16 hex digits) with `<id>` and abbreviated
        /// commit ids (7 to 12 hex digits, at least one a digit, which no
        /// English word of that shape has) with `<sha>`.
        fn mask_ids(line: &str) -> String {
            let mut out = String::new();
            let mut word = String::new();
            let flush = |word: &mut String, out: &mut String| {
                let placeholder = if word.len() == 16 && is_lower_hex(word) {
                    Some("<id>")
                } else if (7..=12).contains(&word.len())
                    && is_lower_hex(word)
                    && word.chars().any(|c| c.is_ascii_digit())
                {
                    Some("<sha>")
                } else {
                    None
                };
                out.push_str(placeholder.unwrap_or(word));
                word.clear();
            };
            for c in line.chars() {
                if c.is_ascii_alphanumeric() {
                    word.push(c);
                } else {
                    flush(&mut word, &mut out);
                    out.push(c);
                }
            }
            flush(&mut word, &mut out);
            out
        }

        /// Replaces the path of the project's state directory, wherever the
        /// machine keeps it, with `<state-dir>`.
        fn mask_state_dir(line: &str) -> String {
            const TAIL: &str = "/ktask-rs/<id>";
            let Some(found) = line.find(TAIL) else {
                return line.to_string();
            };
            let start = line[..found]
                .rfind(|c: char| c.is_whitespace() || c == '"')
                .map_or(0, |at| at + 1);
            format!(
                "{}<state-dir>{}",
                &line[..start],
                &line[found + TAIL.len()..]
            )
        }

        /// A line with the parts that legitimately vary replaced by
        /// placeholders, so the guide's line and the real one compare equal.
        fn normalize(line: &str) -> String {
            mask_state_dir(&mask_ids(&mask_timestamps(line)))
        }

        // ---- running the guide -------------------------------------------

        /// A scratch `sh` whose environment is isolated from the developer's
        /// (own home, state and config directories, no git identity or global
        /// git config) and whose standard output and error are one stream.
        struct Session {
            child: Child,
            stdin: ChildStdin,
            output: Receiver<String>,
            _dir: tempfile::TempDir,
        }

        impl Session {
            fn start(bin_dir: &Path) -> Result<Self, String> {
                let dir = tempfile::tempdir().map_err(|e| e.to_string())?;
                for name in ["home", "state", "config", "work"] {
                    std::fs::create_dir(dir.path().join(name)).map_err(|e| e.to_string())?;
                }
                let path = std::env::var_os("PATH").unwrap_or_default();
                let path = std::env::join_paths(
                    std::iter::once(bin_dir.to_path_buf()).chain(std::env::split_paths(&path)),
                )
                .map_err(|e| e.to_string())?;

                // Pinned dates make the demo repository's commit ids the
                // same on every machine, so a test failure is never a fluke.
                let date = "2026-01-01T00:00:00Z";
                let mut child = Command::new("sh")
                    .args(["-c", "exec sh 2>&1"])
                    .current_dir(dir.path().join("work"))
                    .env("PATH", path)
                    .env("HOME", dir.path().join("home"))
                    .env("XDG_STATE_HOME", dir.path().join("state"))
                    .env("XDG_CONFIG_HOME", dir.path().join("config"))
                    .env("NO_COLOR", "1")
                    .env("GIT_CONFIG_GLOBAL", "/dev/null")
                    .env("GIT_CONFIG_NOSYSTEM", "1")
                    .env("GIT_AUTHOR_NAME", "Guide Reader")
                    .env("GIT_AUTHOR_EMAIL", "reader@example.com")
                    .env("GIT_COMMITTER_NAME", "Guide Reader")
                    .env("GIT_COMMITTER_EMAIL", "reader@example.com")
                    .env("GIT_AUTHOR_DATE", date)
                    .env("GIT_COMMITTER_DATE", date)
                    .env_remove("EDITOR")
                    .env_remove("VISUAL")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .map_err(|e| format!("cannot start sh: {e}"))?;
                let stdin = child.stdin.take().ok_or("sh has no stdin")?;
                let stdout = child.stdout.take().ok_or("sh has no stdout")?;

                let (send, output) = channel();
                std::thread::spawn(move || {
                    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                        if send.send(line).is_err() {
                            break;
                        }
                    }
                });
                Ok(Session {
                    child,
                    stdin,
                    output,
                    _dir: dir,
                })
            }

            /// Types `command` into the shell and returns what it printed.
            /// The shell's `$?` afterwards is the command's own status.
            fn run(&mut self, command: &[String]) -> Result<Vec<String>, String> {
                let script = format!(
                    "{}\n__s=$?; printf '%s\\n' {DONE}; (exit $__s)\n",
                    command.join("\n")
                );
                self.stdin
                    .write_all(script.as_bytes())
                    .and_then(|()| self.stdin.flush())
                    .map_err(|e| format!("cannot write to sh: {e}"))?;

                let mut printed = Vec::new();
                loop {
                    let line = self.output.recv_timeout(COMMAND_TIMEOUT).map_err(|e| {
                        match e {
                            RecvTimeoutError::Timeout => "the command timed out",
                            RecvTimeoutError::Disconnected => "the shell exited",
                        }
                        .to_string()
                    })?;
                    match line.strip_suffix(DONE) {
                        Some("") => return Ok(printed),
                        // Output that did not end in a newline shares the
                        // marker's line.
                        Some(rest) => {
                            printed.push(rest.to_string());
                            return Ok(printed);
                        }
                        None => printed.push(line),
                    }
                }
            }
        }

        impl Drop for Session {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }

        /// Runs every `console` block of `guide`, in order, in one shell and
        /// compares each command's output with the one shown. Returns how
        /// many commands it ran, or describes the first that differed.
        fn replay(guide: &str, bin_dir: &Path) -> Result<usize, String> {
            let mut session = Session::start(bin_dir)?;
            let mut ran = 0;
            for block in blocks(guide).iter().filter(|b| b.info == "console") {
                for step in steps(block)? {
                    let actual = session.run(&step.command)?;
                    let shown = step.command.join("\n");
                    let same = actual.len() == step.expected.len()
                        && actual
                            .iter()
                            .zip(&step.expected)
                            .all(|(a, e)| normalize(a) == normalize(e));
                    if !same {
                        return Err(format!(
                            "`{shown}` printed:\n{}\nbut the guide shows:\n{}",
                            actual.join("\n"),
                            step.expected.join("\n")
                        ));
                    }
                    ran += 1;
                }
            }
            Ok(ran)
        }

        // ---- the commands the guide names --------------------------------

        /// Everything that follows `ktask-rs` wherever the guide writes an
        /// invocation, in document order: inline code in prose and the
        /// commands of console blocks.
        fn invocations(guide: &str) -> Vec<String> {
            let mut snippets: Vec<String> = Vec::new();
            let mut fence: Option<&str> = None;
            for line in guide.lines() {
                match (line.strip_prefix("```"), fence) {
                    (Some(_), Some(_)) => fence = None,
                    (Some(info), None) => fence = Some(info.trim()),
                    (None, Some("console")) => {
                        snippets.extend(line.strip_prefix("$ ").map(str::to_string));
                    }
                    (None, Some(_)) => {}
                    (None, None) => {
                        snippets.extend(line.split('`').skip(1).step_by(2).map(str::to_string));
                    }
                }
            }
            snippets
                .iter()
                .flat_map(|snippet| snippet.split("ktask-rs").skip(1).map(str::to_string))
                .collect()
        }

        /// What follows `ktask-rs` in an invocation: the command path (`run`,
        /// `plan lint`) and the long flags, up to the first shell operator.
        fn command_and_flags(after: &str) -> (Vec<String>, Vec<String>) {
            let mut path: Vec<String> = Vec::new();
            let mut flags = Vec::new();
            let mut path_done = false;
            for word in after.split_whitespace() {
                if matches!(word, "|" | "||" | "&&" | ";" | ">" | ">>" | "<" | "2>&1") {
                    break;
                }
                if let Some(flag) = word.strip_prefix("--") {
                    let name: String = flag
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                        .collect();
                    flags.push(format!("--{name}"));
                    path_done = true;
                } else if !path_done && word.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                    path.push(word.to_string());
                    // Only `plan` has a subcommand of its own.
                    path_done = path.first().map(String::as_str) != Some("plan") || path.len() == 2;
                } else {
                    path_done = true;
                }
            }
            (path, flags)
        }

        fn help_of(args: &[String]) -> Result<String, String> {
            let output = Command::new(env!("CARGO_BIN_EXE_ktask-rs"))
                .args(args)
                .arg("--help")
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!(
                    "`ktask-rs {} --help` failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr)
                ));
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        }

        // ---- the tests ---------------------------------------------------

        /// The whole guide, run for real: every command prints what the guide
        /// says it prints. This is the test that makes "real command output"
        /// true rather than a claim.
        #[test]
        fn guide_transcripts_are_what_the_commands_really_print() {
            let ran = replay(GUIDE, &bin_dir()).expect("the guide must be reproducible");

            assert!(ran >= 20, "the guide runs its walk-through, ran {ran}");
        }

        /// The task's own verify command selects these tests only because
        /// they live in `docs::guide`; renaming the modules would leave it
        /// selecting nothing and passing.
        #[test]
        fn guide_tests_are_found_by_the_verify_command() {
            assert!(module_path!().contains("docs::guide"), "{}", module_path!());
            assert!(
                GUIDE.contains(VERIFY),
                "the guide names the command that verifies it"
            );
        }

        /// The walk-through covers install, init, add, plan lint, run and
        /// status, and in that order: each is first used after the one before.
        #[test]
        fn guide_walks_through_the_commands_in_order() {
            let text: Vec<Vec<String>> = invocations(GUIDE)
                .iter()
                .map(|after| command_and_flags(after).0)
                .collect();
            let first_use = |wanted: &[&str]| {
                text.iter()
                    .position(|path| path.iter().map(String::as_str).eq(wanted.iter().copied()))
            };

            let order: Vec<Option<usize>> = [
                &["init"][..],
                &["add"],
                &["plan", "lint"],
                &["run"],
                &["status"],
            ]
            .iter()
            .map(|command| first_use(command))
            .collect();

            assert!(
                order.iter().all(Option::is_some),
                "every step is shown, got {order:?}"
            );
            assert!(
                order.windows(2).all(|pair| pair[0] < pair[1]),
                "steps come in the order a user takes them, got {order:?}"
            );
            let install = GUIDE.find("cargo install").expect("an install command");
            let init = GUIDE.find("$ ktask-rs init").expect("an init command");
            assert!(install < init, "install comes before init");
        }

        /// Every command and every long flag the guide mentions, in prose or
        /// in a transcript, exists in the binary's own help.
        #[test]
        fn guide_names_only_commands_and_flags_that_exist() {
            let top = help_of(&[]).expect("top-level help");
            let mut checked = 0;
            for after in invocations(GUIDE) {
                let (path, flags) = command_and_flags(&after);
                let help = if path.is_empty() {
                    top.clone()
                } else {
                    help_of(&path).unwrap_or_else(|why| panic!("{why}"))
                };
                for flag in &flags {
                    assert!(
                        help.contains(flag.as_str()) || top.contains(flag.as_str()),
                        "`ktask-rs {}` has no {flag}: {after:?}",
                        path.join(" ")
                    );
                    checked += 1;
                }
            }

            assert!(checked >= 6, "the guide names several flags, got {checked}");
        }

        /// The installation command names a real package, and that package
        /// builds the `ktask-rs` binary the rest of the guide runs.
        #[test]
        fn guide_install_command_builds_the_binary_it_then_uses() {
            let block = blocks(GUIDE)
                .into_iter()
                .find(|b| b.info == "sh")
                .expect("an install block");
            let line = block.lines.first().expect("an install command");
            let words: Vec<&str> = line.split_whitespace().collect();
            assert_eq!(&words[..2], ["cargo", "install"], "{line}");
            assert!(words.contains(&"--locked"), "installs the locked build");
            let at = words
                .iter()
                .position(|w| *w == "--path")
                .expect("installs from a path");

            let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(words[at + 1])
                .join("Cargo.toml");
            let manifest = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
            assert!(
                manifest.contains("name = \"ktask-rs\""),
                "the package installs a binary named ktask-rs"
            );
        }

        /// The README leads a new reader to the guide, and the link resolves.
        #[test]
        fn guide_is_linked_from_the_readme() {
            assert!(README.contains("(docs/GUIDE.md)"), "README links the guide");
            let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/GUIDE.md");
            assert!(guide.is_file(), "{} exists", guide.display());
        }

        // ---- the checker checks ------------------------------------------

        /// A transcript that shows different output than the command prints
        /// is rejected, naming the command; one that matches is accepted.
        /// Without this a `replay` that compared nothing would pass.
        #[test]
        fn guide_replay_rejects_output_the_command_did_not_print() {
            let good = "```console\n$ echo hello\nhello\n```\n";
            let bad = "```console\n$ echo hello\ngoodbye\n```\n";

            assert_eq!(replay(good, &bin_dir()), Ok(1));
            let err = replay(bad, &bin_dir()).expect_err("must differ");
            assert!(err.contains("echo hello"), "{err}");
            assert!(err.contains("goodbye"), "{err}");
        }

        /// Output the guide leaves out, or adds, is a difference too.
        #[test]
        fn guide_replay_rejects_missing_and_extra_output_lines() {
            let missing = "```console\n$ printf 'a\\nb\\n'\na\n```\n";
            let extra = "```console\n$ printf 'a\\n'\na\nb\n```\n";

            assert!(replay(missing, &bin_dir()).is_err());
            assert!(replay(extra, &bin_dir()).is_err());
        }

        /// The shell is shared across commands and blocks, standard error is
        /// part of what is compared, a command's exit status survives to the
        /// next command, and a `> ` line continues a command.
        #[test]
        fn guide_replay_shares_one_shell_and_merges_stderr() {
            let guide = "```console\n\
                $ cd /\n\
                $ WHO=reader\n\
                ```\n\n\
                ```console\n\
                $ pwd\n\
                /\n\
                $ echo \"$WHO\" >&2\n\
                reader\n\
                $ false\n\
                $ echo $?\n\
                1\n\
                $ cat <<EOF\n\
                > one\n\
                >\n\
                > EOF\n\
                one\n\
                \n\
                ```\n";

            assert_eq!(replay(guide, &bin_dir()), Ok(7));
        }

        /// Output ending without a newline is still seen, not swallowed.
        #[test]
        fn guide_replay_sees_output_without_a_final_newline() {
            let guide = "```console\n$ printf partial\npartial\n```\n";

            assert_eq!(replay(guide, &bin_dir()), Ok(1));
        }

        /// Variable parts compare as placeholders; nothing else does.
        #[test]
        fn guide_normalize_masks_ids_commits_timestamps_and_the_state_path() {
            assert_eq!(normalize("project: 3f2a9c1d5b7e4a60"), "project: <id>");
            assert_eq!(
                normalize("state: /home/you/.local/state/ktask-rs/3f2a9c1d5b7e4a60"),
                "state: <state-dir>"
            );
            assert_eq!(
                normalize("state: /tmp/x/state/ktask-rs/00112233445566ff"),
                normalize("state: /var/lib/state/ktask-rs/3f2a9c1d5b7e4a60"),
            );
            assert_eq!(
                normalize("task 1: publishing 20c088b"),
                "task 1: publishing <sha>"
            );
            assert_eq!(
                normalize(r#""started_at":"2026-09-24T14:54:19.040177723Z","attempts":1"#),
                r#""started_at":"<timestamp>","attempts":1"#
            );
        }

        /// Words that merely look like ids stay as they are, so a wrong word
        /// in the guide still fails.
        #[test]
        fn guide_normalize_leaves_ordinary_words_and_numbers_alone() {
            for line in [
                "task 1: attempt 1 finished (exit 0)",
                "summary: Done=2",
                "effaced decade",
                "1234567890123456789",
                "-1 2026-09-24",
            ] {
                assert_eq!(normalize(line), line);
            }
            assert_eq!(
                normalize("state: a/ktask-rs/3f2a9c1d5b7e4a60/x"),
                "state: <state-dir>/x"
            );
        }

        /// A guide with no transcript is not a passing guide.
        #[test]
        fn guide_replay_of_prose_alone_runs_nothing() {
            assert_eq!(replay("# Title\n\nJust words.\n", &bin_dir()), Ok(0));
        }
    }
}
