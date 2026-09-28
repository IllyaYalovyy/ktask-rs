//! Runs the real `ktask-rs` binary in a pseudo-terminal and reads its screen.
//!
//! The child gets the same sandbox as in the CLI harness. Its output is parsed by a terminal
//! emulator, so what a test sees is what an operator would see. Every frame the binary draws
//! is bracketed by the terminal's synchronized-update markers (`ESC[?2026h` … `ESC[?2026l`);
//! this harness watches for the end marker and only ever hands a test the screen as it stood
//! at the end of a complete frame, never one caught mid-draw. Waiting is always for a
//! condition, with a timeout that guards against a hung child, never a fixed sleep. Everything
//! that is specific to the platform's terminals stays in this file.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use nix::sys::termios::LocalFlags;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::support::{Result, Sandbox};

/// How long a wait for the screen or for an exit lasts before it fails.
const TIMEOUT: Duration = Duration::from_secs(20);

/// What the reader and waiter threads learn about the child.
struct State {
    /// The live parser: it may hold a frame only partly drawn, so nothing outside this file
    /// reads it directly.
    parser: vt100::Parser,
    /// The screen as it stood at the end of the last complete frame. This, not the live
    /// parser, is what a test ever sees.
    frame: vt100::Screen,
    /// How many complete frames have been drawn so far, counting from the first. A test that
    /// wants to know whether the child is still drawing without being told to compares this
    /// before and after waiting, rather than trying to catch a frame in the act.
    frames: u64,
    /// The child's exit code; `None` while it runs. A signal that ended it reads as a failure.
    exit: Option<u32>,
    /// Whether the reader has drained the terminal to its end. The child's exit and the last
    /// of its output arrive on separate threads; a screen read straight after the exit is seen
    /// can otherwise miss output that was still on its way.
    eof: bool,
}

/// Recognises the terminal's synchronized-update end marker (`ESC[?2026l`) in a byte stream,
/// even when a read splits it across two chunks.
#[derive(Default)]
struct FrameScanner {
    matched: usize,
}

/// The synchronized-update end marker: everything up to and including it belongs to a frame
/// that is now whole.
const FRAME_END: &[u8] = b"\x1b[?2026l";

impl FrameScanner {
    /// Feeds one more byte in. Returns whether it is the last byte of the end marker.
    fn step(&mut self, byte: u8) -> bool {
        if FRAME_END.get(self.matched) == Some(&byte) {
            self.matched += 1;
            if self.matched == FRAME_END.len() {
                self.matched = 0;
                return true;
            }
        } else {
            self.matched = usize::from(FRAME_END.first() == Some(&byte));
        }
        false
    }
}

/// The state and the signal that it changed.
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A running `ktask-rs` and the terminal it is attached to.
pub(crate) struct Terminal {
    shared: Arc<Shared>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    /// The child's process id, for reading its CPU time from `/proc`; `None` when the
    /// platform did not hand one out.
    pid: Option<u32>,
}

impl Terminal {
    /// Starts `ktask-rs` with `args` in `cwd`, on a terminal of `rows` × `cols`.
    pub(crate) fn launch(
        sandbox: &Sandbox,
        cwd: &Path,
        args: &[&str],
        rows: u16,
        cols: u16,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(size(rows, cols))?;
        let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        command.env_clear();
        for (name, value) in sandbox.environment() {
            command.env(name, value);
        }
        command.env("TERM", "xterm-256color");
        command.cwd(cwd);
        let mut child = pair.slave.spawn_command(command)?;
        // With our copy of the slave closed, the terminal reads end when the child is gone.
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let parser = vt100::Parser::new(rows, cols, 0);
        let frame = parser.screen().clone();
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                parser,
                frame,
                frames: 0,
                exit: None,
                eof: false,
            }),
            changed: Condvar::new(),
        });
        let output = Arc::clone(&shared);
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            let mut scanner = FrameScanner::default();
            // A read error is how a closed terminal ends on Linux, like end of file.
            while let Ok(count @ 1..) = reader.read(&mut chunk) {
                let bytes = chunk.get(..count).unwrap_or_default();
                let mut state = output.lock();
                let mut start = 0;
                for (index, &byte) in bytes.iter().enumerate() {
                    if scanner.step(byte) {
                        state
                            .parser
                            .process(bytes.get(start..=index).unwrap_or_default());
                        start = index + 1;
                        state.frame = state.parser.screen().clone();
                        state.frames += 1;
                        output.changed.notify_all();
                    }
                }
                state.parser.process(bytes.get(start..).unwrap_or_default());
            }
            // What is left once the child is gone was never closed by a frame's end marker —
            // most of it is the terminal being restored on the way out — but nothing further
            // will ever draw over it, so it is as final and whole as a screen gets.
            let mut state = output.lock();
            state.frame = state.parser.screen().clone();
            state.eof = true;
            drop(state);
            output.changed.notify_all();
        });
        let exits = Arc::clone(&shared);
        thread::spawn(move || {
            let code = child.wait().map_or(1, |status| status.exit_code());
            exits.lock().exit = Some(code);
            exits.changed.notify_all();
        });
        Ok(Self {
            shared,
            master: pair.master,
            writer,
            killer,
            pid,
        })
    }

    /// Types `keys` into the terminal.
    pub(crate) fn send(&mut self, keys: &str) -> Result<()> {
        self.writer.write_all(keys.as_bytes())?;
        Ok(self.writer.flush()?)
    }

    /// Changes the size of the terminal, as a user resizing its window would.
    pub(crate) fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        self.shared.lock().parser.screen_mut().set_size(rows, cols);
        Ok(self.master.resize(size(rows, cols))?)
    }

    /// The screen as it stood at the end of the last complete frame, one line per row, trailing
    /// blanks trimmed. Never a frame caught half drawn.
    pub(crate) fn screen(&self) -> String {
        self.shared.lock().frame.contents()
    }

    /// How many complete frames have been drawn so far. A caller checks that this is unchanged
    /// after a wait to show that nothing was drawn while it waited.
    pub(crate) fn frame_count(&self) -> u64 {
        self.shared.lock().frames
    }

    /// The user and system CPU time the child has used so far, in clock ticks, read from
    /// `/proc`. A caller compares this before and after a wait to show that the child did
    /// no work while it waited, rather than merely that it drew no frames.
    ///
    /// # Errors
    ///
    /// Fails when the platform gave out no process id, or `/proc/<pid>/stat` cannot be read
    /// or does not have the shape it always has on Linux.
    pub(crate) fn cpu_ticks(&self) -> Result<u64> {
        let pid = self.pid.ok_or("the child's process id is not known")?;
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
        // `comm`, the second field, is written in parentheses and may itself contain spaces
        // or parentheses, so the fields before and including it are skipped by looking for
        // the last `)` rather than splitting on whitespace from the start.
        let after_comm = stat
            .rsplit_once(')')
            .map(|(_, rest)| rest)
            .ok_or("/proc/<pid>/stat has no comm field")?;
        let fields: Vec<&str> = after_comm.split_whitespace().collect();
        // Field 3 (state) is fields[0] here; utime is field 14 and stime is field 15.
        let utime: u64 = fields
            .get(11)
            .ok_or("/proc/<pid>/stat has no utime field")?
            .parse()?;
        let stime: u64 = fields
            .get(12)
            .ok_or("/proc/<pid>/stat has no stime field")?
            .parse()?;
        Ok(utime + stime)
    }

    /// Waits until `condition` holds of a complete frame, and returns that frame's text.
    /// `condition` never sees a frame that is only partly drawn.
    ///
    /// # Errors
    ///
    /// Fails, showing the last complete frame, when the condition still does not hold after
    /// the timeout; the timeout is a guard against a hung child, not a source of pass or fail.
    pub(crate) fn wait_for(
        &self,
        what: &str,
        condition: impl Fn(&vt100::Screen) -> bool,
    ) -> Result<String> {
        let (state, timeout) = self
            .shared
            .changed
            .wait_timeout_while(self.shared.lock(), TIMEOUT, |state| {
                !condition(&state.frame)
            })
            .unwrap_or_else(PoisonError::into_inner);
        let screen = state.frame.contents();
        if timeout.timed_out() {
            return Err(
                format!("timed out waiting for {what}; the screen shows:\n{screen}").into(),
            );
        }
        Ok(screen)
    }

    /// Waits until `text` is on the screen, and returns the screen text then.
    pub(crate) fn wait_for_text(&self, text: &str) -> Result<String> {
        self.wait_for(&format!("{text:?}"), |screen| {
            screen.contents().contains(text)
        })
    }

    /// Waits until the child has exited and its last output has been read, and returns its
    /// exit code.
    ///
    /// # Errors
    ///
    /// Fails, showing the screen, when the child is still running after the timeout.
    pub(crate) fn wait_for_exit(&self) -> Result<u32> {
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(self.shared.lock(), TIMEOUT, |state| {
                state.exit.is_none() || !state.eof
            })
            .unwrap_or_else(PoisonError::into_inner);
        state.exit.ok_or_else(|| {
            let screen = state.frame.contents();
            format!("timed out waiting for the exit; the screen shows:\n{screen}").into()
        })
    }

    /// The exit code of the child if it has exited, without waiting for it.
    pub(crate) fn exit(&self) -> Option<u32> {
        self.shared.lock().exit
    }

    /// Whether the terminal is in the mode a shell expects — line editing and echo on —
    /// and not the raw mode a full-screen program puts it in.
    pub(crate) fn is_cooked(&self) -> Result<bool> {
        let termios = self
            .master
            .get_termios()
            .ok_or("cannot read the terminal modes")?;
        Ok(termios
            .local_flags
            .contains(LocalFlags::ICANON | LocalFlags::ECHO))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // A child that already exited cannot be killed; there is nothing left to do then.
        let _ = self.killer.kill();
    }
}

fn size(rows: u16, cols: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// The lines of `screen` with whatever the frame draws on either side stripped.
pub(crate) fn lines_inside_frame(screen: &str) -> Vec<String> {
    screen
        .lines()
        .map(|line| {
            line.trim_start_matches('│')
                .trim_end_matches('│')
                .trim_end()
                .to_owned()
        })
        .collect()
}
