//! B-22: a `ktask-rs run` this test starts through the TUI's `r` — detached, by design, so
//! quitting the screen alone does not stop it — together with its watcher and its provider,
//! and whatever that provider itself goes on to start, does not outlive the test when the
//! test panics before the run is ever let to finish.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::pty::Terminal;
use super::repo::{git_repository, scratch};
use super::support::{Result, Sandbox};

/// Whether process `pid` is still running. A killed process's entry under `/proc` can
/// briefly outlive the signal that ended it, as a zombie waiting for its new parent to reap
/// it once it is orphaned, so that alone does not count as still running.
fn is_running(pid: i32) -> bool {
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        return false;
    };
    let state = stat
        .split(')')
        .next_back()
        .and_then(|rest| rest.split_whitespace().next());
    state != Some("Z")
}

/// Waits, for up to a few seconds, until `probe` gives a value; fails naming `what` when it
/// never does.
fn wait_until_some<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = probe() {
            return Ok(value);
        }
        if Instant::now() >= deadline {
            return Err(format!("timed out waiting for {what}").into());
        }
        std::thread::park_timeout(Duration::from_millis(20));
    }
}

/// A sandbox with a git repository called `my-app` and one task whose script — standing in
/// for a real agent's own process tree — records its own process id and that of a
/// grandchild it deliberately backgrounds, then blocks forever on a fifo this test never
/// writes to: everything a real provider might start, however deep, left in place to see
/// whether it outlives the test.
struct Fixture {
    sandbox: Sandbox,
    _keep: tempfile::TempDir,
    repository: PathBuf,
    provider_pid_file: PathBuf,
    grandchild_pid_file: PathBuf,
}

impl Fixture {
    fn new() -> Result<Self> {
        let sandbox = Sandbox::new()?;
        let (keep, work) = scratch()?;
        let repository = git_repository(&sandbox, &work, "my-app")?;
        let provider_pid_file = work.join("provider.pid");
        let grandchild_pid_file = work.join("grandchild.pid");
        let go = work.join("go");
        let body = format!(
            "```bash\necho $$ > \"{provider}\"\nsleep 999 &\necho $! > \"{grandchild}\"\n[ -p \"{go}\" ] || mkfifo \"{go}\"\nread _ < \"{go}\"\nktask-rs report --token \"$1\" done\n```\n",
            provider = provider_pid_file.display(),
            grandchild = grandchild_pid_file.display(),
            go = go.display(),
        );
        let added = sandbox.run(
            &repository,
            &[
                "add",
                "--title",
                "gated",
                "--criterion",
                "it works",
                "--body",
                &body,
            ],
        )?;
        assert_eq!(added.code, Some(0), "{}", added.stderr);
        Ok(Self {
            sandbox,
            _keep: keep,
            repository,
            provider_pid_file,
            grandchild_pid_file,
        })
    }

    /// Opens the queue screen and waits until it is drawn whole.
    fn open(&self) -> Result<Terminal> {
        let terminal = Terminal::launch(&self.sandbox, &self.repository, &["tui"], 24, 110)?;
        terminal.wait_for("the queue screen", |screen| {
            screen.contents().ends_with('┘')
        })?;
        Ok(terminal)
    }
}

/// However a test above left the run it started through the TUI, nothing of it survives the
/// test itself — the same guarantee every other fixture that can start one gives.
impl Drop for Fixture {
    fn drop(&mut self) {
        super::run_cleanup::kill_run_if_in_progress(&self.sandbox, "my-app");
    }
}

#[test]
fn a_run_the_tui_started_and_everything_its_provider_forked_dies_even_when_the_test_panics_first() {
    let mut provider_pid = 0_i32;
    let mut grandchild_pid = 0_i32;

    // The default panic hook would print this simulated failure's message to stderr, which
    // would only confuse anyone reading this test's own output; restored right after, so a
    // real failure elsewhere still prints as usual.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<()> {
        let fixture = Fixture::new()?;
        let mut terminal = fixture.open()?;
        terminal.send("r")?;

        provider_pid = wait_until_some("the provider to record its process id", || {
            std::fs::read_to_string(&fixture.provider_pid_file)
                .ok()?
                .trim()
                .parse()
                .ok()
        })?;
        grandchild_pid = wait_until_some("the grandchild to record its process id", || {
            std::fs::read_to_string(&fixture.grandchild_pid_file)
                .ok()?
                .trim()
                .parse()
                .ok()
        })?;
        assert!(
            is_running(provider_pid),
            "provider {provider_pid} is not running"
        );
        assert!(
            is_running(grandchild_pid),
            "grandchild {grandchild_pid} is not running"
        );

        // Simulates this test failing while the run it started through the TUI is still in
        // progress, the screen never even asked to quit — exactly the situation that used to
        // leave a `ktask-rs run`, its watcher and its provider alive long after: `fixture`
        // (and `terminal`) unwind with this closure, and it is that unwind's own `Drop` that
        // must end all three, and the grandchild along with them.
        panic!("a simulated failure, with the run still in progress");
    }));
    std::panic::set_hook(previous_hook);

    assert!(unwound.is_err(), "the inner closure was expected to panic");
    assert_ne!(provider_pid, 0, "the provider's pid was never captured");
    assert_ne!(grandchild_pid, 0, "the grandchild's pid was never captured");

    let deadline = Instant::now() + Duration::from_secs(10);
    while (is_running(provider_pid) || is_running(grandchild_pid)) && Instant::now() < deadline {
        std::thread::park_timeout(Duration::from_millis(20));
    }
    assert!(
        !is_running(provider_pid),
        "the provider outlived the test that panicked with its run in progress"
    );
    assert!(
        !is_running(grandchild_pid),
        "the grandchild the provider forked outlived the test"
    );
}
