use std::{
    io::{Read, Write},
    path::Path,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use ktask_rs_mk3::scratch::Scratch;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
const TIMEOUT: Duration = Duration::from_secs(5);
const FRAME_END: &[u8] = b"\x1b[?2026l";

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    frames: mpsc::Receiver<String>,
    exit: mpsc::Receiver<std::io::Result<portable_pty::ExitStatus>>,
    reader: Option<thread::JoinHandle<()>>,
    exited: bool,
    _sandbox: Option<Scratch>,
}

impl Terminal {
    pub fn open(cols: u16, rows: u16) -> Result<Self> {
        let sandbox = sandbox()?;
        let mut terminal =
            Self::open_at(env!("CARGO_BIN_EXE_ktask-dev"), sandbox.path(), cols, rows)?;
        terminal._sandbox = Some(sandbox);
        Ok(terminal)
    }

    pub fn open_at(binary: &str, sandbox: &Path, cols: u16, rows: u16) -> Result<Self> {
        Self::open_mode(binary, sandbox, sandbox, cols, rows, true)
    }

    pub fn open_real(binary: &str, sandbox: &Path, cols: u16, rows: u16) -> Result<Self> {
        Self::open_mode(binary, sandbox, sandbox, cols, rows, false)
    }

    pub fn open_project(
        binary: &str,
        sandbox: &Path,
        project: &Path,
        cols: u16,
        rows: u16,
    ) -> Result<Self> {
        Self::open_mode(binary, sandbox, project, cols, rows, false)
    }

    fn open_mode(
        binary: &str,
        sandbox: &Path,
        project: &Path,
        cols: u16,
        rows: u16,
        demo: bool,
    ) -> Result<Self> {
        let pair = native_pty_system().openpty(size(cols, rows))?;
        let mut command = CommandBuilder::new(binary);
        if demo {
            command.arg("--demo");
        }
        command.env_clear();
        command.env("TERM", "xterm-256color");
        command.env("HOME", sandbox);
        command.env("XDG_CONFIG_HOME", sandbox.join("config"));
        command.env("XDG_STATE_HOME", sandbox.join("state"));
        command.env("XDG_CACHE_HOME", sandbox.join("cache"));
        command.env("XDG_RUNTIME_DIR", sandbox.join("runtime"));
        command.env("TMPDIR", sandbox);
        command.env(
            "PATH",
            format!("{}:/usr/bin:/bin", sandbox.join("bin").display()),
        );
        command.cwd(project);
        let mut child = pair.slave.spawn_command(command)?;
        let killer = child.clone_killer();
        drop(pair.slave);
        let writer = pair.master.take_writer()?;
        let mut stream = pair.master.try_clone_reader()?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
        let read_parser = Arc::clone(&parser);
        let (send_frame, frames) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut marker = Vec::new();
            while let Ok(count) = stream.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                let mut parser = read_parser.lock().unwrap();
                for byte in &bytes[..count] {
                    parser.process(&[*byte]);
                    marker.push(*byte);
                    if marker.ends_with(FRAME_END) {
                        let _ = send_frame.send(parser.screen().contents());
                    }
                    if marker.len() > FRAME_END.len() {
                        marker.remove(0);
                    }
                }
            }
        });
        let (send_exit, exit) = mpsc::channel();
        thread::spawn(move || {
            let _ = send_exit.send(child.wait());
        });
        Ok(Self {
            master: pair.master,
            writer,
            killer,
            parser,
            frames,
            exit,
            reader: Some(reader),
            exited: false,
            _sandbox: None,
        })
    }

    pub fn send(&mut self, keys: &str) -> Result {
        while self.frames.try_recv().is_ok() {}
        self.writer.write_all(keys.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn click(&mut self, label: &str) -> Result {
        let screen = self.parser.lock().unwrap().screen().contents();
        let button = format!("[{label}]");
        let label = if screen.contains(&button) {
            &button
        } else {
            label
        };
        let (row, col) = screen
            .lines()
            .enumerate()
            .find_map(|(row, line)| {
                line.find(label)
                    .map(|byte| (row, line[..byte].chars().count()))
            })
            .ok_or_else(|| format!("click target {label:?} missing:\n{screen}"))?;
        self.send(&format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            col + 1,
            row + 1,
            col + 1,
            row + 1
        ))
    }

    pub fn wheel(&mut self, down: bool, col: u16, row: u16) -> Result {
        self.send(&format!(
            "\x1b[<{};{};{}M",
            if down { 65 } else { 64 },
            col + 1,
            row + 1
        ))
    }

    pub fn screen(&self, description: &str, matches: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + TIMEOUT;
        let mut last = self.parser.lock().unwrap().screen().contents();
        let mut matched = matches(&last);
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = if matched {
                remaining.min(Duration::from_millis(40))
            } else {
                remaining
            };
            match self.frames.recv_timeout(wait) {
                Ok(frame) => {
                    last = frame;
                    matched = matches(&last);
                }
                Err(mpsc::RecvTimeoutError::Timeout) if matched => return Ok(last),
                Err(error) => {
                    return Err(format!("{description}: {error}\nLast screen:\n{last}").into());
                }
            }
        }
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result {
        while self.frames.try_recv().is_ok() {}
        self.parser
            .lock()
            .unwrap()
            .screen_mut()
            .set_size(rows, cols);
        self.master.resize(size(cols, rows))?;
        // Resizing the parser only crops its old contents. Wait for a real redraw
        // before a caller can match that stale screen or click during its replacement.
        self.frames.recv_timeout(TIMEOUT)?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result {
        let status = self.exit.recv_timeout(TIMEOUT)??;
        self.exited = true;
        assert!(status.success(), "exit status: {status:?}");
        if let Some(reader) = self.reader.take() {
            reader.join().expect("terminal reader panicked");
        }
        assert!(
            !self.parser.lock().unwrap().screen().alternate_screen(),
            "terminal was not restored"
        );
        assert_eq!(
            self.parser.lock().unwrap().screen().mouse_protocol_mode(),
            vt100::MouseProtocolMode::None,
            "mouse capture was not restored"
        );
        assert!(
            !self.parser.lock().unwrap().screen().bracketed_paste(),
            "paste mode was not restored"
        );
        Ok(())
    }

    pub fn kill(&mut self) -> Result {
        self.killer.kill()?;
        self.exit.recv_timeout(TIMEOUT)??;
        self.exited = true;
        if let Some(reader) = self.reader.take() {
            reader.join().expect("terminal reader panicked");
        }
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if !self.exited {
            let _ = self.killer.kill();
            let _ = self.exit.recv_timeout(TIMEOUT);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        if let Some(sandbox) = self._sandbox.take()
            && let Err(error) = sandbox.close()
        {
            if std::thread::panicking() {
                eprintln!("test sandbox cleanup failed: {error}");
            } else {
                panic!("test sandbox cleanup failed: {error}");
            }
        }
    }
}

pub fn sandbox() -> std::io::Result<Scratch> {
    Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
