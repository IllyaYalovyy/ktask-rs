//! The approved set of terminal screens under `tests/golden`: its file format, how two
//! sets compare and how their differences read. Nothing here touches a PTY; the recording
//! helper and the `screens` gate share it.
use std::{collections::BTreeMap, path::Path};

/// How the recorded session was driven.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Input {
    Keyboard,
    Mouse,
}

/// Tests loop over `mouse` in `[false, true]`.
impl From<bool> for Input {
    fn from(mouse: bool) -> Self {
        if mouse { Self::Mouse } else { Self::Keyboard }
    }
}

/// The input paths a stored frame covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Paths {
    Keyboard,
    Mouse,
    Both,
}

impl Paths {
    fn of(input: Input) -> Self {
        match input {
            Input::Keyboard => Self::Keyboard,
            Input::Mouse => Self::Mouse,
        }
    }
    fn covers(self, input: Input) -> bool {
        self == Self::Both || self == Self::of(input)
    }
    fn label(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::Both => "keyboard+mouse",
        }
    }
    fn parse(label: &str) -> Option<Self> {
        [Self::Keyboard, Self::Mouse, Self::Both]
            .into_iter()
            .find(|paths| paths.label() == label)
    }
}

/// One labelled screen of a test.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub step: String,
    /// Columns and rows of the terminal.
    pub size: (u16, u16),
    pub paths: Paths,
    /// Normalised screen text, one terminal row per line.
    pub text: String,
}

const HEADER: &str = "@@ ";
const SEPARATOR: &str = " · ";

/// Adds a frame in recording order. A screen that is identical for the other input path is
/// stored once and marked as covering both. A step an input path has already recorded at
/// this size is numbered, `step #2`, in the order the test shows them.
pub fn add(
    frames: &mut Vec<Frame>,
    step: &str,
    size: (u16, u16),
    input: Input,
    text: String,
) -> Result<(), String> {
    if step.is_empty() || step.contains('\n') {
        return Err(format!("screen step {step:?} is not a one-line label"));
    }
    if text.lines().any(|line| line.starts_with(HEADER)) {
        return Err(format!(
            "screen {step:?} has a line starting with {HEADER:?}"
        ));
    }
    let covered = |step: &str| {
        frames
            .iter()
            .any(|f| f.step == step && f.size == size && f.paths.covers(input))
    };
    let mut label = step.to_owned();
    let mut number = 1;
    while covered(&label) {
        number += 1;
        label = format!("{step} #{number}");
    }
    match frames
        .iter_mut()
        .find(|f| f.step == label && f.size == size && f.text == text)
    {
        Some(frame) => frame.paths = Paths::Both,
        None => frames.push(Frame {
            step: label,
            size,
            paths: Paths::of(input),
            text,
        }),
    }
    Ok(())
}

/// A golden file: each frame under a `@@ step · COLSxROWS · input` line.
pub fn render(frames: &[Frame]) -> String {
    let mut out = String::new();
    for frame in frames {
        out.push_str(&format!(
            "{HEADER}{}{SEPARATOR}{}x{}{SEPARATOR}{}\n{}",
            frame.step,
            frame.size.0,
            frame.size.1,
            frame.paths.label(),
            frame.text
        ));
    }
    out
}

pub fn parse(file: &str) -> Result<Vec<Frame>, String> {
    let mut frames: Vec<Frame> = Vec::new();
    for line in file.split_inclusive('\n') {
        let Some(header) = line.strip_prefix(HEADER) else {
            let frame = frames.last_mut().ok_or("text before the first frame")?;
            frame.text.push_str(line);
            continue;
        };
        let bad = || format!("bad frame header: {line:?}");
        // From the right: a step may itself hold the separator.
        let mut parts = header.trim_end().rsplitn(3, SEPARATOR);
        let (Some(paths), Some(size), Some(step)) = (parts.next(), parts.next(), parts.next())
        else {
            return Err(bad());
        };
        let (cols, rows) = size.split_once('x').ok_or_else(bad)?;
        frames.push(Frame {
            step: step.into(),
            size: (
                cols.parse().map_err(|_| bad())?,
                rows.parse().map_err(|_| bad())?,
            ),
            paths: Paths::parse(paths).ok_or_else(bad)?,
            text: String::new(),
        });
    }
    Ok(frames)
}

/// Test name to the frames it recorded, as stored in one directory.
pub type Set = BTreeMap<String, Vec<Frame>>;

pub const EXTENSION: &str = "screens";

/// The golden file name of the test whose thread libtest named `test`.
pub fn file_name(test: &str) -> String {
    format!("{}.{EXTENSION}", test.replace("::", "."))
}

/// Every screen file of `directory`; none when it does not exist.
pub fn load(directory: &Path) -> Result<Set, String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Set::new()),
        Err(error) => return Err(format!("{}: {error}", directory.display())),
    };
    let mut set = Set::new();
    for entry in entries {
        let path = entry.map_err(|error| error.to_string())?.path();
        if path.extension().is_none_or(|ext| ext != EXTENSION) {
            continue;
        }
        let read = std::fs::read_to_string(&path)
            .map_err(|error| error.to_string())
            .and_then(|text| parse(&text))
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let name = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .ok_or_else(|| format!("{}: unreadable name", path.display()))?;
        set.insert(name.into(), read);
    }
    Ok(set)
}

/// A frame that is not the same on both sides.
#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    pub test: String,
    pub step: String,
    pub size: (u16, u16),
    pub paths: Paths,
    /// None for a new screen.
    pub before: Option<String>,
    /// None for a removed screen.
    pub after: Option<String>,
}

impl Change {
    pub fn kind(&self) -> &'static str {
        match (&self.before, &self.after) {
            (None, _) => "NEW",
            (_, None) => "REMOVED",
            _ => "CHANGED",
        }
    }
    pub fn label(&self) -> String {
        format!(
            "{}{SEPARATOR}{}{SEPARATOR}{}x{}{SEPARATOR}{}",
            self.test,
            self.step,
            self.size.0,
            self.size.1,
            self.paths.label()
        )
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Comparison {
    /// Stored frames that are identical on both sides.
    pub matched: usize,
    pub changes: Vec<Change>,
}

impl Comparison {
    pub fn count(&self, kind: &str) -> usize {
        self.changes.iter().filter(|c| c.kind() == kind).count()
    }
    pub fn summary(&self) -> String {
        format!(
            "{} matched, {} new, {} changed, {} removed",
            self.matched,
            self.count("NEW"),
            self.count("CHANGED"),
            self.count("REMOVED")
        )
    }
}

type Sides<'a> = (Option<&'a str>, Option<&'a str>);

/// Compares screen by screen: a label and input path on one side against the same on the
/// other. Both input paths with the same difference are reported once.
pub fn compare(before: &Set, after: &Set) -> Comparison {
    let mut result = Comparison::default();
    let tests: std::collections::BTreeSet<&String> = before.keys().chain(after.keys()).collect();
    for test in tests {
        let frames = |set: &'_ Set| set.get(test).cloned().unwrap_or_default();
        let (old, new) = (frames(before), frames(after));
        let mut labels: Vec<(&str, (u16, u16))> = Vec::new();
        for frame in new.iter().chain(&old) {
            if !labels.contains(&(frame.step.as_str(), frame.size)) {
                labels.push((&frame.step, frame.size));
            }
        }
        for (step, size) in labels {
            let find = |frames: &'_ [Frame], input| {
                frames
                    .iter()
                    .position(|f| f.step == step && f.size == size && f.paths.covers(input))
            };
            let mut groups: Vec<(Sides<'_>, Paths)> = Vec::new();
            for input in [Input::Keyboard, Input::Mouse] {
                let sides = (
                    find(&old, input).map(|at| old[at].text.as_str()),
                    find(&new, input).map(|at| new[at].text.as_str()),
                );
                if sides == (None, None) {
                    continue;
                }
                match groups.iter_mut().find(|(seen, _)| *seen == sides) {
                    Some((_, paths)) => *paths = Paths::Both,
                    None => groups.push((sides, Paths::of(input))),
                }
            }
            for ((old_text, new_text), paths) in groups {
                if old_text == new_text {
                    result.matched += 1;
                    continue;
                }
                result.changes.push(Change {
                    test: test.clone(),
                    step: step.into(),
                    size,
                    paths,
                    before: old_text.map(str::to_owned),
                    after: new_text.map(str::to_owned),
                });
            }
        }
    }
    result
}

/// The numbers of the lines that differ, and those lines as they were (`-`) and are (`+`).
pub fn line_diff(before: &str, after: &str) -> (Vec<usize>, String) {
    let (old, new): (Vec<&str>, Vec<&str>) = (before.lines().collect(), after.lines().collect());
    let mut numbers = Vec::new();
    let mut text = String::new();
    for index in 0..old.len().max(new.len()) {
        let (was, is) = (old.get(index), new.get(index));
        if was == is {
            continue;
        }
        numbers.push(index + 1);
        for (sign, line) in [('-', was), ('+', is)] {
            if let Some(line) = line {
                text.push_str(&format!("{sign}{:>4} {line}\n", index + 1));
            }
        }
    }
    (numbers, text)
}

/// Differences shown in full; the rest are listed by name.
const FULL_DIFFS: usize = 40;

/// What stands between the golden set and the screens the tests produced.
pub fn verify(golden: &Set, produced: &Set) -> Result<usize, String> {
    let comparison = compare(golden, produced);
    let mut problems = Vec::new();
    for (index, change) in comparison.changes.iter().enumerate() {
        let (headline, detail) = match (&change.before, &change.after) {
            (Some(before), Some(after)) => {
                let (lines, diff) = line_diff(before, after);
                let lines: Vec<String> = lines.iter().map(usize::to_string).collect();
                (
                    format!("differs from its golden on lines {}", lines.join(", ")),
                    diff,
                )
            }
            (None, Some(after)) => ("has no golden".into(), after.clone()),
            (before, _) => (
                "is golden but no test produced it".into(),
                before.clone().unwrap_or_default(),
            ),
        };
        problems.push(format!(
            "{} {} {headline}{}",
            change.kind(),
            change.label(),
            if index < FULL_DIFFS {
                format!("\n{detail}")
            } else {
                String::new()
            }
        ));
    }
    // Equal screens in another order or layout still make a different file.
    for (test, frames) in produced {
        if golden.get(test).is_some_and(|stored| stored != frames)
            && !comparison.changes.iter().any(|c| &c.test == test)
        {
            problems.push(format!(
                "REORDERED {test}: its screens are stored differently"
            ));
        }
    }
    if problems.is_empty() {
        return Ok(comparison.matched);
    }
    Err(format!(
        "Screens differ from tests/golden ({}):\n{}\nIf the change is intended, run `make golden` and keep the updated files in the change.",
        comparison.summary(),
        problems.join("\n")
    ))
}

/// The changed-screens report against the approved set; None when no screen changed.
pub fn report(approved: &Set, produced: &Set, base: &str) -> (Comparison, Option<String>) {
    let comparison = compare(approved, produced);
    if comparison.changes.is_empty() {
        return (comparison, None);
    }
    let mut out = format!(
        "Changed screens report. Approved set: tests/golden at {base}.\n{}\nMatched screens equal the approved set, were verified in code and are not repeated here.\n",
        comparison.summary()
    );
    for change in &comparison.changes {
        out.push_str(&format!("\n=== {} {} ===\n", change.kind(), change.label()));
        if let Some(before) = &change.before {
            out.push_str(&format!("--- before\n{before}"));
        }
        if let (Some(before), Some(after)) = (&change.before, &change.after) {
            let (lines, _) = line_diff(before, after);
            let lines: Vec<String> = lines.iter().map(usize::to_string).collect();
            out.push_str(&format!("--- lines that differ: {}\n", lines.join(", ")));
        }
        if let Some(after) = &change.after {
            out.push_str(&format!("+++ after\n{after}"));
        }
    }
    (comparison, Some(out))
}
