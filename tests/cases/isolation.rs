use std::{collections::BTreeMap, fs, path::Path, process::Command};

use crate::support::{self, Result, Terminal};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");

fn paths(binary: &str, root: &Path, cwd: &Path) -> Result<BTreeMap<String, String>> {
    let output = Command::new(binary)
        .arg("--paths")
        .env_clear()
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        // Neither inherited channel guesses nor a production-first PATH may switch identity.
        .env("KTASK_INSTANCE", "production")
        .env("KTASK_RS_CHANNEL", "user")
        .env("PATH", Path::new(PROD).parent().unwrap())
        .current_dir(cwd)
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .map(|line| {
            let (key, value) = line.split_once('\t').expect("label and value");
            (key.to_owned(), value.to_owned())
        })
        .collect())
}

#[test]
fn binaries_have_separate_roots_even_when_renamed_or_started_elsewhere() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let elsewhere = root.join("elsewhere");
    fs::create_dir(&elsewhere)?;
    let dev = paths(DEV, root, root)?;
    let prod = paths(PROD, root, root)?;
    for name in ["config", "state", "cache", "runtime"] {
        assert_eq!(
            dev[name],
            root.join(name).join("ktask-dev").to_string_lossy()
        );
        assert_eq!(prod[name], root.join(name).join("ktask").to_string_lossy());
        assert!(!Path::new(&dev[name]).exists(), "--paths created storage");
        assert!(!Path::new(&prod[name]).exists(), "--paths created storage");
    }
    assert_eq!(dev, paths(DEV, root, &elsewhere)?);
    let renamed = root.join("ktask");
    fs::hard_link(DEV, &renamed)?;
    let renamed_paths = paths(renamed.to_str().unwrap(), root, &elsewhere)?;
    assert_eq!(renamed_paths["instance"], "ktask-dev");
    assert_eq!(renamed_paths["state"], dev["state"]);
    assert_eq!(renamed_paths["executable"], renamed.to_string_lossy());
    // A callback using the advertised absolute executable retains the same instance.
    assert_eq!(
        paths(&renamed_paths["executable"], root, root)?,
        renamed_paths
    );
    Ok(())
}

#[test]
fn both_instances_run_side_by_side_in_one_checkout() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let sentinel = root.join("state/ktask/queue.txt");
    fs::create_dir_all(sentinel.parent().unwrap())?;
    fs::write(&sentinel, "production task queue")?;
    let mut prod = Terminal::open_at(PROD, root, 100, 26)?;
    let production = prod.screen("production", |s| {
        s.contains(" ktask ") && s.contains("MOCK DATA")
    })?;
    let mut dev = Terminal::open_at(DEV, root, 100, 26)?;
    let development = dev.screen("development", |s| {
        s.contains("ktask-dev") && s.contains("MOCK DATA")
    })?;
    println!("PRODUCTION\n{production}\nDEVELOPMENT\n{development}");
    dev.send("2j")?;
    dev.screen("dev selection", |s| s.contains("› #2"))?;
    dev.send("q")?;
    dev.finish()?;
    prod.send("?")?;
    prod.screen("production remains live", |s| {
        s.contains("Keyboard help") && s.contains("Current #1")
    })?;
    prod.send("\x03")?;
    prod.finish()?;
    assert_eq!(fs::read_to_string(sentinel)?, "production task queue");
    Ok(())
}

#[test]
fn default_paths_are_scoped_and_relative_xdg_roots_are_rejected() -> Result {
    let sandbox = support::sandbox()?;
    for (binary, name) in [(DEV, "ktask-dev"), (PROD, "ktask")] {
        let output = Command::new(binary)
            .arg("--paths")
            .env_clear()
            .env("HOME", sandbox.path())
            .output()?;
        assert!(output.status.success());
        let text = String::from_utf8(output.stdout)?;
        for suffix in [
            format!(".config/{name}"),
            format!(".local/state/{name}"),
            format!(".cache/{name}"),
            format!(".local/state/{name}/run"),
        ] {
            assert!(
                text.contains(sandbox.path().join(suffix).to_str().unwrap()),
                "{text}"
            );
        }
        let invalid = Command::new(binary)
            .arg("--paths")
            .env_clear()
            .env("HOME", sandbox.path())
            .env("XDG_STATE_HOME", "relative")
            .output()?;
        assert!(!invalid.status.success());
        assert!(String::from_utf8_lossy(&invalid.stderr).contains("absolute path"));
        for flag in ["--version", "--help"] {
            let output = Command::new(binary)
                .arg(flag)
                .env_clear()
                .env("HOME", sandbox.path())
                .output()?;
            assert!(output.status.success());
            assert!(String::from_utf8_lossy(&output.stdout).contains(name));
        }
        let bad = Command::new(binary)
            .arg("--wrong")
            .env_clear()
            .env("HOME", sandbox.path())
            .output()?;
        assert!(!bad.status.success());
    }
    Ok(())
}
