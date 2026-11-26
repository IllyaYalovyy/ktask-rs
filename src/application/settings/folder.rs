//! Paths between computers. Export writes every occurrence of the project folder as a
//! placeholder relative to the project root; import puts the actual folder there. Other
//! absolute paths stay as they are and are listed for the owner.
use super::file::SettingsFile;
use crate::storage::{Error, Result};
use serde_json::Value;
use std::path::Path;

pub(super) const ROOT: &str = "${KTASK_PROJECT_ROOT}";

/// Characters that make the text before a `/` part of the same path or word.
fn continues_path(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '~' | '$' | '}' | ')')
}

/// Characters a file name can continue with after a folder's name.
fn continues_name(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '~' | '+' | '@')
}

fn ends_token(c: char) -> bool {
    c.is_whitespace() || "\"'`;:|&<>()[]{},=".contains(c)
}

/// Whether a path can begin at byte `at`: not inside a longer path or word, except after a
/// compiler-style flag such as `-I`.
fn begins_path(text: &str, at: usize) -> bool {
    let before = &text[..at];
    let run = before
        .rsplit(|c| !continues_path(c))
        .next()
        .unwrap_or_default();
    run.is_empty() || run.starts_with('-') && !run.contains('/')
}

/// Whether the folder name that ends where `rest` starts is complete.
fn ends_folder(rest: &str) -> bool {
    let mut next = rest.chars();
    match next.next() {
        None | Some('/') => true,
        // A full stop ends a sentence; `project.bak` is another folder.
        Some('.') => !next.next().is_some_and(|c| continues_name(c) || c == '.'),
        Some(c) => !continues_name(c),
    }
}

/// The absolute paths written in `text`, with the byte each starts at.
fn absolute_paths(text: &str) -> Vec<(usize, &str)> {
    text.match_indices('/')
        .filter(|(at, _)| {
            begins_path(text, *at)
                && text[at + 1..]
                    .chars()
                    .next()
                    .is_some_and(|c| c != '/' && !ends_token(c))
        })
        .map(|(at, _)| (at, token(&text[at..])))
        .collect()
}

/// The path at the start of `text`, without a sentence's full stop.
fn token(text: &str) -> &str {
    let end = text.find(ends_token).unwrap_or(text.len());
    text[..end].trim_end_matches('.')
}

/// A path that reaches inside `root` only through a symbolic link, written from the root.
fn through_links(path: &str, root: &Path) -> Option<String> {
    let path_ref = Path::new(path);
    let (existing, resolved) = path_ref
        .ancestors()
        .find_map(|part| Some((part, part.canonicalize().ok()?)))?;
    let below = resolved
        .strip_prefix(root)
        .ok()?
        .join(path_ref.strip_prefix(existing).ok()?);
    let mut written = ROOT.to_owned();
    if !below.as_os_str().is_empty() {
        written.push('/');
        written.push_str(below.to_str()?);
    }
    if path.ends_with('/') {
        written.push('/');
    }
    Some(written)
}

/// `text` with the project folder `root` replaced by the placeholder.
fn relativize(text: &str, root: &str) -> String {
    let mut result = String::new();
    let mut copied = 0;
    for (at, _) in text.match_indices(root) {
        let begins = begins_path(text, at) || text[..at].ends_with("file://");
        if at >= copied && begins && ends_folder(&text[at + root.len()..]) {
            result.push_str(&text[copied..at]);
            result.push_str(ROOT);
            copied = at + root.len();
        }
    }
    result.push_str(&text[copied..]);
    let linked: Vec<_> = absolute_paths(&result)
        .into_iter()
        .filter_map(|(at, path)| Some((at, path.len(), through_links(path, Path::new(root))?)))
        .collect();
    for (at, length, written) in linked.into_iter().rev() {
        result.replace_range(at..at + length, &written);
    }
    result
}

fn map_value(value: &mut Value, change: &impl Fn(&str) -> String) {
    match value {
        Value::String(text) => *text = change(text),
        Value::Array(items) => items.iter_mut().for_each(|item| map_value(item, change)),
        Value::Object(fields) => fields.values_mut().for_each(|item| map_value(item, change)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// `file` with every string of every setting changed; the informational source folder stays.
fn map_strings(file: &SettingsFile, change: impl Fn(&str) -> String) -> Result<SettingsFile> {
    let mut value = serde_json::to_value(file).expect("settings are JSON values");
    for section in ["project", "shared"] {
        map_value(&mut value[section], &change);
    }
    serde_json::from_value(value)
        .map_err(|error| Error::Conflict(format!("Malformed settings file: {error}")))
}

fn utf8(root: &Path) -> Result<&str> {
    root.to_str()
        .ok_or_else(|| Error::Conflict("Settings need a UTF-8 project folder".into()))
}

pub(super) fn to_placeholders(file: &SettingsFile, root: &Path) -> Result<SettingsFile> {
    let root = utf8(root)?;
    map_strings(file, |text| relativize(text, root))
}

pub(super) fn to_folder(file: &SettingsFile, root: &Path) -> Result<SettingsFile> {
    let root = utf8(root)?;
    map_strings(file, |text| text.replace(ROOT, root))
}

fn strings<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => found.push(text),
        Value::Array(items) => items.iter().for_each(|item| strings(item, found)),
        Value::Object(fields) => fields.values().for_each(|item| strings(item, found)),
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// A path one setting holds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct PathNote {
    pub setting: String,
    pub path: String,
}

/// The paths of a file still holding placeholders: those import rewrites to `root`, and
/// the absolute paths outside the project that it keeps.
pub(super) fn paths(file: &SettingsFile, root: &Path) -> Result<(Vec<PathNote>, Vec<PathNote>)> {
    let folder = utf8(root)?;
    let (mut rewritten, mut outside) = (Vec::new(), Vec::new());
    for entry in file.entries() {
        let mut texts = Vec::new();
        strings(&entry.value, &mut texts);
        let note = |path: String| PathNote {
            setting: entry.setting.clone(),
            path,
        };
        for text in texts {
            for (at, _) in text.match_indices(ROOT) {
                let below = &text[at + ROOT.len()..];
                let below = if below.starts_with('/') {
                    token(below)
                } else {
                    ""
                };
                add(&mut rewritten, note(format!("{folder}{below}")));
            }
            for (_, path) in absolute_paths(text) {
                // Device files exist on every computer.
                if !path.starts_with("/dev/") && !Path::new(path).starts_with(root) {
                    add(&mut outside, note(path.to_owned()));
                }
            }
        }
    }
    Ok((rewritten, outside))
}

fn add(notes: &mut Vec<PathNote>, note: PathNote) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_project_folder_itself_becomes_relative() {
        let root = "/home/alice/work/app";
        for (text, expected) in [
            ("/home/alice/work/app", "${R}"),
            ("make -C /home/alice/work/app check", "make -C ${R} check"),
            (
                "cc -I/home/alice/work/app/include x.c",
                "cc -I${R}/include x.c",
            ),
            (
                "ROOT=/home/alice/work/app:'/home/alice/work/app/bin'",
                "ROOT=${R}:'${R}/bin'",
            ),
            ("Read /home/alice/work/app.", "Read ${R}."),
            ("file:///home/alice/work/app/x", "file://${R}/x"),
            (
                "ls /home/alice/work/app-old /home/alice/work/app.bak /home/alice/work/apps",
                "ls /home/alice/work/app-old /home/alice/work/app.bak /home/alice/work/apps",
            ),
            ("ls /mnt/home/alice/work/app", "ls /mnt/home/alice/work/app"),
        ] {
            assert_eq!(
                relativize(text, root),
                expected.replace("${R}", ROOT),
                "{text}"
            );
        }
    }

    #[test]
    fn absolute_paths_are_found_without_urls_relative_paths_or_prose() {
        let found = |text: &str| {
            absolute_paths(text)
                .into_iter()
                .map(|(_, path)| path.to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            found("/opt/lint/run.sh --config=/etc/lint.toml > /var/log/x."),
            ["/opt/lint/run.sh", "/etc/lint.toml", "/var/log/x"]
        );
        for text in [
            "curl https://example.com/a",
            "./scripts/run.sh src/main.rs ~/bin/x $HOME/y ${DIR}/z $(pwd)/w",
            "review / validation // comment and/or {{title}}/{{goal}}",
        ] {
            assert!(found(text).is_empty(), "{text}: {:?}", found(text));
        }
    }
}
