//! The independent stdin/stdout/stderr threads for one process command.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;

use ktask_core::CommandsError;

/// The threads which feed a command and retain its two output streams.
pub(super) type IoThreads = (
    thread::JoinHandle<()>,
    thread::JoinHandle<Vec<u8>>,
    thread::JoinHandle<Vec<u8>>,
);

/// Starts feeding `input` to stdin and draining stdout/stderr on their own threads, so none of
/// the three can block the other two or the wait for the child to end.
pub(super) fn spawn_io_threads(
    mut stdin: std::process::ChildStdin,
    mut stdout: std::process::ChildStdout,
    mut stderr: std::process::ChildStderr,
    input: Vec<u8>,
    output_path: Option<&Path>,
) -> Result<IoThreads, CommandsError> {
    let writer = thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let output = open_output(output_path)?;
    let error_output = output
        .as_ref()
        .map(File::try_clone)
        .transpose()
        .map_err(|e| {
            CommandsError::new(format!("cannot open the attempt output for streaming: {e}"))
        })?;
    let stdout_reader = thread::spawn(move || read_stream(&mut stdout, output));
    let stderr_reader = thread::spawn(move || read_stream(&mut stderr, error_output));
    Ok((writer, stdout_reader, stderr_reader))
}

/// Opens an append-only attempt log before its provider starts writing.
fn open_output(path: Option<&Path>) -> Result<Option<File>, CommandsError> {
    let Some(path) = path else {
        return Ok(None);
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CommandsError::new(format!(
                "cannot create attempt output directory {}: {e}",
                parent.display()
            ))
        })?;
    }
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(Some)
        .map_err(|e| {
            CommandsError::new(format!(
                "cannot open attempt output {}: {e}",
                path.display()
            ))
        })
}

/// Retains a stream's exact bytes while appending each received chunk to the live attempt log.
fn read_stream(reader: &mut impl Read, output: Option<File>) -> Vec<u8> {
    let mut captured = Vec::new();
    let _ = read_and_copy(reader, &mut captured, output);
    captured
}

/// Copies one stream in chunks, recording each chunk before asking the process for more.
fn read_and_copy(
    reader: &mut impl Read,
    captured: &mut Vec<u8>,
    mut output: Option<File>,
) -> io::Result<()> {
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            return Ok(());
        }
        let Some(bytes) = chunk.get(..count) else {
            return Err(io::Error::other(
                "reader returned a count beyond its buffer",
            ));
        };
        captured.extend_from_slice(bytes);
        if let Some(file) = &mut output {
            file.write_all(bytes)?;
            file.flush()?;
        }
    }
}
