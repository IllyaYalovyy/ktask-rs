use std::{
    fs,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use ktask_rs_mk3::scratch::Scratch;

use crate::support::{self, Result, Terminal};

fn runs(root: &Path) -> Result<Vec<PathBuf>> {
    Ok(fs::read_dir(root)?
        .filter_map(|entry| {
            let entry = entry.ok()?;
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("run-")
                .then(|| entry.path())
        })
        .collect())
}

#[test]
fn simultaneous_sweep_and_close_never_recreate_a_removed_lease() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path().join("concurrent");
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let root = root.clone();
            thread::spawn(move || -> std::io::Result<()> {
                for n in 0..100 {
                    let scratch = Scratch::new(&root)?;
                    fs::write(scratch.path().join("work"), "temporary")?;
                    if n % 2 == 0 {
                        scratch.close()?;
                    } else {
                        drop(scratch);
                    }
                }
                Ok(())
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("cleanup worker panicked")?;
    }
    assert!(runs(&root)?.is_empty());
    Ok(())
}

#[test]
fn scratch_cleans_large_trees_and_unwinding_without_touching_live_owners() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path().join("scratch");
    let live = Scratch::new(&root)?;
    fs::write(live.path().join("keep"), "live work")?;
    let other = Scratch::new(&root)?;
    for n in 0..1000 {
        let directory = other.path().join(format!("test-{n}"));
        fs::create_dir(&directory)?;
        fs::write(directory.join("output"), "temporary")?;
    }
    let removed = other.path().to_owned();
    other.close()?;
    assert!(!removed.exists());
    assert_eq!(fs::read_to_string(live.path().join("keep"))?, "live work");
    let panic = std::panic::catch_unwind(|| {
        let scratch = Scratch::new(&root).unwrap();
        fs::write(scratch.path().join("partial"), "failed test").unwrap();
        panic!("simulated test failure");
    });
    assert!(panic.is_err());
    assert_eq!(runs(&root)?, [live.path()]);
    live.close()?;
    assert!(runs(&root)?.is_empty());
    Ok(())
}

#[test]
fn crash_debris_is_reclaimed_but_other_instances_and_live_runs_are_preserved() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let dev_scratch = root.join("cache/ktask-dev/scratch");
    let mut prod = Terminal::open_at(env!("CARGO_BIN_EXE_ktask"), root, 100, 26)?;
    prod.screen("production ready", |s| s.contains("Queue"))?;
    let prod_dirs = runs(&root.join("cache/ktask/scratch"))?;
    let mut live = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 26)?;
    live.screen("live development instance", |s| s.contains("Queue"))?;
    let live_dirs = runs(&dev_scratch)?;
    let mut crashed = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 26)?;
    crashed.screen("second development instance", |s| s.contains("Queue"))?;
    let abandoned = runs(&dev_scratch)?
        .into_iter()
        .find(|path| !live_dirs.contains(path))
        .unwrap();
    fs::create_dir(abandoned.join("agent-files"))?;
    fs::write(abandoned.join("agent-files/leftover"), "unfinished")?;
    crashed.kill()?;
    assert!(
        abandoned.exists(),
        "abrupt termination leaves work for the next launch to reclaim"
    );
    let mut next = Terminal::open_at(env!("CARGO_BIN_EXE_ktask-dev"), root, 100, 26)?;
    next.screen("restarted development instance", |s| s.contains("Queue"))?;
    assert!(!abandoned.exists());
    for path in live_dirs.iter().chain(&prod_dirs) {
        assert!(
            path.exists(),
            "deleted a live owner's directory: {}",
            path.display()
        );
    }
    for terminal in [&mut next, &mut live, &mut prod] {
        terminal.send("q")?;
        terminal.finish()?;
    }
    assert!(runs(&dev_scratch)?.is_empty());
    assert!(runs(&root.join("cache/ktask/scratch"))?.is_empty());
    Ok(())
}

#[test]
fn cleanup_ignores_unmanaged_files_and_does_not_follow_symlinks() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path().join("scratch");
    fs::create_dir(&root)?;
    let outside = sandbox.path().join("outside");
    fs::create_dir(&outside)?;
    fs::write(outside.join("keep"), "user data")?;
    fs::write(root.join("notes"), "keep me")?;
    std::os::unix::fs::symlink(&outside, root.join("run-symlink"))?;
    let incomplete = root.join("run-incomplete");
    fs::create_dir(&incomplete)?;
    let scratch = Scratch::new(&root)?;
    assert!(!incomplete.exists());
    assert_eq!(fs::read_to_string(outside.join("keep"))?, "user data");
    assert_eq!(fs::read_to_string(root.join("notes"))?, "keep me");
    scratch.close()?;
    Ok(())
}

// Runs only when the crash-recovery test explicitly launches this test executable.
#[test]
fn scratch_owner_process() -> Result {
    let Some(root) = std::env::var_os("KTASK_TEST_CRASH_ROOT") else {
        return Ok(());
    };
    let scratch = Scratch::new(Path::new(&root))?;
    fs::write(scratch.path().join("partial-work"), "unfinished test")?;
    println!("SCRATCH_READY {}", scratch.path().display());
    std::io::stdout().flush()?;
    std::io::stdin().read_exact(&mut [0])?;
    Ok(())
}

struct Owner(Child);

impl Drop for Owner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn killed_test_process_is_reclaimed_by_the_next_test() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path().join("test-runs");
    let mut owner = Owner(
        Command::new(std::env::current_exe()?)
            .args(["--exact", "cleanup::scratch_owner_process", "--nocapture"])
            .env_clear()
            .env("HOME", sandbox.path())
            .env("TMPDIR", sandbox.path())
            .env("KTASK_TEST_CRASH_ROOT", &root)
            .current_dir(sandbox.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?,
    );
    let stdout = owner.0.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    let reader = thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(std::result::Result::ok)
        {
            if let Some(path) = line.strip_prefix("SCRATCH_READY ") {
                let _ = sender.send(PathBuf::from(path));
                break;
            }
        }
    });
    let abandoned = receiver.recv_timeout(Duration::from_secs(5))?;
    reader.join().unwrap();
    let live = Scratch::new(&root)?;
    assert!(abandoned.join("partial-work").exists());
    owner.0.kill()?;
    owner.0.wait()?;
    assert!(abandoned.exists(), "SIGKILL cannot run destructors");
    let next = Scratch::new(&root)?;
    assert!(!abandoned.exists());
    assert!(live.path().exists());
    next.close()?;
    live.close()?;
    assert!(runs(&root)?.is_empty());
    Ok(())
}
