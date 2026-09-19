//! The Claude adapter: the words the Claude CLI is started with, where that
//! command lives, and what one session leaves behind.
//!
//! VISION.md §12 lists `claude` as one of the two launch adapters, and an
//! adapter is only ever the things that differ between two CLIs: which words
//! start the process, and which door the prompt goes through. Everything else —
//! streaming, the two clocks, the process group, the capture bound, the shape of
//! an answer — belongs to [`run_streaming`], and a difference between Claude and
//! Codex found anywhere but in this file is a defect in one of the two files
//! (T060's done-when reads the same sentence from the other side).
//!
//! The door this one chooses is standard input: the whole prompt is handed to the
//! child to read and the write end closed, never appended to the command line. A
//! prompt is prose, and prose sent as an argument is bounded by what a kernel
//! accepts in one exec and readable by whoever runs `ps` — where VISION.md §11
//! keeps prompts outside repositories and out of what a run logs.
//!
//! Three decisions are made here rather than by whoever calls this.
//!
//! **The words are built by a pure function.** `arguments` answers "what will
//! run" from a model id alone, with no process in sight, which is the only way
//! the claim survives review: the alternative is a [`Command`] assembled on the
//! way to a spawn, whose flags can then be checked only by running the CLI — a
//! CLI that costs money and talks to a network. The same function is why an
//! unusable model id is refused rather than sent: a value that begins with `-`
//! is read as a flag by the CLI's own parser, so the session would run on some
//! other model while the attempt record named the configured one, which
//! VISION.md §12 requires rejecting rather than tolerating. `git::publish` met
//! the same trap from the other direction and wrote down what a value wearing an
//! option's clothes did when it was allowed to reach a command.
//!
//! **A command that cannot be executed is answered before one is started.**
//! `locate` resolves the configured command — a bare word along `PATH`, a word
//! naming a directory where it stands — and refuses a file that is absent, is a
//! directory, or has no execute bit. Letting the spawn answer instead returns
//! whatever the OS's wording for `ENOENT` happens to be, inside a message about
//! a command line, and a failure whose class has to be recognised from prose is
//! the thing `classify` exists to avoid. Resolving first also means the file that
//! was checked is the file that runs: the located path is made absolute, so a
//! relative word cannot be resolved once by the search and again, after the
//! child's `chdir`, somewhere else.
//!
//! What a refusal says matters, because a missing executable is a provider
//! *configuration* failure — [`crate::FailureClass::ProviderConfiguration`]'s own
//! definition names it, and VISION.md §7 pauses that class for a human instead of
//! spending another attempt on it. The refusal therefore starts a session nowhere
//! (no exit code is invented for a process that never ran, which ADR-0050 assigns
//! to the error channel), names the adapter, names the configured command, and
//! says in plain words that a retry cannot fix it.
//!
//! **A session's failure still carries the adapter's name.** `run_streaming`
//! attributes a failure to the program word it was handed, which after `locate` is
//! an absolute path. That is the right answer about a process and the wrong one
//! about a provider — [`Provider::name`] and [`crate::Error::Provider`] both say a
//! failure is attributed to `claude` — so [`Claude::invoke`] takes the name back
//! and hands the reason on exactly as the session gave it.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use super::process::run_streaming;
use super::{Capabilities, Invocation, Outcome, Provider};
use crate::paths::process_env;
use crate::{Bus, Error, Result};

/// The name an operator writes in configuration and reads in the TUI, and the
/// name a refusal is attributed to.
const PROVIDER_NAME: &str = "claude";

/// Ask for one turn and print what it produced.
///
/// Without it the CLI starts an interactive conversation, and a conversation
/// waiting for the next human line is a session this supervisor would have to
/// kill on a clock.
const PRINT_FLAG: &str = "--print";

/// The flag that decides what the CLI may do without asking.
const PERMISSION_MODE_FLAG: &str = "--permission-mode";

/// The mode this starts a session with: nothing is waited on.
///
/// A run has no human answering an approval prompt, so a prompt is not a safety
/// property here — it is a hang with a question mark on it. What stands between
/// an agent's wish and an effect is the mechanical gates and the git transaction
/// model (VISION.md §3, §10), which no permission mode of a CLI can bypass
/// because neither runs inside the session.
const BYPASS_PERMISSIONS: &str = "bypassPermissions";

/// The flag a model id is sent with.
const MODEL_FLAG: &str = "--model";

/// The bits that make a file executable by someone.
///
/// Any one of the three is enough: this answers "could this process run it", not
/// "did the owner intend everyone to". A file with only the group bit set runs
/// for a run that shares the CLI's group, and refusing it would be a supervisor
/// inventing a stricter policy than the kernel applies.
const EXECUTE_BITS: u32 = 0o111;

/// The Claude CLI, reached as a [`Provider`].
///
/// One adapter serves a whole run: every method takes `&self`, it holds no
/// session, and nothing about a session survives its [`Outcome`]. That is a
/// requirement and not an economy — the runner holds `Box<dyn Provider>` and may
/// hand the same adapter an implementation task and a review task.
///
/// The command and the two clocks are the whole of its configuration, and they
/// arrive through [`Claude::new`] rather than a [`crate::Config`]: an adapter
/// that reached for configuration mid-session could not have its session
/// reproduced from the arguments it was called with.
#[derive(Debug, Clone)]
pub struct Claude {
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

impl Claude {
    /// The adapter that starts `command` for a session and bounds that session by
    /// the two clocks.
    ///
    /// No default command is invented for an empty `command`: a provider whose
    /// location was guessed is a provider whose version nobody chose, so an empty
    /// one is refused when a session is asked for, in the words
    /// `Claude::program` gives.
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
    /// own about a command rather than the OS's about a `fork`.
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
    /// [`Provider::invoke`] is this function with the process's own environment.
    /// The accessor is a parameter for the reason `crate::paths` takes one: a
    /// test has to be able to say where commands live without rewriting the
    /// machine's, and `PATH` is precisely the input that decides which file runs.
    ///
    /// # Errors
    ///
    /// [`Error::Provider`] for a model id that cannot be sent as a value or a
    /// command that cannot be executed, both before the CLI is started, and for
    /// anything `run_streaming` refuses about a session that was.
    fn run_with_env(
        &self,
        inv: &Invocation,
        bus: Option<&Bus>,
        env: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Outcome> {
        let words = arguments(inv.model.as_deref())?;
        let program = self.program(env)?;
        let mut command = Command::new(&program);
        command.args(&words).current_dir(&inv.working_dir);
        // Resolving the command is what turns the program word into the file that
        // runs, and `run_streaming` names a session's failure after the word it was
        // given — the absolute path here. That is the right answer about a process
        // and the wrong one about a provider: `Provider::name` documents that the
        // name is how a failure is attributed, and `Error::Provider`'s payload says
        // the same in its own words. So the adapter takes the name back and keeps
        // the reason exactly as the session gave it.
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

impl Provider for Claude {
    fn name(&self) -> &str {
        PROVIDER_NAME
    }

    fn capabilities(&self) -> Capabilities {
        // `model_selection` is the one promise this file's own argv keeps: `--model`
        // goes out with every session that was given an id, so an id is honoured
        // rather than dropped in silence — which is the only answer that lets a
        // caller *refuse* a model the CLI cannot run (VISION.md §12).
        //
        // The other two are the conservative answer, and they are conservative
        // because nothing here asks for them: no output format is requested, so the
        // session answers in prose and no document arrives to be read, and no figure
        // of a session's cost or token count is parsed out of that prose. So
        // `Outcome::usage` stays `None` — ADR-0049's unknown, never a zero — and a
        // run of such sessions is one whose cost is unknown rather than cheap. Each
        // flag flips to `true` here, in this file, with the argv that earns it.
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

/// The words one Claude session is started with, after its command word.
///
/// Pure: given a model id it answers what will run, and it is the whole of the
/// adapter's opinion about the CLI's interface. Three words always, five when a
/// model was configured, in that order — the order is what a reviewer of a
/// failure log reads an argv against.
///
/// # Errors
///
/// [`Error::Provider`] when `model` names something that cannot be sent as a
/// value: an id that is empty names no model, and one that begins with `-` is a
/// flag to the CLI's own parser. Both are refused before a session starts,
/// because the alternative is an attempt recorded against a model the session was
/// never told to run.
fn arguments(model: Option<&str>) -> Result<Vec<String>> {
    let mut words = vec![
        PRINT_FLAG.to_owned(),
        PERMISSION_MODE_FLAG.to_owned(),
        BYPASS_PERMISSIONS.to_owned(),
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
    Ok(words)
}

/// Where `command` lives, or `None` when this process could not execute it.
///
/// The two forms an operator writes are kept apart on purpose. A word holding a
/// `/` is a location and is read at that location: searching `PATH` for a
/// same-named program instead would run a binary the configuration never named,
/// which is how a machine with two `claude`s starts answering for the wrong one.
/// A bare word is searched along `PATH`, entry by entry, and the first entry that
/// holds an executable file wins — which is the same order the kernel would have
/// used, and the order `doctor` will report.
fn locate(command: &str, env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    candidates(command, &env("PATH").unwrap_or_default())
        .into_iter()
        .find_map(|candidate| usable_program(&candidate))
}

/// Every place `command` would be looked for, in the order they are tried.
///
/// Pure, like [`arguments`]: the search itself needs the filesystem, and a rule
/// about *which directories are searched* should not need one to be checked. A
/// word holding a `/` is its own only candidate; a bare word is joined to each
/// non-empty `PATH` entry, in order.
///
/// An empty `PATH` entry is skipped. POSIX reads one as the current directory,
/// and a task worktree holding a file named `claude` is the last place a
/// supervisor should be told its configured CLI was found. That is a deliberate
/// divergence from `execvp`, taken because every directory a run works in is
/// written in by an agent.
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
/// Both halves are needed. A directory named `claude` is a path that exists, and
/// a file that is there but cannot be executed is a session that would die on a
/// permission error for reasons the operator would have to reconstruct; either
/// way the honest answer is "this is not a command to run".
fn is_executable(candidate: &Path) -> bool {
    match std::fs::metadata(candidate) {
        Ok(metadata) => metadata.is_file() && metadata.permissions().mode() & EXECUTE_BITS != 0,
        Err(_) => false,
    }
}

/// `path` below this process's working directory, with links left unresolved.
///
/// The located path is handed to a child that has also been told to `chdir`, and
/// a relative word would then mean two different files depending on which side
/// of that `chdir` the kernel resolved it. Making it absolute pins the exec to
/// the file the check above looked at. Symlinks are deliberately not resolved:
/// what was checked must be what runs, and a target is a different file from the
/// one the operator named.
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

/// A fixture CLI: `body`, written to `dir/name` and made executable.
#[cfg(test)]
fn write_executable(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = write_file(dir, name, body);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
        .expect("a fixture CLI can be made executable");
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
#[cfg(test)]
mod words {
    // What this adapter promises, and the words it would start a session with.
    // Nothing here runs a process: every claim is about a vector of words, a
    // capability record, or where a command was found to live — all of it
    // answerable from fixture files and a `PATH` the test hands in.

    use super::{
        Claude, Provider, absolute, arguments, candidates, locate, path_of, without_path,
        write_executable, write_file,
    };
    use std::path::Path;
    use std::time::Duration;

    /// Clocks no fixture here can exhaust: these tests assert on words, not on
    /// a session being stopped.
    const IDLE: Duration = Duration::from_secs(30);
    const HARD: Duration = Duration::from_secs(60);

    #[test]
    fn a_claude_session_is_started_in_print_mode_with_permissions_bypassed() {
        assert_eq!(
            arguments(None).expect("a session that asks for no model still has words"),
            ["--print", "--permission-mode", "bypassPermissions"],
            "one turn, no interactive approval prompt, in that order: the flags are \
             what make a Claude session usable unattended at all"
        );
    }

    #[test]
    fn a_configured_model_is_asked_for_by_id_after_a_flag_of_its_own() {
        assert_eq!(
            arguments(Some("claude-sonnet-4-5"))
                .expect("a configured model is a word like any other"),
            [
                "--print",
                "--permission-mode",
                "bypassPermissions",
                "--model",
                "claude-sonnet-4-5",
            ],
            "the id goes as its own word after its own flag, which is the only form \
             that survives a later flag"
        );
    }

    #[test]
    fn an_unconfigured_model_is_left_to_the_cli_rather_than_asked_for_as_an_empty_request() {
        let words = arguments(None).expect("a session that asks for no model still has words");

        assert!(
            !words.iter().any(|word| word == "--model"),
            "the operator expressed no preference, so no `--model` may be sent: an \
             empty or invented id would be recorded as a model the session was told to \
             run: {words:?}"
        );
    }

    #[test]
    fn a_model_id_that_reads_as_a_flag_is_refused_before_the_cli_starts() {
        let error = arguments(Some("-f")).expect_err("a model id wearing a flag is not a model id");

        assert_eq!(
            error.to_string(),
            "provider `claude` failed: the configured model `-f` cannot be asked for: \
             a value that begins with `-` is read as a flag, not as a model id, and a \
             session started on some other model would be recorded as running this one",
            "the refusal names the value at fault and why a session must not be started \
             on a model nobody configured"
        );
    }

    #[test]
    fn a_model_id_that_names_nothing_is_refused_rather_than_dropped() {
        let error = arguments(Some("")).expect_err("an empty id names no model");

        assert_eq!(
            error.to_string(),
            "provider `claude` failed: the configured model `` cannot be asked for: it \
             names nothing, and a session started on some other model would be recorded \
             as running this one"
        );
    }

    #[test]
    fn the_adapter_is_reachable_as_a_provider_and_answers_to_the_configured_name() {
        let claude = Claude::new("claude", IDLE, HARD);
        let provider: &dyn Provider = &claude;

        assert_eq!(
            provider.name(),
            "claude",
            "the name an operator writes in configuration and reads in the TUI is what \
             a failure is attributed to"
        );
    }

    #[test]
    fn what_the_adapter_promises_is_model_selection_and_nothing_else() {
        let claude = Claude::new("claude", IDLE, HARD);

        let promised = claude.capabilities();
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
        let claude = Claude::new("claude", IDLE, HARD);

        assert_eq!(
            claude.capabilities(),
            Claude::new("claude", IDLE, HARD).capabilities(),
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
            "claude",
            r"#!/bin/sh
echo found
",
        );

        let absent = scratch.path().join("nowhere");
        let found = locate("claude", &path_of(&[absent.as_path(), &bin]));

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
            "claude",
            r"#!/bin/sh
echo the-one-configured
",
        );
        let decoy = write_executable(
            &scratch.path().join("elsewhere"),
            "claude",
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
            locate("claude", &path_of(&path)).as_deref(),
            Some(decoy.as_path()),
            "and the decoy is a real candidate, so the assertion above is about the \
             rule and not about an empty search"
        );
    }

    #[test]
    fn a_file_that_cannot_be_executed_is_not_a_command() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        write_file(&bin, "claude", "not a program at all\n");

        assert_eq!(
            locate("claude", &path_of(&[&bin])),
            None,
            "a file that is there but cannot be executed is not a provider: the answer \
             has to say it is not usable rather than start a session on it"
        );
    }

    #[test]
    fn a_directory_is_not_a_command() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        std::fs::create_dir_all(bin.join("claude")).expect("a directory named `claude`");

        assert_eq!(
            locate("claude", &path_of(&[&bin])),
            None,
            "a directory named `claude` is not the Claude CLI"
        );
    }

    #[test]
    fn an_empty_path_entry_searches_nothing_rather_than_where_the_run_stands() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        write_executable(
            scratch.path(),
            "claude",
            r"#!/bin/sh
echo the-working-directory
",
        );

        assert_eq!(
            locate("claude", &path_of(&[Path::new(""), Path::new("")])),
            None,
            "POSIX reads an empty `PATH` entry as the current directory, and a task \
             worktree holding a file named `claude` is the last place a supervisor \
             should look for the configured CLI"
        );
    }

    #[test]
    fn a_bare_command_without_a_path_to_search_is_answered_with_nothing() {
        assert_eq!(
            locate("claude", &without_path),
            None,
            "with no `PATH` there is nowhere to look, and an invented location would be \
             the substitution ADR-0049 refuses elsewhere"
        );
    }

    #[test]
    fn an_empty_path_entry_is_out_of_the_search_rather_than_read_as_the_working_directory() {
        assert_eq!(
            candidates("claude", "/opt/ktask/bin::/usr/local/bin"),
            [
                Path::new("/opt/ktask/bin/claude"),
                Path::new("/usr/local/bin/claude")
            ],
            "an empty entry is the current directory to POSIX, and every directory a \
             run works in is written into by an agent: a worktree holding a file named \
             `claude` must never be reported as where the configured CLI lives"
        );
    }

    #[test]
    fn a_command_written_as_a_path_yields_one_candidate_and_searches_nothing() {
        assert_eq!(
            candidates("/opt/two/claude", "/opt/one:/opt/two"),
            [Path::new("/opt/two/claude")],
            "a command written as a path is that file alone: a `PATH` holding another \
             `claude` is never consulted, so configuration and process cannot disagree \
             about which binary a run is on"
        );
        assert_eq!(
            candidates("./vendor/claude", "/opt/one"),
            [Path::new("./vendor/claude")],
            "a relative path stands where it was written: joining it onto a `PATH` \
             entry would run a file in a directory the configuration never named"
        );
    }

    #[test]
    fn a_located_command_is_made_absolute_without_reading_what_it_points_at() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let stands = write_executable(&scratch.path().join("bin"), "claude", "#!/bin/sh\necho x\n");
        std::os::unix::fs::symlink("/opt/the/other/claude", scratch.path().join("alias"))
            .expect("a symlink to a program that is not there can be made");

        assert_eq!(
            absolute(Path::new("claude")),
            std::env::current_dir()
                .expect("this process stands somewhere")
                .join("claude"),
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

        assert_eq!(locate("claude", &path_of(&[&bin])), None);
    }
}

#[cfg(test)]
mod sessions {
    // What a session actually does: a fixture CLI is started, and what it read,
    // where it ran, what it printed and what it exited with are read back from
    // the answer. These are the claims a real Claude session is made of, tested
    // against a stand-in so that no test needs the CLI, an account, or a network.

    use super::{Claude, Provider, attributed, invocation, path_of, write_executable};
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

    /// The detail of a provider refusal, after checking it is one.
    fn refusal(error: &Error) -> String {
        let Error::Provider { provider, detail } = &error else {
            panic!(
                "a provider that could not run a session fails as a provider error, not {error:?}"
            );
        };
        assert_eq!(
            provider, "claude",
            "the refusal names the adapter, which is what a preflight and a human read: \
             {error}"
        );
        detail.clone()
    }

    #[test]
    fn a_session_takes_its_prompt_on_stdin_and_answers_with_what_it_printed() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "claude",
            r"#!/bin/sh
printf 'prompt:'
cat
printf '\nanswered\n'
",
        );
        let prompt = "Implement T059 and report.\nRefs: VISION.md section 12.";

        let outcome = Claude::new(cli.display().to_string(), IDLE, HARD)
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
    }

    #[test]
    fn a_session_runs_in_the_directory_it_was_handed_and_nowhere_else() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "claude",
            r"#!/bin/sh
cat > /dev/null
pwd
",
        );
        let work = scratch.path().join("worktree");
        std::fs::create_dir_all(&work).expect("a task worktree to run in");

        let outcome = Claude::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", &work), None)
            .expect("a session runs in the directory it was handed");

        let seen = Path::new(outcome.stdout.trim());
        assert_eq!(
            seen.canonicalize().ok().as_deref(),
            work.canonicalize().ok().as_deref(),
            "the isolation an attempt's evidence is attributed to comes from \
               `Invocation::working_dir`, so that is the directory the CLI must have \
             stood in: it reported `{seen:?}`"
        );
        assert_ne!(
            seen.canonicalize().ok().as_deref(),
            std::env::current_dir()
                .ok()
                .and_then(|here| here.canonicalize().ok())
                .as_deref(),
            "and not the directory this process is standing in, which is the mistake \
             that puts an attempt's work in the main checkout"
        );
    }

    #[test]
    fn a_cli_that_exits_non_zero_is_an_answer_that_notes_its_code_not_a_refusal() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "claude",
            r"#!/bin/sh
cat > /dev/null
printf 'Nothing to do here.\n' >&2
exit 1
",
        );

        let outcome = Claude::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect("a session that ran and refused still answers");

        assert_eq!(
            outcome.exit_code, 1,
            "the exit status is reported as the CLI left it; VISION.md section 3's \
             fourth invariant decides what to make of it, which is not this adapter's call"
        );
        assert_eq!(outcome.stderr, "Nothing to do here.\n");
        assert_eq!(outcome.stdout, "");
    }

    #[test]
    fn a_configured_model_reaches_the_cli_as_two_words_of_its_own() {
        let scratch = tempfile::tempdir().expect("a scratch directory for the fixture CLI");
        let cli = write_executable(
            scratch.path(),
            "claude",
            r#"#!/bin/sh
cat > /dev/null
printf '%s\n' "$@"
"#,
        );
        let mut inv = invocation("a prompt", scratch.path());
        inv.model = Some("claude-opus-4-1".to_owned());

        let outcome = Claude::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&inv, None)
            .expect("a session with a configured model runs");

        assert_eq!(
            outcome.stdout,
            "--print\n--permission-mode\nbypassPermissions\n--model\nclaude-opus-4-1\n",
            "the words the pure builder produced are the words the CLI was started with, \
             one per line of the fixture's own report"
        );
    }

    #[test]
    fn a_bare_command_runs_from_the_directory_the_path_named() {
        let scratch = tempfile::tempdir().expect("a scratch directory for fixture CLIs");
        let bin = scratch.path().join("bin");
        write_executable(
            &bin,
            "claude",
            r"#!/bin/sh
cat > /dev/null
printf 'from-the-path\n'
",
        );

        let outcome = Claude::new("claude", IDLE, HARD)
            .run_with_env(
                &invocation("a prompt", scratch.path()),
                None,
                &path_of(&[&bin]),
            )
            .expect("a command the path holds is runnable");

        assert_eq!(
            outcome.stdout, "from-the-path\n",
            "a configured command is found where the machine says commands live, and \
             the file found there is the one that ran"
        );
    }

    #[test]
    fn a_command_that_is_not_there_is_refused_before_anything_starts() {
        let bus = Bus::new();
        let mut watcher = bus.subscribe();
        let adapter = Claude::new("ktask-claude-that-is-not-installed", IDLE, HARD);

        let error = adapter
            .invoke(&invocation("a prompt", Path::new("/nowhere")), Some(&bus))
            .expect_err("a CLI that is not installed cannot run a session");

        assert_eq!(
            refusal(&error),
            "the configured command `ktask-claude-that-is-not-installed` is not an \
             executable program on `PATH`: nothing was started, and no retry can start one",
            "a missing executable is answered as a provider configuration failure, in \
             words that say a retry cannot fix it — VISION.md section 7 pauses on that \
             class rather than looping"
        );
        let (events, dropped) = watcher.drain();
        assert!(
            events.is_empty() && dropped == 0,
            "nothing was started, so nothing was published to a watching frontend: \
             {} events, {dropped} dropped",
            events.len()
        );
    }

    #[test]
    fn a_command_with_no_words_in_it_is_refused_as_a_configuration_failure() {
        let adapter = Claude::new("   ", IDLE, HARD);

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
        let cli = super::write_file(scratch.path(), "claude", "not a program\n");

        let error = Claude::new(cli.display().to_string(), IDLE, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect_err("a non-executable file cannot run a session");

        let detail = refusal(&error);
        assert!(
            detail.contains("not an executable file where it stands"),
            "a command that names a directory is answered about the file, not about \
             `PATH`: {detail}"
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
            "claude",
            r"#!/bin/sh
cat > /dev/null
printf 'here\n'
sleep 5
",
        );

        let error = Claude::new(cli.display().to_string(), SILENCE_BUDGET, HARD)
            .invoke(&invocation("a prompt", scratch.path()), None)
            .expect_err("a session that stops answering is stopped");

        let detail = refusal(&error);
        assert!(
            detail.contains("past its 400ms idle timeout"),
            "the clocks the adapter was built with are the clocks the session is \
             bounded by: {detail}"
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
            provider: "/opt/claude/bin/claude".to_owned(),
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
            "provider `claude` failed: silent for 400ms, past its 400ms idle timeout",
            "what is replaced is who failed: a caller, a preflight and a TUI all ask \
             which provider, and `claude` is the answer `Provider::name` gives"
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
