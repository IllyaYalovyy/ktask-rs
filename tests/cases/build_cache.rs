//! Ownership, liveness, bounds and recovery of managed build caches, then the real binary
//! through the CLI and the terminal.
use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    build_cache::{self, Policy},
    instance::Paths,
};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, SystemTime},
};

const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");

struct Fixture {
    _sandbox: support::Sandbox,
    root: PathBuf,
    paths: Paths,
    project: PathBuf,
    target: PathBuf,
}

fn fixture() -> Result<Fixture> {
    let sandbox = support::sandbox()?;
    let root = sandbox.path().to_path_buf();
    let paths = Paths {
        config: root.join("config/ktask-dev"),
        state: root.join("state/ktask-dev"),
        cache: root.join("cache/ktask-dev"),
        runtime: root.join("runtime/ktask-dev"),
        executable: std::env::current_exe()?,
    };
    let project = root.join("project");
    let target = project.join("target");
    fs::create_dir_all(&project)?;
    Ok(Fixture {
        _sandbox: sandbox,
        root,
        paths,
        project,
        target,
    })
}

fn age(path: &Path, seconds: u64) -> Result {
    let when = SystemTime::now() - Duration::from_secs(seconds);
    // Read-only: a writable descriptor that a concurrently forked child inherits would make
    // executing this file fail with ETXTBSY.
    File::open(path)?.set_modified(when)?;
    Ok(())
}

fn cargo_skeleton(target: &Path) -> Result {
    fs::create_dir_all(target.join("debug/deps"))?;
    fs::create_dir_all(target.join("debug/.fingerprint"))?;
    fs::create_dir_all(target.join("debug/build"))?;
    fs::create_dir_all(target.join("debug/incremental"))?;
    fs::write(
        target.join("CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n",
    )?;
    fs::write(target.join("debug/.cargo-lock"), "")?;
    Ok(())
}

/// The compilation units the fixture writes for every version, as cargo check, cargo clippy,
/// cargo build and cargo test leave them for one package: each has its own hash, its own
/// fingerprint naming the unit, and its own incremental state.
struct FixtureUnit {
    /// Added to the version to give the unit's hash.
    offset: u64,
    /// Cargo's unit name in the fingerprint directory.
    unit: &'static str,
    /// rustc's crate name, which names the deps/ files and the incremental directory.
    crate_name: &'static str,
    /// Output files below deps/, `{h}` standing for the hash.
    outputs: &'static [&'static str],
    clippy: bool,
}

const TEST_BINARY: u64 = 0;
const LIBRARY: u64 = 0x1000;

const UNITS: [FixtureUnit; 4] = [
    FixtureUnit {
        offset: TEST_BINARY,
        unit: "test-integration-test-tui",
        crate_name: "tui",
        outputs: &["tui-{h}"],
        clippy: false,
    },
    FixtureUnit {
        offset: LIBRARY,
        unit: "lib-ktask",
        crate_name: "ktask",
        outputs: &["libktask-{h}.rlib", "libktask-{h}.rmeta"],
        clippy: false,
    },
    FixtureUnit {
        offset: 0x2000,
        unit: "lib-ktask",
        crate_name: "ktask",
        outputs: &["libktask-{h}.rmeta"],
        clippy: false,
    },
    FixtureUnit {
        offset: 0x3000,
        unit: "lib-ktask",
        crate_name: "ktask",
        outputs: &["libktask-{h}.rmeta"],
        clippy: true,
    },
];

fn unit_hash(version: u64, offset: u64) -> String {
    format!("{:016x}", version + offset)
}

/// One version's generations as cargo lays them out, all aged `age_secs`.
fn build(target: &Path, version: u64, age_secs: u64) -> Result {
    let debug = target.join("debug");
    for unit in &UNITS {
        let hash = unit_hash(version, unit.offset);
        let mut files: Vec<String> = unit
            .outputs
            .iter()
            .map(|o| format!("deps/{}", o.replace("{h}", &hash)))
            .collect();
        files.push(format!("deps/{}-{hash}.d", unit.crate_name));
        files.push(format!(
            ".fingerprint/ktask-rs-mk3-{hash}/{}.json",
            unit.unit
        ));
        files.push(format!(".fingerprint/ktask-rs-mk3-{hash}/{}", unit.unit));
        files.push(format!(
            "incremental/{}-{:013x}/s-1-abc/dep-graph.bin",
            unit.crate_name,
            version + unit.offset
        ));
        if unit.offset == LIBRARY {
            files.push(format!("build/ktask-rs-mk3-{hash}/output"));
        }
        for relative in &files {
            let path = debug.join(relative);
            fs::create_dir_all(path.parent().unwrap())?;
            let body = if relative.ends_with(".d") {
                let clippy = if unit.clippy {
                    "\n# env-dep:CLIPPY_ARGS=\n# env-dep:CLIPPY_CONF_DIR"
                } else {
                    ""
                };
                format!("{}: tests/tui.rs src/lib.rs{clippy}\n", path.display())
            } else {
                "x".repeat(8192)
            };
            fs::write(&path, body)?;
            age(&path, age_secs)?;
        }
    }
    Ok(())
}

/// Incremental directories one version leaves: one per unit.
const INCREMENTAL_PER_VERSION: usize = UNITS.len();

/// Every file below `dir`, relative to it.
fn relative_files(dir: &Path) -> std::collections::BTreeSet<PathBuf> {
    snapshot(dir)
        .into_iter()
        .map(|(path, _)| path.strip_prefix(dir).unwrap().to_path_buf())
        .collect()
}

fn registry_dependency(target: &Path) -> Result {
    let deps = target.join("debug/deps");
    let fingerprint = target.join("debug/.fingerprint/serde-00000000000000aa");
    fs::create_dir_all(&fingerprint)?;
    fs::write(fingerprint.join("lib-serde.json"), "{}")?;
    fs::write(
        deps.join("libserde-00000000000000aa.rlib"),
        "s".repeat(4096),
    )?;
    fs::write(
        deps.join("serde-00000000000000aa.d"),
        "/x/deps/serde-00000000000000aa.d: /home/u/.cargo/registry/src/serde/lib.rs\n",
    )?;
    for name in ["libserde-00000000000000aa.rlib", "serde-00000000000000aa.d"] {
        age(&deps.join(name), 90_000)?;
    }
    Ok(())
}

fn count(dir: &Path) -> usize {
    fs::read_dir(dir).map_or(0, |d| d.count())
}

fn tree_files(root: &Path) -> usize {
    let mut total = 0;
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            total += 1;
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    total
}

fn quick(paths: &Paths, keep: u64) -> Result<Policy> {
    let current = build_cache::policy(paths)?;
    Ok(build_cache::set_policy(
        paths,
        current,
        Policy {
            keep_generations: keep,
            grace_secs: 0,
            ..current
        },
    )?)
}

fn owned(f: &Fixture) -> Result {
    cargo_skeleton(&f.target)?;
    let claim = build_cache::adopt(&f.paths, &f.project, &f.target)?;
    assert!(!claim.already_owned);
    Ok(())
}

#[test]
fn repeated_version_builds_stay_bounded_and_the_newest_generation_stays_warm() -> Result {
    let f = fixture()?;
    owned(&f)?;
    let keep = 2;
    let versions = 40_u64;
    quick(&f.paths, keep)?;
    registry_dependency(&f.target)?;
    let mut bound = 0;
    for version in 1..=versions {
        build(&f.target, version, 40_000 - version * 100)?;
        if version % 4 == 0 {
            let done = build_cache::clean(&f.paths, true)?;
            assert!(done.reports.iter().all(|r| r.deferred.is_none()));
            bound = bound.max(tree_files(&f.target));
        }
    }
    let after = build_cache::clean(&f.paths, true)?;
    assert!(after.reports[0].after_files <= after.reports[0].before_files);
    let files = tree_files(&f.target);
    // The bound does not grow with versions: 40 generations are far above it.
    assert!(
        files <= bound,
        "{files} files after the final clean exceeded {bound}"
    );
    // What remains is exactly a target that only ever built the newest `keep` versions.
    let reference = f.root.join("reference");
    cargo_skeleton(&reference)?;
    registry_dependency(&reference)?;
    for version in versions + 1 - keep..=versions {
        build(&reference, version, 0)?;
    }
    assert_eq!(
        relative_files(&f.target.join("debug")),
        relative_files(&reference.join("debug"))
    );
    let deps = f.target.join("debug/deps");
    let test_binaries = fs::read_dir(&deps)?
        .flatten()
        .filter(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.starts_with("tui-") && !n.ends_with(".d")
        })
        .count();
    assert_eq!(
        test_binaries as u64, keep,
        "keeps exactly the newest generations"
    );
    assert!(
        deps.join(format!("tui-{:016x}", versions)).is_file(),
        "newest stays warm"
    );
    assert!(
        deps.join("libserde-00000000000000aa.rlib").is_file(),
        "unchanged registry dependencies stay warm"
    );
    assert_eq!(
        count(&f.target.join("debug/incremental")),
        keep as usize * INCREMENTAL_PER_VERSION
    );
    Ok(())
}

/// Every path below `dir` with its size, in a stable order.
fn snapshot(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut seen = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(dir).into_iter().flatten().flatten() {
            let meta = fs::symlink_metadata(entry.path()).unwrap();
            seen.push((entry.path(), meta.len()));
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    seen.sort();
    seen
}

/// Adopts a skeleton, replaces `link` (relative to the target) by a symbolic link to a
/// directory outside it holding old generations, cleans with keep=1 and proves that nothing
/// outside moved and the report names the link.
fn symlinked_below_root_is_never_followed(link: &str, outside_part: &str) -> Result {
    let f = fixture()?;
    owned(&f)?;
    quick(&f.paths, 1)?;
    let outside = f.root.join("elsewhere");
    for version in 1..=4_u64 {
        build(&outside, version, 40_000 - version * 100)?;
    }
    let link_path = f.target.join(link);
    fs::remove_dir_all(&link_path)?;
    std::os::unix::fs::symlink(outside.join(outside_part), &link_path)?;
    let before = snapshot(&outside);
    assert!(!before.is_empty());
    for apply in [false, true] {
        let done = build_cache::clean(&f.paths, apply)?;
        let report = &done.reports[0];
        assert!(report.deferred.is_none());
        let expected = format!(
            "{}: symbolic link; not managed",
            fs::canonicalize(&f.target)?.join(link).display()
        );
        assert!(
            report.skipped.contains(&expected),
            "{:?} lacks {expected}",
            report.skipped
        );
        assert_eq!(report.removed_generations, 0);
        assert_eq!(report.freed_bytes, 0);
        assert_eq!(
            snapshot(&outside),
            before,
            "outside changed (apply={apply})"
        );
    }
    assert!(fs::symlink_metadata(&link_path)?.file_type().is_symlink());
    Ok(())
}

#[test]
fn a_symlinked_incremental_directory_is_a_boundary_cleanup_never_crosses() -> Result {
    symlinked_below_root_is_never_followed("debug/incremental", "debug/incremental")
}

#[test]
fn a_symlinked_deps_directory_is_a_boundary_cleanup_never_crosses() -> Result {
    symlinked_below_root_is_never_followed("debug/deps", "debug/deps")
}

#[test]
fn symlinked_fingerprint_build_and_profile_directories_are_boundaries() -> Result {
    symlinked_below_root_is_never_followed("debug/.fingerprint", "debug/.fingerprint")?;
    symlinked_below_root_is_never_followed("debug/build", "debug/build")?;
    symlinked_below_root_is_never_followed("debug", "debug")
}

#[test]
fn a_symlinked_entry_inside_a_managed_directory_is_skipped_and_its_target_kept() -> Result {
    let f = fixture()?;
    owned(&f)?;
    quick(&f.paths, 1)?;
    let outside = f.root.join("elsewhere");
    fs::create_dir_all(outside.join("ktask-0000000000001/s-1-abc"))?;
    fs::write(
        outside.join("ktask-0000000000001/s-1-abc/dep-graph.bin"),
        "x",
    )?;
    for version in 1..=3_u64 {
        build(&f.target, version, 40_000 - version * 100)?;
    }
    let incremental = f.target.join("debug/incremental");
    let link = incremental.join("ktask-0000000000000");
    std::os::unix::fs::symlink(outside.join("ktask-0000000000001"), &link)?;
    let before = snapshot(&outside);
    let done = build_cache::clean(&f.paths, true)?;
    let report = &done.reports[0];
    assert!(
        report
            .skipped
            .iter()
            .any(|n| n.ends_with("ktask-0000000000000: symbolic link; not managed")),
        "{:?}",
        report.skipped
    );
    assert!(fs::symlink_metadata(&link)?.file_type().is_symlink());
    assert_eq!(snapshot(&outside), before);
    assert!(
        report.removed_generations > 0,
        "real generations still retire"
    );
    Ok(())
}

#[test]
fn capacity_limit_evicts_oldest_but_never_the_last_warm_generation() -> Result {
    let f = fixture()?;
    owned(&f)?;
    let current = quick(&f.paths, 32)?;
    let deps = f.target.join("debug/deps");
    let generations = 5_u64;
    let generation_bytes = 20 * build_cache::MIB;
    let limit = 64 * build_cache::MIB;
    // Registry-crate generations of 20 MiB each (sparse, but counted by length).
    for generation in 1..=generations {
        let name = format!("libbig-{generation:016x}.rlib");
        let file = File::create(deps.join(&name))?;
        file.set_len(generation_bytes)?;
        drop(file);
        let fingerprint = f
            .target
            .join(format!("debug/.fingerprint/big-{generation:016x}"));
        fs::create_dir_all(&fingerprint)?;
        fs::write(fingerprint.join("lib-big.json"), "{}")?;
        let dep_info = deps.join(format!("big-{generation:016x}.d"));
        fs::write(
            &dep_info,
            "/x/big.d: /home/u/.cargo/registry/src/big/lib.rs\n",
        )?;
        // Age every file of the generation: an unaged .d would carry the creation time, and
        // generations written within one filesystem timestamp tick would tie for newest.
        let aged = 90_000 - generation * 1_000;
        age(&deps.join(&name), aged)?;
        age(&dep_info, aged)?;
    }
    let limited = build_cache::set_policy(
        &f.paths,
        current,
        Policy {
            max_bytes: limit,
            ..current
        },
    )?;
    let preview = build_cache::clean(&f.paths, false)?;
    // The oldest generations whose removal brings the cache under its limit.
    let excess = preview.reports[0].before_bytes - limit;
    assert_eq!(
        preview.reports[0].removed_generations,
        excess.div_ceil(generation_bytes),
        "preview plans eviction"
    );
    assert!(
        deps.join(format!("libbig-{:016x}.rlib", 1)).is_file(),
        "preview deletes nothing"
    );
    let done = build_cache::clean(&f.paths, true)?;
    let report = &done.reports[0];
    assert!(
        !report.over_capacity && report.after_bytes <= limited.max_bytes,
        "{report:?}"
    );
    assert!(
        !deps.join(format!("libbig-{:016x}.rlib", 1)).exists(),
        "oldest evicted first"
    );
    assert!(
        deps.join(format!("libbig-{:016x}.rlib", 5)).is_file(),
        "newest stays warm"
    );
    // One warm generation larger than the whole budget cannot be bounded without wiping it:
    // it stays and the pressure is reported rather than hidden.
    for generation in 1..=4_u64 {
        let _ = fs::remove_file(deps.join(format!("libbig-{generation:016x}.rlib")));
    }
    build_cache::set_policy(
        &f.paths,
        limited,
        Policy {
            max_bytes: limit,
            max_files: 1_000,
            ..limited
        },
    )?;
    File::options()
        .write(true)
        .open(deps.join(format!("libbig-{:016x}.rlib", 5)))?
        .set_len(limit + generation_bytes)?;
    let done = build_cache::clean(&f.paths, true)?;
    assert!(done.reports[0].over_capacity);
    assert!(deps.join(format!("libbig-{:016x}.rlib", 5)).is_file());
    let inventory = build_cache::inventory(&f.paths, &f.project)?;
    assert!(
        inventory
            .pressure
            .iter()
            .any(|p| p.contains("exceeds its limit")),
        "{:?}",
        inventory.pressure
    );
    Ok(())
}

#[test]
fn unowned_caches_are_inventoried_and_never_cleaned() -> Result {
    let f = fixture()?;
    cargo_skeleton(&f.target)?;
    for version in 1..=6_u64 {
        build(&f.target, version, 50_000)?;
    }
    quick(&f.paths, 1)?;
    let before = tree_files(&f.target);
    let done = build_cache::clean(&f.paths, true)?;
    assert!(done.reports.is_empty(), "nothing is owned");
    assert_eq!(tree_files(&f.target), before);
    let inventory = build_cache::inventory(&f.paths, &f.project)?;
    let entry = inventory
        .unowned
        .iter()
        .find(|e| e.path == f.target)
        .expect("target listed");
    assert_eq!(entry.ownership, build_cache::Ownership::Unowned);
    assert!(entry.bytes > 0 && entry.files > 0);
    assert!(inventory.unowned_bytes > 0);
    assert!(inventory.pressure.iter().any(|p| p.contains("unowned")));
    // A forged marker without this instance's registration proves nothing.
    fs::write(f.target.join(".ktask-cache-owner"), r#"{"token":"forged"}"#)?;
    assert!(!build_cache::is_owned(&f.paths, &f.target));
    let forged = snapshot(&f.target);
    let error = build_cache::prune(&f.paths, &f.target, &build_cache::policy(&f.paths)?, true)
        .unwrap_err()
        .to_string();
    assert!(error.contains("not owned"), "{error}");
    assert_eq!(
        snapshot(&f.target),
        forged,
        "a refused prune changes nothing"
    );
    Ok(())
}

#[test]
fn copies_moves_and_symlinks_lose_ownership() -> Result {
    let f = fixture()?;
    owned(&f)?;
    assert!(build_cache::is_owned(&f.paths, &f.target));
    let copy = f.root.join("copy");
    let status = Command::new("cp")
        .arg("-r")
        .arg(&f.target)
        .arg(&copy)
        .status()?;
    assert!(status.success());
    assert!(
        !build_cache::is_owned(&f.paths, &copy),
        "a copy is not owned"
    );
    let moved = f.root.join("moved");
    fs::rename(&f.target, &moved)?;
    assert!(
        !build_cache::is_owned(&f.paths, &moved),
        "a moved tree is not owned"
    );
    assert!(build_cache::owned_roots(&f.paths).is_empty());
    let link = f.root.join("link");
    std::os::unix::fs::symlink(&moved, &link)?;
    let error = build_cache::adopt(&f.paths, &f.project, &link)
        .unwrap_err()
        .to_string();
    assert!(error.contains("symbolic link"), "{error}");
    Ok(())
}

#[test]
fn adopt_refuses_things_that_are_not_build_caches() -> Result {
    let f = fixture()?;
    let plain = f.root.join("plain");
    fs::create_dir_all(&plain)?;
    fs::write(plain.join("notes.txt"), "mine")?;
    for (path, why) in [
        (&plain, "not recognizably"),
        (&f.root, "contains the project"),
    ] {
        let error = build_cache::adopt(&f.paths, &f.project, path)
            .unwrap_err()
            .to_string();
        assert!(error.contains(why), "{path:?}: {error}");
    }
    let git = f.root.join("gitted");
    cargo_skeleton(&git)?;
    fs::create_dir_all(git.join(".git"))?;
    let error = build_cache::adopt(&f.paths, &f.project, &git)
        .unwrap_err()
        .to_string();
    assert!(error.contains("Git metadata"), "{error}");
    // The directory holding the running supervisor is never adoptable.
    let bin = f.root.join("bin-cache");
    cargo_skeleton(&bin)?;
    let mut paths = clone_paths(&f.paths);
    paths.executable = bin.join("debug/ktask");
    fs::write(&paths.executable, "x")?;
    let error = build_cache::adopt(&paths, &f.project, &bin)
        .unwrap_err()
        .to_string();
    assert!(error.contains("supervisor executable"), "{error}");
    assert_eq!(fs::read_to_string(plain.join("notes.txt"))?, "mine");
    Ok(())
}

fn clone_paths(p: &Paths) -> Paths {
    Paths {
        config: p.config.clone(),
        state: p.state.clone(),
        cache: p.cache.clone(),
        runtime: p.runtime.clone(),
        executable: p.executable.clone(),
    }
}

const HOLD_GENERATION: &str = "KTASK_TEST_HOLD_GENERATION";

// Runs only when the liveness test launches a copy of this test executable from deps/.
#[test]
fn running_generation_process() -> Result {
    if std::env::var_os(HOLD_GENERATION).is_none() {
        return Ok(());
    }
    println!("GENERATION_RUNNING");
    std::io::Write::flush(&mut std::io::stdout())?;
    let _ = std::io::Read::read(&mut std::io::stdin(), &mut [0]);
    Ok(())
}

/// A copy of this test binary running from a cache generation until it is finished.
struct RunningGeneration {
    child: std::process::Child,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

impl RunningGeneration {
    fn start(executable: &Path, home: &Path) -> Result<Self> {
        use std::io::BufRead;
        let mut child = Command::new(executable)
            .args([
                "--exact",
                "build_cache::running_generation_process",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", home)
            .env("TMPDIR", home)
            .env(HOLD_GENERATION, "1")
            .current_dir(home)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()?;
        let stdout = child.stdout.take().ok_or("child stdout")?;
        let mut running = Self {
            child,
            stdout: std::io::BufReader::new(stdout),
        };
        let mut line = String::new();
        while running.stdout.read_line(&mut line)? > 0 && line.trim_end() != "GENERATION_RUNNING" {
            line.clear();
        }
        assert_eq!(
            line.trim_end(),
            "GENERATION_RUNNING",
            "the copied test binary did not start"
        );
        Ok(running)
    }

    fn finish(mut self) -> Result {
        drop(self.child.stdin.take());
        std::io::copy(&mut self.stdout, &mut std::io::sink())?;
        let status = self.child.wait()?;
        assert!(status.success(), "{status}");
        Ok(())
    }
}

impl Drop for RunningGeneration {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn live_builds_open_files_running_programs_and_fresh_output_are_kept() -> Result {
    let f = fixture()?;
    owned(&f)?;
    quick(&f.paths, 1)?;
    for version in 1..=5_u64 {
        build(&f.target, version, 50_000 - version)?;
    }
    let deps = f.target.join("debug/deps");
    // An active cargo build holds the profile lock: nothing is removed.
    let lock = File::options()
        .write(true)
        .open(f.target.join("debug/.cargo-lock"))?;
    // Create the cleanup lock file first, so the tree size is stable while the build lock is held.
    build_cache::clean(&f.paths, false)?;
    lock.lock()?;
    let before = tree_files(&f.target);
    let done = build_cache::clean(&f.paths, true)?;
    assert!(
        done.reports[0]
            .deferred
            .as_deref()
            .is_some_and(|d| d.contains("build is active"))
    );
    assert_eq!(tree_files(&f.target), before);
    lock.unlock()?;
    // A file another process (here: this test) has open is not removed.
    let open_library = deps.join(format!("libktask-{:016x}.rlib", 2 + LIBRARY));
    let open = File::open(&open_library)?;
    // A running executable is not removed either: a copy of this test binary, so its
    // inode is its own, waits in deps/ until its stdin closes.
    let running = deps.join(format!("tui-{:016x}", 3));
    support::copy_executable(std::env::current_exe()?, &running)?;
    age(&running, 50_000 - 3)?;
    let child = RunningGeneration::start(&running, &f.root)?;
    let done = build_cache::clean(&f.paths, true)?;
    let in_use = [open_library, running.clone()];
    assert_eq!(done.reports[0].protected_generations, in_use.len() as u64);
    assert!(
        deps.join(format!("libktask-{:016x}.rlib", 2 + 0x1000))
            .is_file()
    );
    assert!(running.is_file());
    assert!(
        !deps
            .join(format!("libktask-{:016x}.rlib", 1 + 0x1000))
            .exists()
    );
    assert!(deps.join(format!("tui-{:016x}", 5)).is_file());
    child.finish()?;
    drop(open);
    build_cache::clean(&f.paths, true)?;
    assert!(
        !running.exists(),
        "released generations retire at the next safe point"
    );
    assert!(
        !deps
            .join(format!("libktask-{:016x}.rlib", 2 + 0x1000))
            .exists()
    );
    // Output newer than the grace window is active compiler output.
    let current = build_cache::policy(&f.paths)?;
    build_cache::set_policy(
        &f.paths,
        current,
        Policy {
            grace_secs: 3_600,
            ..current
        },
    )?;
    for version in 6..=9_u64 {
        build(&f.target, version, 5)?;
    }
    build_cache::clean(&f.paths, true)?;
    for version in 6..=9_u64 {
        assert!(
            deps.join(format!("tui-{version:016x}")).is_file(),
            "fresh output {version} is active compiler output"
        );
    }
    Ok(())
}

#[test]
fn final_outputs_are_never_removed_even_when_their_generation_is_old() -> Result {
    let f = fixture()?;
    owned(&f)?;
    quick(&f.paths, 1)?;
    for version in 1..=3_u64 {
        build(&f.target, version, 50_000 - version)?;
    }
    let deps = f.target.join("debug/deps");
    let old = deps.join(format!("tui-{:016x}", 1));
    fs::hard_link(&old, f.target.join("debug/tui"))?;
    build_cache::clean(&f.paths, true)?;
    assert!(old.is_file() && f.target.join("debug/tui").is_file());
    Ok(())
}

#[test]
fn concurrent_and_interrupted_cleanups_converge_without_losing_warm_files() -> Result {
    let f = fixture()?;
    owned(&f)?;
    let keep = 2;
    let versions = 12;
    quick(&f.paths, keep)?;
    for version in 1..=versions {
        build(&f.target, version, 60_000 - version)?;
    }
    // A cleanup killed mid-generation leaves a partial one behind.
    let deps = f.target.join("debug/deps");
    // Cleanup removes the fingerprint first: Cargo rebuilds a unit without one, so the
    // outputs it leaves are a remnant.
    fs::remove_dir_all(f.target.join(format!(
        "debug/.fingerprint/ktask-rs-mk3-{}",
        unit_hash(5, TEST_BINARY)
    )))?;
    // Partial generations without outputs are remnants too.
    fs::remove_file(deps.join(format!("tui-{:016x}", 3)))?;
    fs::remove_file(deps.join(format!("libktask-{:016x}.rlib", 4 + 0x1000)))?;
    fs::remove_file(deps.join(format!("libktask-{:016x}.rmeta", 4 + 0x1000)))?;
    fs::write(f.target.join(".ktask-cache.lock"), "")?;
    let results: Vec<_> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..4)
            .map(|_| scope.spawn(|| build_cache::clean(&f.paths, true).map_err(|e| e.to_string())))
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    for result in &results {
        let done = result.as_ref().map_err(|e| e.clone())?;
        assert_eq!(done.reports.len(), 1);
    }
    build_cache::clean(&f.paths, true)?;
    let leftovers: Vec<_> = fs::read_dir(&deps)?
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| {
            (versions + 1 - keep..=versions).all(|kept| {
                UNITS
                    .iter()
                    .all(|unit| !n.contains(&unit_hash(kept, unit.offset)))
            })
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "stale generations remain: {leftovers:?}"
    );
    assert!(deps.join(format!("tui-{:016x}", versions)).is_file());
    Ok(())
}

#[test]
fn abandoned_scratch_is_reclaimed_but_a_live_lease_is_kept() -> Result {
    let f = fixture()?;
    let workers = f.paths.cache.join("workers");
    let live = ktask_rs_mk3::scratch::Scratch::new(&workers)?;
    fs::write(live.path().join("keep"), "live")?;
    let dead = workers.join("run-dead000");
    fs::create_dir_all(&dead)?;
    fs::write(dead.join(".lease"), "")?;
    fs::write(dead.join("junk"), "x".repeat(4096))?;
    let done = build_cache::clean(&f.paths, true)?;
    assert_eq!(done.scratch_reclaimed, 1);
    assert!(!dead.exists());
    assert!(live.path().join("keep").is_file());
    Ok(())
}

#[test]
fn a_fresh_ignored_target_is_claimed_but_never_a_populated_or_tracked_one() -> Result {
    let f = fixture()?;
    support::empty_repository(&f.project)?;
    fs::write(f.project.join(".git/info/exclude"), "")?;
    fs::write(f.project.join("Cargo.toml"), "[package]\n")?;
    assert!(
        build_cache::claim_fresh(&f.paths, &f.project)?.is_none(),
        "not ignored"
    );
    assert!(!f.target.exists());
    fs::write(f.project.join(".git/info/exclude"), "/target/\n")?;
    let claim = build_cache::claim_fresh(&f.paths, &f.project)?.expect("claimed");
    assert!(build_cache::is_owned(&f.paths, &claim.root));
    fs::remove_dir_all(&f.target)?;
    fs::create_dir_all(&f.target)?;
    fs::write(f.target.join("foreign"), "x")?;
    assert!(
        build_cache::claim_fresh(&f.paths, &f.project)?.is_none(),
        "populated"
    );
    assert!(!build_cache::is_owned(&f.paths, &f.target));
    Ok(())
}

/// Cargo names a library `lib<crate>-<h>.rlib` but its dependency-info `<crate>-<h>.d`, so a
/// crate called `libc` has `liblibc-<h>.rlib` beside `libc-<h>.d`. They are one generation.
fn lib_prefixed(target: &Path, version: u64, age_secs: u64, sources: &str) -> Result {
    let debug = target.join("debug");
    let hash = format!("{version:016x}");
    for relative in [
        format!("deps/liblibc-{hash}.rlib"),
        format!("deps/liblibc-{hash}.rmeta"),
        format!("deps/libc-{hash}.d"),
        format!(".fingerprint/libc-{hash}/lib-libc.json"),
    ] {
        let path = debug.join(&relative);
        fs::create_dir_all(path.parent().unwrap())?;
        let body = if relative.ends_with(".d") {
            format!("{}: {sources}\n", path.display())
        } else {
            "x".repeat(4096)
        };
        fs::write(&path, body)?;
        age(&path, age_secs)?;
    }
    Ok(())
}

fn lib_prefixed_survivors(target: &Path, version: u64) -> Vec<bool> {
    let debug = target.join("debug");
    let hash = format!("{version:016x}");
    vec![
        debug.join(format!("deps/liblibc-{hash}.rlib")).is_file(),
        debug.join(format!("deps/liblibc-{hash}.rmeta")).is_file(),
        debug.join(format!("deps/libc-{hash}.d")).is_file(),
        debug.join(format!(".fingerprint/libc-{hash}")).is_dir(),
    ]
}

#[test]
fn crates_named_with_a_lib_prefix_keep_their_dependency_info_and_fingerprint() -> Result {
    for local in [true, false] {
        let f = fixture()?;
        owned(&f)?;
        quick(&f.paths, 1)?;
        let sources = if local {
            "src/lib.rs"
        } else {
            "/home/u/.cargo/registry/src/libc/src/lib.rs"
        };
        for version in 1..=4_u64 {
            lib_prefixed(&f.target, version, 50_000 - version, sources)?;
        }
        let done = build_cache::clean(&f.paths, true)?;
        assert!(done.reports[0].deferred.is_none());
        assert_eq!(
            lib_prefixed_survivors(&f.target, 4),
            [true; 4],
            "the current generation stays whole (local={local})"
        );
        for version in 1..=3_u64 {
            let kept = lib_prefixed_survivors(&f.target, version);
            if local {
                assert_eq!(kept, [false; 4], "obsolete generation {version} is retired");
            } else {
                assert_eq!(
                    kept, [true; 4],
                    "registry crates are not evicted without pressure"
                );
            }
        }
    }
    Ok(())
}

/// Fingerprint directories of every unit in a Cargo profile.
fn fingerprint_dirs(profile: &Path) -> usize {
    count(&profile.join(".fingerprint"))
}

/// Cargo's real layout, not a fixture's: after versions pile up under every command a
/// worker runs (build, check, clippy, tests) and cleanup keeps one generation, the next pass
/// of the same commands compiles and checks nothing.
#[test]
fn real_cargo_stays_warm_after_cleanup_of_lib_prefixed_crates() -> Result {
    let f = fixture()?;
    let workspace = f.root.join("ws");
    let target = f.root.join("cargo-target");
    for dir in ["libdemo/src", "libdemo/tests", "app/src"] {
        fs::create_dir_all(workspace.join(dir))?;
    }
    fs::write(
        workspace.join("Cargo.toml"),
        "[workspace]\nmembers = [\"libdemo\", \"app\"]\nresolver = \"2\"\n",
    )?;
    fs::write(
        workspace.join("libdemo/src/lib.rs"),
        "pub fn f() -> u32 { 1 }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn one() { assert_eq!(super::f(), 1); }\n}\n",
    )?;
    fs::write(
        workspace.join("libdemo/tests/api.rs"),
        "#[test]\nfn api() { assert_eq!(libdemo::f(), 1); }\n",
    )?;
    fs::write(
        workspace.join("app/src/main.rs"),
        "fn main() { println!(\"{}\", libdemo::f()); }\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn runs() { assert_eq!(libdemo::f(), 1); }\n}\n",
    )?;
    let manifest = |version: u64| -> Result {
        fs::write(
            workspace.join("libdemo/Cargo.toml"),
            format!(
                "[package]\nname = \"libdemo\"\nversion = \"0.1.{version}\"\nedition = \"2021\"\n"
            ),
        )?;
        fs::write(
            workspace.join("app/Cargo.toml"),
            format!(
                "[package]\nname = \"app\"\nversion = \"0.1.{version}\"\nedition = \"2021\"\n[dependencies]\nlibdemo = {{ path = \"../libdemo\" }}\n"
            ),
        )?;
        Ok(())
    };
    let cargo = |args: &[&str]| -> Result<String> {
        let output = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args(args)
            .arg("--offline")
            .arg("--manifest-path")
            .arg(workspace.join("Cargo.toml"))
            .env("CARGO_TARGET_DIR", &target)
            .env("CARGO_HOME", f.root.join("cargo-home"))
            .env("CARGO_INCREMENTAL", "1")
            .env("TMPDIR", &f.root)
            .output()?;
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(output.status.success(), "cargo {args:?}: {stderr}");
        Ok(format!("--- cargo {}\n{stderr}", args.join(" ")))
    };
    // Every command a worker and the supervisor checks run, each a distinct set of units.
    let pass = || -> Result<String> {
        [
            &["build", "--workspace"][..],
            &["check", "--workspace"],
            &["check", "--workspace", "--all-targets"],
            &["clippy", "--workspace", "--all-targets"],
            &["test", "--workspace", "--no-run"],
        ]
        .iter()
        .map(|args| cargo(args))
        .collect()
    };
    let versions = 3;
    let keep = 1;
    let mut units_per_version = 0;
    for version in 1..=versions {
        manifest(version)?;
        pass()?;
        if version == 1 {
            units_per_version = fingerprint_dirs(&target.join("debug"));
        }
    }
    assert_eq!(
        fingerprint_dirs(&target.join("debug")),
        units_per_version * versions as usize,
        "every version adds its own units"
    );
    build_cache::adopt(&f.paths, &workspace, &target)?;
    quick(&f.paths, keep)?;
    let done = build_cache::clean(&f.paths, true)?;
    let report = &done.reports[0];
    assert!(
        report.deferred.is_none() && report.freed_bytes > 0,
        "{report:?}"
    );
    assert_eq!(
        fingerprint_dirs(&target.join("debug")),
        units_per_version * keep as usize,
        "exactly the newest generation of every unit remains"
    );
    let deps = target.join("debug/deps");
    let libraries = fs::read_dir(&deps)?
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().starts_with("liblibdemo-")
                && e.file_name().to_string_lossy().ends_with(".rlib")
        })
        .count();
    assert_eq!(libraries, 1, "only the current library generation remains");
    let transcript = pass()?;
    if let Some(dir) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        let _ = fs::write(
            Path::new(&dir).join("real-cargo-warm-after-clean.txt"),
            format!("{report:#?}\n{transcript}"),
        );
    }
    assert!(
        !transcript.contains("Compiling") && !transcript.contains("Checking"),
        "{transcript}"
    );
    let second = build_cache::clean(&f.paths, true)?;
    assert_eq!(second.reports[0].freed_bytes, 0, "cleanup is idempotent");
    Ok(())
}

#[test]
fn runner_safe_point_claims_a_fresh_target_and_retires_obsolete_generations() -> Result {
    let f = fixture()?;
    support::empty_repository(&f.project)?;
    fs::write(f.project.join(".git/info/exclude"), "/target/\n")?;
    fs::write(f.project.join("Cargo.toml"), "[package]\n")?;
    assert!(build_cache::maintain(&f.paths, &f.project).is_some());
    assert!(
        build_cache::is_owned(&f.paths, &f.target),
        "claimed on first use"
    );
    cargo_skeleton(&f.target)?;
    quick(&f.paths, 2)?;
    for version in 1..=8_u64 {
        build(&f.target, version, 50_000 - version)?;
    }
    let done = build_cache::maintain(&f.paths, &f.project).expect("maintained");
    assert!(done.reports[0].freed_bytes > 0);
    let deps = f.target.join("debug/deps");
    assert!(deps.join(format!("tui-{:016x}", 8)).is_file());
    assert!(!deps.join(format!("tui-{:016x}", 1)).exists());
    Ok(())
}

#[test]
fn policy_is_finite_validated_and_version_guarded() -> Result {
    let f = fixture()?;
    let first = build_cache::policy(&f.paths)?;
    assert_eq!(first.version, 0);
    assert!(first.max_bytes > 0 && first.max_files > 0);
    let saved = build_cache::set_policy(
        &f.paths,
        first,
        Policy {
            max_bytes: 5 * build_cache::GIB,
            ..first
        },
    )?;
    assert_eq!((saved.version, saved.max_bytes), (1, 5 * build_cache::GIB));
    let stale = build_cache::set_policy(&f.paths, first, first)
        .unwrap_err()
        .to_string();
    assert!(stale.contains("changed"), "{stale}");
    for bad in [
        Policy {
            max_bytes: 1,
            ..saved
        },
        Policy {
            keep_generations: 0,
            ..saved
        },
        Policy {
            max_files: 0,
            ..saved
        },
    ] {
        assert!(build_cache::set_policy(&f.paths, saved, bad).is_err());
    }
    assert_eq!(build_cache::policy(&f.paths)?, saved);
    Ok(())
}

fn cli(f: &Fixture, args: &[&str]) -> Result<serde_json::Value> {
    let output = Command::new(DEV)
        .env_clear()
        .env("HOME", &f.root)
        .env("XDG_STATE_HOME", f.root.join("state"))
        .env("XDG_CONFIG_HOME", f.root.join("config"))
        .env("XDG_CACHE_HOME", f.root.join("cache"))
        .env("XDG_DATA_HOME", f.root.join("share"))
        .env("XDG_RUNTIME_DIR", f.root.join("runtime"))
        .env("TMPDIR", &f.root)
        .current_dir(&f.project)
        .arg("--json")
        .arg("cache")
        .args(args)
        .output()?;
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(value["ok"], true);
    Ok(value["data"].clone())
}

#[test]
fn cli_inventories_adopts_cleans_and_separates_provider_storage() -> Result {
    let f = fixture()?;
    cargo_skeleton(&f.target)?;
    let versions = 6;
    for version in 1..=versions {
        build(&f.target, version, 50_000 - version)?;
    }
    let codex = f.root.join(".codex/sessions");
    fs::create_dir_all(&codex)?;
    fs::write(codex.join("resume-me.jsonl"), "session ".repeat(1000))?;
    fs::write(f.root.join(".codex/auth.json"), "secret")?;
    let data = cli(&f, &[])?;
    let unowned = data["cache"]["unowned"].as_array().unwrap();
    assert!(unowned.iter().any(|e| e["ownership"] == "unowned" && e["path"].as_str().unwrap().ends_with("target")));
    let provider = data["cache"]["provider"].as_array().unwrap();
    assert!(
        provider.iter().any(
            |e| e["path"].as_str().unwrap().ends_with(".codex") && e["ownership"] == "excluded"
        )
    );
    assert!(
        data["cache"]["exclusions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e.as_str().unwrap().contains("never cleaned"))
    );
    // Nothing is cleaned before ownership.
    let before = tree_files(&f.target);
    let clean = cli(&f, &["clean", "--apply"])?;
    assert_eq!(clean["caches"].as_array().unwrap().len(), 0);
    assert_eq!(tree_files(&f.target), before);
    let keep = 1;
    let limits = cli(
        &f,
        &[
            "limits",
            "--keep-generations",
            &keep.to_string(),
            "--grace-secs",
            "0",
            "--if-version",
            "0",
        ],
    )?;
    assert_eq!(limits["limits"]["version"], 1);
    let adopted = cli(&f, &["adopt"])?;
    assert_eq!(adopted["already_owned"], false);
    let managed = snapshot(&f.target.join("debug"));
    let preview = cli(&f, &["clean"])?;
    assert_eq!(preview["applied"], false);
    assert_eq!(
        snapshot(&f.target.join("debug")),
        managed,
        "preview deletes nothing"
    );
    let applied = cli(&f, &["clean", "--apply"])?;
    let report = &applied["caches"][0];
    assert!(report["freed_bytes"].as_u64().unwrap() > 0);
    assert!(report["after_bytes"].as_u64().unwrap() < report["before_bytes"].as_u64().unwrap());
    assert_eq!(
        count(&f.target.join("debug/incremental")),
        keep * INCREMENTAL_PER_VERSION
    );
    assert!(
        f.target
            .join("debug/deps")
            .join(format!("tui-{:016x}", versions))
            .is_file()
    );
    // Provider sessions and credentials were never touched.
    assert!(codex.join("resume-me.jsonl").is_file());
    assert_eq!(
        fs::read_to_string(f.root.join(".codex/auth.json"))?,
        "secret"
    );
    let stale = Command::new(DEV)
        .env_clear()
        .env("HOME", &f.root)
        .env("XDG_CONFIG_HOME", f.root.join("config"))
        .env("XDG_CACHE_HOME", f.root.join("cache"))
        .current_dir(&f.project)
        .args([
            "cache",
            "limits",
            "--max-files",
            "5000",
            "--if-version",
            "0",
        ])
        .output()?;
    assert_eq!(stale.status.code(), Some(4));
    Ok(())
}

fn screen(t: &Terminal, label: &str) -> Result<String> {
    let rendered = t.screen(label, |s| s.contains(label))?;
    println!("{label}\n{rendered}");
    Ok(rendered)
}

#[test]
fn build_cache_flows_work_from_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let f = fixture()?;
        let target = f.root.join("target");
        cargo_skeleton(&target)?;
        for version in 1..=6_u64 {
            build(&target, version, 50_000 - version)?;
        }
        let mut t = Terminal::open_real(DEV, &f.root, 100, 32)?;
        screen(&t, "No current task")?;
        let go = |t: &mut Terminal, key: &str, label: &str| -> Result {
            if mouse { t.click(label) } else { t.send(key) }
        };
        go(&mut t, "4", "4 Config")?;
        screen(&t, "Storage: SQLite")?;
        go(&mut t, "b", "b Build cache")?;
        let opened = screen(&t, "Not managed (opt in to clean)")?;
        assert!(opened.contains("target") && opened.contains("Maximum bytes per cache"));
        assert!(opened.contains("Never touched"));
        // Choose Adopt, confirm.
        if mouse {
            t.click("Save limits")?;
            screen(&t, "Clean obsolete")?;
            t.click("Clean obsolete")?;
        } else {
            t.send("\t\t\t  ")?;
        }
        screen(&t, "Adopt project target")?;
        go(&mut t, "\x13", "Apply")?;
        screen(&t, "Adopt build cache?")?;
        go(&mut t, "\x13", "Confirm")?;
        screen(&t, "Build cache owned")?;
        assert!(build_cache::is_owned(&f.paths, &target));
        // Reopen and clean: preview, then confirm.
        go(&mut t, "b", "b Build cache")?;
        screen(&t, "Owned:")?;
        // Keep a single warm generation through the form itself.
        if mouse {
            t.click("Warm generations kept")?;
        } else {
            t.send("\t\t")?;
        }
        t.send("\x011")?;
        go(&mut t, "\x13", "Apply")?;
        screen(&t, "Build cache limits saved")?;
        assert_eq!(build_cache::policy(&f.paths)?.keep_generations, 1);
        go(&mut t, "b", "b Build cache")?;
        // Saving closed the form, so its fields are back only once it reopens; the footer
        // keeps "limits saved" visible, or hidden behind a tall form, depending on paths.
        t.screen("reopened", |s| {
            s.contains("Owned:") && s.contains("Warm generations kept")
        })?;
        if mouse {
            t.click("Save limits")?;
        } else {
            t.send("\t\t\t ")?;
        }
        screen(&t, "Clean obsolete")?;
        go(&mut t, "\x13", "Apply")?;
        let preview = screen(&t, "Clean build caches?")?;
        assert!(preview.contains("generation(s)"));
        go(&mut t, "\x13", "Confirm")?;
        screen(&t, "Build cache cleaned")?;
        assert!(
            target
                .join("debug/deps")
                .join(format!("tui-{:016x}", 6))
                .is_file()
        );
        assert!(
            !target
                .join("debug/deps")
                .join(format!("tui-{:016x}", 1))
                .exists()
        );
        t.send("\x03")?;
        t.finish()?;
    }
    Ok(())
}
