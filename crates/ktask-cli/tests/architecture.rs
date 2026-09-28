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
