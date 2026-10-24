use std::{
    io::{Read, Write},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use ktask_rs_mk3::scratch::Scratch;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;
/// How long a test waits for something it expects. A wait ends as soon as the condition
/// holds, so this only turns a hang into a failure. It must never bound how fast work runs:
/// the suite runs tests in parallel, and the machine's load varies.
pub const PATIENCE: Duration = Duration::from_secs(60);
const FRAME_END: &[u8] = b"\x1b[?2026l";

/// A private Git baseline for storage fixtures; generated fixture/state files stay ignored.
pub fn empty_repository(root: &Path) -> Result {
    let git = |args: &[&str]| -> Result {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    git(&["init", "-q"])?;
    std::fs::write(root.join(".git/info/exclude"), "*\n")?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "--allow-empty",
        "-qm",
        "baseline",
    ])
}

/// Tests run on parallel threads, and a child that another thread starts holds copies of
/// all of this process's open files until it execs. Launching a file this process has just
/// written then fails with ETXTBSY. So a short-lived child writes every executable a test
/// launches and renames it into place; this process never opens it for writing.
fn install_executable(to: &Path, source: Stdio, script: Option<&[u8]>) -> Result {
    let staged = to.with_extension("installing");
    let mut child = std::process::Command::new("sh")
        .args([
            "-c",
            r#"cat > "$2" && chmod 755 "$2" && mv -f -- "$2" "$1""#,
            "sh",
        ])
        .args([to, &staged])
        .stdin(source)
        .spawn()?;
    if let Some(script) = script {
        child
            .stdin
            .take()
            .ok_or("installer stdin")?
            .write_all(script)?;
    }
    let status = child.wait()?;
    assert!(
        status.success(),
        "installing {} failed: {status}",
        to.display()
    );
    Ok(())
}

pub fn copy_executable(from: impl AsRef<Path>, to: &Path) -> Result {
    install_executable(to, std::fs::File::open(from)?.into(), None)
}

pub fn write_executable(to: &Path, script: impl AsRef<[u8]>) -> Result {
    install_executable(to, Stdio::piped(), Some(script.as_ref()))
}

/// The kernel start time of `pid`, or None once it no longer exists. Unlike the pid,
/// which the kernel reuses, the pair identifies one process.
pub fn start_time(pid: u32) -> Result<Option<u64>> {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return Ok(None);
    };
    // Field 22 of proc_pid_stat(5); the command name before it may contain spaces.
    let after_name = &stat[stat.rfind(')').ok_or("bad stat")? + 2..];
    let started = after_name.split(' ').nth(19).ok_or("short stat")?;
    Ok(Some(started.parse()?))
}

/// Polls `probe` until it yields a value, failing with `what` after PATIENCE.
pub fn eventually<T>(what: &str, mut probe: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(value) = probe()? {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("{what}: not reached within {PATIENCE:?}").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

pub fn location(screen: &str, needle: &str) -> Result<(usize, usize)> {
    screen
        .lines()
        .enumerate()
        .find_map(|(row, line)| {
            line.find(needle)
                .map(|byte| (line[..byte].chars().count(), row))
        })
        .ok_or_else(|| format!("missing {needle:?} in screen:\n{screen}").into())
}

pub fn drag(t: &mut Terminal, from: (usize, usize), to: (usize, usize)) -> Result {
    let (x, y) = from;
    let (end_x, end_y) = to;
    t.send(&format!(
        "\x1b[<0;{};{}M\x1b[<32;{};{}M\x1b[<0;{};{}m",
        x + 1,
        y + 1,
        end_x + 1,
        end_y + 1,
        end_x + 1,
        end_y + 1
    ))
}

pub fn copied(t: &Terminal) -> Result<String> {
    let raw = t.output_bytes();
    let start = raw
        .windows(7)
        .rposition(|w| w == b"\x1b]52;c;")
        .ok_or("OSC 52 copy not emitted")?
        + 7;
    let end = raw[start..]
        .iter()
        .position(|b| *b == 7)
        .ok_or("OSC 52 terminator missing")?
        + start;
    let mut decoded = Vec::new();
    let mut bits = 0u32;
    let mut count = 0;
    for &b in &raw[start..end] {
        let value = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return Err("invalid base64".into()),
        };
        bits = (bits << 6) | value as u32;
        count += 6;
        if count >= 8 {
            count -= 8;
            decoded.push((bits >> count) as u8);
            bits &= (1 << count) - 1;
        }
    }
    Ok(String::from_utf8(decoded)?)
}

pub fn copy(t: &mut Terminal) -> Result<String> {
    copy_after(t, "new OSC 52 copy", |t| t.send("\x1b[17~"))
}

pub fn copy_key(t: &mut Terminal, keys: &str) -> Result<String> {
    copy_after(t, "new OSC 52 copy from key", |t| t.send(keys))
}

pub fn mouse_copy(t: &mut Terminal) -> Result<String> {
    copy_after(t, "new OSC 52 copy from mouse", |t| {
        // A modal can cover the main Copy button; click its visible button.
        let screen = t.current_screen();
        let (row, col) = screen
            .lines()
            .enumerate()
            .filter_map(|(row, line)| {
                line.find("[Copy]")
                    .map(|byte| (row, line[..byte].chars().count()))
            })
            .last()
            .ok_or("Copy button missing")?;
        t.send(&format!(
            "\x1b[<0;{};{}M\x1b[<0;{};{}m",
            col + 1,
            row + 1,
            col + 1,
            row + 1
        ))
    })
}

fn copy_after(
    t: &mut Terminal,
    description: &str,
    trigger: impl FnOnce(&mut Terminal) -> Result,
) -> Result<String> {
    let before = t
        .output_bytes()
        .windows(7)
        .filter(|w| *w == b"\x1b]52;c;")
        .count();
    trigger(t)?;
    // OSC 52 is emitted after the frame terminator and may arrive in a later
    // read. Wait for its complete payload, not another rendered frame.
    let deadline = Instant::now() + PATIENCE;
    loop {
        let bytes = t.output_bytes();
        if bytes.windows(7).filter(|w| *w == b"\x1b]52;c;").count() > before && copied(t).is_ok() {
            break;
        }
        if Instant::now() >= deadline {
            return Err(format!("{description}: no complete OSC 52 payload").into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    copied(t)
}

pub struct Terminal {
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    parser: Arc<Mutex<vt100::Parser>>,
    output: Arc<Mutex<Vec<u8>>>,
    frames: mpsc::Receiver<String>,
    exit: mpsc::Receiver<std::io::Result<portable_pty::ExitStatus>>,
    reader: Option<thread::JoinHandle<()>>,
    exited: bool,
    /// The screen before input that no wait has observed yet.
    before_input: Mutex<Option<String>>,
    _sandbox: Option<Sandbox>,
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
        let output = Arc::new(Mutex::new(Vec::new()));
        let read_output = Arc::clone(&output);
        let (send_frame, frames) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut bytes = [0; 4096];
            let mut marker = Vec::new();
            while let Ok(count) = stream.read(&mut bytes) {
                if count == 0 {
                    break;
                }
                read_output
                    .lock()
                    .unwrap()
                    .extend_from_slice(&bytes[..count]);
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
            output,
            frames,
            exit,
            reader: Some(reader),
            exited: false,
            before_input: Mutex::new(None),
            _sandbox: None,
        })
    }

    pub fn send(&mut self, keys: &str) -> Result {
        self.before_input
            .lock()
            .unwrap()
            .get_or_insert_with(|| self.parser.lock().unwrap().screen().contents());
        while self.frames.try_recv().is_ok() {}
        self.writer.write_all(keys.as_bytes())?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn output_bytes(&self) -> Vec<u8> {
        self.output.lock().unwrap().clone()
    }

    pub fn redraw_count(&self) -> usize {
        self.output_bytes()
            .windows(FRAME_END.len())
            .filter(|window| *window == FRAME_END)
            .count()
    }

    pub fn selected_at(&self, col: usize, row: usize) -> bool {
        self.parser
            .lock()
            .unwrap()
            .screen()
            .cell(row as u16, col as u16)
            .is_some_and(|cell| cell.bgcolor() == vt100::Color::Rgb(255, 210, 111))
    }
    pub fn mouse_capture(&self) -> bool {
        self.parser.lock().unwrap().screen().mouse_protocol_mode() != vt100::MouseProtocolMode::None
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

    /// Click the first `label` below the line holding `anchor`, such as one field's choice.
    pub fn click_after(&mut self, anchor: &str, label: &str) -> Result {
        let screen = self.parser.lock().unwrap().screen().contents();
        let (row, col) = screen
            .lines()
            .enumerate()
            .skip_while(|(_, line)| !line.contains(anchor))
            .skip(1)
            .find_map(|(row, line)| {
                line.find(label)
                    .map(|byte| (row, line[..byte].chars().count()))
            })
            .ok_or_else(|| format!("click target {label:?} after {anchor:?} missing:\n{screen}"))?;
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

    pub fn current_screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Waits until the screen matches. After input, the condition must describe its result:
    /// one that the screen before the input already met cannot tell whether the input has
    /// been handled, so the wait fails at once. Use `still` to check that input changed nothing.
    /// The condition must depend only on the screen text it is given; for state outside the
    /// text (selection colour, redraw count) check it is false before the input, then `still`.
    pub fn screen(&self, description: &str, matches: impl Fn(&str) -> bool) -> Result<String> {
        if let Some(before) = self.before_input.lock().unwrap().take()
            && matches(&before)
        {
            return Err(format!(
                "{description}: ambiguous wait, the screen already matched before the input:\n{before}"
            )
            .into());
        }
        self.wait_for(description, matches)
    }

    /// Like `screen`, for a condition the screen met before the input too. It cannot prove
    /// the input was handled, only that the screen still matches.
    pub fn still(&self, description: &str, matches: impl Fn(&str) -> bool) -> Result<String> {
        self.before_input.lock().unwrap().take();
        self.wait_for(description, matches)
    }

    fn wait_for(&self, description: &str, matches: impl Fn(&str) -> bool) -> Result<String> {
        let deadline = Instant::now() + PATIENCE;
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
        self.frames.recv_timeout(PATIENCE)?;
        Ok(())
    }

    pub fn finish(&mut self) -> Result {
        let status = self.exit.recv_timeout(PATIENCE)??;
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
        self.exit.recv_timeout(PATIENCE)??;
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
            let _ = self.exit.recv_timeout(PATIENCE);
        }
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
        // Dropping the sandbox checks that the TUI left no processes behind.
        self._sandbox.take();
    }
}

pub fn sandbox() -> std::io::Result<Sandbox> {
    Scratch::new(&std::env::temp_dir().join("ktask-test-scratch")).map(|s| Sandbox(Some(s)))
}

/// A test's private directory. A test must stop everything it starts: when the sandbox
/// goes, every process that still refers to it must exit, or the test fails naming them.
pub struct Sandbox(Option<Scratch>);

impl Sandbox {
    pub fn path(&self) -> &Path {
        self.0.as_ref().expect("open sandbox").path()
    }

    fn release(&mut self) -> Result {
        let Some(scratch) = self.0.take() else {
            return Ok(());
        };
        let stray = eventually("processes left by the test exit", || {
            let stray = processes_using(scratch.path());
            Ok(stray.is_empty().then_some(()))
        });
        let result = if stray.is_err() {
            let stray = processes_using(scratch.path());
            for (pid, _) in &stray {
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(*pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
            let list: Vec<String> = stray
                .iter()
                .map(|(pid, cmd)| format!("{pid} {cmd}"))
                .collect();
            Err(format!("test left processes running:\n{}", list.join("\n")).into())
        } else {
            Ok(())
        };
        scratch.close()?;
        result
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            if thread::panicking() {
                eprintln!("{error}");
            } else {
                panic!("{error}");
            }
        }
    }
}

/// Live processes, other than this one, whose command line, working directory or HOME is
/// inside `dir`.
fn processes_using(dir: &Path) -> Vec<(i32, String)> {
    let me = std::process::id() as i32;
    let needle = dir.to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
        .filter(|&pid| pid != me)
        .filter_map(|pid| {
            let proc = Path::new("/proc").join(pid.to_string());
            let cmdline = std::fs::read(proc.join("cmdline")).ok()?;
            let cmdline = String::from_utf8_lossy(&cmdline).replace('\0', " ");
            let cwd = std::fs::read_link(proc.join("cwd")).ok();
            let home = std::fs::read(proc.join("environ"))
                .ok()
                .and_then(|environ| {
                    environ
                        .split(|&b| b == 0)
                        .find_map(|var| var.strip_prefix(b"HOME=").map(|v| v.to_vec()))
                });
            let uses = cmdline.contains(&needle)
                || cwd.is_some_and(|cwd| cwd.starts_with(dir))
                || home.is_some_and(|home| {
                    Path::new(&*String::from_utf8_lossy(&home)).starts_with(dir)
                });
            // A zombie has exited; its parent reaps it.
            let zombie = std::fs::read_to_string(proc.join("stat")).is_ok_and(|stat| {
                stat.rsplit_once(')')
                    .is_some_and(|(_, rest)| rest.starts_with(" Z"))
            });
            (uses && !zombie).then_some((pid, cmdline))
        })
        .collect()
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
