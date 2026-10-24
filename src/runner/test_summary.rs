//! Test counts read from a check transcript: the summaries printed by Rust's test harness,
//! pytest, unittest (and Django), Jest, Vitest, Mocha and `node --test`. Only a framework's own
//! summary counts: it has the exact shape that framework prints and agrees with itself, so
//! quoted or indented copies of earlier results are not mistaken for this run.

use crate::application::CheckCounts;
use std::borrow::Cow;

const NONE: CheckCounts = CheckCounts {
    passed: 0,
    failed: 0,
    ignored: 0,
};

/// The sum of every framework summary in `transcript`; None when there is none.
pub(super) fn parse(transcript: &str) -> Option<CheckCounts> {
    let lines: Vec<Cow<str>> = transcript.lines().map(plain).collect();
    let mut total: Option<CheckCounts> = None;
    for (index, line) in lines.iter().enumerate() {
        let rest = &lines[index + 1..];
        let Some(next) = cargo(line)
            .or_else(|| pytest(line))
            .or_else(|| unittest(line, rest))
            .or_else(|| jest(line))
            .or_else(|| vitest(line))
            .or_else(|| mocha(line, rest))
            .or_else(|| node(line, rest))
        else {
            continue;
        };
        total = Some(match total {
            None => next,
            Some(prior) => add(prior, next)?,
        });
    }
    total
}

/// The line without terminal colour codes or trailing whitespace.
fn plain(line: &str) -> Cow<'_, str> {
    let line = line.trim_end();
    if !line.contains('\x1b') {
        return Cow::Borrowed(line);
    }
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.next() == Some('[') {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out.trim_end().to_owned())
}

fn add(a: CheckCounts, b: CheckCounts) -> Option<CheckCounts> {
    Some(CheckCounts {
        passed: a.passed.checked_add(b.passed)?,
        failed: a.failed.checked_add(b.failed)?,
        ignored: a.ignored.checked_add(b.ignored)?,
    })
}

fn number(text: &str) -> Option<i64> {
    text.parse::<u32>().ok().map(i64::from)
}

#[derive(Clone, Copy)]
enum Kind {
    Passed,
    Failed,
    Ignored,
    /// Printed, but not a test outcome (warnings, deselected tests).
    Uncounted,
}

/// "N kind<sep>N kind..." with every kind known; the counts and the number of tests listed.
fn items(text: &str, separator: &str, kinds: &[(&str, Kind)]) -> Option<(CheckCounts, i64)> {
    let mut counts = NONE;
    let mut listed = 0;
    for item in text.split(separator) {
        let (n, name) = item.split_once(' ')?;
        let n = number(n)?;
        let (_, kind) = kinds.iter().find(|(known, _)| *known == name)?;
        let field = match kind {
            Kind::Passed => &mut counts.passed,
            Kind::Failed => &mut counts.failed,
            Kind::Ignored => &mut counts.ignored,
            Kind::Uncounted => continue,
        };
        *field += n;
        listed += n;
    }
    Some((counts, listed))
}

/// `test result: ok. 3 passed; 0 failed; 1 ignored; ...`, its verdict agreeing with its count.
fn cargo(line: &str) -> Option<CheckCounts> {
    let (verdict, counts) = line.strip_prefix("test result: ")?.split_once(". ")?;
    let mut fields = counts.split("; ");
    let mut next = |kind: &str| {
        let (n, name) = fields.next()?.split_once(' ')?;
        (name == kind).then(|| number(n))?
    };
    let counts = CheckCounts {
        passed: next("passed")?,
        failed: next("failed")?,
        ignored: next("ignored")?,
    };
    let genuine = match verdict {
        "ok" => counts.failed == 0,
        "FAILED" => counts.failed > 0,
        _ => false,
    };
    genuine.then_some(counts)
}

/// `===== 1 failed, 2 passed, 1 skipped in 0.12s =====`, or the same unpadded under `-q`.
fn pytest(line: &str) -> Option<CheckCounts> {
    let body = if line.starts_with('=') {
        line.trim_start_matches('=')
            .strip_prefix(' ')?
            .trim_end_matches('=')
            .strip_suffix(' ')?
    } else {
        line
    };
    let (outcomes, time) = body.rsplit_once(" in ")?;
    let seconds = match time.split_once(" (") {
        Some((seconds, clock)) => clock.ends_with(')').then_some(seconds)?,
        None => time,
    };
    seconds.strip_suffix('s')?.parse::<f64>().ok()?;
    if outcomes == "no tests ran" {
        return Some(NONE);
    }
    use Kind::*;
    let kinds = [
        ("passed", Passed),
        ("xpassed", Passed),
        ("failed", Failed),
        ("error", Failed),
        ("errors", Failed),
        ("skipped", Ignored),
        ("xfailed", Ignored),
        ("deselected", Uncounted),
        ("warning", Uncounted),
        ("warnings", Uncounted),
        ("rerun", Uncounted),
    ];
    items(outcomes, ", ", &kinds).map(|(counts, _)| counts)
}

/// `Ran 6 tests in 0.001s`, then `OK`, `FAILED (failures=1, errors=1, skipped=1, ...)` or
/// `NO TESTS RAN` as the next printed line.
fn unittest(line: &str, rest: &[Cow<str>]) -> Option<CheckCounts> {
    let (ran, time) = line.strip_prefix("Ran ")?.split_once(" in ")?;
    let ran = number(
        ran.strip_suffix(" tests")
            .or_else(|| ran.strip_suffix(" test"))?,
    )?;
    time.strip_suffix('s')?.parse::<f64>().ok()?;
    let verdict = rest.iter().find(|line| !line.is_empty())?;
    let (verdict, details) = match verdict.split_once(" (") {
        Some((verdict, details)) => (verdict, details.strip_suffix(')')?),
        None => (verdict.as_ref(), ""),
    };
    let mut counts = NONE;
    let mut unexpected = 0;
    for detail in details.split(", ").filter(|d| !d.is_empty()) {
        let (kind, n) = detail.split_once('=')?;
        let n = number(n)?;
        match kind {
            "failures" | "errors" => counts.failed += n,
            "skipped" | "expected failures" => counts.ignored += n,
            "unexpected successes" => unexpected += n,
            _ => return None,
        }
    }
    // Python 3.11+ fails a run with an unexpected success; earlier versions reported it as OK.
    match verdict {
        "OK" if counts.failed == 0 => {}
        "FAILED" if counts.failed + unexpected > 0 => counts.failed += unexpected,
        "NO TESTS RAN" if ran == 0 => {}
        _ => return None,
    }
    counts.passed = ran - counts.failed - counts.ignored;
    (counts.passed >= 0).then_some(counts)
}

/// `Tests:       1 failed, 1 skipped, 1 todo, 3 passed, 6 total`.
fn jest(line: &str) -> Option<CheckCounts> {
    let body = line.strip_prefix("Tests:")?.trim_start();
    let (outcomes, total) = body.rsplit_once(", ").unwrap_or(("", body));
    let total = number(total.strip_suffix(" total")?)?;
    use Kind::*;
    let kinds = [
        ("passed", Passed),
        ("failed", Failed),
        ("skipped", Ignored),
        ("todo", Ignored),
    ];
    let (counts, listed) = if outcomes.is_empty() {
        (NONE, 0)
    } else {
        items(outcomes, ", ", &kinds)?
    };
    (listed == total).then_some(counts)
}

/// `      Tests  1 failed | 1 passed | 1 skipped | 1 todo (4)`.
fn vitest(line: &str) -> Option<CheckCounts> {
    let (outcomes, total) = line
        .trim_start()
        .strip_prefix("Tests  ")?
        .trim_start()
        .rsplit_once(" (")?;
    let total = number(total.strip_suffix(')')?)?;
    use Kind::*;
    let kinds = [
        ("passed", Passed),
        ("failed", Failed),
        ("skipped", Ignored),
        ("todo", Ignored),
    ];
    let (counts, listed) = items(outcomes, " | ", &kinds)?;
    (listed == total).then_some(counts)
}

/// `  1 passing (9ms)`, then optional `  1 pending` and `  1 failing` lines.
fn mocha(line: &str, rest: &[Cow<str>]) -> Option<CheckCounts> {
    let (passing, time) = line.strip_prefix("  ")?.split_once(" passing (")?;
    time.strip_suffix(')')?;
    let mut counts = CheckCounts {
        passed: number(passing)?,
        ..NONE
    };
    for line in rest.iter().take(2) {
        if let Some(n) = line
            .strip_prefix("  ")
            .and_then(|l| l.strip_suffix(" pending"))
        {
            counts.ignored = number(n)?;
        } else if let Some(n) = line
            .strip_prefix("  ")
            .and_then(|l| l.strip_suffix(" failing"))
        {
            counts.failed = number(n)?;
        }
    }
    Some(counts)
}

/// The TAP footer of `node --test`: `# tests 4`, `# suites 0`, `# pass 1`, `# fail 1`,
/// `# cancelled 0`, `# skipped 1`, `# todo 1`, one per line.
fn node(line: &str, rest: &[Cow<str>]) -> Option<CheckCounts> {
    number(line.strip_prefix("# tests ")?)?;
    let mut fields = rest.iter();
    let mut next = |kind: &str| number(fields.next()?.strip_prefix("# ")?.strip_prefix(kind)?);
    next("suites ")?;
    let passed = next("pass ")?;
    let failed = next("fail ")?.checked_add(next("cancelled ")?)?;
    let ignored = next("skipped ")?.checked_add(next("todo ")?)?;
    Some(CheckCounts {
        passed,
        failed,
        ignored,
    })
}

#[cfg(test)]
mod tests {
    use super::parse;
    use crate::application::CheckCounts;

    fn counts(passed: i64, failed: i64, ignored: i64) -> Option<CheckCounts> {
        Some(CheckCounts {
            passed,
            failed,
            ignored,
        })
    }

    #[test]
    fn rust_summaries_count_only_when_genuine() {
        let transcript = "\
running 4 tests
test result: ok. 3 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.1s
    test result: FAILED. 0 passed; 9 failed; 0 ignored; quoted from an old log
test result: ok. 0 passed; 2 failed; 0 ignored; contradicts its own verdict
test result: FAILED. 4 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\r
";
        assert_eq!(parse(transcript), counts(7, 1, 1));
        assert_eq!(parse("no harness output"), None);
    }

    /// Summaries captured from the real frameworks (pytest 8, Python 3.14 unittest, Jest 30,
    /// Vitest 3, Mocha 11, Node 22), each with passing, failing and skipped tests.
    #[test]
    fn python_and_javascript_summaries_are_counted() {
        let cases = [
            (
                "FAILED tests/test_calc.py::test_bad - assert 2 == 3\n\
                 ========== 1 failed, 1 passed, 1 skipped, 1 xfailed, 1 error in 0.04s ==========",
                counts(1, 2, 2),
            ),
            (
                "1 failed, 1 passed, 1 skipped, 1 xfailed, 1 error in 0.03s",
                counts(1, 2, 2),
            ),
            (
                "======================= 1 passed, 4 deselected in 0.02s ========================",
                counts(1, 0, 0),
            ),
            (
                "=== 3 passed, 1 warning in 75.12s (0:01:15) ===",
                counts(3, 0, 0),
            ),
            (
                "-----\nRan 6 tests in 0.001s\n\nFAILED (failures=1, errors=1, skipped=1, \
                 expected failures=1, unexpected successes=1)",
                counts(1, 3, 2),
            ),
            ("Ran 1 test in 0.000s\n\nOK", counts(1, 0, 0)),
            ("Ran 3 tests in 0.010s\n\nOK (skipped=1)", counts(2, 0, 1)),
            ("Ran 0 tests in 0.000s\n\nNO TESTS RAN", counts(0, 0, 0)),
            (
                "Test Suites: 1 failed, 1 passed, 2 total\n\
                 Tests:       2 failed, 2 skipped, 1 todo, 3 passed, 8 total\n\
                 Snapshots:   0 total",
                counts(3, 2, 3),
            ),
            ("Tests:       1 passed, 1 total", counts(1, 0, 0)),
            (
                " Test Files  1 failed (1)\n      Tests  1 failed | 1 passed | 1 skipped | 1 todo (4)",
                counts(1, 1, 2),
            ),
            (
                "\x1b[2m      Tests \x1b[22m \x1b[1m\x1b[31m1 failed\x1b[39m\x1b[22m\x1b[2m | \
                 \x1b[22m\x1b[1m\x1b[32m1 passed\x1b[39m\x1b[22m\x1b[90m (2)\x1b[39m",
                counts(1, 1, 0),
            ),
            (
                "  ✔ ok\n  1) bad\n\n  1 passing (9ms)\n  1 pending\n  1 failing\n\n  1) bad:",
                counts(1, 1, 1),
            ),
            ("  12 passing (40ms)\n", counts(12, 0, 0)),
            (
                "1..4\n# tests 4\n# suites 0\n# pass 1\n# fail 1\n# cancelled 0\n# skipped 1\n\
                 # todo 1\n# duration_ms 247.599148",
                counts(1, 1, 2),
            ),
        ];
        for (transcript, expected) in cases {
            assert_eq!(parse(transcript), expected, "{transcript}");
        }
    }

    #[test]
    fn lookalike_lines_are_not_summaries() {
        for transcript in [
            "Ran 3 tests in 0.1s\n\nOK (failures=1)",
            "Ran 3 tests in 0.1s\n\nFAILED",
            "Ran 2 tests in 0.1s\n\nFAILED (failures=3)",
            "Tests:       1 failed, 3 passed, 5 total",
            "      Tests  1 failed | 1 passed (3)",
            "    1 failed, 1 passed in 0.03s",
            "1 failed, 1 passed in the morning",
            "1 failed, 1 exploded in 0.03s",
            "== 1 passed in 0.1s",
            "# tests 4\n# pass 4",
            "    3 passing (9ms)",
        ] {
            assert_eq!(parse(transcript), None, "{transcript}");
        }
    }

    #[test]
    fn summaries_of_several_frameworks_add_up() {
        let transcript = "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured\n\
                          ===== 2 passed, 1 skipped in 0.5s =====\n\
                          Tests:       4 passed, 4 total";
        assert_eq!(parse(transcript), counts(9, 0, 1));
    }
}
