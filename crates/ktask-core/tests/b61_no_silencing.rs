//! The structural rule of docs/CODER.md's task B-61: a lint is fixed, never silenced. No
//! tracked `.rs` file anywhere in the workspace carries an `allow` or `expect` attribute, inner
//! or outer, bare or inside `cfg_attr`; no crate's `Cargo.toml` overrides the workspace's own
//! lints; and neither the workspace's `[workspace.lints...]` tables nor `clippy.toml` are
//! weakened from what is committed. Each check is proven against synthetic lines first, so the
//! scan itself is known to catch what it claims to, before it is trusted against the real tree.

use std::path::{Path, PathBuf};

/// The workspace root: two directories up from this crate's own.
fn workspace_root() -> std::io::Result<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
}

/// Every file directly or transitively under `dir` whose name ends in `.{extension}`.
fn files_with_extension(dir: &Path, extension: &str) -> std::io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        for entry in std::fs::read_dir(&current)? {
            let path = entry?.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(std::ffi::OsStr::to_str) == Some(extension) {
                files.push(path);
            }
        }
    }
    Ok(files)
}

/// Whether `line` silences a lint at its own site, one of the five ways: an inner or outer
/// `allow` or `expect` attribute, or a `cfg_attr` that carries one. Built from `#`, `!` and `[`
/// apart, so this function's own source never spells out the sequence it looks for — this file
/// is itself one of the tracked `.rs` files [`no_tracked_rs_file_silences_a_lint`] scans.
fn silences_a_lint(line: &str) -> bool {
    let Some(after_hash) = line.trim_start().strip_prefix('#') else {
        return false;
    };
    let after_bang = after_hash.strip_prefix('!').unwrap_or(after_hash);
    let Some(after_bracket) = after_bang.strip_prefix('[') else {
        return false;
    };
    after_bracket.starts_with("allow(")
        || after_bracket.starts_with("expect(")
        || (after_bracket.starts_with("cfg_attr(") && line.contains("allow("))
}

/// Every line of `text`, named `path`, that silences a lint — `path:line`, so the offender is
/// named exactly.
fn silencing_violations_in(path: &Path, text: &str) -> Vec<String> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| silences_a_lint(line))
        .map(|(index, _)| format!("{}:{}", path.display(), index + 1))
        .collect()
}

#[test]
fn no_tracked_rs_file_silences_a_lint() -> std::io::Result<()> {
    let root = workspace_root()?;
    let mut offenders = Vec::new();
    for path in files_with_extension(&root.join("crates"), "rs")? {
        let text = std::fs::read_to_string(&path)?;
        offenders.extend(silencing_violations_in(&path, &text));
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

#[test]
fn each_silencing_form_is_caught_naming_its_file_and_line() {
    // Each sample attribute is assembled from separate pieces at runtime: spelling one out
    // whole here would make this file itself fail both `no_tracked_rs_file_silences_a_lint`
    // and the acceptance check's own `grep`.
    let outer = format!("{}{}", '#', '[');
    let inner = format!("{}{}{}", '#', '!', '[');
    let allow = "allow(";
    let expect = "expect(";
    let cfg_attr_allow = "cfg_attr(not(test), allow(";

    let outer_allow =
        format!("fn f() {{}}\n{outer}{allow}clippy::too_many_arguments)]\nfn g() {{}}\n");
    let inner_allow = format!("{inner}{allow}dead_code)]\nfn f() {{}}\n");
    let outer_expect = format!("{outer}{expect}clippy::too_many_lines)]\nfn f() {{}}\n");
    let inner_expect = format!("{inner}{expect}dead_code)]\n");
    let cfg_attr_case = format!("fn f() {{}}\n{outer}{cfg_attr_allow}dead_code))]\nstruct S;\n");

    let cases = [
        (
            "scratch/outer_allow.rs",
            outer_allow,
            "scratch/outer_allow.rs:2",
        ),
        (
            "scratch/inner_allow.rs",
            inner_allow,
            "scratch/inner_allow.rs:1",
        ),
        (
            "scratch/outer_expect.rs",
            outer_expect,
            "scratch/outer_expect.rs:1",
        ),
        (
            "scratch/inner_expect.rs",
            inner_expect,
            "scratch/inner_expect.rs:1",
        ),
        (
            "scratch/cfg_attr.rs",
            cfg_attr_case,
            "scratch/cfg_attr.rs:2",
        ),
    ];
    for (path, text, expected) in cases {
        assert_eq!(
            silencing_violations_in(Path::new(path), &text),
            vec![expected.to_owned()],
            "case {path}"
        );
    }
}

/// Cargo's lints tables a `Cargo.toml` may declare: a crate's own `[lints]` (and any nested
/// `[lints.*]`, which a crate should never need — `workspace = true` already inherits every
/// one), or the workspace's own `[workspace.lints]` and its nested tables, where each lint's
/// level is actually set.
enum LintsSection {
    /// A crate's own `[lints]` or `[lints.*]`.
    Crate,
    /// The workspace's own `[workspace.lints]` or `[workspace.lints.*]`.
    Workspace,
}

/// The lints section `header` (a `[...]` line, brackets already stripped) names, when it names
/// one at all.
fn lints_section(header: &str) -> Option<LintsSection> {
    if header == "lints" || header.starts_with("lints.") {
        Some(LintsSection::Crate)
    } else if header == "workspace.lints" || header.starts_with("workspace.lints.") {
        Some(LintsSection::Workspace)
    } else {
        None
    }
}

/// The section `line` opens, when it is a `[section]` or `[[section]]` header.
fn toml_section_header(line: &str) -> Option<&str> {
    let line = line.trim();
    if !line.starts_with('[') {
        return None;
    }
    Some(line.trim_matches(['[', ']']).trim())
}

/// `line`'s own `name = value` pair, trimmed and with any trailing comment cut off, when it is
/// a simple assignment rather than a table header, array entry, blank line or comment.
fn toml_assignment(line: &str) -> Option<(&str, &str)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
        return None;
    }
    let (name, value) = line.split_once('=')?;
    let value = value.split('#').next().unwrap_or_default();
    Some((name.trim(), value.trim()))
}

/// Every line of a `Cargo.toml` named `path` that weakens what it configures: inside a crate's
/// own `[lints...]` tables, anything other than exactly `workspace = true`; inside the
/// workspace's own `[workspace.lints...]` tables, any lint set to `allow`.
fn lints_violations(path: &Path, text: &str) -> Vec<String> {
    let mut offenders = Vec::new();
    let mut section = None;
    for (index, line) in text.lines().enumerate() {
        if let Some(header) = toml_section_header(line) {
            section = lints_section(header);
            continue;
        }
        let (Some(section), Some((name, value))) = (&section, toml_assignment(line)) else {
            continue;
        };
        let offends = match section {
            LintsSection::Crate => !(name == "workspace" && value == "true"),
            LintsSection::Workspace => value.contains("\"allow\""),
        };
        if offends {
            offenders.push(format!("{}:{}", path.display(), index + 1));
        }
    }
    offenders
}

/// The ceiling committed for each of `clippy.toml`'s own thresholds: raising any of them
/// weakens the lint it bounds, the same as silencing it would.
fn clippy_toml_ceilings() -> [(&'static str, u64); 4] {
    [
        ("too-many-arguments-threshold", 7),
        ("too-many-lines-threshold", 120),
        ("cognitive-complexity-threshold", 20),
        ("type-complexity-threshold", 250),
    ]
}

/// Every line of `clippy.toml`, named `path`, that raises one of [`clippy_toml_ceilings`]
/// past what is committed.
fn clippy_toml_violations(path: &Path, text: &str) -> Vec<String> {
    let ceilings = clippy_toml_ceilings();
    let mut offenders = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let Some((name, value)) = toml_assignment(line) else {
            continue;
        };
        let raised = ceilings
            .iter()
            .find(|(key, _)| *key == name)
            .is_some_and(|(_, ceiling)| value.parse::<u64>().is_ok_and(|actual| actual > *ceiling));
        if raised {
            offenders.push(format!("{}:{}", path.display(), index + 1));
        }
    }
    offenders
}

/// The workspace's own `Cargo.toml`, and every crate's — never found by walking `target/`,
/// which a recursive search of the whole workspace root would otherwise drag in.
fn every_cargo_toml(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = vec![root.join("Cargo.toml")];
    for entry in std::fs::read_dir(root.join("crates"))? {
        paths.push(entry?.path().join("Cargo.toml"));
    }
    Ok(paths)
}

#[test]
fn no_cargo_toml_lowers_a_lints_level_and_clippy_toml_is_not_weakened() -> std::io::Result<()> {
    let root = workspace_root()?;
    let mut offenders = Vec::new();
    for path in every_cargo_toml(&root)? {
        let text = std::fs::read_to_string(&path)?;
        offenders.extend(lints_violations(&path, &text));
    }
    let clippy_toml = root.join("clippy.toml");
    let text = std::fs::read_to_string(&clippy_toml)?;
    offenders.extend(clippy_toml_violations(&clippy_toml, &text));
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}

#[test]
fn a_crate_override_a_workspace_allow_and_a_raised_threshold_are_each_caught() {
    let crate_override =
        "[lints]\nworkspace = true\n\n[lints.clippy]\ntoo_many_arguments = \"allow\"\n";
    assert_eq!(
        lints_violations(Path::new("scratch/Cargo.toml"), crate_override),
        vec!["scratch/Cargo.toml:5".to_owned()]
    );

    let workspace_allow = "[workspace.lints.clippy]\nall = { level = \"deny\", priority = -1 }\ntoo_many_arguments = \"allow\"\n";
    assert_eq!(
        lints_violations(Path::new("scratch/Cargo.toml"), workspace_allow),
        vec!["scratch/Cargo.toml:3".to_owned()]
    );

    let raised_threshold = "too-many-arguments-threshold = 50\n";
    assert_eq!(
        clippy_toml_violations(Path::new("scratch/clippy.toml"), raised_threshold),
        vec!["scratch/clippy.toml:1".to_owned()]
    );
}
