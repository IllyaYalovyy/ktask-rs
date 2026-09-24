//! The release build (T159): what is shipped is what is proven.
//!
//! [`release::profile`] reads the workspace `Cargo.toml` and checks the
//! release profile is the one the install path documents: thin LTO and
//! stripped symbols, which is what makes the binary a single small file that
//! runs on a machine with no Rust toolchain.
//!
//! [`release::artifact`] builds that profile for real, and then drives the
//! resulting `release/ktask-rs` — not the debug binary the other suites use —
//! through a complete dummy-provider queue in a scratch project: init, add,
//! run, status. A release build that linked differently from the debug one
//! (or lost a bundled dependency to stripping) fails here.

mod release {
    mod profile {
        const MANIFEST: &str = include_str!("../../../Cargo.toml");

        /// The `key = value` lines of the `[profile.release]` table, with
        /// comments and blank lines dropped.
        fn release_table() -> Vec<(String, String)> {
            let mut in_table = false;
            let mut entries = Vec::new();
            for line in MANIFEST.lines().map(str::trim) {
                if line.starts_with('[') {
                    in_table = line == "[profile.release]";
                    continue;
                }
                if !in_table || line.is_empty() || line.starts_with('#') {
                    continue;
                }
                if let Some((key, value)) = line.split_once('=') {
                    let value = value.split('#').next().unwrap_or_default();
                    entries.push((key.trim().to_string(), value.trim().to_string()));
                }
            }
            entries
        }

        fn setting(key: &str) -> Option<String> {
            release_table()
                .into_iter()
                .find_map(|(name, value)| (name == key).then_some(value))
        }

        #[test]
        fn release_profile_uses_thin_lto() {
            assert_eq!(setting("lto").as_deref(), Some("\"thin\""));
        }

        #[test]
        fn release_profile_strips_the_binary() {
            assert_eq!(setting("strip").as_deref(), Some("true"));
        }

        /// The reader above must find what it is looking for, and only in
        /// the right table: a setting in another profile does not count.
        #[test]
        fn release_table_is_the_release_profile_and_nothing_else() {
            let table = release_table();
            assert!(
                !table.is_empty(),
                "no [profile.release] table in Cargo.toml"
            );
            assert!(
                table.iter().all(|(key, _)| !key.starts_with('[')),
                "the reader leaked into another table: {table:?}"
            );
        }
    }

    mod artifact {
        use std::io;
        use std::path::{Path, PathBuf};
        use std::process::{Command, Output};

        use ktask_core::testing::scratch_repo;

        /// A task the dummy scenario below completes.
        const PLAN: &str = "\
## Add a greeting

**Outcome:** the repository has a greeting file.

**Done-when:** `greeting.txt` exists.

**Verify:** `true`

**Refs:** none

## Say goodbye

**Outcome:** the repository says goodbye.

**Done-when:** goodbye exists.

**Verify:** `true`

**Refs:** none
";

        /// The target directory the test binary was built into: the parent of
        /// its `debug` (or `release`) directory.
        fn target_dir() -> io::Result<PathBuf> {
            Path::new(env!("CARGO_BIN_EXE_ktask-rs"))
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .ok_or_else(|| io::Error::other("the test binary is not inside a target dir"))
        }

        /// Builds the release profile and returns the path of the binary it
        /// produced.
        ///
        /// Coverage and mutation runs put instrumentation in the environment;
        /// the shipped artifact is built without it.
        fn build_release() -> io::Result<PathBuf> {
            let target = target_dir()?;
            let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
            let output = Command::new(cargo)
                .args(["build", "--release", "--locked", "-p", "ktask-cli"])
                .arg("--target-dir")
                .arg(&target)
                .env_remove("RUSTFLAGS")
                .env_remove("CARGO_ENCODED_RUSTFLAGS")
                .env_remove("LLVM_PROFILE_FILE")
                .output()?;
            if !output.status.success() {
                return Err(io::Error::other(format!(
                    "release build failed:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            let binary = target.join("release").join("ktask-rs");
            if binary.is_file() {
                Ok(binary)
            } else {
                Err(io::Error::other(format!(
                    "{} was not produced",
                    binary.display()
                )))
            }
        }

        /// Runs the release binary in `project` with its state and config
        /// homes redirected into scratch directories.
        fn ktask(binary: &Path, project: &Path, homes: &Path, args: &[&str]) -> io::Result<Output> {
            Command::new(binary)
                .args(args)
                .current_dir(project)
                .env("XDG_STATE_HOME", homes.join("state"))
                .env("XDG_CONFIG_HOME", homes.join("config"))
                .output()
        }

        fn stdout_of(output: &Output) -> String {
            String::from_utf8_lossy(&output.stdout).into_owned()
        }

        fn describe(output: &Output) -> String {
            format!(
                "exit {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                stdout_of(output),
                String::from_utf8_lossy(&output.stderr)
            )
        }

        /// The two dummy steps a successful first attempt at `task` consumes:
        /// the provider probe, then the attempt, which writes its report.
        fn steps_for(state: &Path, task: u32) -> String {
            let report = state
                .join("attempts")
                .join(task.to_string())
                .join("1")
                .join("report.md");
            format!(
                "[[steps]]\noutcome = \"success\"\n\n\
                 [[steps]]\noutcome = \"success\"\nstdout = \"worked on {task}\\n\"\n\n\
                 [[steps.files]]\npath = \"{}\"\n\
                 content = \"KTASK_RESULT: DONE\\nSummary: task {task}\\n\"\n\n",
                report.display()
            )
        }

        #[test]
        fn release_binary_drains_a_dummy_provider_queue() {
            let binary = build_release().expect("build the release binary");

            let version = Command::new(&binary)
                .arg("--version")
                .output()
                .expect("run --version");
            assert!(version.status.success(), "{}", describe(&version));
            assert!(
                stdout_of(&version).starts_with("ktask-rs "),
                "{}",
                describe(&version)
            );

            // `strip = true` in the profile only counts if the linker obeyed
            // it: a stripped ELF has no symbol table section.
            #[cfg(target_os = "linux")]
            {
                let bytes = std::fs::read(&binary).expect("read the release binary");
                assert!(
                    !bytes.windows(b".symtab".len()).any(|w| w == b".symtab"),
                    "the release binary still has a symbol table"
                );
            }

            let repo = scratch_repo().expect("scratch repository");
            let homes = tempfile::tempdir().expect("scratch homes");

            let init = ktask(&binary, &repo.path, homes.path(), &["init"])
                .expect("run the release binary");
            assert!(init.status.success(), "{}", describe(&init));
            let state = PathBuf::from(
                stdout_of(&init)
                    .lines()
                    .find_map(|line| line.strip_prefix("state: "))
                    .expect("init reports the state directory"),
            );

            let scenario = state.join("scenario.toml");
            let steps: String = (1..=2).map(|task| steps_for(&state, task)).collect();
            std::fs::write(&scenario, steps).expect("write scenario");
            std::fs::write(
                state.join("config.toml"),
                format!(
                    "provider = \"dummy\"\ndummy_scenario_path = \"{}\"\n\
                     verify_command = [\"true\"]\n",
                    scenario.display()
                ),
            )
            .expect("write config");
            let plan = state.join("plan.md");
            std::fs::write(&plan, PLAN).expect("write plan");

            let add = ktask(
                &binary,
                &repo.path,
                homes.path(),
                &["add", "--file", &plan.to_string_lossy()],
            )
            .expect("run the release binary");
            assert!(add.status.success(), "{}", describe(&add));

            let run =
                ktask(&binary, &repo.path, homes.path(), &["run"]).expect("run the release binary");
            assert_eq!(run.status.code(), Some(0), "{}", describe(&run));
            let results: Vec<String> = stdout_of(&run).lines().map(str::to_string).collect();
            assert_eq!(results.len(), 2, "one result line per task: {results:?}");
            assert!(results[0].starts_with("task 1: done"), "{results:?}");
            assert!(results[1].starts_with("task 2: done"), "{results:?}");

            let status = ktask(&binary, &repo.path, homes.path(), &["status", "--json"])
                .expect("run the release binary");
            assert!(status.status.success(), "{}", describe(&status));
            let status: serde_json::Value =
                serde_json::from_str(stdout_of(&status).trim()).expect("status --json is JSON");
            assert_eq!(status["summary"], serde_json::json!({"Done": 2}));
        }
    }
}
