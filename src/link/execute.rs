//! The child's side of a forwarded command. This same ktask runs it as a process of its own,
//! so parsing, validation, version guards, the state machine, output and exit status are the
//! local ones. What it accepts from a parent is decided there, in `cli::remote`.
use super::protocol::{CHUNK, MAX_COMMANDS, MAX_OUTPUT, Part};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::process::CommandExt,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::Sender,
    },
    thread,
};

/// The most of each stream that is returned.
const MAX_STREAM: u64 = (MAX_OUTPUT / 2) as u64;

pub(super) struct Executor {
    /// This ktask.
    program: PathBuf,
    /// Where the unnamed files holding a command's output live.
    scratch: PathBuf,
    running: Arc<AtomicUsize>,
}

impl Executor {
    pub fn new(program: PathBuf, scratch: PathBuf) -> Self {
        Self {
            program,
            scratch,
            running: Arc::default(),
        }
    }

    /// Runs one command line from `parent` once and sends the parts of its answer. It runs
    /// to its end even when the connection does not last that long; nothing repeats it.
    pub fn start(&self, parent: &str, id: u64, args: Vec<String>, answers: &Sender<(u64, Part)>) {
        let answers = answers.clone();
        if self.running.load(Ordering::Relaxed) >= MAX_COMMANDS {
            let reason = format!("it is already running {MAX_COMMANDS} commands for its parent");
            let _ = answers.send((id, Part::Refused(reason)));
            return;
        }
        self.running.fetch_add(1, Ordering::Relaxed);
        let running = Arc::clone(&self.running);
        let (program, scratch) = (self.program.clone(), self.scratch.clone());
        let parent = format!("--from-parent={parent}");
        thread::spawn(move || {
            let parts = run(&program, &scratch, &parent, &args);
            running.fetch_sub(1, Ordering::Relaxed);
            for part in parts {
                if answers.send((id, part)).is_err() {
                    break;
                }
            }
        });
    }
}

fn run(program: &PathBuf, scratch: &PathBuf, parent: &str, args: &[String]) -> Vec<Part> {
    let started = || -> io::Result<_> {
        fs::create_dir_all(scratch)?;
        let stdout = tempfile::tempfile_in(scratch)?;
        let stderr = tempfile::tempfile_in(scratch)?;
        let child = Command::new(program)
            .arg(parent)
            .args(args)
            .stdin(Stdio::null())
            .stdout(stdout.try_clone()?)
            .stderr(stderr.try_clone()?)
            // Stopping the link must not signal a command, such as a run, that it started.
            .process_group(0)
            .spawn()?;
        Ok((child, stdout, stderr))
    };
    let (mut child, stdout, stderr) = match started() {
        Ok(started) => started,
        Err(error) => return vec![Part::Refused(format!("it could not start ktask: {error}"))],
    };
    let Some(status) = child.wait().ok().and_then(|status| status.code()) else {
        return vec![Part::Interrupted];
    };
    answer(status as u8, stdout, stderr).unwrap_or_else(|_| vec![Part::Interrupted])
}

/// The end of a stream, and whether that is all of it.
fn read(mut file: File) -> io::Result<(String, bool)> {
    let length = file.seek(SeekFrom::End(0))?;
    file.seek(SeekFrom::Start(length.saturating_sub(MAX_STREAM)))?;
    let mut data = Vec::new();
    file.take(MAX_STREAM).read_to_end(&mut data)?;
    Ok((
        String::from_utf8_lossy(&data).into_owned(),
        length <= MAX_STREAM,
    ))
}

fn answer(status: u8, stdout: File, stderr: File) -> io::Result<Vec<Part>> {
    let (stdout, whole) = read(stdout)?;
    // Of a long run's diagnostics the end matters; a cut result would not be a result.
    let (stderr, _) = read(stderr)?;
    if !whole {
        let notice = format!(
            "io: the command ended with status {status}, but its output is larger than the link carries; run it on that host\n"
        );
        return Ok(vec![Part::Stderr(notice), Part::Exit(status.max(1))]);
    }
    Ok(chunks(&stderr)
        .map(|text| Part::Stderr(text.into()))
        .chain(chunks(&stdout).map(|text| Part::Stdout(text.into())))
        .chain([Part::Exit(status)])
        .collect())
}

/// `text` in pieces a message can carry, split between characters.
fn chunks(mut text: &str) -> impl Iterator<Item = &str> {
    std::iter::from_fn(move || {
        if text.is_empty() {
            return None;
        }
        let (piece, rest) = text.split_at(text.floor_char_boundary(CHUNK));
        text = rest;
        Some(piece)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_output_is_split_between_characters_and_rejoined() {
        let text = "é€x".repeat(CHUNK / 2);
        let pieces: Vec<_> = chunks(&text).collect();
        assert!(pieces.len() > 2);
        assert!(pieces.iter().all(|piece| piece.len() <= CHUNK));
        assert_eq!(pieces.concat(), text);
        assert_eq!(chunks("").count(), 0);
    }
}
