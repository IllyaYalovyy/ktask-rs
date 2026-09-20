//! The Codex adapter: the words the Codex CLI is started with, and what one
//! session leaves behind.
//!
//! VISION.md §12 names Codex and Claude as the two launch adapters, and an
//! adapter is only ever the two things that differ between two CLIs: which words
//! start the process, and which door the prompt goes through. Everything else —
//! output as it is read, the two clocks, the process group, the capture bound,
//! the shape of an answer — belongs to [`run_streaming`], which is deliberately
//! indifferent to which CLI it runs. That is what makes this file reviewable as
//! a diff against `claude.rs`: a difference between the two adapters found
//! anywhere but in those two files is a defect in one of them, which is also
//! what T060's done-when says from the other side.
//!
//! The door is standard input again, for the same reason: a prompt is prose, and
//! prose sent as a command-line argument is bounded by what one `exec` call
//! accepts and readable by whoever runs `ps` — where VISION.md §11 keeps prompts
//! out of repositories and out of what a run logs. Codex *names* the door rather
//! than assuming it: the trailing `-` operand is what tells the CLI to read its
//! prompt from what this process writes and then closes.
//!
//! Three words are the difference from Claude, and each of the three is Codex's
//! interface rather than this file's preference.
//!
//! **`exec` is a subcommand, not a flag.** Bare `codex` opens the CLI's own
//! interactive TUI, and a TUI waiting for the next human keystroke is a session
//! this supervisor could only end on a clock. `exec` is the word that asks for
//! one instruction, one answer, and an exit, so it goes first — a subcommand
//! placed after an option is the CLI's own parser reading the wrong sentence.
//!
//! **Approvals and the CLI's own sandbox are set aside by one long word**
//! (`SANDBOX_BYPASS_FLAG`), where Claude takes a mode name after
//! `--permission-mode`. The reason is the same one ADR-0054 recorded there: a run
//! has no human answering an approval prompt, so a prompt is not a safety
//! property here — it is a hang with a question mark on it. What stands between
//! an agent's wish and an effect is the mechanical gates and the git transaction
//! model (VISION.md §3, §10), and neither runs inside the session, so neither is
//! bypassed by any word on this command line. The word's own name is the
//! CLI telling everyone how load-bearing it is, which is why it is written out
//! here rather than assembled from a shorter one.
//!
//! **The directory is an argument as well as a `chdir`.** `-C <dir>` is how Codex
//! is told where to work, and [`run_streaming`] separately starts the child with
//! `current_dir` set to the same place. Handing over the identical value twice is
//! not redundancy: the CLI resolves what it was given *after* the `chdir` already
//! happened, so a relative `Invocation::working_dir` would be resolved against
//! itself and land somewhere neither the caller nor the attempt record named. So
//! the value this file sends is made absolute first, by the same rule `locate`
//! uses for the program word — the directory the check saw is the directory the
//! session works in.
//!
//! Two rules are the provider layer's rather than Codex's, and they are kept here
//! in the shape ADR-0054 gave them: the configured command is located and checked
//! *before* anything is spawned, so a missing executable is answered as a provider
//! configuration failure that no retry can fix; and a session's failure is given
//! back attributed to `codex` rather than to the absolute path the located command
//! became. ADR-0055 records that both rules now stand in two adapters, and that
//! hoisting them into the shared half is a later task's, not this one's.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::process::run_streaming;
use super::{Capabilities, Invocation, Outcome, Provider};
use crate::paths::process_env;
use crate::{Bus, Error, Result};

/// The name an operator writes in configuration (`provider = "codex"`) and reads
/// in the TUI, and the name a refusal is attributed to.
const PROVIDER_NAME: &str = "codex";

/// Ask for one instruction, answered and exited, rather than a conversation.
///
/// Without it the CLI opens its own interactive TUI, and a TUI waiting for the
/// next human keystroke is a session this supervisor would have to kill on a
/// clock — the hang VISION.md §16 ranks first, manufactured by our own argv.
const EXEC_SUBCOMMAND: &str = "exec";

/// The word that runs a session with nothing waited on and nothing sandboxed.
///
/// No human is present to answer an approval prompt, and the CLI's own sandbox
/// cannot let an agent edit the task worktree it exists to work in. What stands
/// between an agent's wish and an effect is the gates and the publication
/// transaction (VISION.md §3, §10), which live outside the session and so are
/// untouched by this word.
const SANDBOX_BYPASS_FLAG: &str = "--dangerously-bypass-approvals-and-sandbox";

/// Tell the CLI it is not required to be standing in a git repository.
///
/// The worktree it runs in *is* one, but the CLI's check is about *its* safety
/// assumption — that a session may write to whatever checkout surrounds it — and
/// ktask keeps the repository's own safety in its publication transaction
/// (VISION.md §10). Refusing to start a session in a worktree that is a
/// repository but whose state the CLI does not like would fail a run for a
/// reason the supervisor already covers.
const SKIP_GIT_REPO_CHECK_FLAG: &str = "--skip-git-repo-check";

/// The flag that names the directory the session works in.
const WORKING_DIR_FLAG: &str = "-C";

/// The flag a model id is sent with.
const MODEL_FLAG: &str = "--model";

/// The operand that means "the prompt is on standard input".
///
/// It is last, after every flag, because an operand is what ends option parsing:
/// placed before a flag it would be read as that flag's value, and a CLI that
/// reads a flag's value as its prompt runs a session on a prompt nobody wrote.
const STDIN_PROMPT_ARGUMENT: &str = "-";

/// The bits that make a file executable by someone.
///
/// Any one of the three is enough: this answers "could this process run it", not
/// "did the owner intend everyone to", which is the same reading `claude.rs`
/// gives and the one ADR-0054 records.
const EXECUTE_BITS: u32 = 0o111;

/// The Codex CLI, reached as a [`Provider`].
///
/// One adapter serves a whole run: every method takes `&self`, it holds no
/// session, and nothing about a session survives its [`Outcome`], because the
/// runner may hand the same adapter an implementation task and a review task
/// (VISION.md §12's selection by task type) and must not be able to tell that it
/// did.
///
/// The command and the two clocks are the whole of its configuration, and they
/// arrive through [`Codex::new`] rather than a [`crate::Config`]: an adapter that
/// reached for configuration mid-session could not have its session reproduced
/// from the arguments it was called with, which is what a scenario replay and an
/// attempt's evidence both depend on.
#[derive(Debug, Clone)]
pub struct Codex {
    /// The command as it was configured — a bare word searched for along `PATH`,
    /// or a path used where it stands.
    command: String,
    /// How long a session may go without printing before it is stopped:
    /// `Config::idle_timeout_secs`, in a run.
    idle_timeout: Duration,
    /// How long a session may run however productive it is:
    /// `Config::attempt_timeout_secs`, in a run.
    hard_timeout: Duration,
}

impl Codex {
    /// The adapter that starts `command` for a session and bounds that session by
    /// the two clocks.
    ///
    /// No default command is invented for an empty `command`: a provider whose
    /// location was guessed is a provider whose version nobody chose, so an empty
    /// one is refused when a session is asked for, in the words
    /// `Codex::program` gives.
    #[must_use]
    pub fn new(command: impl Into<String>, idle_timeout: Duration, hard_timeout: Duration) -> Self {
        Self {
            command: command.into(),
            idle_timeout,
            hard_timeout,
        }
    }

    /// The program to run, or the refusal that says it cannot be run.
    ///
    /// Checked before the spawn rather than after, so the answer is this file's
    /// own about a command rather than the OS's about a `fork`, and so a missing
    /// CLI reads as the provider configuration failure VISION.md §7 pauses for a
    /// human instead of spending another attempt on.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`] naming the configured command and why nothing was
    /// started, when no command was configured or when the one that was cannot be
    /// executed.
    fn program(&self, env: &dyn Fn(&str) -> Option<String>) -> Result<PathBuf> {
        if self.command.trim().is_empty() {
            return Err(configuration(
                "no command is configured for this provider: an empty command cannot \
                 start a session, and no retry can start one"
                    .to_owned(),
            ));
        }
        locate(&self.command, env).ok_or_else(|| {
            let where_it_was_read = if self.command.contains('/') {
                "is not an executable file where it stands"
            } else {
                "is not an executable program on `PATH`"
            };
            configuration(format!(
                "the configured command `{}` {where_it_was_read}: nothing was started, \
                 and no retry can start one",
                self.command
            ))
        })
    }

    /// Run one session, resolving the configured command through `env`.
    ///
    /// [`Provider::invoke`] is this function with the process's own environment,
    /// and a test hands in its own `PATH` instead — which is the only way the
    /// search below is checkable without installing a CLI into a test machine.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`] naming this adapter, for a model id that cannot be
    /// sent, a command that cannot be located, a command that cannot be executed,
    /// and anything `run_streaming` refuses about a session that was started.
    fn run_with_env(
        &self,
        inv: &Invocation,
        bus: Option<&Bus>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Outcome> {
        let words = arguments(inv.model.as_deref(), &inv.working_dir)?;
        let program = self.program(env)?;
        let mut command = Command::new(&program);
        command.args(&words).current_dir(&inv.working_dir);
        // The located program is an absolute path, and `run_streaming` names a
        // session's failure after the program word it was handed. That is the
        // right answer about a process and the wrong one about a provider —
        // [`Provider::name`] and [`Error::Provider`] both say a failure is
        // attributed to `codex` — so the adapter takes the name back and keeps the
        // reason exactly as the session gave it.
        run_streaming(
            &mut command,
            Some(&inv.prompt),
            self.idle_timeout,
            self.hard_timeout,
            bus,
        )
        .map_err(attributed)
    }
}

impl Provider for Codex {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> Capabilities {
        // `model_selection` is the promise this file's own argv keeps: `--model`
        // goes out with every session that was given an id, so an id is honoured
        // rather than dropped in silence, and a caller can therefore refuse a
        // model the CLI cannot run (VISION.md §12).
        //
        // The other two are `false` because nothing here asks for them, which is
        // the only answer capability detection can mean: no output format is
        // requested of this CLI, so the session answers in prose and no document
        // arrives to be read, and no figure of a session's cost or token count is
        // parsed out of that prose. So [`Outcome::usage`] stays `None` —
        // ADR-0049's unknown, never a zero — and a run of such sessions is one
        // whose cost is unknown rather than cheap. Each flag flips to `true`
        // here, in this file, with the argv and the parsing that earn it.
        Capabilities {
            structured_output: false,
            model_selection: true,
            usage_telemetry: false,
        }
    }

    fn invoke(&self, inv: &Invocation, bus: Option<&Bus>) -> Result<Outcome> {
        self.run_with_env(inv, bus, &process_env)
    }
}

/// The words one Codex session is started with, after its command word.
///
/// Pure: given a model id and a directory it answers what will run, with no
/// `Command` and no process in sight, which is the only way the claim survives
/// review — the real CLI costs money and talks to a network, so an argv checkable
/// only by running it is a check nobody runs. Six words always, eight when a model
/// was configured, in the order a reviewer reads a failure log against.
///
/// # Errors
///
/// [`Error::Provider`] when `model` names something that cannot be sent as a
/// value: an id that is empty names no model, and one that begins with `-` is a
/// flag to the CLI's own parser. Both are refused before a session starts, because
/// the alternative is an attempt recorded against a model the session was never
/// told to run — the mismatch VISION.md §12 says is rejected rather than
/// tolerated.
fn arguments(model: Option<&str>, working_dir: &Path) -> Result<Vec<String>> {
    let mut words = vec![
        EXEC_SUBCOMMAND.to_owned(),
        SANDBOX_BYPASS_FLAG.to_owned(),
        SKIP_GIT_REPO_CHECK_FLAG.to_owned(),
        WORKING_DIR_FLAG.to_owned(),
        absolute(working_dir).display().to_string(),
    ];
    if let Some(id) = model {
        if id.is_empty() || id.starts_with('-') {
            let why = if id.is_empty() {
                "it names nothing"
            } else {
                "a value that begins with `-` is read as a flag, not as a model id"
            };
            return Err(configuration(format!(
                "the configured model `{id}` cannot be asked for: {why}, and a session \
                 started on some other model would be recorded as running this one"
            )));
        }
        words.push(MODEL_FLAG.to_owned());
        words.push(id.to_owned());
    }
    // Last, after every flag, because an operand is what ends option parsing.
    words.push(STDIN_PROMPT_ARGUMENT.to_owned());
    Ok(words)
}

/// Where `command` lives, or `None` when this process could not execute it.
///
/// The two forms an operator writes are kept apart on purpose, exactly as
/// `claude.rs` keeps them: a word holding a `/` is a location and is read at that
/// location, because searching `PATH` for a same-named program instead would run
/// a binary the configuration never named; a bare word is searched along `PATH`,
/// entry by entry, first executable hit wins, which is the order the kernel would
/// have used and the order `ktask-rs doctor` will report.
///
/// ADR-0054 records the reasoning and the two deliberate divergences from
/// `execvp` — the empty-`PATH`-entry rule below, and the result made absolute.
/// Both are the provider layer's rules rather than Codex's; this copy exists here
/// because the task is one file, and ADR-0055 names hoisting them as the
/// follow-up.
fn locate(command: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    candidates(command, &env("PATH").unwrap_or_default())
        .into_iter()
        .find_map(|candidate| usable_program(&candidate))
}

/// Every place `command` would be looked for, in the order they are tried.
///
/// Pure, like [`arguments`]: the search itself needs the filesystem, and a rule
/// about *which directories are searched* should not need one to be checked.
///
/// An empty `PATH` entry is skipped. POSIX reads one as the current directory,
/// and a task worktree holding a file named `codex` is the last place a supervisor
/// should be told its configured CLI was found — every directory a run works in is
/// written into by an agent.
fn candidates(command: &str, path: &str) -> Vec<PathBuf> {
    if command.contains('/') {
        return vec![PathBuf::from(command)];
    }
    path.split(':')
        .filter(|directory| !directory.is_empty())
        .map(|directory| Path::new(directory).join(command))
        .collect()
}

/// `candidate`, made absolute, when it is a file this process could execute.
fn usable_program(candidate: &Path) -> Option<PathBuf> {
    is_executable(candidate).then(|| absolute(candidate))
}

/// Whether `candidate` is an existing regular file with an execute bit.
///
/// Both halves are needed: a directory named `codex` is a path that exists, and a
/// file that is there but cannot be executed is a session that would die on a
/// permission error for reasons the operator would have to reconstruct.
fn is_executable(candidate: &Path) -> bool {
    match std::fs::metadata(candidate) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0,
        Err(_) => false,
    }
}

/// `path` below this process's working directory, with links left unresolved.
///
/// Two consumers need this, and they need it for the same reason. The located
/// program word is handed to a child that has also been told to `chdir`, so a
/// relative word would mean two different files depending on which side of that
/// `chdir` the kernel resolved it; and the `-C` value is resolved by the CLI
/// *after* that same `chdir`, so a relative working directory would be resolved
/// twice. Making both absolute pins the session to what this file looked at.
/// Symlinks are deliberately not resolved: what was checked must be what runs, and
/// a target is a different file from the one the operator named.
fn absolute(path: &Path) -> PathBuf {
    match std::path::absolute(path) {
        Ok(made) => made,
        Err(_) => path.to_path_buf(),
    }
}

/// A refusal to run a session, in the shape every provider refusal has.
fn configuration(detail: String) -> Error {
    Error::Provider {
        provider: PROVIDER_NAME.to_owned(),
        detail,
    }
}

/// Attribute `failure` to this adapter rather than to the file that ran.
///
/// Only the name an [`Error::Provider`] carries changes; its `detail` is the
/// session's own account and is handed on unchanged, because a reason rewritten by
/// an adapter is a reason a later reader cannot trust. Anything that is not a
/// provider failure is not this adapter's to reinterpret and arrives as it was.
fn attributed(failure: Error) -> Error {
    match failure {
        Error::Provider { detail, .. } => Error::Provider {
            provider: PROVIDER_NAME.to_owned(),
            detail,
        },
        other => other,
    }
}

/// A fixture file, written and left non-executable.
#[cfg(test)]
fn write_file(dir: &Path, name: &str, body: &str) -> PathBuf {
    std::fs::create_dir_all(dir).expect("a fixture directory can be made");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("a fixture file can be written");
    path
}

/// A fixture CLI: `body`, written to `dir/name`, made executable, startable.
///
/// Startability is waited for rather than assumed, because this file has just
/// written the file it is about to have execed; see
/// [`super::wait_until_startable`] for what a session that never started would
/// otherwise be read as.
#[cfg(test)]
fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = write_file(dir, name, body);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("a fixture CLI can be made executable");
    super::wait_until_startable(&path);
    path
}

/// A `PATH` holding exactly `directories`, as an injected environment answers it.
#[cfg(test)]
fn path_of(directories: &[&Path]) -> impl Fn(&str) -> Option<String> {
    let value = directories
        .iter()
        .map(|directory| directory.display().to_string())
        .collect::<Vec<_>>()
        .join(":");
    move |key: &str| (key == "PATH").then(|| value.clone())
}

/// A `PATH` the machine never answered — the variable unset.
#[cfg(test)]
fn without_path(_key: &str) -> Option<String> {
    None
}

/// One session's worth of work, as a test would hand it to an adapter.
#[cfg(test)]
fn invocation(prompt: &str, working_dir: &Path) -> Invocation {
    Invocation {
        prompt: prompt.to_owned(),
        model: None,
        working_dir: working_dir.to_path_buf(),
    }
}

/// The value of the `-C` word in `words`, which a test asserts on rather than
/// eyeballs, because the flag's operand is the whole of what tells Codex where to
/// work.
#[cfg(test)]
fn directory_that_was_offered(words: &[String]) -> String {
    let position = words
        .iter()
        .position(|word| word == WORKING_DIR_FLAG)
        .expect("a session is always told its directory");
    words
        .get(position + 1)
        .expect("the flag is never sent without the directory it names")
        .clone()
}

#[cfg(test)]
mod words {
    // What this adapter promises, and the words it would start a session with.
    // Nothing here runs a process: every claim is about a vector of words, a
    // capability record, or where a command was found to live — all of it
    // answerable from fixture files and a `PATH` the test hands in. The real CLI
    // costs money and talks to a network, which is why the argument vector is the
    // thing under test rather than a session that used it.

    use super::{
        Codex, Provider, absolute, arguments, candidates, directory_that_was_offered, locate,
        path_of, without_path, write_executable, write_file,
    };
    use std::path::Path;
    use std::time::Duration;

    /// Clocks no fixture here can exhaust: these tests assert on words, not on a
    /// session being stopped.
    const IDLE: Duration = Duration::from_secs(30);
    const HARD: Duration = Duration::from_secs(60);

    /// A task worktree, as an absolute path a run would hand an adapter.
    const WORKTREE: &str = "/work/t7";

    #[test]
    fn a_codex_session_is_started_on_the_exec_subcommand_with_the_prompt_left_for_stdin() {
        assert_eq!(
            arguments(None, Path::new(WORKTREE))
                .expect("a session that asks for no model still has words"),
            [
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-C",
                WORKTREE,
                "-",
            ],
            "one instruction rather than a conversation, nothing waited on, no git \
             repository demanded, the directory named, and the prompt's door left open \
             for stdin — in that order, which is what a failure log is read against"
        );
    }

    #[test]
    fn a_configured_model_is_asked_for_by_id_after_a_flag_of_its_own() {
        assert_eq!(
            arguments(Some("gpt-5-codex"), Path::new(WORKTREE))
                .expect("a configured model is a word like any other"),
            [
                "exec",
                "--dangerously-bypass-approvals-and-sandbox",
                "--skip-git-repo-check",
                "-C",
                WORKTREE,
                "--model",
                "gpt-5-codex",
                "-",
            ],
            "the id goes as its own word after its own flag, which is the only form that \
             survives a later flag, and it goes before the prompt operand so the operand \
             still ends option parsing"
        );
    }

    #[test]
    fn the_prompt_operand_is_last_so_the_cli_reads_its_prompt_and_not_a_flags_value() {
        for model in [None, Some("gpt-5-codex")] {
            let words = arguments(model, Path::new(WORKTREE))
                .expect("a session always has words to be started with");
            assert_eq!(
                words
                    .last()
                    .expect("an argv always has a last word")
                    .as_str(),
                "-",
                "the operand that means `read the prompt from stdin` is last whatever \
                 else was configured: anywhere else it is the value of the flag it \
                 follows, and a CLI that reads a flag's value as its prompt runs a \
                 session on a prompt nobody wrote: {words:?}"
            );
        }
    }

    #[test]
    fn an_unconfigured_model_is_left_to_the_cli_rather_than_asked_for_as_an_empty_request() {
        let words = arguments(None, Path::new(WORKTREE))
            .expect("a session that asks for no model still has words");

        assert!(
            !words.iter().any(|word| word == "--model"),
            "the operator expressed no preference, so no `--model` may be sent: an empty \
             or invented id would be recorded as a model the session was told to run: \
             {words:?}"
        );
    }

    #[test]
    fn a_model_id_that_reads_as_a_flag_is_refused_before_the_cli_starts() {
        let error = arguments(Some("-f"), Path::new(WORKTREE))
            .expect_err("a model id wearing a flag is not a model id");

        assert_eq!(
            error.to_string(),
            "provider `codex` failed: the configured model `-f` cannot be asked for: a \
             value that begins with `-` is read as a flag, not as a model id, and a \
             session started on some other model would be recorded as running this one",
            "the refusal names the value at fault and why a session must not be started \
             on a model nobody configured"
        );
    }

    #[test]
    fn a_model_id_that_names_nothing_is_refused_rather_than_dropped() {
        let error =
            arguments(Some(""), Path::new(WORKTREE)).expect_err("an empty id names no model");

        assert_eq!(
            error.to_string(),
            "provider `codex` failed: the configured model `` cannot be asked for: it \
             names nothing, and a session started on some other model would be recorded \
             as running this one"
        );
    }

    #[test]
    fn the_directory_is_offered_once_and_as_an_absolute_path() {
        let words = arguments(Some("gpt-5-codex"), Path::new(WORKTREE))
            .expect("a session always has words to be started with");

        assert_eq!(
            words.iter().filter(|word| *word == "-C").count(),
            1,
            "the directory is named once: a CLI told to work in two places picks one of \
             them and the attempt record names the other: {words:?}"
        );
        assert_eq!(
            directory_that_was_offered(&words),
            WORKTREE,
            "the directory the invocation named is the directory the CLI is told, word \
             for word"
        );
    }

    #[test]
    fn a_relative_working_directory_is_offered_as_an_absolute_path() {
        let words = arguments(None, Path::new("worktrees/t7"))
            .expect("a relative worktree is still a directory to work in");
        let offered_in_words = directory_that_was_offered(&words);
        let offered = Path::new(&offered_in_words);

        assert!(
            offered.is_absolute(),
            "the session is already started with its working directory set, and the CLI \
             resolves what it is handed from there — so a relative word would be resolved \
             a second time and land in a directory nobody named: {offered:?}"
        );
        assert!(
            offered.ends_with(Path::new("worktrees/t7")),
            "and it is the same directory made absolute, not a different one: {offered:?}"
        );
    }

    #[test]
    fn the_adapter_is_reachable_as_a_provider_and_answers_to_the_configured_name() {
        let codex = Codex::new("codex", IDLE, HARD);
        let provider: &dyn Provider = &codex;

        assert_eq!(
            provider.name(),
            "codex",
            "the name an operator writes in configuration and reads in the TUI is what a \
             failure is attributed to"
        );
    }

    #[test]
    fn what_the_adapter_promises_is_model_selection_and_nothing_else() {
        let codex = Codex::new("codex", IDLE, HARD);

        let promised = codex.capabilities();
        assert!(
            promised.model_selection,
            "an id can be handed to this CLI — which is what `--model` is for"
        );
        assert!(
            !promised.structured_output,
            "a session is started for its prose answer, so nothing structured is asked \
             of it and nothing structured may be read out of it"
        );
        assert!(
            !promised.usage_telemetry,
            "nothing here asks the CLI what it spent, so a caller must not be told a \
             figure is available"
        );
    }

    #[test]
    fn capabilities_are_the_same_answer_whenever_they_are_asked_for() {
        let codex = Codex::new("codex", IDLE, HARD);

        assert_eq!(
            codex.capabilities(),
            Codex::new("codex", IDLE, HARD).capabilities(),
            "a capability is a promise about this CLI, not a mood: a caller that asked \
             twice mid-run must get the same answer to build a decision on"
        );
    }

    #[test]
    fn a_bare_command_is_found_where_the_path_names_it() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        let lives = write_executable(
            &bin,
            "codex",
            r"#!/bin/sh
echo found
",
        );

        let absent = scratch.path().join("nowhere");
        let found = locate("codex", &path_of(&[absent.as_path(), &bin]));

        assert_eq!(
            found.as_deref(),
            Some(lives.as_path()),
            "the entry that holds the command answers, and the one that does not is \
             passed over without stopping the search"
        );
    }

    #[test]
    fn a_command_that_names_a_directory_is_used_where_it_stands_and_never_searched_for() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let stands = write_executable(
            &scratch.path().join("here"),
            "codex",
            r"#!/bin/sh
echo the-one-configured
",
        );
        let decoy = write_executable(
            &scratch.path().join("elsewhere"),
            "codex",
            r"#!/bin/sh
echo the-one-on-the-path
",
        );
        let elsewhere = scratch.path().join("elsewhere");
        let path = [elsewhere.as_path()];

        let found = locate(&stands.display().to_string(), &path_of(&path));

        assert_eq!(
            found.as_deref(),
            Some(stands.as_path()),
            "a command that names a directory is that file: searching `PATH` for a \
             same-named program would run a binary the configuration never named"
        );
        assert_eq!(
            locate("codex", &path_of(&path)).as_deref(),
            Some(decoy.as_path()),
            "and the decoy is a real candidate, so the assertion above is about the rule \
             and not about an empty search"
        );
    }

    #[test]
    fn a_file_that_cannot_be_executed_is_not_a_command() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        write_file(&bin, "codex", "not a program at all\n");

        assert_eq!(
            locate("codex", &path_of(&[&bin])),
            None,
            "a file that is there but cannot be executed is not a provider: the answer \
             has to say it is not usable rather than start a session on it"
        );
    }

    #[test]
    fn a_directory_is_not_a_command() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(bin.join("codex")).expect("a directory named `codex`");

        assert_eq!(
            locate("codex", &path_of(&[&bin])),
            None,
            "a directory named `codex` is not the Codex CLI"
        );
    }

    #[test]
    fn an_empty_path_entry_searches_nothing_rather_than_where_the_run_stands() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        write_executable(
            scratch.path(),
            "codex",
            r"#!/bin/sh
echo the-working-directory
",
        );

        assert_eq!(
            locate("codex", &path_of(&[Path::new(""), Path::new("")])),
            None,
            "POSIX reads an empty `PATH` entry as the current directory, and a task \
             worktree holding a file named `codex` is the last place a supervisor should \
             look for the configured CLI"
        );
    }

    #[test]
    fn an_empty_path_entry_is_out_of_the_search_rather_than_read_as_the_working_directory() {
        assert_eq!(
            candidates("codex", "/opt/ktask/bin::/usr/local/bin"),
            [
                Path::new("/opt/ktask/bin/codex"),
                Path::new("/usr/local/bin/codex")
            ],
            "an empty entry is the current directory to POSIX, and every directory a run \
             works in is written into by an agent: a worktree holding a file named \
             `codex` must never be reported as where the configured CLI lives"
        );
    }

    #[test]
    fn a_bare_command_without_a_path_to_search_is_answered_with_nothing() {
        assert_eq!(
            locate("codex", &without_path),
            None,
            "with no `PATH` there is nowhere to look, and an invented location would be \
             the substitution ADR-0049 refuses elsewhere"
        );
    }

    #[test]
    fn a_command_written_as_a_path_yields_one_candidate_and_searches_nothing() {
        assert_eq!(
            candidates("/opt/two/codex", "/opt/one:/opt/two"),
            [Path::new("/opt/two/codex")],
            "a command written as a path is that file alone: a `PATH` holding another \
             `codex` is never consulted, so configuration and process cannot disagree \
             about which binary a run is on"
        );
        assert_eq!(
            candidates("./vendor/codex", "/opt/one"),
            [Path::new("./vendor/codex")],
            "a relative path stands where it was written: joining it onto a `PATH` entry \
             would run a file in a directory the configuration never named"
        );
    }

    #[test]
    fn a_located_command_is_made_absolute_without_reading_what_it_points_at() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let stands = write_executable(&scratch.path().join("bin"), "codex", "#!/bin/sh\necho x\n");
        std::os::unix::fs::symlink("/opt/the/other/codex", scratch.path().join("alias"))
            .expect("a symlink to a program that is not there can be made");

        assert_eq!(
            absolute(Path::new("codex")),
            std::env::current_dir()
                .expect("this process stands somewhere")
                .join("codex"),
            "a relative command is pinned to one file before the child is told to \
             `chdir`, because a word resolved twice means two different files"
        );
        assert_eq!(
            absolute(&stands),
            stands,
            "a path already absolute is not rewritten: the check and the exec must see \
             the same name"
        );
        assert_eq!(
            absolute(&scratch.path().join("alias")),
            scratch.path().join("alias"),
            "a symlink is left pointing where the operator wrote it: resolving it would \
             execute a different file from the one that was checked"
        );
    }

    #[test]
    fn a_command_the_path_does_not_hold_is_answered_with_nothing() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(&bin).expect("an empty fixture directory");

        assert_eq!(locate("codex", &path_of(&[&bin])), None);
    }
}

#[cfg(test)]
mod sessions {
    // What a session actually does: a fixture CLI is started, and what it read,
    // where it stood, what it was told, what it printed and what it exited with
    // are read back from the answer. These are the claims a real Codex session is
    // made of, tested against a stand-in so that no test needs the CLI, an
    // account, or a network — and so that a wrong word is caught here rather than
    // by a session that spent money to find out.

    use super::{Codex, Provider, attributed, invocation, path_of, write_executable};
    use crate::{Bus, Error};
    use std::path::Path;
    use std::time::Duration;

    /// Clocks generous enough that nothing a fixture CLI does on its own is
    /// mistaken for a hang.
    const IDLE: Duration = Duration::from_secs(30);
    const HARD: Duration = Duration::from_secs(60);

    /// An idle budget short enough for a test to watch expire, matching the one
    /// `provider::process` tuned its own watchdog tests to.
    const SILENCE_BUDGET: Duration = Duration::from_millis(400);

    /// A CLI that takes its prompt on stdin and reports, in its own words, where
    /// it was started and which directory it was handed as an argument.
    const REPORTER: &str = r#"#!/bin/sh
cat > /dev/null
printf 'pwd=%s\n' "$(pwd)"
while [ "$#" -gt 0 ]; do
    if [ "$1" = "-C" ]; then
        shift
        printf 'offered=%s\n' "$1"
    fi
    shift
done
"#;

    /// The detail of a provider refusal, after checking it is one.
    fn refusal(error: &Error) -> String {
        let Error::Provider { provider, detail } = &error else {
            panic!(
                "a provider that could not run a session fails as a provider error, not {error:?}"
            );
        };
        assert_eq!(
            provider, "codex",
            "the refusal names the adapter, which is what a preflight and a human read: \
             {error}"
        );
        detail.clone()
    }

    /// The value of the `offered=` line a fixture CLI reported.
    fn offered_by(fixture_output: &str) -> Option<String> {
        fixture_output
            .lines()
            .find_map(|line| line.strip_prefix("offered="))
            .map(str::to_owned)
    }

    #[test]
    fn a_session_takes_its_prompt_on_stdin_and_answers_with_what_it_printed() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "codex",
            r"#!/bin/sh
printf 'prompt:'
cat
printf '\nanswered\n'
",
        );
        let prompt = "Implement T060 and report.\nRefs: VISION.md section 12.";

        let outcome = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation(prompt, scratch.path()), None)
            .expect("a CLI that reads its prompt runs a session");

        assert_eq!(
            outcome.stdout,
            format!("prompt:{prompt}\nanswered\n"),
            "the whole prompt arrived on stdin, byte for byte, and everything the CLI \
             printed came back in order"
        );
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.stderr, "");
        assert_eq!(
            outcome.usage, None,
            "nothing was asked about what the session spent, so nothing is recorded — \
             ADR-0049 keeps an unreported figure unknown rather than zero"
        );
        assert_eq!(outcome.session_id, None, "and no session id was disclosed");
        assert_eq!(
            outcome.model_reported, None,
            "and no model was reported either: this CLI is started with plain text, so \
             an id here would be a guess about which model ran the session, and \
             section 12 compares recorded ids rather than invented ones"
        );
    }

    #[test]
    fn a_prompt_is_handed_over_stdin_rather_than_written_into_a_word_of_its_own() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "codex",
            r#"#!/bin/sh
printf 'prompt:%s\n' "$(cat)"
"#,
        );
        let prompt = "A prompt with 'quotes', \"doubles\" and a $dollar in it.";

        let outcome = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation(prompt, scratch.path()), None)
            .expect("the CLI reads what it is handed");

        assert_eq!(
            outcome.stdout,
            format!("prompt:{prompt}\n"),
            "the prompt arrives byte for byte with no shell having seen it, which is the \
             only way prose survives an argv and the reason a prompt never appears in \
             what a run logs about its command line"
        );
    }

    #[test]
    fn a_session_stands_in_the_directory_it_was_handed_and_is_told_the_same_one() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(scratch.path(), "codex", REPORTER);
        let work = scratch.path().join("worktree");
        std::fs::create_dir_all(&work).expect("a task worktree to run in");

        let outcome = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", &work), None)
            .expect("a session runs in the directory it was handed");

        let stood_in = outcome
            .stdout
            .lines()
            .find_map(|line| line.strip_prefix("pwd="))
            .expect("the fixture reports the directory it stood in");
        assert_eq!(
            Path::new(stood_in).canonicalize().ok().as_deref(),
            work.canonicalize().ok().as_deref(),
            "the isolation an attempt's evidence is attributed to comes from \
             `Invocation::working_dir`, so that is the directory the CLI must have stood \
             in: it reported `{stood_in}`"
        );
        assert_ne!(
            Path::new(stood_in).canonicalize().ok().as_deref(),
            std::env::current_dir()
                .ok()
                .and_then(|here| here.canonicalize().ok())
                .as_deref(),
            "and not the directory this process is standing in, which is the mistake that \
             puts an attempt's work in the main checkout"
        );
        assert_eq!(
            offered_by(&outcome.stdout).as_deref(),
            Some(work.display().to_string().as_str()),
            "Codex is told its directory as an argument as well as started in it, and the \
             argument must name the same place — a relative value would be resolved again \
             from where the child already stands"
        );
    }

    #[test]
    fn a_cli_that_exits_non_zero_is_an_answer_that_notes_its_code_not_a_refusal() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "codex",
            r"#!/bin/sh
cat > /dev/null
printf 'Nothing to do here.\n' >&2
exit 1
",
        );

        let outcome = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect("a session that ran and refused still answers");

        assert_eq!(
            outcome.exit_code, 1,
            "the exit status is reported as the CLI left it; VISION.md section 3's fourth \
             invariant decides what to make of it, which is not this adapter's call"
        );
        assert_eq!(outcome.stderr, "Nothing to do here.\n");
        assert_eq!(outcome.stdout, "");
    }

    #[test]
    fn the_words_the_cli_is_started_with_are_the_words_the_pure_builder_produced() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "codex",
            r#"#!/bin/sh
cat > /dev/null
printf '%s\n' "$@"
"#,
        );
        let mut inv = invocation("a prompt", scratch.path());
        inv.model = Some("gpt-5-codex".to_owned());

        let outcome = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&inv, None)
            .expect("a session with a configured model runs");

        assert_eq!(
            outcome.stdout,
            format!(
                "exec\n--dangerously-bypass-approvals-and-sandbox\n\
                 --skip-git-repo-check\n-C\n{}\n--model\ngpt-5-codex\n-\n",
                scratch.path().display()
            ),
            "the argument vector a unit test checked is the vector the CLI was started \
             with, one word per line of the fixture's own report"
        );
    }

    #[test]
    fn a_bare_command_runs_from_the_directory_the_path_named() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        write_executable(
            &bin,
            "codex",
            r"#!/bin/sh
cat > /dev/null
printf 'from-the-path\n'
",
        );

        let outcome = Codex::new("codex", IDLE, HARD)
            .run_with_env(
                &invocation("a prompt", scratch.path()),
                None,
                &path_of(&[&bin]),
            )
            .expect("a command the path holds is runnable");

        assert_eq!(
            outcome.stdout, "from-the-path\n",
            "a configured command is found where the machine says commands live, and the \
             file found there is the one that ran"
        );
    }

    #[test]
    fn a_command_that_is_not_there_is_refused_before_anything_starts() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();
        let adapter = Codex::new("ktask-codex-that-is-not-installed", IDLE, HARD);

        let error = adapter
            .invoke(&invocation("a prompt", Path::new("/nowhere")), Some(&bus))
            .expect_err("a CLI that is not installed cannot run a session");

        assert_eq!(
            refusal(&error),
            "the configured command `ktask-codex-that-is-not-installed` is not an \
             executable program on `PATH`: nothing was started, and no retry can start \
             one",
            "a missing executable is answered as a provider configuration failure, in \
             words that say a retry cannot fix it — VISION.md section 7 pauses on that \
             class rather than looping"
        );
        let (events, dropped) = watcher.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "nothing was started, so nothing was published to a watching frontend: {} \
             events, {dropped} dropped",
            events.len()
        );
    }

    #[test]
    fn a_command_with_no_words_in_it_is_refused_as_a_configuration_failure() {
        let adapter = Codex::new("   ", IDLE, HARD);

        let error = adapter
            .invoke(&invocation("a prompt", Path::new("/nowhere")), None)
            .expect_err("there is no command to start");

        assert_eq!(
            refusal(&error),
            "no command is configured for this provider: an empty command cannot start a \
             session, and no retry can start one"
        );
    }

    #[test]
    fn a_command_that_cannot_be_executed_is_refused_rather_than_started_badly() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = super::write_file(scratch.path(), "codex", "not a program\n");

        let error = Codex::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect_err("a non-executable file cannot run a session");

        let detail = refusal(&error);
        assert!(
            detail.contains("not an executable file where it stands"),
            "a command written as a path is answered about the file, not about `PATH`: \
             {detail}"
        );
        assert!(
            !detail.contains("could not start"),
            "the refusal is this adapter's own answer, not the OS's `could not start`: \
             starting the session is what the check exists to avoid: {detail}"
        );
    }

    #[test]
    fn the_adapters_idle_clock_is_the_one_that_stops_a_silent_session() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "codex",
            r"#!/bin/sh
cat > /dev/null
printf 'here\n'
sleep 5
",
        );

        let error = Codex::new(cli.display().to_string(), SILENCE_BUDGET, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect_err("a session that stops answering is stopped");

        let detail = refusal(&error);
        assert!(
            detail.contains("past its 400ms idle timeout"),
            "the clocks the adapter was built with are the clocks the session is bounded \
             by: {detail}"
        );
        assert!(
            !detail.contains("hard timeout"),
            "a session that went silent is a hang, not a long run, and the answer says \
             which it was: {detail}"
        );
    }

    #[test]
    fn a_failure_about_the_session_is_attributed_to_the_adapter_not_the_file_that_ran() {
        let failure = attributed(Error::Provider {
            provider: "/opt/codex/bin/codex".to_owned(),
            detail: "silent for 400ms, past its 400ms idle timeout".to_owned(),
        });

        assert_eq!(
            refusal(&failure),
            "silent for 400ms, past its 400ms idle timeout",
            "the reason a session stopped belongs to the session and is handed on \
             unwritten: {failure}"
        );
        assert_eq!(
            failure.to_string(),
            "provider `codex` failed: silent for 400ms, past its 400ms idle timeout",
            "what is replaced is who failed: a caller, a preflight and a TUI all ask which \
             provider, and `codex` is the answer `Provider::name` gives"
        );
    }

    #[test]
    fn a_failure_that_is_not_about_a_provider_is_handed_back_exactly_as_it_arrived() {
        let arrived = Error::NotFound {
            what: "the attempt".to_owned(),
        };

        assert_eq!(
            attributed(arrived).to_string(),
            "not found: the attempt",
            "rewriting a failure nobody owns is how an error loses the reason it was \
             raised"
        );
    }
}
