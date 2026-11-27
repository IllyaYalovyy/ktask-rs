//! What may differ between two runs of the same code, and how it reads in a recorded
//! screen. Each rule is named and replaces only the volatile token it matches; the rest of
//! the frame is compared as shown. No rule hides a line, a frame or a test.
//!
//! - **temporary paths**: a test sandbox, however it is wrapped or cut, reads as a fixed
//!   path of the same length, so the layout around it is compared as it was. Sandboxes are
//!   padded to one length and named after their test.
//! - **scratch names**: the random part of any `run-XXXXXX` directory reads as zeros.
//! - **checkout path**: the folder this project is built in reads as `<checkout>`.
//! - **version**: this build's version reads as `<ver>`.
//! - **times**: dates with clock times read as `<date>`, epoch milliseconds as `<epoch>`.
//! - **simulated progress**: on the demo workspace's screens, the step and mock cost its
//!   timer advances read as `<n>` and `<cost>`.
//! - **durations**: elapsed, active, waiting and countdown times read as `<t>`.
//!   Configured values, such as a time limit or a retry delay, are compared. A duration so
//!   long that it wrapped onto its own line, which a dialog may hide, is put back where a
//!   short one fits.
//! - **measured storage**: on a `storage_max_bytes` pause, the bytes it measured as needed
//!   and the budget proposed from them read as zeros. The configured budget is compared.
//! - **disk space**: the host's free and total space read as `<size> of <size>`, and a
//!   count of free bytes as `<size>`.
//! - **generated IDs**: commit, tree and checksum hashes read as `x` of the same length,
//!   and installer operation IDs, which hold a process number, as `<id>`.
use regex::{Captures, Regex};
use std::sync::LazyLock;

/// What this run alone knows to be volatile.
pub struct Volatile<'a> {
    /// A path as this run has it, and the fixed text of the same length it reads as.
    pub paths: &'a [(String, String)],
    pub checkout: &'a str,
    pub version: &'a str,
}

/// The length a test sandbox's parent has, so that screens wrap and cut paths alike.
pub const TEMPORARY: usize = 41;
/// The sandbox folder under the temporary directory, padded to make up [`TEMPORARY`].
const SANDBOXES: &str = "ktask-test-scratch";
/// The length of a random scratch directory name, `run-XXXXXX`.
const SCRATCH_NAME: &str = "run-000000";

/// The folder under `temporary` that holds test sandboxes.
pub fn sandboxes(temporary: &str) -> String {
    let padding = TEMPORARY.saturating_sub(temporary.trim_end_matches('/').len());
    // A dialog can cover the beginning of a path. Each padding character identifies
    // its offset, so a visible fragment maps to the same columns as the full path.
    const PADDING: &str = "tDQGsuCNoSnIMBcRfVKaJphmlqrkFTPjHdLegObiE";
    format!("{SANDBOXES}{}", &PADDING[..padding])
}

/// The replacements for one sandbox: its whole path and its directory name.
pub fn sandbox_paths(sandbox: &str) -> Vec<(String, String)> {
    let Some((parent, name)) = sandbox.rsplit_once('/') else {
        return Vec::new();
    };
    let fixed = format!("{:-<TEMPORARY$}/{SANDBOXES}", "/tmp/ktask-screens");
    let mut paths = vec![(name.to_owned(), SCRATCH_NAME.to_owned())];
    if parent.len() == fixed.len() {
        paths.insert(0, (sandbox.to_owned(), format!("{fixed}/{SCRATCH_NAME}")));
    }
    paths
}

/// Characters that separate panes, so a column's text can be followed and kept in place.
fn border(c: char) -> bool {
    matches!(c, '│' | '╭' | '╮' | '╰' | '╯' | '─')
}

/// The shortest part of a path that cannot be taken for ordinary text.
const FRAGMENT: usize = 10;
/// The shortest beginning of a path that counts when a pane cuts it off.
const CUT: usize = 6;
/// The shortest beginning of the checkout path that no temporary path shares.
const CHECKOUT: usize = 12;

/// One screen character, and whether a path rule already wrote it. A replaced part shares
/// text with the paths and must not be taken for one again.
#[derive(Clone, Copy)]
struct Cell {
    shown: char,
    replaced: bool,
}

fn cells(line: &str) -> Vec<Cell> {
    line.chars()
        .map(|shown| Cell {
            shown,
            replaced: false,
        })
        .collect()
}

/// How far `line` still shows the beginning of `path`.
fn matching(line: &[Cell], path: &[char]) -> usize {
    line.iter()
        .zip(path)
        .take_while(|(cell, c)| !cell.replaced && cell.shown == **c)
        .count()
}

fn write(cells: &mut [Cell], text: &[char]) {
    for (cell, &shown) in cells.iter_mut().zip(text) {
        *cell = Cell {
            shown,
            replaced: true,
        };
    }
}

/// A part of a path found in one line, to read as the same part of the fixed text.
struct Part {
    row: usize,
    start: usize,
    offset: usize,
    length: usize,
}

/// The rest of a path cut at the end of a pane continues in the same pane below.
fn follow(lines: &[Vec<Cell>], path: &[char], parts: &mut Vec<Part>) {
    let Some(&Part {
        mut row,
        start: column,
        offset,
        length,
    }) = parts.first()
    else {
        return;
    };
    let mut done = offset + length;
    while done < path.len() {
        row += 1;
        let Some(line) = lines.get(row).filter(|line| line.len() > column) else {
            return;
        };
        let mut start = line[..=column]
            .iter()
            .rposition(|cell| border(cell.shown))
            .map_or(0, |at| at + 1);
        while line.get(start).is_some_and(|cell| cell.shown == ' ') {
            start += 1;
        }
        let length = matching(&line[start.min(line.len())..], &path[done..]);
        if length == 0 {
            return;
        }
        parts.push(Part {
            row,
            start,
            offset: done,
            length,
        });
        done += length;
    }
}

/// Nothing but padding or an ellipsis before the pane ends.
fn ends_pane(rest: &[Cell]) -> bool {
    rest.iter()
        .take_while(|cell| !border(cell.shown))
        .all(|cell| matches!(cell.shown, ' ' | '…'))
}

/// The checkout can have any length and be cut anywhere: whatever shows of the path reads
/// as `<checkout>`, padded so the pane keeps its width.
fn replace_checkout(line: &str, checkout: &str) -> String {
    let mut line: Vec<char> = line.chars().collect();
    let checkout: Vec<char> = checkout.chars().collect();
    let shortest = CHECKOUT.min(checkout.len());
    let mut at = 0;
    while at < line.len() {
        let shown = line[at..]
            .iter()
            .zip(&checkout)
            .take_while(|(a, b)| a == b)
            .count();
        if shown < shortest {
            at += 1;
            continue;
        }
        let end = line[at..]
            .iter()
            .position(|&c| border(c) || c == ' ' || c == '…')
            .map_or(line.len(), |end| at + end);
        let name: Vec<char> = format!("{:<width$}", "<checkout>", width = end - at)
            .chars()
            .collect();
        let after = at + name.len();
        line.splice(at..end, name);
        at = after;
    }
    line.into_iter().collect()
}

/// Nothing but padding between the pane's left border and `at`.
fn starts_pane(line: &[Cell], at: usize) -> bool {
    line[..at]
        .iter()
        .rev()
        .find(|cell| cell.shown != ' ')
        .is_none_or(|cell| border(cell.shown))
}

/// What of `path` shows from this position on, following it over wrapped lines. A part
/// that is not the beginning of the path counts only where a pane starts, as when a wrapped
/// or overlaid path has its beginning out of sight: elsewhere it is ordinary text.
fn shown_at(lines: &[Vec<Cell>], row: usize, at: usize, path: &[char]) -> Vec<Part> {
    let line = &lines[row];
    let offsets = if starts_pane(line, at) { path.len() } else { 1 };
    let (offset, length) = (0..offsets)
        .map(|offset| (offset, matching(&line[at..], &path[offset..])))
        .max_by_key(|&(offset, length)| (length, usize::MAX - offset))
        .unwrap_or_default();
    let cut = offset + length < path.len() && ends_pane(&line[at + length..]);
    if length < FRAGMENT.min(path.len()) && !(cut && offset == 0 && length >= CUT) {
        return Vec::new();
    }
    let mut parts = vec![Part {
        row,
        start: at,
        offset,
        length,
    }];
    if cut {
        follow(lines, path, &mut parts);
    }
    parts
}

/// Replaces every shown part of the volatile paths with the same part of the fixed text of
/// the same length. Where several begin alike, the one that shows furthest is meant.
fn replace_paths(lines: &mut [Vec<Cell>], paths: &[(String, String)]) {
    let paths: Vec<(Vec<char>, Vec<char>)> = paths
        .iter()
        .map(|(path, fixed)| (path.chars().collect(), fixed.chars().collect()))
        .filter(|(path, fixed): &(Vec<char>, Vec<char>)| path.len() == fixed.len())
        .collect();
    for row in 0..lines.len() {
        let mut at = 0;
        while at < lines[row].len() {
            let best = paths
                .iter()
                .map(|(path, fixed)| (shown_at(lines, row, at, path), fixed))
                .max_by_key(|(parts, _)| parts.iter().map(|part| part.length).sum::<usize>());
            let Some((parts, fixed)) = best.filter(|(parts, _)| !parts.is_empty()) else {
                at += 1;
                continue;
            };
            for part in &parts {
                write(
                    &mut lines[part.row][part.start..part.start + part.length],
                    &fixed[part.offset..part.offset + part.length],
                );
            }
            at += parts[0].length;
        }
    }
}

struct Rule {
    pattern: Regex,
    /// The volatile group is replaced with this; the rest of the match stays.
    with: &'static str,
}

fn rule(pattern: &str, with: &'static str) -> Rule {
    Rule {
        pattern: Regex::new(pattern).expect("normalisation rule"),
        with,
    }
}

/// What introduces a runtime duration.
const RUNTIME: &str = "(?i:\\b(?:active|ctive|tive|waiting|waited|provider time|took|running for|resumes in|resets in|elapsed|elapsed execution:|active time:|waiting time:))";

const DURATION: &str =
    r"(?:\d+d \d+h|\d+h \d\dm(?: \d\ds)?|\d+m \d\ds|\d+(?:\.\d+)?(?:ms|s|m|h|d))";
const MONTH: &str = "(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec)";

/// Rules that replace a token, in order: later ones may rely on earlier replacements.
static TOKENS: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        // times, also when a pane wraps them
        rule(
            &format!(r"(?P<v>\b{MONTH} [ \d]?\d(?: \d\d(?::\d\d)?)?)"),
            "<date>",
        ),
        rule(r"(?P<v>(?:\b[0-3]\d )?\b[0-2]\d:[0-5]\d\b)", "<date>"),
        rule(&format!(r"(?P<v>\b{MONTH}) *(?:│|$)"), "<mon>"),
        rule(r"(?P<v>\b1[6-9]\d{11}\b)", "<epoch>"),
        // generated IDs that hold a process number
        rule(r"(?P<v>\b\d+-\d+-<epoch>-\d+)", "<id>"),
        // durations, named by what comes before them
        rule(&format!(r"{RUNTIME} +(?P<v>{DURATION})\b"), "<t>"),
        // the header's time on the current task
        rule(&format!(r"· (?P<v>{DURATION}) +\|"), "<t>"),
        // a duration that wrapped onto its own pane line
        rule(
            r"(?:^|[│])\s*(?:time )?(?P<v>\d+(?:\.\d+)?s|\d+m \d\ds)\s*(?:$|[│])",
            "<t>",
        ),
        // the History table: closed date, attempts, active time
        rule(&format!(r"<date>(?: *│)? +\d+ +(?P<v>{DURATION})\b"), "<t>"),
        rule(
            &format!(r"\b\d+ +(?P<v>{DURATION}) +(?:\d+(?:\.\d+)?[kMG]? tkn|\$\d)"),
            "<t>",
        ),
        // disk space of the host
        rule(r"(?P<v>\b\d+(?:\.\d+)? [KMGT]iB) of \d", "<size>"),
        rule(r"<size> of (?P<v>\d+(?:\.\d+)? [KMGT]iB)", "<size>"),
        rule(r"free bytes Ok\((?P<v>\d+)\)", "<size>"),
    ]
});

/// A scratch directory name, whole or cut off by its pane.
/// The demo workspace's progress, which only its timer advances.
static SIMULATED: LazyLock<Vec<Rule>> = LazyLock::new(|| {
    vec![
        rule(r"Attempt \d+ · step (?P<v>\d+)/\d+", "<n>"),
        rule(r"Cost (?P<v>\$\d+\.\d\d) \(mock\)", "<cost>"),
    ]
});

static SCRATCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\brun-(?P<v>[A-Za-z0-9]{6}\b|[A-Za-z0-9]{1,5}(?P<end> *(?:[│╭╮╰╯…]|$)))")
        .expect("scratch names")
});
/// A runtime duration its pane cuts off: the digits left of it read as zeros.
static CUT_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r"{RUNTIME} +(?P<v>\d+(?:[dhm] ?\d*)?)(?P<end>[│╭╮╰╯])"
    ))
    .expect("cut durations")
});
/// What a storage budget pause needed and proposes: it grows with the bytes the run wrote,
/// timings included. Seven digits or more, or the digits left where the line is cut.
static MEASURED_STORAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:needed |Raise to |storage_max_bytes to |│)(?P<v>\d{7,}|\d+…)")
        .expect("measured storage")
});
static HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\b[0-9a-f]{7,64}\b").expect("hashes"));

/// A scratch name wrapped inside its pane: part ends one line, the rest starts the next.
fn wrapped_scratch(lines: &mut [Vec<char>]) {
    let name: Vec<char> = SCRATCH_NAME.chars().collect();
    for row in 1..lines.len() {
        let (above, below) = lines.split_at_mut(row);
        let (above, line) = (&mut above[row - 1], &mut below[0]);
        for start in 0..line.len() {
            if !(start > 0 && border(line[start - 1])) {
                continue;
            }
            let end = above[start.min(above.len())..]
                .iter()
                .position(|&c| border(c))
                .map_or(above.len(), |end| start + end);
            let held = above[..end]
                .iter()
                .rposition(|&c| c != ' ')
                .map_or(0, |at| at + 1);
            // The longest beginning of a scratch name the line above ends with.
            let Some(split) = (1..name.len()).rev().find(|&split| {
                held >= split
                    && (held == split || !above[held - split - 1].is_alphanumeric())
                    && above[held - split..held]
                        .iter()
                        .enumerate()
                        .all(|(at, &c)| c == name[at] || (at >= 4 && c.is_ascii_alphanumeric()))
            }) else {
                continue;
            };
            let rest = name.len() - split;
            let follows = start + rest <= line.len()
                && line[start..start + rest]
                    .iter()
                    .enumerate()
                    .all(|(at, &c)| {
                        let at = split + at;
                        c == name[at] || (at >= 4 && c.is_ascii_alphanumeric())
                    })
                && line.get(start + rest).is_none_or(|c| !c.is_alphanumeric());
            if follows {
                above[held - split..held].copy_from_slice(&name[..split]);
                line[start..start + rest].copy_from_slice(&name[split..]);
            }
        }
    }
}

/// A duration its width pushed onto a line of its own is put back where a short one fits,
/// and the rest of its pane moves up to where it would then be.
fn rejoin_durations(lines: &mut [String]) {
    let mut cells: Vec<Vec<char>> = lines.iter().map(|line| line.chars().collect()).collect();
    for row in 1..cells.len() {
        let starts: Vec<usize> = (1..cells[row].len())
            .filter(|&at| border(cells[row][at - 1]) && !border(cells[row][at]))
            .collect();
        for start in starts {
            let Some(width) = cells[row][start..].iter().position(|&c| border(c)) else {
                continue;
            };
            let pane = start..start + width;
            let framed = |line: &[char]| {
                line.len() > pane.end && line[start - 1] == '│' && line[pane.end] == '│'
            };
            let alone = cells[row][pane.clone()].iter().collect::<String>();
            if !framed(&cells[row - 1]) {
                continue;
            }
            let above: String = cells[row - 1][pane.clone()].iter().collect();
            let above = above.trim_end();
            let shown = alone.trim() == "<t>"
                && ["Active", "waited", "waiting", "took"]
                    .iter()
                    .any(|word| above.ends_with(word));
            // A dialog can hide where the wrapped duration went. "· waited" ends a line
            // only when its value did not fit.
            let hidden = alone.trim().is_empty() && above.ends_with("· waited");
            if !(shown || hidden) {
                continue;
            }
            // Like a short value in a full line, the placeholder may reach past the pane.
            let joined: Vec<char> = format!("{:<width$}", format!("{above} <t>"))
                .chars()
                .collect();
            cells[row - 1].splice(pane.clone(), joined);
            let mut at = row;
            while cells.get(at + 1).is_some_and(|line| framed(line)) {
                let below = cells[at + 1][pane.clone()].to_vec();
                cells[at][pane.clone()].copy_from_slice(&below);
                at += 1;
            }
            cells[at][pane.clone()].fill(' ');
        }
    }
    for (line, cells) in lines.iter_mut().zip(cells) {
        *line = cells.into_iter().collect();
    }
}

/// Replaces the `v` group of every match. The spaces around it become one, and the pane it
/// is in is padded back to its width, so a shorter or longer token reads the same.
fn replace_token(line: &str, rule: &Rule) -> String {
    let mut line = line.to_owned();
    let mut from = 0;
    while let Some(found) = line
        .get(from..)
        .and_then(|_| rule.pattern.captures_at(&line, from))
        .and_then(|captures| captures.name("v"))
    {
        let (start, end) = (found.start(), found.end());
        let before = line[..start].trim_end_matches(' ');
        let before = if before.len() < start {
            format!("{before} ")
        } else {
            before.to_owned()
        };
        let after = &line[end..];
        let pane_end = after.find(border).unwrap_or(after.len());
        let (pane, rest) = after.split_at(pane_end);
        let trimmed = pane.trim_start_matches(' ');
        let pane = if trimmed.len() < pane.len() {
            format!(" {trimmed}")
        } else {
            pane.to_owned()
        };
        // The width the pane had from the token on, restored at its end.
        let width = line[before.len().min(start)..end + pane_end]
            .chars()
            .count();
        let mut middle = format!("{}{pane}", rule.with);
        let have = middle.chars().count();
        if have < width {
            middle.push_str(&" ".repeat(width - have));
        } else {
            let spare = middle.len() - middle.trim_end_matches(' ').len();
            middle.truncate(middle.len() - spare.min(have - width));
        }
        from = before.len() + rule.with.len();
        line = format!("{before}{middle}{rest}");
    }
    line
}

/// The match with the digits of its `group` as zeros, so the layout stays.
fn zeroed(found: &Captures<'_>, group: &str) -> String {
    let whole = found.get(0).expect("whole match");
    let value = found.name(group).expect("volatile group");
    let (start, end) = (value.start() - whole.start(), value.end() - whole.start());
    let zeros: String = value
        .as_str()
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    format!(
        "{}{zeros}{}",
        &whole.as_str()[..start],
        &whole.as_str()[end..]
    )
}

/// The screen as it is stored and compared.
pub fn normalise(screen: &str, volatile: &Volatile<'_>) -> String {
    // The checkout goes first: it begins like a temporary directory under the same home.
    let mut lines: Vec<Vec<Cell>> = screen
        .lines()
        .map(|line| cells(&replace_checkout(line, volatile.checkout)))
        .collect();
    replace_paths(&mut lines, volatile.paths);
    let version = rule(
        &format!(r"(?P<v>\b{}\b)", regex::escape(volatile.version)),
        "<ver>",
    );
    let mut lines: Vec<Vec<char>> = lines
        .into_iter()
        .map(|line| line.into_iter().map(|cell| cell.shown).collect())
        .collect();
    wrapped_scratch(&mut lines);
    let simulated = screen.contains("(mock)") || screen.contains("demo workspace");
    let mut replaced = Vec::new();
    for line in lines {
        let mut line: String = line.into_iter().collect();
        line = SCRATCH
            .replace_all(&line, |found: &Captures<'_>| {
                let end = found.name("end").map_or("", |end| end.as_str());
                let random = found["v"].len() - end.len();
                format!("run-{}{end}", "0".repeat(random))
            })
            .into_owned();
        line = replace_token(&line, &version);
        for rule in SIMULATED.iter().filter(|_| simulated) {
            line = replace_token(&line, rule);
        }
        for rule in TOKENS.iter() {
            line = replace_token(&line, rule);
        }
        line = CUT_DURATION
            .replace_all(&line, |found: &Captures<'_>| zeroed(found, "v"))
            .into_owned();
        if screen.contains("storage_max_bytes") {
            line = MEASURED_STORAGE
                .replace_all(&line, |found: &Captures<'_>| zeroed(found, "v"))
                .into_owned();
        }
        line = HEX
            .replace_all(&line, |found: &Captures<'_>| {
                let hex = &found[0];
                // Decimal numbers and words are not hashes.
                if hex.bytes().any(|b| b.is_ascii_digit()) && hex.bytes().any(|b| b > b'9') {
                    "x".repeat(hex.len())
                } else {
                    hex.to_owned()
                }
            })
            .into_owned();
        replaced.push(line);
    }
    rejoin_durations(&mut replaced);
    let mut out = String::new();
    for line in &replaced {
        out.push_str(line.trim_end());
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}
