//! The dependency rule of docs/ARCHITECTURE.md, checked on the crate manifests.

use std::path::Path;

/// The workspace crates that `crate_name` depends on, from its `Cargo.toml`.
fn workspace_dependencies(crate_name: &str) -> std::io::Result<Vec<String>> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_name)
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)?;
    let mut names: Vec<String> = text
        .lines()
        .filter(|line| line.starts_with("ktask-"))
        .filter_map(|line| line.split_once(" = "))
        .map(|(name, _)| name.to_owned())
        .collect();
    names.sort();
    Ok(names)
}

#[test]
fn core_depends_on_nothing_in_the_workspace() -> std::io::Result<()> {
    assert_eq!(workspace_dependencies("ktask-core")?, Vec::<String>::new());
    Ok(())
}

#[test]
fn adapters_and_tui_depend_only_on_core() -> std::io::Result<()> {
    assert_eq!(workspace_dependencies("ktask-adapters")?, ["ktask-core"]);
    assert_eq!(workspace_dependencies("ktask-tui")?, ["ktask-core"]);
    Ok(())
}

#[test]
fn the_binary_crate_reaches_every_other_crate() -> std::io::Result<()> {
    assert_eq!(
        workspace_dependencies("ktask-cli")?,
        ["ktask-adapters", "ktask-core", "ktask-tui"]
    );
    Ok(())
}

/// A provider is a value `run` is handed; `core` decides nothing about which providers exist,
/// so its production code must not name or know `echo` — the one built-in provider, defined
/// in `ktask-adapters`. Test code is exempt: it is free to use `"echo"` as an arbitrary
/// sample provider name, same as any other string.
#[test]
fn core_does_not_name_or_know_the_echo_provider() -> std::io::Result<()> {
    let source_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ktask-core")
        .join("src");
    for entry in std::fs::read_dir(&source_dir)? {
        let path = entry?.path();
        if path.extension().and_then(std::ffi::OsStr::to_str) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path)?;
        let production_code = text.split("#[cfg(test)]").next().unwrap_or(&text);
        assert!(
            !production_code.to_lowercase().contains("echo"),
            "{} names or knows the echo provider outside its tests",
            path.display()
        );
    }
    Ok(())
}

/// Every `.rs` file under `dir`, recursively, with its text.
fn rust_sources(dir: &Path, found: &mut Vec<(std::path::PathBuf, String)>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            rust_sources(&path, found)?;
        } else if path.extension().and_then(std::ffi::OsStr::to_str) == Some("rs") {
            let text = std::fs::read_to_string(&path)?;
            found.push((path, text));
        }
    }
    Ok(())
}

/// Starting a run is one call: `run_queue` takes the ports and one `RunRequest` carrying every
/// choice the CLI makes. A further choice is a field of that struct, never another entry point.
#[test]
fn starting_a_run_is_the_one_run_queue_taking_a_run_request() -> std::io::Result<()> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut sources = Vec::new();
    for name in ["ktask-core", "ktask-adapters", "ktask-cli", "ktask-tui"] {
        rust_sources(&crates.join(name).join("src"), &mut sources)?;
    }
    let entry_points: Vec<_> = sources
        .iter()
        .filter(|(_, text)| text.contains("fn run_queue"))
        .collect();
    assert_eq!(
        entry_points.len(),
        1,
        "{:?}",
        entry_points
            .iter()
            .map(|(path, _)| path)
            .collect::<Vec<_>>()
    );
    let (_, text) = entry_points[0];
    assert_eq!(text.matches("fn run_queue").count(), 1);
    assert!(text.contains("request: RunRequest<'_>"), "{text}");
    for (path, text) in &sources {
        assert!(
            !text.contains("run_queue_with"),
            "{} names a longer form of run_queue",
            path.display()
        );
    }
    Ok(())
}

/// The state and config roots are built from the channel in one place: only `state.rs` of
/// `ktask-adapters` names the directory a channel keeps its world in, so no other code can
/// build a path that ignores the channel.
#[test]
fn the_directories_of_the_channels_are_named_in_one_place() -> std::io::Result<()> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut sources = Vec::new();
    for name in ["ktask-core", "ktask-adapters", "ktask-cli", "ktask-tui"] {
        rust_sources(&crates.join(name).join("src"), &mut sources)?;
    }
    let owner = crates.join("ktask-adapters/src/state.rs");
    let mut owners = 0;
    for (path, text) in &sources {
        let production_code = text.split("#[cfg(test)]").next().unwrap_or(text);
        let names_a_channel_directory = production_code.contains("ktask-rs-dev")
            || production_code.contains("join(\"ktask-rs\")");
        if path == &owner {
            assert!(names_a_channel_directory, "{} names none", path.display());
            assert!(production_code.contains("ktask-rs-dev"));
            assert!(production_code.contains("=> \"ktask-rs\""));
            owners += 1;
        } else {
            assert!(
                !names_a_channel_directory,
                "{} names a channel's directory; only state.rs of ktask-adapters may",
                path.display()
            );
        }
    }
    assert_eq!(owners, 1, "state.rs of ktask-adapters was not found");
    let state = std::fs::read_to_string(&owner)?;
    let production_code = state.split("#[cfg(test)]").next().unwrap_or(&state);
    for root in ["state_directory(", "config_root_path("] {
        assert!(production_code.contains(root), "{root}");
    }
    assert_eq!(
        production_code.matches("directory_name(channel)").count(),
        2
    );
    Ok(())
}

/// Every failure is routed by one rule table, in `route/` of `ktask-core`: no step module and no
/// part of the attempt loop decides wait, retry, decide or stop for itself. The only caller of
/// the router outside it is `steps/execute.rs`, which every step passes through.
#[test]
fn only_the_router_holds_a_routing_rule() -> std::io::Result<()> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("ktask-core")
        .join("src");
    let mut sources = Vec::new();
    rust_sources(&source, &mut sources)?;
    let router = source.join("route");
    let executor = source.join("steps/execute.rs");
    let mut callers = Vec::new();
    for (path, text) in &sources {
        let production_code = text.split("#[cfg(test)]").next().unwrap_or(text);
        if path.starts_with(&router) {
            continue;
        }
        for rule in [
            "StopCause::classify",
            "stream disconnected before completion",
            "Reconnecting",
            "const RULES",
            "DEFAULT_LIMIT_BACKOFF",
            "fn route(",
        ] {
            assert!(
                !production_code.contains(rule),
                "{} holds a routing rule ({rule}) outside route/",
                path.display()
            );
        }
        if production_code.contains("route(&facts)") {
            callers.push(path.clone());
        }
    }
    assert_eq!(callers, [executor]);
    Ok(())
}
