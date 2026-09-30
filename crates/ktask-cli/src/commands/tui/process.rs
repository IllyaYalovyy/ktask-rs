//! Starting `ktask-rs run --json` detached and reading back its own typed report.

use std::fmt;
use std::io::Read;
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, Stdio};

use ktask_core::{Project, RunReport};

use crate::run_report_json::RunReportJson;

/// Why a run this screen started could not even be begun — the same kind every other refusal
/// with no further structure worth decomposing is given as: shown verbatim, since there is
/// nothing more to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RunRefusal(String);

impl RunRefusal {
    fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for RunRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Starts `binary_path run --project <project.name> --json`, detached from this process — its
/// own process group, so neither this process quitting nor its terminal going away stops it —
/// and waits for it to end, which, when it refuses to start at all, is at once. Reads its own
/// typed report back from what it printed as JSON; a run that could not even be started prints
/// none, so what came through instead — the same words `ktask-rs run` itself would show for
/// the same refusal — is kept, verbatim, as why.
pub(super) fn start_run(binary_path: &Path, project: &Project) -> Result<RunReport, RunRefusal> {
    let mut child = Command::new(binary_path)
        .arg("run")
        .arg("--project")
        .arg(&project.name)
        .arg("--json")
        .current_dir(&project.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| RunRefusal::new(format!("cannot start ktask-rs run: {e}")))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| RunRefusal::new("ktask-rs run has no standard output"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| RunRefusal::new("ktask-rs run has no standard error"))?;
    let out = std::thread::spawn(move || read_all(&mut stdout));
    let err = std::thread::spawn(move || read_all(&mut stderr));
    child
        .wait()
        .map_err(|e| RunRefusal::new(format!("cannot wait for ktask-rs run: {e}")))?;
    let stdout = text(&out.join().unwrap_or_default());
    let stderr = text(&err.join().unwrap_or_default());
    RunReportJson::parse(&stdout).map_err(|_| RunRefusal::new(merged(stdout, stderr)))
}

/// Reads `reader` to its end, giving up whatever came through even when it failed partway.
fn read_all(reader: &mut impl Read) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = reader.read_to_end(&mut buf);
    buf
}

/// `bytes`, decoded and with its trailing newline dropped.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim_end().to_owned()
}

/// What `stdout` and `stderr` — a spawned command's captured standard output and standard
/// error, once a run's own report could not be read back from `stdout` as JSON — printed,
/// kept exactly as the lines they were written on, `stdout`'s lines first, then `stderr`'s
/// when both said something.
fn merged(stdout: String, stderr: String) -> String {
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
        (true, true) => String::new(),
    }
}
