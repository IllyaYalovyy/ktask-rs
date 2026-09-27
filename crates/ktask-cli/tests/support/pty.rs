//! Runs the real `ktask-rs` binary in a pseudo-terminal and reads its screen.
//!
//! The child gets the same sandbox as in the CLI harness. Its output is parsed by a terminal
//! emulator, so what a test sees is what an operator would see. Waiting is always for a
//! condition, with a timeout; nothing here sleeps for a fixed time. Everything that is
//! specific to the platform's terminals stays in this file.

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
    parser: vt100::Parser,
    /// The child's exit code; `None` while it runs. A signal that ended it reads as a failure.
    exit: Option<u32>,
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
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;

        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                parser: vt100::Parser::new(rows, cols, 0),
                exit: None,
            }),
            changed: Condvar::new(),
        });
        let output = Arc::clone(&shared);
        thread::spawn(move || {
            let mut chunk = [0_u8; 4096];
            // A read error is how a closed terminal ends on Linux, like end of file.
            while let Ok(count @ 1..) = reader.read(&mut chunk) {
                output
                    .lock()
                    .parser
                    .process(chunk.get(..count).unwrap_or_default());
                output.changed.notify_all();
            }
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

    /// The text on the screen now, one line per row, trailing blanks trimmed.
    pub(crate) fn screen(&self) -> String {
        self.shared.lock().parser.screen().contents()
    }

    /// Waits until `condition` holds of the screen, and returns the screen text then.
    ///
    /// # Errors
    ///
    /// Fails, showing the screen, when the condition still does not hold after the timeout.
    pub(crate) fn wait_for(
        &self,
        what: &str,
        condition: impl Fn(&vt100::Screen) -> bool,
    ) -> Result<String> {
        let (state, timeout) = self
            .shared
            .changed
            .wait_timeout_while(self.shared.lock(), TIMEOUT, |state| {
                !condition(state.parser.screen())
            })
            .unwrap_or_else(PoisonError::into_inner);
        let screen = state.parser.screen().contents();
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

    /// Waits until the child has exited, and returns its exit code.
    ///
    /// # Errors
    ///
    /// Fails, showing the screen, when the child is still running after the timeout.
    pub(crate) fn wait_for_exit(&self) -> Result<u32> {
        let (state, _) = self
            .shared
            .changed
            .wait_timeout_while(self.shared.lock(), TIMEOUT, |state| state.exit.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.exit.ok_or_else(|| {
            let screen = state.parser.screen().contents();
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
