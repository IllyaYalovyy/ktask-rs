//! The prompt a provider is handed, and where the documents it is built from
//! live.
//!
//! VISION.md §6 makes context assembly the runner's job: "Task context is
//! assembled by the runner, never hand-injected per task: in v0.1 it is a static
//! context document from the private prompt library plus every ADR recorded so
//! far." [`assemble`] is that assembly, and it is a projection of what the caller
//! already holds rather than a reader of anything: the context document and the
//! template come from the private prompt library (VISION.md §11), the task body
//! comes from the queue, and the ADRs are the one input a caller may have read
//! from inside the repository — because a recorded decision is the one
//! operational document VISION.md §3 lets live there. [`collect_adrs`] does that
//! reading and [`build_prompt`] is where the four inputs meet, so a runner holds a
//! prompt without having to know where any of its parts live.
//!
//! The result belongs to the attempt it was assembled for.
//! [`crate::write_evidence`] files it as that attempt's `context.md`, which is
//! what lets a remediation, a failures screen or an operator with a shell read
//! back the exact words a session was given.
//!
//! ## The two halves of this module
//!
//! [`assemble`] is the pure half: it reads no clock, no environment and no path,
//! which is the whole of why its output is reproducible. A remediation is judged
//! against the attempt it replaces, and a prompt assembled from an instant or a
//! directory listing could not be. ADR-0075 records the decisions that file had to
//! make, including what the header can name as the report path when no project is
//! in scope to say where its state lives.
//!
//! [`ensure_defaults`], [`load_template`] and [`collect_adrs`] are the other half,
//! and they do touch the filesystem, because a prompt has to come from somewhere
//! before it can be assembled. VISION.md §11 is where the first two send an
//! operator: the global, private prompt library below `$XDG_CONFIG_HOME`, and a
//! project's own override below its state directory — never the working copy, which
//! VISION.md §3's invariant 6 keeps clear of the supervisor's files. Both rules are
//! mechanical here rather than advisory: a template is read from one of those two
//! places or the call refuses, and a path reached through a symbolic link is refused
//! rather than followed, because a link is the one way an override could point back
//! inside the repository. [`collect_adrs`] reads the one thing invariant 6 excepts:
//! the decisions below the repository's `docs/adr`. The first two read the
//! environment to find the library, so like `paths`, `config` and `project` they
//! take it through an accessor and keep the environment-blind answer in
//! [`assemble`]: see `docs/DESIGN.md` Conventions.

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, OpenOptions, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use crate::attempt::EVIDENCE_ROOT;
use crate::ids::{AttemptId, TaskId};
use crate::paths::{process_env, prompt_library_with};
use crate::project::Project;
use crate::task::Task;
use crate::{Error, Result};

/// Where a prompt template wants the task body, as [`Task::body`].
const TASK_PLACEHOLDER: &str = "{{TASK}}";

/// The stand-in for a project's state directory in the path an agent is told to
/// write its report to.
///
/// It is a marker rather than a resolved directory because this signature carries
/// no [`crate::Project`], and a path resolved from `$XDG_STATE_HOME` and the
/// process's home directory would depend on something that is not one of the
/// prompt's inputs. A path beginning with a character no path may start with is
/// also the answer that fails loudly: an agent that tries to open it gets an
/// error, where a bare relative path would quietly write a run's report into the
/// repository, which is what VISION.md §3's invariant 6 forbids.
const STATE_DIR: &str = "<state_dir>";

/// The file an attempt's own report is written to, inside that attempt's evidence
/// directory.
///
/// Distinct from the `report.md` that [`crate::write_evidence`] generates from an
/// [`crate::AttemptRecord`]: one is what the runner concluded from the gates and
/// SHAs it watched, the other is what the agent said in its own words, and
/// VISION.md §3's invariant 4 keeps those two apart rather than letting the
/// second stand in for the first.
const AGENT_REPORT_FILE: &str = "agent-report.md";

/// The heading a template that never named [`TASK_PLACEHOLDER`] gets the task
/// under, so that a prompt is never handed over without the task it is about.
const TASK_HEADING: &str = "# The task";

/// The blank line that separates one part of a prompt from the next.
const SECTION_BREAK: &str = "\n\n";

/// The filename of the prompt template, both as the library's default and as a
/// project's own override beside it.
const TEMPLATE_NAME: &str = "task.md";

/// The filename of the context document in the prompt library.
const CONTEXT_NAME: &str = "context.md";

/// The directory below a project's state directory where its own prompts live.
///
/// It carries the library's own directory name on purpose: an override is spelled
/// the same way in either place, so the whole workflow is to copy
/// `prompts/task.md` out of the library, edit the copy, and put it in the
/// project's `prompts/`.
const OVERRIDE_DIR: &str = "prompts";

/// The directory below a repository's root where its recorded decisions live.
///
/// `docs/PROCESS.md` fixes the path. It is also the single deliberate exception to
/// VISION.md §3's invariant 6 — a recorded decision is project documentation as
/// much as it is operational state — which is why this is the one path below a
/// project's working copy this module reads, and why it reads nothing else there.
const ADR_DIR: &str = "docs/adr";

/// The record every repository starts with and that no session is ever sent.
///
/// It is a shape, not a decision anybody made, and a prompt that carried it would
/// teach a session the format of a decision in place of the decision.
const ADR_TEMPLATE: &str = "0000-template.md";

/// The extension a recorded decision is written in.
const ADR_SUFFIX: &str = "md";

/// The mode of the prompt library and of a project's override directory: the
/// prompts of every project one machine supervises belong to one operator.
const LIBRARY_DIR_MODE: u32 = 0o700;

/// The mode of a document this module writes.
const DOCUMENT_MODE: u32 = 0o600;

/// The prompt template a machine that never had one starts with.
///
/// It names [`TASK_PLACEHOLDER`], because a template without it hands a provider a
/// prompt that asks for nothing, and it states the two rules that hold on every
/// task this tool runs: scope, and the fact that completion is mechanical.
const DEFAULT_TEMPLATE: &str = "# Task\n\n\
                                {{TASK}}\n\n\
                                ## How to work\n\n\
                                - Do the task above, and nothing else. Work you noticed but were \
                                not asked for belongs in your report, not in the diff.\n\
                                - Nothing is done on your say-so. The runner re-runs every check \
                                itself, so a weakened check, a skipped test or an edited gate \
                                proves nothing and reads as a policy failure.\n\
                                - Prompts, context, logs, reports and state belong to the \
                                supervisor and live outside the repository. Never write one \
                                inside it.\n\n\
                                ## Finishing\n\n\
                                Write your report to the path the header names. Make its first \
                                line exactly one of `KTASK_RESULT: DONE`, \
                                `KTASK_RESULT: FAILED` or `KTASK_RESULT: NEEDS_INPUT`, then say \
                                what you changed, what you ran to prove it, and what you \
                                deliberately left undone. If a decision is not yours to make, \
                                ask rather than guess.\n";

/// The context document a machine that never had one starts with.
///
/// A placeholder rather than an opinion: VISION.md §6 makes this the standing half
/// of every prompt, and the project it describes is the one thing this crate cannot
/// know. It names no [`TASK_PLACEHOLDER`], because a context document is passed to
/// a session as it stands and is not a template to fill.
const DEFAULT_CONTEXT: &str = "# Project context\n\n\
                              Replace this document with what an agent cannot see in the \
                              repository but has to know to work in it. It is sent to every \
                              session, ahead of the task and the recorded decisions, so \
                              whatever is written here is paid for on every attempt.\n\n\
                              ## What this project is\n\n\
                              One paragraph: what it is for, who uses it, and what done means \
                              here.\n\n\
                              ## How work is done here\n\n\
                              - The commands that build, test and check this project, and the \
                              order they run in.\n\
                              - The conventions no linter will catch.\n\
                              - The paths and subjects an agent must not touch.\n";

/// The prompt one attempt is handed.
///
/// Four parts, in this order, joined by a blank line:
///
/// 1. a header naming the task number and how many tasks the queue holds, the
///    attempt within that task, and the path the report is to be written to;
/// 2. `context_doc`, the project's context document, as written;
/// 3. every ADR recorded so far, in the order the caller gave them, each under a
///    heading naming its position;
/// 4. `template`, with every `{{TASK}}` replaced by [`Task::body`].
///
/// # Determinism
///
/// Equal inputs are the same bytes: nothing here reads a clock, the environment
/// or the filesystem. That is what lets a later attempt be compared against the
/// one it replaced, and it is the determinism VISION.md §6 asks of assembly.
///
/// # What is passed through untouched
///
/// Each part keeps its own text; only trailing whitespace goes, so a context
/// document or an ADR ending in a newline does not open a gap where the next part
/// starts. Nothing inside a part is reflowed or escaped, and nothing is trimmed
/// at the front — indentation inside somebody's code fence is their text.
///
/// # A template that names no placeholder
///
/// The task body is appended under a `# The task` heading instead. A prompt with
/// no task in it is not a prompt, and this answers `String` rather than
/// [`crate::Result`] because a template that got here has already been read from
/// the prompt library, where a missing placeholder is a defect the runner can
/// only answer by handing something whole to the provider.
///
/// # What is deliberately absent
///
/// No instant and no absolute path: both are things a rerun of one task always
/// changes, and a prompt carrying them could not be compared against the attempt
/// it describes. Those are recorded beside the attempt instead, by
/// [`crate::write_evidence`].
#[must_use]
pub fn assemble(
    task: &Task,
    context_doc: &str,
    adrs: &[String],
    template: &str,
    attempt: AttemptId,
    total: usize,
) -> String {
    let parts = [
        header(task.id, attempt, total),
        context_doc.to_string(),
        decisions(adrs),
        filled(template, task.body.trim_end()),
    ];
    // Every part loses the whitespace it picked up at the end of the file it was
    // read out of, and a part that holds nothing contributes nothing — which is
    // the difference between a prompt with a section missing and a prompt that
    // stacked two separators where that section used to be.
    parts
        .into_iter()
        .map(|part| part.trim_end().to_string())
        .filter(|part| !part.is_empty())
        .collect::<Vec<String>>()
        .join(SECTION_BREAK)
}

/// The two lines a session reads first: which task, which attempt, and where its
/// report goes.
///
/// The queue length rides beside the task number because an agent that knows it
/// is 12 of 161 is working to a different sense of urgency from one that knows it
/// is 12 of 12, and the length is the one figure about the queue a prompt can
/// carry without a database in scope.
fn header(task: TaskId, attempt: AttemptId, total: usize) -> String {
    format!(
        "# ktask run — task {task} of {total}, attempt {attempt}\n\nReport: `{path}` — \
         `{STATE_DIR}` is this project's ktask state directory, outside the repository.",
        path = report_path(task, attempt).display(),
    )
}

/// Where one attempt's own report goes, below [`STATE_DIR`].
///
/// The two id levels are the ones [`crate::evidence_dir`] uses, so the file an
/// agent is told to write joins its attempt's directory rather than a third
/// layout nobody else reads.
fn report_path(task: TaskId, attempt: AttemptId) -> PathBuf {
    PathBuf::from(STATE_DIR)
        .join(EVIDENCE_ROOT)
        .join(task.to_string())
        .join(attempt.to_string())
        .join(AGENT_REPORT_FILE)
}

/// Every decision recorded so far, oldest first, under a heading that counts them.
///
/// The order is the caller's, unchanged: ADRs are recorded in the order they were
/// decided, and a later one supersedes an earlier one by number, so an assembly
/// that sorted them alphabetically would hand a session its history upside down.
/// Each one is numbered out of the total as well as counted in the heading, so a
/// session trimming its reading knows how much is left.
fn decisions(adrs: &[String]) -> String {
    let total = adrs.len();
    let heading = format!("# Decisions on record ({total})");
    if adrs.is_empty() {
        return format!("{heading}\n\nNone recorded yet.");
    }
    let blocks: Vec<String> = adrs
        .iter()
        .enumerate()
        .map(|(index, adr)| format!("## {} of {total}\n\n{}", index + 1, adr.trim_end()))
        .collect();
    format!("{heading}\n\n{}", blocks.join(SECTION_BREAK))
}

/// `template` with every [`TASK_PLACEHOLDER`] replaced by `body`.
///
/// Every occurrence, not the first: a template may name the task twice, and a
/// second placeholder left standing is a literal `{{TASK}}` in front of a
/// provider. A template that named none gets the task appended instead, because
/// the alternative is a prompt that asks for nothing.
fn filled(template: &str, body: &str) -> String {
    if template.contains(TASK_PLACEHOLDER) {
        return template.replace(TASK_PLACEHOLDER, body);
    }
    format!("{template}\n\n{TASK_HEADING}\n\n{body}")
}

/// Create the private prompt library and the two documents it starts with.
///
/// VISION.md §11 makes the prompt library the home of the template and the context
/// document every session is sent, and VISION.md §6 makes both of them the standing
/// half of a prompt. A library that has never been written into is neither: on a
/// fresh machine the first task of the first project would otherwise fail on a
/// missing file before an agent had been asked for anything. This answers by making
/// the library, so a first run has a prompt to send and an operator has two files
/// worth editing before the second run.
///
/// ## Only the half that is missing
///
/// A document that is already there is left exactly as it is — its bytes, and the
/// mode somebody gave it. An operator's own template is the reason a prompt library
/// exists, and a run that reset it on its way past would be the third tool this
/// decade to quietly overwrite somebody's configuration.
///
/// ## Permissions
///
/// The library is `0700` and each document written here is `0600`: VISION.md §11
/// asks for restrictive permissions rather than for a considerate umask. A library
/// directory somebody else opened is taken back to `0700` on the way past, because
/// a grant that was never asked for cannot be removed by asking for it.
///
/// # Errors
///
/// [`Error::Config`] when neither `XDG_CONFIG_HOME` nor `HOME` names a base (see
/// [`crate::prompt_library`]); [`Error::Policy`] when the library path or one of its
/// documents is occupied by something else, or is reached through a symbolic link;
/// [`Error::Io`] when a directory or a document could not be written.
pub fn ensure_defaults() -> Result<()> {
    ensure_defaults_with(&process_env)
}

/// The prompt template one project is sent: its own override, or the library's.
///
/// The order is VISION.md §11's: a per-project override wins over the global
/// default, and both live outside the repository, so a project's prompt is never a
/// file an agent could commit into the project's own history. The override is
/// `<state_dir>/prompts/task.md`; the fallback is [`crate::prompt_library`]`/task.md`.
///
/// The library is created first, exactly as [`ensure_defaults`] creates it, so a
/// first run has a template to hand back rather than a missing file to report, and
/// so the two documents an operator was meant to find are the two that are there.
///
/// The text is handed back as the file holds it — untrimmed, undecorated. Whether
/// it wants a task in it is [`assemble`]'s question, and it is asked the same way
/// of a project's own template as of the default.
///
/// # What is never read
///
/// Nothing below `project.root`. A working copy that holds its own `task.md`, or a
/// `prompts/` directory of its own, is not an override: it is the supervisor's
/// business leaking into the repository, which VISION.md §3's invariant 6 forbids
/// and this function does not negotiate with. An override reached through a
/// symbolic link is refused for the same reason — a link is the one way an override
/// could point back inside the working copy, and it is refused rather than followed
/// because where it points is exactly what cannot be checked from here.
///
/// # Errors
///
/// As [`ensure_defaults`], plus [`Error::Policy`] when an override path is occupied
/// by something that is not a file or is reached through a link, and
/// [`Error::Corrupt`] when the chosen document is not UTF-8 text. A project with no
/// state directory yet is not an error: it has written no override, so the library
/// answers.
pub fn load_template(project: &Project) -> Result<String> {
    load_template_with(&process_env, project)
}

/// [`ensure_defaults`] with the environment supplied by the caller, which is how a
/// test aims the library at a scratch directory: `docs/DESIGN.md` Conventions keeps
/// a test out of both this repository and the operator's real configuration.
fn ensure_defaults_with(env: &dyn Fn(&str) -> Option<String>) -> Result<()> {
    let library = prompt_library_with(env)?;
    private_directory(&library)?;
    write_default(&library.join(TEMPLATE_NAME), DEFAULT_TEMPLATE)?;
    write_default(&library.join(CONTEXT_NAME), DEFAULT_CONTEXT)?;
    Ok(())
}

/// [`load_template`] with the environment supplied by the caller.
fn load_template_with(env: &dyn Fn(&str) -> Option<String>, project: &Project) -> Result<String> {
    ensure_defaults_with(env)?;
    let source = match project_override(project)? {
        Some(path) => path,
        None => prompt_library_with(env)?.join(TEMPLATE_NAME),
    };
    read_document(&source)
}

/// The prompt one attempt of one project's task is handed, read from where its
/// documents live.
///
/// Every standing input of [`assemble`] has a home, and this is where they are
/// looked up: the context document from the private prompt library, the template
/// through [`load_template`] (which prefers this project's own override), and the
/// recorded decisions from [`collect_adrs`] below the project's root. This is the
/// reading half of the split ADR-0075 made — `assemble` stays the half that reads
/// nothing — so a runner calls it once at the start of an attempt, files what comes
/// back as that attempt's own `context.md` through [`crate::write_evidence`], and can
/// reproduce the exact words the session was given from that file rather than from a
/// second read of two directories that have moved on since.
///
/// The library is ensured before anything is read from it, exactly as
/// [`load_template`] ensures it, so a first run gets a prompt rather than a missing
/// file. Reading it twice on one call is the cheap half of that guarantee (ADR-0077:
/// two lookups and a permission set once the library exists) and is worth being
/// explicit about: the rule is "the library exists before any of its documents are
/// opened", not "the last function to run made it so".
///
/// # What is never read
///
/// A prompt document from inside the working copy. Only decisions are read there, and
/// only below `docs/adr`. The context document is the library's even for a project
/// that wrote an override template: ADR-0077 declined a per-project `load_context`
/// because nothing called it, and §11 names an override for the template alone.
///
/// # Errors
///
/// As [`load_template`], plus the errors of [`collect_adrs`]. A prompt is not built
/// from a decision archive in the wrong shape: a session handed a prompt with
/// decisions quietly missing from it is a session that will re-decide something
/// already settled, and it will look like the supervisor forgot.
pub fn build_prompt(
    project: &Project,
    task: &Task,
    attempt: AttemptId,
    total: usize,
) -> Result<String> {
    build_prompt_with(&process_env, project, task, attempt, total)
}

/// [`build_prompt`] with the environment supplied by the caller. See
/// [`ensure_defaults_with`] for why the accessor is threaded this far.
fn build_prompt_with(
    env: &dyn Fn(&str) -> Option<String>,
    project: &Project,
    task: &Task,
    attempt: AttemptId,
    total: usize,
) -> Result<String> {
    ensure_defaults_with(env)?;
    let context_doc = read_document(&prompt_library_with(env)?.join(CONTEXT_NAME))?;
    let template = load_template_with(env, project)?;
    let adrs = collect_adrs(&project.root)?;
    Ok(assemble(
        task,
        &context_doc,
        &adrs,
        &template,
        attempt,
        total,
    ))
}

/// The project's own template, when it wrote one.
///
/// `Ok(None)` is the ordinary answer for a project that never wrote one, including
/// one whose state directory does not exist yet: not having an override is the
/// common case, not a defect, and a run is not entitled to register a project just
/// by reading a prompt. Anything that is *there* in the wrong shape is refused by
/// name, because an operator who wrote an override and pointed it at the wrong
/// thing deserves to be told rather than to be sent the default.
fn project_override(project: &Project) -> Result<Option<PathBuf>> {
    let directory = project.state_dir.join(OVERRIDE_DIR);
    match presence(&directory)? {
        Presence::Directory => {}
        Presence::Link => return Err(reached_by_link(&directory)),
        Presence::File => return Err(unusable(&directory, "a directory")),
        Presence::Absent => return Ok(None),
    }
    let path = directory.join(TEMPLATE_NAME);
    match presence(&path)? {
        Presence::File => Ok(Some(path)),
        Presence::Link => Err(reached_by_link(&path)),
        Presence::Directory => Err(unusable(&path, "a file")),
        Presence::Absent => Ok(None),
    }
}

/// Every decision this repository has recorded, oldest first, each as its own text.
///
/// VISION.md §3's invariant 8 makes a recorded decision available to the tasks that
/// come after it, and `ktask-rs resolve` is what records one: it writes
/// `docs/adr/NNNN-short-title.md` into the working copy. This is where those words
/// come back out, and it is the only place in this module that reads below a
/// project's root — the one exception invariant 6 grants, and therefore the only
/// place a prompt is assembled out of text this module did not write and cannot vouch
/// for beyond its own shape.
///
/// ## The order is the filename, and the filename is the number
///
/// `read_dir` hands entries back in whatever order the filesystem keeps them, which
/// is nobody's idea of an order, so the names are sorted. `docs/PROCESS.md` fixes the
/// name as `NNNN-short-title.md` with a four-digit number, so filename order *is*
/// decision order, and a session reads its project's history the way it was decided:
/// a later ADR supersedes an earlier one by number, so handing the two over the other
/// way round hands a session the answer that was replaced. The padding is
/// load-bearing — past 9999 records, `10000-…` sorts before `9999-…` — and a project
/// that gets that far revisits this rule rather than discovering it.
///
/// ## What is not a decision
///
/// `0000-template.md` is skipped by name: it is the shape every repository copies,
/// not something anybody decided. Anything that is not a `.md` document sitting
/// directly in `docs/adr` is skipped too, because that directory collects the
/// neighbours a `docs/` directory always grows — an editor's `0003-real.md.bak`, a
/// `README`, a `drafts/` folder, and a directory whose name happens to end in `.md`.
/// A symbolic link is refused rather than followed, as a prompt document is
/// (ADR-0077): where a link points is undecidable from here, and a decision read
/// through one is a decision whose number and author the filename does not describe.
///
/// ## No directory at all is the ordinary answer
///
/// A project on its first task has decided nothing and never ran `resolve`, so an
/// empty list is a fact about the project rather than a defect in it. [`assemble`]
/// renders the empty list as "None recorded yet." instead of leaving a hole where the
/// decisions belong.
///
/// # Errors
///
/// [`Error::Policy`] when `docs/adr` is occupied by something that is not a
/// directory, or when the directory or one of its documents is reached through a
/// symbolic link; [`Error::Corrupt`] when a record is not UTF-8 text; [`Error::Io`]
/// when the directory is there and cannot be listed.
pub fn collect_adrs(repo_root: &Path) -> Result<Vec<String>> {
    let directory = repo_root.join(ADR_DIR);
    match presence(&directory)? {
        Presence::Absent => return Ok(Vec::new()),
        Presence::Directory => {}
        Presence::File => return Err(unusable(&directory, "a directory")),
        Presence::Link => return Err(reached_by_link(&directory)),
    }
    let mut documents: Vec<PathBuf> = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        if !is_decision_document(&path) {
            continue;
        }
        match presence(&path)? {
            Presence::File => documents.push(path),
            // A directory named like a record, and a record deleted while the
            // listing was in progress, are both reasons to move on. A link is a
            // reason to stop: see the rule above.
            Presence::Directory | Presence::Absent => {}
            Presence::Link => return Err(reached_by_link(&path)),
        }
    }
    // Every path here shares one parent, so sorting the paths sorts the filenames,
    // and the filenames are the order the numbers were chosen to be read in.
    documents.sort();
    documents.iter().map(|path| read_document(path)).collect()
}

/// Whether `path` names a decision record rather than one of the shapes that gather
/// beside them.
///
/// The template is answered first because it is the one `.md` file in the directory
/// that must never reach a session, and the extension is asked of the name rather
/// than matched as a substring, so `0003-real.md.bak` is read as what it is — a
/// backup somebody left behind.
fn is_decision_document(path: &Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    name != ADR_TEMPLATE && Path::new(name).extension() == Some(OsStr::new(ADR_SUFFIX))
}

/// Make `path` exist as a directory only its owner can read.
///
/// The mode is set rather than only asked for at creation, which is how
/// `crate::project` makes a state directory and why the two agree: a directory that
/// already exists keeps whatever mode somebody gave it, and only a set takes a grant
/// back.
fn private_directory(path: &Path) -> Result<()> {
    match presence(path)? {
        Presence::Directory => {}
        Presence::Link => return Err(reached_by_link(path)),
        Presence::File => return Err(unusable(path, "a directory")),
        Presence::Absent => {
            // `recursive` with a `mode` applies that mode to every directory it
            // creates, so the `ktask-rs` directory above the library is no more
            // open than the library.
            DirBuilder::new()
                .mode(LIBRARY_DIR_MODE)
                .recursive(true)
                .create(path)?;
        }
    }
    fs::set_permissions(path, Permissions::from_mode(LIBRARY_DIR_MODE))?;
    Ok(())
}

/// Write `text` to `path` unless something is already there.
///
/// An existing document is left alone — bytes and mode both — because the reason a
/// prompt library exists is to hold the prompt somebody wrote. A missing one is
/// created with `create_new` rather than with a write that follows: two runs on one
/// machine then agree without either having to lock, and the loser of the race
/// finds its own bytes already in place because both write the same default.
fn write_default(path: &Path, text: &str) -> Result<()> {
    match presence(path)? {
        Presence::File => return Ok(()),
        Presence::Link => return Err(reached_by_link(path)),
        Presence::Directory => return Err(unusable(path, "a file")),
        Presence::Absent => {}
    }
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(DOCUMENT_MODE)
        .open(path)
    {
        Ok(file) => file,
        // Another run got here between the look above and this one. Its bytes are
        // these bytes, and overwriting them is what this function exists to refuse.
        Err(why) if created_by_another_run(&why) => return Ok(()),
        Err(why) => return Err(why.into()),
    };
    file.write_all(text.as_bytes())?;
    fs::set_permissions(path, Permissions::from_mode(DOCUMENT_MODE))?;
    // Synced, not merely written: a document created but left in the page cache
    // comes back as an empty file, and an empty template is left in place by the
    // rule above, because something is there.
    file.sync_all()?;
    Ok(())
}

/// Read a prompt document as the text it is required to be.
fn read_document(path: &Path) -> Result<String> {
    let bytes = fs::read(path)?;
    String::from_utf8(bytes).map_err(|why| Error::Corrupt {
        detail: format!("`{}` is not UTF-8 text: {why}", path.display()),
        seq: None,
    })
}

/// What the filesystem says a path is, without following a symbolic link.
enum Presence {
    /// Nothing is at the path, and no directory above it refused the question.
    Absent,
    /// A directory.
    Directory,
    /// An ordinary file.
    File,
    /// A symbolic link, wherever it points.
    Link,
}

/// [`Presence`] for `path`, as the filesystem says it rather than as the name
/// suggests.
///
/// A link is answered before its type because a link is never the thing this module
/// may read or write, and asking what it points at is the question this module must
/// not answer.
fn presence(path: &Path) -> Result<Presence> {
    match fs::symlink_metadata(path) {
        Ok(found) if found.file_type().is_symlink() => Ok(Presence::Link),
        Ok(found) if found.is_dir() => Ok(Presence::Directory),
        Ok(found) if found.is_file() => Ok(Presence::File),
        Ok(_) => Err(unusable(path, "a file or a directory")),
        Err(why) if refused_because_absent(&why) => Ok(Presence::Absent),
        Err(why) => Err(why.into()),
    }
}

/// Whether the filesystem says the path simply is not there.
///
/// `NotADirectory` is the same answer for this module: a level above the path is not
/// a directory, so nothing that belongs below it can be there.
fn refused_because_absent(why: &io::Error) -> bool {
    matches!(
        why.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Whether a creation refused with `AlreadyExists` is the race this function would
/// rather win than report.
///
/// Two runs on one machine can reach the same missing document at the same moment,
/// and both write the same default, so the loser has nothing to say: the bytes it
/// came to write are already there. Every other refusal — a directory that may not
/// be written into, a full disk, a file system that is read-only — is a real
/// failure, and saying which is which in a named function is what keeps the
/// decision readable from inside the match without being untestable there.
fn created_by_another_run(why: &io::Error) -> bool {
    why.kind() == io::ErrorKind::AlreadyExists
}

/// Refuse a path that is there and is not the kind of thing this module needs.
fn unusable(path: &Path, wanted: &str) -> Error {
    Error::Policy {
        detail: format!("`{}` is already there and is not {wanted}", path.display()),
        paths: vec![path.to_path_buf()],
    }
}

/// Refuse a prompt-library or override path reached through a symbolic link.
///
/// Refused in both directions on one rule: this module neither writes through a
/// link, which would file a run's document wherever it points, nor reads through
/// one, which is how a template meant for the private library turns out to be a
/// file inside the repository the run supervises. Copying the file is the workflow
/// VISION.md §11 describes, and the one that can be checked.
fn reached_by_link(path: &Path) -> Error {
    Error::Policy {
        detail: format!(
            "`{}` is refused because it is a symbolic link; copy the file rather than \
             linking it, since a link can lead into the repository these documents must \
             stay out of",
            path.display(),
        ),
        paths: vec![path.to_path_buf()],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_TEMPLATE, DOCUMENT_MODE, LIBRARY_DIR_MODE, TASK_PLACEHOLDER, assemble,
        build_prompt, build_prompt_with, collect_adrs, created_by_another_run, ensure_defaults,
        ensure_defaults_with, load_template, load_template_with, write_default,
    };
    use crate::{
        AttemptId, AttemptRecord, Error, Project, Task, TaskId, TaskStatus, Usage, evidence_dir,
        prompt_library, write_evidence,
    };
    use proptest::collection::vec;
    use proptest::prelude::*;
    use std::env::var_os;
    use std::fs::{self, Permissions};
    use std::io;
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    use std::os::unix::net::UnixListener;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use tempfile::{TempDir, tempdir};
    use time::macros::datetime;

    /// The queue position, the queue length and the attempt the fixtures use.
    const TASK_NUMBER: u32 = 12;
    const TOTAL: usize = 40;

    /// The id the prompt-library fixtures give their project, so a test can name
    /// the state directory the way a registration would.
    const PROJECT_ID: &str = "0123456789abcdef";

    /// Set on a child copy of this binary to name the scratch home it is to work
    /// in; its presence is the whole of the child role. See
    /// `the_public_entry_points_read_the_environment_the_process_actually_has`.
    const CHILD_SCRATCH: &str = "KTASK_PROMPT_LIBRARY_CHILD";

    /// The one test a child copy of this binary is told to run: itself, with the
    /// variable above set so that it takes the other branch.
    const CHILD_TEST: &str =
        "context::tests::the_public_entry_points_read_the_environment_the_process_actually_has";

    /// A context document in the shape `.ktask/context.md` is written in.
    const CONTEXT: &str = "# Project context\n\nRead VISION.md first.";

    /// A prompt template in the shape `.ktask/prompt.md` is written in.
    const TEMPLATE: &str = "# Prompt template\n\n## Your task\n\n{{TASK}}\n\n## Finishing\n\n\
                            Run ./scripts/quality.sh.";

    const ADR_ONE: &str = "# 0074. First decision\n\nRedaction runs inside the write path.\n";
    const ADR_TWO: &str = "# 0075. Second decision\n\nAssembly reads nothing.\n";

    /// The task as the queue holds it: its body is what the template is given.
    fn task() -> Task {
        Task {
            id: TaskId::new(TASK_NUMBER),
            status: TaskStatus::Pending,
            body: "## T080 Context assembly\n\n**Outcome:** the runner assembles the prompt."
                .to_owned(),
            outcome: "the prompt handed to a provider is assembled by the runner".to_owned(),
            done_when: "assembly is deterministic for the same inputs".to_owned(),
            verify: "cargo nextest run -p ktask-core -E 'test(/context/)'".to_owned(),
            refs: "VISION.md sections 6 and 11".to_owned(),
            gate: None,
            protocol: None,
        }
    }

    /// The same task holding `body` and nothing else changed.
    fn task_with(body: &str) -> Task {
        Task {
            body: body.to_owned(),
            ..task()
        }
    }

    /// The two decisions a project that has been running for a while holds.
    fn decisions() -> Vec<String> {
        vec![ADR_ONE.to_owned(), ADR_TWO.to_owned()]
    }

    /// The prompt the fixtures assemble, with both decisions on record.
    fn prompt() -> String {
        assemble(
            &task(),
            CONTEXT,
            &decisions(),
            TEMPLATE,
            AttemptId::new(2),
            TOTAL,
        )
    }

    #[test]
    fn the_prompt_is_the_header_the_context_the_decisions_then_the_template() {
        let expected = [
            "# ktask run — task 12 of 40, attempt 2",
            "",
            "Report: `<state_dir>/attempts/12/2/agent-report.md` — `<state_dir>` is this \
             project's ktask state directory, outside the repository.",
            "",
            "# Project context",
            "",
            "Read VISION.md first.",
            "",
            "# Decisions on record (2)",
            "",
            "## 1 of 2",
            "",
            "# 0074. First decision",
            "",
            "Redaction runs inside the write path.",
            "",
            "## 2 of 2",
            "",
            "# 0075. Second decision",
            "",
            "Assembly reads nothing.",
            "",
            "# Prompt template",
            "",
            "## Your task",
            "",
            "## T080 Context assembly",
            "",
            "**Outcome:** the runner assembles the prompt.",
            "",
            "## Finishing",
            "",
            "Run ./scripts/quality.sh.",
        ]
        .join("\n");
        assert_eq!(prompt(), expected);
    }

    #[test]
    fn the_report_path_names_the_attempt_it_was_assembled_for() {
        let prompt = assemble(&task(), CONTEXT, &[], TEMPLATE, AttemptId::new(11), TOTAL);
        assert!(
            prompt.contains("Report: `<state_dir>/attempts/12/11/agent-report.md`"),
            "the header has to name the attempt whose evidence the report joins: {prompt}"
        );
    }

    #[test]
    fn the_context_document_is_passed_through_word_for_word() {
        let odd = "# Context\n\n  indented code\n\ttab\n\ntrailing\n\n";
        let prompt = assemble(&task(), odd, &[], TEMPLATE, AttemptId::new(1), TOTAL);
        let starts = prompt
            .find("# Context")
            .expect("the context document is in the prompt");
        let decisions = prompt
            .find("# Decisions on record")
            .expect("the decisions are in the prompt");
        assert!(
            prompt[starts..].starts_with(odd.trim_end()),
            "the document keeps its own lines and its own indentation: {prompt}"
        );
        assert!(
            prompt.contains(&format!("{}\n\n# Decisions on record", odd.trim_end())),
            "a document's trailing newlines are its own, not a gap between sections: {prompt}"
        );
        assert!(
            starts < decisions,
            "the context document has to precede the decisions: {prompt}"
        );
    }

    #[test]
    fn the_task_body_lands_without_the_gap_it_was_read_in_with() {
        let gap = "**Outcome:** the runner assembles the prompt.\n\n";
        let prompt = assemble(
            &task_with(gap),
            CONTEXT,
            &[],
            "{{TASK}}\n\n## Finishing\n\nrun the gates",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with(
                "**Outcome:** the runner assembles the prompt.\n\n## Finishing\n\nrun \
                             the gates"
            ),
            "a body copied out of a plan file must not push the template's next heading away: \
             {prompt}"
        );
    }

    #[test]
    fn every_decision_recorded_reaches_the_prompt_in_its_own_order() {
        let three = vec![
            "ADR alpha".to_owned(),
            "ADR beta".to_owned(),
            "ADR gamma".to_owned(),
        ];
        let prompt = assemble(&task(), CONTEXT, &three, TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(prompt.contains("# Decisions on record (3)"));
        let mut place = 0;
        for (index, adr) in three.iter().enumerate() {
            let found = prompt[place..]
                .find(adr.as_str())
                .unwrap_or_else(|| panic!("decision {} is missing: {prompt}", index + 1));
            place += found + adr.len();
            assert!(
                prompt.contains(&format!("## {} of 3", index + 1)),
                "decision {} is not numbered where it stands: {prompt}",
                index + 1,
            );
        }
    }

    #[test]
    fn a_project_with_no_decisions_says_so_rather_than_leaving_the_slot_empty() {
        let prompt = assemble(&task(), CONTEXT, &[], TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(
            prompt.contains("# Decisions on record (0)\n\nNone recorded yet."),
            "an empty list is a fact about the project, not an omission: {prompt}"
        );
    }

    #[test]
    fn the_task_body_replaces_every_placeholder_the_template_named() {
        let template = "Do the task.\n\n{{TASK}}\n\nRestate it: {{TASK}}\nDone?";
        let prompt = assemble(&task(), CONTEXT, &[], template, AttemptId::new(1), TOTAL);
        assert!(
            !prompt.contains(TASK_PLACEHOLDER),
            "a placeholder that survives reaches the provider as a literal: {prompt}"
        );
        let body = task().body;
        assert_eq!(
            prompt.matches(body.as_str()).count(),
            2,
            "the template named the placeholder twice: {prompt}"
        );
    }

    #[test]
    fn a_template_that_never_named_the_placeholder_still_carries_the_task() {
        let prompt = assemble(
            &task(),
            CONTEXT,
            &[],
            "Work on whatever you find.",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with(
                "\n\n# The task\n\n## T080 Context assembly\n\n**Outcome:** the \
                            runner assembles the prompt."
            ),
            "a prompt with no task in it is not a prompt: {prompt}"
        );
    }

    #[test]
    fn an_empty_part_leaves_no_empty_section_behind() {
        let prompt = assemble(&task(), "", &[], TEMPLATE, AttemptId::new(1), TOTAL);
        assert!(
            !prompt.contains("\n\n\n"),
            "an absent context document stacked two separators: {prompt}"
        );
    }

    #[test]
    fn the_prompt_ends_where_the_last_words_do() {
        let prompt = assemble(
            &task(),
            CONTEXT,
            &[],
            "{{TASK}}\n\nfin.\n\n\n",
            AttemptId::new(1),
            TOTAL,
        );
        assert!(
            prompt.ends_with("\n\nfin.") && !prompt.ends_with(char::is_whitespace),
            "a prompt that ends in a gap invites the next section nobody wrote: {prompt}"
        );
    }

    #[test]
    fn the_same_inputs_assemble_the_same_bytes_twice() {
        let first = prompt();
        let second = assemble(
            &task(),
            CONTEXT,
            &decisions(),
            TEMPLATE,
            AttemptId::new(2),
            TOTAL,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn the_assembled_prompt_is_recorded_with_the_attempt() {
        let scratch = tempdir().expect("a scratch directory to hold a state directory");
        let state_dir = scratch.path().join("state").join("0123456789abcdef");
        fs::create_dir_all(&state_dir).expect("a state directory to file evidence below");
        let project = Project {
            root: scratch.path().join("repository"),
            id: "0123456789abcdef".to_owned(),
            state_dir,
        };
        let attempt = AttemptId::new(2);

        write_evidence(&project, &record(attempt), &prompt())
            .expect("an attempt's evidence accepts the prompt it was started with");
        let filed = evidence_dir(&project, TaskId::new(TASK_NUMBER), attempt).join("context.md");
        let filed =
            fs::read_to_string(&filed).unwrap_or_else(|why| panic!("{}: {why}", filed.display()));

        assert!(
            filed.contains("Report: `<state_dir>/attempts/12/2/agent-report.md`"),
            "the prompt an attempt was started with is the prompt its evidence holds: {filed}"
        );

        assert_eq!(
            filed,
            prompt(),
            "the prompt an attempt was started with is the prompt its evidence holds",
        );
    }

    /// The attempt the prompt is recorded against, with no gates and no usage.
    fn record(attempt: AttemptId) -> AttemptRecord {
        AttemptRecord {
            id: attempt,
            task: TaskId::new(TASK_NUMBER),
            started: datetime!(2026-09-21 09:14:03.5 UTC),
            ended: Some(datetime!(2026-09-21 09:41:47 UTC)),
            model_configured: Some("gpt-5.6-sol".to_owned()),
            model_reported: Some("gpt-5.6-sol-2026-09-01".to_owned()),
            session_id: Some("sess_01HQZK".to_owned()),
            exit_reason: "exited 0".to_owned(),
            gates: Vec::new(),
            usage: Some(Usage::unavailable()),
            base_sha: "0b78d3f1c2a4".to_owned(),
            candidate_sha: None,
        }
    }

    proptest! {
        /// Whatever the four parts hold, every one of them reaches the prompt, no
        /// placeholder is left standing, and two calls over equal inputs are the
        /// same bytes. The inputs are stripped of the placeholder so the question
        /// asked is about the assembly rather than about a body quoting it.
        #[test]
        fn every_part_reaches_the_prompt_and_the_answer_is_stable(
            body in any::<String>(),
            context in any::<String>(),
            adrs in vec(any::<String>(), 0..4),
            template in any::<String>(),
            attempt in 1u32..8,
            total in 0usize..500,
        ) {
            let body = body.replace(TASK_PLACEHOLDER, "");
            let context = context.replace(TASK_PLACEHOLDER, "");
            let template = template.replace(TASK_PLACEHOLDER, "");
            let adrs: Vec<String> = adrs
                .iter()
                .map(|adr| adr.replace(TASK_PLACEHOLDER, ""))
                .collect();
            let once = assemble(
                &task_with(&body),
                &context,
                &adrs,
                &template,
                AttemptId::new(attempt),
                total,
            );
            let twice = assemble(
                &task_with(&body),
                &context,
                &adrs,
                &template,
                AttemptId::new(attempt),
                total,
            );
            prop_assert_eq!(&once, &twice, "assembly is a function of its inputs");
            prop_assert!(once.contains(body.trim_end()), "the task body went missing");
            prop_assert!(!once.contains(TASK_PLACEHOLDER), "a placeholder reached the provider");
            prop_assert!(
                !once.ends_with(char::is_whitespace),
                "a prompt ending in whitespace joined parts that were not trimmed",
            );
            for adr in &adrs {
                prop_assert!(once.contains(adr.trim_end()), "a decision went missing");
            }
        }
    }

    /// The three scratch directories one prompt-library test needs.
    ///
    /// None of them is created by the fixture: whether a base directory exists
    /// yet is one of the things these tests are about. All three are below a
    /// [`TempDir`], which `docs/DESIGN.md` Conventions requires of every fixture
    /// so a test never leaves a file in this repository.
    struct Scratch {
        /// What `XDG_CONFIG_HOME` names: the base the prompt library is built under.
        config: PathBuf,
        /// What `XDG_STATE_HOME` names: the base a project's state directory is
        /// built under.
        state: PathBuf,
        /// The project's working copy: the one directory no prompt or context
        /// document of ours is ever read from, and nothing here ever writes.
        repository: PathBuf,
        /// Held so the bases exist until the test ends.
        root: TempDir,
    }

    impl Scratch {
        /// A scratch home whose bases do not exist yet.
        fn new() -> Self {
            let root = tempdir().expect("a scratch home outside the repository");
            let (config, state, repository) = layout(root.path());
            Self {
                config,
                state,
                repository,
                root,
            }
        }

        /// The environment these bases name: both XDG variables and no `HOME`,
        /// so an answer never depends on the machine running the suite.
        fn env(&self) -> impl Fn(&str) -> Option<String> {
            environment(vec![
                ("XDG_CONFIG_HOME", self.config.clone()),
                ("XDG_STATE_HOME", self.state.clone()),
            ])
        }

        /// The state directory a registration names for this scratch's project,
        /// whether or not it exists.
        fn state_dir(&self) -> PathBuf {
            self.state.join(PROJECT_ID)
        }

        /// Where this project's own prompts live, below its state directory.
        fn override_dir(&self) -> PathBuf {
            self.state_dir().join("prompts")
        }

        /// The project under test.
        fn project(&self) -> Project {
            Project {
                root: self.repository.clone(),
                id: PROJECT_ID.to_owned(),
                state_dir: self.state_dir(),
            }
        }

        /// The scratch directory itself, for a file that belongs to nobody's
        /// layout — a link target outside the library, for instance.
        fn outside(&self) -> &Path {
            self.root.path()
        }
    }

    /// An environment made of exactly these variables, owned by the closure so a
    /// test can ask the same environment twice. An empty list is the environment
    /// of a process that was started with nothing usable in it.
    fn environment(entries: Vec<(&str, PathBuf)>) -> impl Fn(&str) -> Option<String> {
        move |key| {
            entries
                .iter()
                .find(|(name, _)| *name == key)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        }
    }

    /// The three bases a scratch home is made of, below `root`.
    ///
    /// A child copy of this test derives the same three from the home its parent
    /// named, so a scratch layout is spelled in exactly one place.
    fn layout(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        (
            root.join("config-home"),
            root.join("state-home"),
            root.join("repository"),
        )
    }

    /// The prompt library the Paths section of `docs/DESIGN.md` specifies below a
    /// configuration base, spelled out here rather than reached through
    /// `crate::paths::prompt_library`: an expectation computed with the code under
    /// test would accept the library quietly moving.
    fn library_under(config_home: &Path) -> PathBuf {
        config_home.join("ktask-rs").join("prompts")
    }

    /// The mode bits a path carries, to the last three.
    fn mode(path: &Path) -> u32 {
        fs::metadata(path)
            .unwrap_or_else(|why| panic!("`{}` could not be looked at: {why}", path.display()))
            .permissions()
            .mode()
            & 0o777
    }

    /// The names inside a directory, sorted.
    fn names(directory: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(directory)
            .unwrap_or_else(|why| panic!("`{}` could not be listed: {why}", directory.display()))
            .map(|entry| {
                entry
                    .expect("a listable directory entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        found.sort();
        found
    }

    /// Read a document back, panicking with its path if it cannot be read.
    fn document(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_else(|why| panic!("`{}` is unreadable: {why}", path.display()))
    }

    /// Write a document, with its parents made first.
    fn write_document(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("a directory to write a document in");
        }
        fs::write(path, text).expect("a document can be written");
    }

    #[test]
    fn a_first_use_writes_a_task_template_and_a_context_document() {
        let home = Scratch::new();
        ensure_defaults_with(&home.env())
            .expect("a machine that never had a prompt library gets one");
        let library = library_under(&home.config);
        let template = library.join("task.md");
        let context = library.join("context.md");

        assert!(
            template.is_file(),
            "no task template was written at {}",
            template.display()
        );
        assert!(
            context.is_file(),
            "no context document was written at {}",
            context.display()
        );
        let written = document(&template);
        assert!(
            written.contains("{{TASK}}"),
            "the default template never names the task, so every prompt built from it \
             would ask for nothing:\n{written}"
        );
        assert!(!document(&context).trim().is_empty());
    }

    #[test]
    fn the_two_defaults_are_different_documents_and_only_one_is_a_template() {
        // The context document is handed to a session as it stands; the template
        // is the part with a hole in it. Swapping the two would send a project's
        // context to the assembler and the task nowhere, which only a test that
        // tells them apart can notice.
        let home = Scratch::new();
        ensure_defaults_with(&home.env()).expect("a first use writes both documents");
        let library = library_under(&home.config);
        let template = document(&library.join("task.md"));
        let context = document(&library.join("context.md"));

        assert_ne!(template, context);
        assert!(
            !context.contains("{{TASK}}"),
            "the context document holds a hole:\n{context}"
        );

        let prompt = assemble(&task(), &context, &[], &template, AttemptId::new(1), 1);
        assert!(
            prompt.contains(task().body.trim_end()),
            "the default template does not carry the task it was written for:\n{prompt}"
        );
        assert!(
            !prompt.contains("{{TASK}}"),
            "a placeholder reached the provider:\n{prompt}"
        );
    }

    #[test]
    fn ensure_defaults_writes_once_and_never_overwrites_a_document_it_finds() {
        let home = Scratch::new();
        ensure_defaults_with(&home.env()).expect("a first use writes the defaults");
        let library = library_under(&home.config);
        let context = library.join("context.md");
        let written = document(&context);
        write_document(&library.join("task.md"), "an operator's own prompt\n");

        ensure_defaults_with(&home.env())
            .expect("a second use finds the library and asks nothing of it");

        assert_eq!(
            document(&library.join("task.md")),
            "an operator's own prompt\n",
            "ensuring the defaults overwrote the template somebody wrote"
        );
        assert_eq!(document(&context), written, "the context document moved");
    }

    #[test]
    fn ensure_defaults_keeps_the_library_and_its_documents_private() {
        let home = Scratch::new();
        ensure_defaults_with(&home.env()).expect("a first use writes the defaults");
        let library = library_under(&home.config);

        assert_eq!(
            mode(&library),
            0o700,
            "the library holds the prompts of every project this machine supervises"
        );
        for name in ["task.md", "context.md"] {
            assert_eq!(
                mode(&library.join(name)),
                0o600,
                "`{name}` is readable by everybody but the operator"
            );
        }
    }

    #[test]
    fn ensure_defaults_takes_back_a_library_directory_somebody_opened() {
        let home = Scratch::new();
        let library = library_under(&home.config);
        fs::create_dir_all(&library).expect("a library somebody else made");
        fs::set_permissions(&library, Permissions::from_mode(0o755))
            .expect("an open library is a mode away");

        ensure_defaults_with(&home.env()).expect("a first use writes the defaults");

        assert_eq!(
            mode(&library),
            0o700,
            "a grant back is a set, not a request"
        );
    }

    #[test]
    fn ensure_defaults_creates_the_configuration_directories_that_are_not_there() {
        let home = Scratch::new();
        assert!(
            !home.config.exists(),
            "the fixture must start with no configuration base"
        );

        ensure_defaults_with(&home.env()).expect("a missing base is made, not complained about");

        assert!(
            library_under(&home.config).join("task.md").is_file(),
            "the base directories above the library were never made"
        );
    }

    #[test]
    fn ensure_defaults_refuses_a_prompts_path_occupied_by_a_file() {
        let home = Scratch::new();
        let occupied = library_under(&home.config);
        write_document(&occupied, "not a directory\n");

        let problem = ensure_defaults_with(&home.env())
            .expect_err("a file where the library belongs is not a library");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![occupied.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("prompts"), "{problem}");
    }

    #[test]
    fn ensure_defaults_refuses_a_document_reached_through_a_link() {
        let home = Scratch::new();
        let library = library_under(&home.config);
        let target = home.outside().join("somebody-elses-prompt.md");
        write_document(&target, "not ours\n");
        fs::create_dir_all(&library).expect("a library with a link in it");
        let template = library.join("task.md");
        symlink(&target, &template).expect("a link stands where the template belongs");

        let problem = ensure_defaults_with(&home.env())
            .expect_err("a link is not a document this module may write");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![template.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("link"), "{problem}");
        assert_eq!(
            document(&target),
            "not ours\n",
            "a default was written through a link, wherever it pointed"
        );
    }

    #[test]
    fn ensure_defaults_refuses_something_that_is_neither_file_nor_directory() {
        // A socket answers to no read and no write of text, so it is neither of the
        // two shapes this module can work with, and overwriting the assumption that
        // anything left of those two is a file is how a prompt ends up coming from
        // nowhere.
        let home = Scratch::new();
        let library = library_under(&home.config);
        fs::create_dir_all(&library).expect("the library directory to bind a socket in");
        let template = library.join("task.md");
        let _socket =
            UnixListener::bind(&template).expect("a socket can be bound in the scratch library");

        let problem = ensure_defaults_with(&home.env())
            .expect_err("something that holds no text is not a template somebody wrote");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![template.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("a file"), "{problem}");
    }

    #[test]
    fn ensure_defaults_reports_a_refusal_to_look_inside_a_base_it_cannot_read() {
        let home = Scratch::new();
        fs::create_dir_all(&home.config).expect("a configuration base");
        fs::set_permissions(&home.config, Permissions::from_mode(0o000))
            .expect("a base this process cannot read");

        let problem = ensure_defaults_with(&home.env())
            .expect_err("a base that cannot be examined cannot be written either");
        fs::set_permissions(&home.config, Permissions::from_mode(0o700))
            .expect("the scratch base is readable again for cleanup");

        assert!(matches!(problem, Error::Io(_)), "{problem}");
    }

    #[test]
    fn a_creation_refused_because_another_run_got_there_first_is_not_a_failure() {
        // The race itself needs two processes arriving in the wrong order, which no
        // headless test can schedule. The decision it turns on is not raced against
        // a clock here: which refusal counts as "somebody already wrote my bytes" is
        // answered from an error value alone, and answered both ways.
        assert!(
            created_by_another_run(&io::Error::from(io::ErrorKind::AlreadyExists)),
            "a document another run created first was treated as a failure to write"
        );
        for other in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::IsADirectory,
            io::ErrorKind::StorageFull,
        ] {
            assert!(
                !created_by_another_run(&io::Error::from(other)),
                "a creation refused with {other:?} was forgiven as though another run \
                 had written the document"
            );
        }
    }

    #[test]
    fn a_creation_the_filesystem_refuses_for_a_reason_of_its_own_is_reported() {
        // One refusal is forgiven and this is the other kind: the directory will not
        // accept a new file, so no document exists and nothing was written. Blurring
        // the two is how a library that could not be written at all starts answering
        // with defaults it never wrote. The look that comes first is allowed here —
        // the directory stays readable — which is what leaves the create, and only
        // the create, as the call that fails.
        let home = Scratch::new();
        let library = library_under(&home.config);
        fs::create_dir_all(&library).expect("a library directory to take writing away from");
        fs::set_permissions(&library, Permissions::from_mode(0o500))
            .expect("a library directory that may not gain a file");

        let refused = write_default(&library.join("task.md"), DEFAULT_TEMPLATE);
        fs::set_permissions(&library, Permissions::from_mode(0o700))
            .expect("the library is writable again so the scratch directory can go");

        let problem = refused
            .expect_err("a document that could not be created is not a document that exists");
        assert!(
            matches!(&problem, Error::Io(why) if why.kind() == io::ErrorKind::PermissionDenied),
            "{problem}"
        );
        assert!(
            !library.join("task.md").exists(),
            "a creation the filesystem refused still left a document behind"
        );
    }

    #[test]
    fn a_first_load_creates_the_defaults_rather_than_failing() {
        let home = Scratch::new();

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("a first run makes the library instead of failing");

        let library = library_under(&home.config);
        assert!(library.join("task.md").is_file());
        assert!(
            library.join("context.md").is_file(),
            "a first load wrote a template into a library with no context to send with it"
        );
        assert_eq!(loaded, document(&library.join("task.md")));
        assert!(
            loaded.contains("{{TASK}}"),
            "the template a first run handed back carries no task:\n{loaded}"
        );
    }

    #[test]
    fn a_project_override_is_preferred_over_the_library_default() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );
        write_document(
            &home.override_dir().join("task.md"),
            "this project's own prompt\n",
        );

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("a project that wrote its own template gets it");

        assert_eq!(loaded, "this project's own prompt\n");
    }

    #[test]
    fn the_library_default_is_used_when_the_project_wrote_no_override() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("a project with no override gets the library's copy");

        assert_eq!(loaded, "the global default\n");
    }

    #[test]
    fn nothing_is_read_or_written_inside_the_repository() {
        // VISION.md §11: prompts and templates live in the private prompt library,
        // and a per-project override lives in the state directory — never in the
        // working copy, where an agent's own commit would carry it into public
        // history. The repository holds a `task.md` and a `prompts/` of its own to
        // prove neither is consulted nor disturbed.
        let home = Scratch::new();
        write_document(&home.repository.join("task.md"), "REPOSITORY COPY\n");
        write_document(
            &home.repository.join("prompts").join("task.md"),
            "REPOSITORY COPY\n",
        );
        write_document(
            &home.repository.join("prompts").join("context.md"),
            "REPOSITORY COPY\n",
        );
        let before = names(&home.repository);

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("a project whose repository holds a template still loads one");

        assert_eq!(
            loaded,
            document(&library_under(&home.config).join("task.md"))
        );
        assert!(
            !loaded.contains("REPOSITORY COPY"),
            "a template was read from inside the repository:\n{loaded}"
        );
        assert_eq!(
            names(&home.repository),
            before,
            "loading a prompt left something behind in the working copy"
        );
    }

    #[test]
    fn an_override_wins_over_both_the_library_and_a_copy_inside_the_repository() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );
        write_document(
            &home.repository.join("prompts").join("task.md"),
            "REPOSITORY COPY\n",
        );
        write_document(
            &home.override_dir().join("task.md"),
            "this project's own prompt\n",
        );

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("the override is readable like any other document");

        assert_eq!(loaded, "this project's own prompt\n");
    }

    #[test]
    fn a_project_with_no_state_directory_yet_gets_the_library_default() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );

        let loaded = load_template_with(&home.env(), &home.project())
            .expect("an unregistered project is not a reason to fail");

        assert_eq!(loaded, "the global default\n");
        assert!(
            !home.state_dir().exists(),
            "loading a prompt registered a project by making its state directory"
        );
    }

    #[test]
    fn load_template_refuses_an_override_reached_through_a_link() {
        // The one way an override could point back inside the repository is a
        // symbolic link, and an operator can write one by accident, so the
        // refusal is mechanical rather than a line in a document.
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );
        let inside = home.repository.join("prompts").join("task.md");
        write_document(&inside, "REPOSITORY COPY\n");
        let override_path = home.override_dir().join("task.md");
        fs::create_dir_all(home.override_dir()).expect("an override directory");
        symlink(&inside, &override_path).expect("a link back into the repository");

        let problem = load_template_with(&home.env(), &home.project())
            .expect_err("an override is read as the file it is, not as whatever it points at");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![override_path.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("link"), "{problem}");
    }

    #[test]
    fn load_template_refuses_an_override_path_occupied_by_a_file() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );
        write_document(&home.override_dir(), "not a directory\n");

        let problem = load_template_with(&home.env(), &home.project())
            .expect_err("an override directory that is a file is a mistake worth naming");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![home.override_dir()]),
            "{problem}"
        );
    }

    #[test]
    fn a_socket_where_the_override_belongs_is_refused_rather_than_read() {
        let home = Scratch::new();
        fs::create_dir_all(home.override_dir()).expect("an override directory");
        let template = home.override_dir().join("task.md");
        let _socket =
            UnixListener::bind(&template).expect("a socket can be bound in the override directory");

        let problem = load_template_with(&home.env(), &home.project())
            .expect_err("a prompt is read from a file, and this path holds no text at all");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![template.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("a file"), "{problem}");
    }

    #[test]
    fn load_template_refuses_a_state_directory_it_cannot_look_inside() {
        // Not being allowed to look is not the same answer as there being nothing
        // there. Confusing the two would hand a project that did write an override
        // the library's default, quietly, because a permission stood in the way of
        // checking — the silent fallback this module refuses everywhere else.
        let home = Scratch::new();
        fs::create_dir_all(home.state_dir()).expect("a state directory to close");
        fs::set_permissions(home.state_dir(), Permissions::from_mode(0o000))
            .expect("a state directory this process may not look inside");

        let refused = load_template_with(&home.env(), &home.project());
        fs::set_permissions(home.state_dir(), Permissions::from_mode(0o700))
            .expect("the state directory is open again for cleanup");

        let problem = refused.expect_err(
            "an override that could not be looked for is not an override that is absent",
        );
        assert!(
            matches!(&problem, Error::Io(why) if why.kind() == io::ErrorKind::PermissionDenied),
            "{problem}"
        );
    }

    #[test]
    fn load_template_refuses_a_template_that_is_not_text() {
        let home = Scratch::new();
        write_document(
            &library_under(&home.config).join("task.md"),
            "the global default\n",
        );
        fs::create_dir_all(home.override_dir()).expect("an override directory");
        fs::write(
            home.override_dir().join("task.md"),
            [0xff_u8, 0xfe, 0x00, 0x01],
        )
        .expect("a document that is not text can be written");

        let problem = load_template_with(&home.env(), &home.project())
            .expect_err("a provider is never handed bytes that are not text");

        assert!(matches!(problem, Error::Corrupt { .. }), "{problem}");
        assert!(problem.to_string().contains("task.md"), "{problem}");
    }

    /// The directory a project's recorded decisions live in, spelled out here
    /// rather than reached through the constant under test: an expectation computed
    /// with the code under test would accept that directory quietly moving.
    fn adr_directory(repository: &Path) -> PathBuf {
        repository.join("docs").join("adr")
    }

    /// Record a decision the way `resolve` records one — the next number, a title
    /// slug, the answer as the body — and hand back the path it landed on.
    fn record_adr(repository: &Path, name: &str, text: &str) -> PathBuf {
        let path = adr_directory(repository).join(name);
        write_document(&path, text);
        path
    }

    /// The prompt one task of `home`'s project is handed, built from the documents
    /// where they actually live rather than from arguments.
    fn built_prompt(home: &Scratch, task: &Task, attempt: u32) -> String {
        build_prompt_with(
            &home.env(),
            &home.project(),
            task,
            AttemptId::new(attempt),
            TOTAL,
        )
        .expect("a prompt is buildable from the documents the library and the project hold")
    }

    #[test]
    fn decisions_are_collected_in_filename_order_not_the_order_they_were_written() {
        let repository = tempdir().expect("a scratch repository to hold decisions");
        // Written newest-number-first, and with the tenth below a gap in the
        // numbering, so a collection that trusted the order the filesystem handed
        // its entries back in, or one that sorted by the text inside, comes back in
        // an order other than the one the numbers say.
        record_adr(repository.path(), "0010-tenth.md", "tenth decision\n");
        record_adr(repository.path(), "0002-second.md", "second decision\n");
        record_adr(repository.path(), "0001-first.md", "first decision\n");

        let collected = collect_adrs(repository.path()).expect("a repository of decisions reads");

        assert_eq!(
            collected,
            ["first decision\n", "second decision\n", "tenth decision\n"],
            "a decision that supersedes an earlier one has to arrive after it, or a \
             session reads its project's history upside down"
        );
    }

    #[test]
    fn the_template_every_repository_copies_is_never_handed_over_as_a_decision() {
        let repository = tempdir().expect("a scratch repository to hold decisions");
        record_adr(
            repository.path(),
            "0000-template.md",
            "# NNNN. Short title\n\n- **Status:** proposed\n",
        );
        record_adr(repository.path(), "0001-real.md", "a real decision\n");

        let collected = collect_adrs(repository.path())
            .expect("a repository whose only record is the template reads");

        assert_eq!(
            collected,
            ["a real decision\n"],
            "the blank shape every repository starts with is not a decision anybody made, \
             and handing it over teaches a session a shape rather than a choice"
        );
    }

    #[test]
    fn a_repository_with_no_decision_directory_has_no_decisions_and_no_error() {
        let repository = tempdir().expect("a scratch repository with nothing in it");

        let collected = collect_adrs(repository.path())
            .expect("a project that has decided nothing yet is not a broken project");

        assert!(collected.is_empty(), "{collected:?}");

        // A `docs` directory that holds no ADR directory is the same answer: the
        // question is about `docs/adr`, and a project on its first task has written
        // no decisions there yet.
        write_document(
            &repository.path().join("docs").join("README.md"),
            "# Docs\n",
        );
        let again = collect_adrs(repository.path())
            .expect("a docs directory that holds no decisions is not an error either");
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn only_the_markdown_documents_directly_in_the_decision_directory_are_collected() {
        let repository = tempdir().expect("a scratch repository to hold decisions");
        let root = repository.path();
        record_adr(root, "0001-real.md", "a real decision\n");
        record_adr(root, "0002-notes.txt", "not markdown\n");
        record_adr(root, "0003-real.md.bak", "markdown that was renamed away\n");
        record_adr(root, "README", "a document with no extension\n");
        // A directory whose name ends in `.md` is not a document, and a decision
        // filed below a directory is not one this function was asked for.
        write_document(
            &adr_directory(root).join("drafts.md").join("0009-inside.md"),
            "a directory is not a decision\n",
        );
        write_document(
            &adr_directory(root).join("drafts").join("0008-nested.md"),
            "a nested decision is not a decision yet\n",
        );

        let collected =
            collect_adrs(root).expect("the shapes around the records do not break the read");

        assert_eq!(collected, ["a real decision\n"], "{collected:?}");
    }

    #[test]
    fn a_decision_that_is_not_text_is_refused_by_name() {
        let repository = tempdir().expect("a scratch repository to hold decisions");
        record_adr(repository.path(), "0001-real.md", "a real decision\n");
        let broken = record_adr(repository.path(), "0004-broken.md", "");
        fs::write(&broken, [0xff_u8, 0xfe, 0x00, 0x01]).expect("bytes that are not text");

        let problem = collect_adrs(repository.path())
            .expect_err("a prompt is never assembled out of bytes that are not text");

        assert!(matches!(problem, Error::Corrupt { .. }), "{problem}");
        assert!(problem.to_string().contains("0004-broken.md"), "{problem}");
    }

    #[test]
    fn a_link_among_the_decisions_is_refused_rather_than_followed() {
        // Where a link points is exactly what cannot be checked from here: targets
        // move and chain, so a decision read through one is a decision whose author
        // and number the filename does not describe.
        let repository = tempdir().expect("a scratch repository to hold decisions");
        let elsewhere = tempdir().expect("a directory outside the repository");
        let target = elsewhere.path().join("somebody-elses-notes.md");
        write_document(&target, "not a decision of this project\n");
        let directory = adr_directory(repository.path());
        fs::create_dir_all(&directory).expect("a decision directory to hold a link");
        let link = directory.join("0005-linked.md");
        symlink(&target, &link).expect("a link stands among the decisions");

        let problem = collect_adrs(repository.path())
            .expect_err("a link is not a decision record this module may read");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![link.clone()]),
            "{problem}"
        );
        assert!(problem.to_string().contains("link"), "{problem}");
        assert_eq!(document(&target), "not a decision of this project\n");
    }

    #[test]
    fn a_file_where_the_decision_directory_belongs_is_refused_by_name() {
        let repository = tempdir().expect("a scratch repository to hold decisions");
        let occupied = adr_directory(repository.path());
        write_document(&occupied, "not a directory\n");

        let problem = collect_adrs(repository.path())
            .expect_err("a file where the decisions belong is not a decision archive");

        assert!(
            matches!(&problem, Error::Policy { paths, .. } if paths == &vec![occupied.clone()]),
            "{problem}"
        );
    }

    #[test]
    fn a_decision_recorded_by_resolve_reaches_the_next_tasks_prompt() {
        let home = Scratch::new();
        let resolved = Task {
            id: TaskId::new(7),
            ..task()
        };
        let successor = Task {
            id: TaskId::new(8),
            ..task()
        };

        // The task that paused for a decision is handed a prompt that says nothing
        // is on record; the answer then arrives as an ADR written into the
        // repository, which is the only operational document VISION.md §3 lets live
        // there.
        let before = built_prompt(&home, &resolved, 1);
        assert!(
            before.contains("# Decisions on record (0)\n\nNone recorded yet."),
            "a first prompt does not say that nothing is on record yet:\n{before}"
        );
        record_adr(
            &home.repository,
            "0078-redaction-runs-inside-the-write-path.md",
            "# 0078. Redaction runs inside the write path\n\nEvery line is redacted by the \
             table at the point it is written, and nothing else formats a line on its way to \
             disk.\n",
        );

        // A different task, a different prompt, assembled after the decision: the
        // decision is what has moved.
        let after = built_prompt(&home, &successor, 1);
        assert!(
            after.contains("# 0078. Redaction runs inside the write path"),
            "the decision a human recorded did not reach the task that came after it:\n{after}"
        );
        assert!(
            after.contains("# Decisions on record (1)\n\n## 1 of 1"),
            "the recorded decision is not counted and numbered where it stands:\n{after}"
        );
    }

    #[test]
    fn the_prompt_is_the_library_document_the_decisions_then_the_template() {
        let home = Scratch::new();
        write_document(&library_under(&home.config).join("context.md"), CONTEXT);
        write_document(&home.override_dir().join("task.md"), TEMPLATE);
        record_adr(&home.repository, "0001-first.md", "first decision\n");
        record_adr(&home.repository, "0002-second.md", "second decision\n");
        // A template inside the working copy is not an override, whatever it says:
        // it is the supervisor's own file leaking into the repository an agent can
        // edit, which VISION.md §3's invariant 6 forbids and this path does not
        // negotiate with.
        write_document(
            &home.repository.join("prompts").join("task.md"),
            "the repository's own prompt\n",
        );

        let prompt = built_prompt(&home, &task(), 1);

        let context = prompt
            .find("# Project context")
            .expect("the library's context document is missing");
        let decisions = prompt
            .find("# Decisions on record (2)")
            .expect("the repository's decisions are missing");
        let template = prompt
            .find("# Prompt template")
            .expect("the project's own template is missing");
        assert!(
            context < decisions && decisions < template,
            "the parts of the prompt arrived out of order:\n{prompt}"
        );
        assert!(
            prompt.contains("first decision") && prompt.contains("second decision"),
            "a recorded decision went missing:\n{prompt}"
        );
        assert!(
            prompt.contains(task().body.trim_end()),
            "the task went missing:\n{prompt}"
        );
        assert!(
            !prompt.contains("the repository's own prompt"),
            "a prompt document inside the working copy won over the one outside it:\n{prompt}"
        );
    }

    #[test]
    fn a_first_prompt_is_built_from_the_defaults_a_machine_started_with() {
        let home = Scratch::new();
        assert!(
            !home.config.exists(),
            "the fixture must start with no configuration base"
        );

        let prompt = built_prompt(&home, &task(), 1);

        let library = library_under(&home.config);
        assert_eq!(
            names(&library),
            ["context.md".to_owned(), "task.md".to_owned()],
            "building the first prompt of a machine left this behind instead of the two \
             default documents: {}",
            library.display()
        );
        assert!(
            prompt.contains("# Project context"),
            "the standing half of the prompt is not the library's context document:\n{prompt}"
        );
        assert!(
            prompt.contains(task().body.trim_end()) && !prompt.contains(TASK_PLACEHOLDER),
            "the default template did not carry the task it was built for:\n{prompt}"
        );
    }

    #[test]
    fn no_prompt_can_be_built_when_nothing_names_a_home() {
        let home = Scratch::new();
        let nothing = environment(Vec::new());

        for problem in [
            ensure_defaults_with(&nothing).expect_err("nothing says where prompts go"),
            load_template_with(&nothing, &home.project())
                .expect_err("nothing says where prompts go"),
            build_prompt_with(&nothing, &home.project(), &task(), AttemptId::new(1), TOTAL)
                .expect_err("nothing says where the standing half of a prompt comes from"),
        ] {
            assert!(
                matches!(&problem, Error::Config { key, .. } if key == "HOME"),
                "{problem}"
            );
        }
    }

    /// The two public entry points read the environment the process actually has.
    ///
    /// Everything else in this module is tested through an injected accessor,
    /// which is what `docs/DESIGN.md` Conventions requires of a test — and that
    /// leaves the two wrappers handing the process environment to those tested
    /// bodies untested by construction. They cannot be reached from inside the
    /// suite: a test may not change the environment of the process it shares with
    /// every other test on the machine running it. So this test starts a second
    /// copy of itself with the variables set on its way in, which is exactly how a
    /// runner gets them, and asserts on what that copy left in a scratch home.
    ///
    /// In the child the same body takes the other branch and becomes the thing
    /// under test, so there is no orphan test that only the parent ever runs and
    /// that asserts nothing when the suite runs it.
    #[test]
    fn the_public_entry_points_read_the_environment_the_process_actually_has() {
        if let Some(home) = var_os(CHILD_SCRATCH) {
            act_as_child(Path::new(&home));
            return;
        }

        let scratch = Scratch::new();
        let outcome = run_child(&scratch);
        assert!(
            outcome.status.success(),
            "a copy of this binary told that XDG_CONFIG_HOME is {} could not reach a \
             prompt (exit {}):\n{}",
            scratch.config.display(),
            outcome.status,
            String::from_utf8_lossy(&outcome.stderr)
        );

        // The child asked for a prompt and got one; what it left behind is what
        // the process-environment half of this module promises.
        let library = library_under(&scratch.config);
        assert_eq!(
            names(&library),
            ["context.md".to_owned(), "task.md".to_owned()],
            "the public `ensure_defaults` left this in {} rather than the two default \
             documents",
            library.display()
        );
        assert_eq!(
            mode(&library),
            LIBRARY_DIR_MODE,
            "the library the public entry points made is readable by somebody other than \
             its owner"
        );
        for document in ["context.md", "task.md"] {
            assert_eq!(
                mode(&library.join(document)),
                DOCUMENT_MODE,
                "`{document}` a first run wrote is not private to its owner"
            );
        }
        assert!(
            !scratch.repository.exists(),
            "reaching a prompt through the public entry points created {} inside the \
             working copy",
            scratch.repository.display()
        );
    }

    /// Start a second copy of this binary whose environment names `scratch`, and
    /// hand back what it said and how it ended.
    fn run_child(scratch: &Scratch) -> Output {
        let executable = std::env::current_exe()
            .expect("the child is another copy of this binary, which can name itself");
        Command::new(executable)
            .env(CHILD_SCRATCH, scratch.root.path())
            .env("XDG_CONFIG_HOME", &scratch.config)
            // Not read by anything here today, and set so that a later resolver
            // of a state directory aims at the scratch home rather than at the
            // operator's real one.
            .env("XDG_STATE_HOME", &scratch.state)
            // No fallback may answer for the variable under test.
            .env_remove("HOME")
            // `cargo nextest` names a protocol descriptor on the environment; a
            // grandchild answering on its parent's protocol stream would corrupt
            // the report of the very test that spawned it.
            .env_remove("NEXTEST_TEST_BUFFER_ID")
            // Coverage runs say where the counts go: not on top of the parent's file.
            .env_remove("LLVM_PROFILE_FILE")
            // The scratch home, so that anything this child writes by accident —
            // including a coverage profile it was told not to name — lands there
            // rather than in the crate whose tests it is running.
            .current_dir(&scratch.root)
            // Exactly one test — this one — and no capture standing between a
            // failure inside the child and the pipe the parent reads.
            .args(["--exact", CHILD_TEST, "--nocapture"])
            .stdin(Stdio::null())
            .output()
            .expect("another copy of this binary could not be started")
    }

    /// Reach the prompt library the way the runner does: only through the public
    /// entry points, which read this process's own environment.
    ///
    /// A failed assertion here is a panic, which the harness ends the child with —
    /// the parent reads it as the non-zero exit it asserts against, and this
    /// message comes back inside the parent's failure.
    fn act_as_child(scratch_home: &Path) {
        let (config, state, repository) = layout(scratch_home);
        ensure_defaults().expect("a first run creates the library instead of failing");
        let library = config.join("ktask-rs").join("prompts");
        assert_eq!(
            prompt_library().expect("the child's own environment names a base"),
            library,
            "the public resolver does not point below the XDG_CONFIG_HOME the child was \
             started with"
        );
        let default = fs::read_to_string(library.join("task.md"))
            .expect("a first run leaves a template behind, not a missing file");
        assert!(
            default.contains(TASK_PLACEHOLDER),
            "the template a first run wrote names no task:\n{default}"
        );

        let project = Project {
            root: repository,
            id: PROJECT_ID.to_owned(),
            state_dir: state.join(PROJECT_ID),
        };
        assert_eq!(
            load_template(&project).expect("a project that wrote no override still gets a prompt"),
            default,
            "a project with no override of its own did not get the library's default"
        );

        write_document(
            &state.join(PROJECT_ID).join("prompts").join("task.md"),
            "the project's own words\n",
        );
        assert_eq!(
            load_template(&project).expect("a project's own template can be read"),
            "the project's own words\n",
            "the library default won over the override this project wrote"
        );

        // And the whole prompt, reached through the entry point a runner calls. The
        // project's working copy holds no decisions and is never created, which the
        // parent asserts on after this child has finished.
        let prompt = build_prompt(&project, &task(), AttemptId::new(3), TOTAL)
            .expect("the child's own environment is enough to build a prompt");
        assert!(
            prompt.contains(document(&library.join("context.md")).trim_end()),
            "the prompt built by the public entry point is not built from the library's \
             context document:\n{prompt}"
        );
        assert!(
            prompt.contains("the project's own words"),
            "the prompt built by the public entry point ignored this project's override:\n{prompt}"
        );
        assert!(
            prompt.contains("# Decisions on record (0)"),
            "a repository with no decisions of its own is not a repository with a broken \
             decision archive:\n{prompt}"
        );
        assert!(
            prompt.contains("attempt 3"),
            "the header does not name the attempt the prompt was built for:\n{prompt}"
        );
    }
}
