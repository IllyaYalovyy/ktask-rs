//! Retained, live provider output through the real command-line binary.

#[path = "support/repo.rs"]
mod repo;
mod support;

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use repo::{git_repository, scratch};
use support::{Result, Sandbox};

fn wait_until(what: &str, mut condition: impl FnMut() -> bool) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !condition() {
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
    Ok(())
}

struct Fixture {
    sandbox: Sandbox,
    repository: PathBuf,
    _keep: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        sandbox.run(&repository, &["settings", "set", "max-attempts", "1"])?;
        Ok(Self {
            sandbox,
            repository,
            _keep: keep,
        })
    }
    fn run(&self, args: &[&str]) -> Result<support::Outcome> {
        self.sandbox.run(&self.repository, args)
    }
    fn add(&self, body: &str) -> Result<()> {
        let out = self.run(&[
            "add",
            "--title",
            "output",
            "--criterion",
            "visible",
            "--body",
            body,
        ])?;
        assert_eq!(out.code, Some(0), "{}", out.stderr);
        Ok(())
    }
    fn spawn(&self, args: &[&str]) -> Result<std::process::Child> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_ktask-rs"));
        command.args(args);
        self.sandbox.isolate(&mut command, &self.repository);
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Ok(command.spawn()?)
    }
}

#[test]
fn output_follows_live_bytes_and_retains_safe_whole_attempts() -> Result<()> {
    let fixture = Fixture::new()?;
    let gate = fixture.repository.join("release-output");
    let body = format!(
        "```bash\nprintf 'live line attempt %s\\n' \"$2\"\nprintf 'before\\033[2J\\ra\\377\\n'\nhead -c 10000 /dev/zero | tr '\\0' x\nprintf '\\n'\nif [ \"$2\" = 1 ]; then [ -p '{0}' ] || mkfifo '{0}'; read _ < '{0}'; fi\nktask-rs report --token \"$1\" failed --reason retry\n```",
        gate.display()
    );
    fixture.add(&body)?;
    let mut run = fixture.spawn(&["run"])?;
    wait_until("the live attempt", || {
        fixture
            .run(&["status"])
            .is_ok_and(|out| out.stdout.contains("running"))
    })?;

    let mut output = fixture.spawn(&["output", "1", "--follow"])?;
    let mut reader = output.stdout.take().expect("piped stdout");
    let (sent, received) = mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = [0_u8; 10];
        let read = reader.read(&mut bytes);
        let _ = sent.send((read, bytes));
        let mut rest = Vec::new();
        let _ = reader.read_to_end(&mut rest);
    });
    let (read, bytes) = received
        .recv_timeout(Duration::from_secs(1))
        .map_err(|_| "output --follow did not print within one second")?;
    assert!(String::from_utf8_lossy(&bytes[..read?]).contains("live line"));
    std::fs::write(&gate, "go\n")?;
    assert!(!run.wait()?.success());
    assert!(output.wait()?.success());

    let retained = fixture.run(&["output", "1"])?;
    assert_eq!(retained.code, Some(0), "{}", retained.stderr);
    assert!(
        retained.stdout.contains("before\\x1b[2J\na�"),
        "{:?}",
        retained.stdout
    );
    assert!(
        retained
            .stdout
            .lines()
            .all(|line| line.chars().count() <= 240)
    );

    let retried = fixture.run(&["retry", "1"])?;
    assert_eq!(retried.code, Some(0));
    let second = fixture.run(&["run"])?;
    assert_eq!(second.code, Some(1));
    let first = fixture.run(&["output", "1", "--attempt", "1"])?;
    let latest = fixture.run(&["output", "1"])?;
    assert!(first.stdout.contains("live line attempt 1"));
    assert!(latest.stdout.contains("live line attempt 2"));
    Ok(())
}
