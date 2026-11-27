//! The golden screen set itself: its format, how differences read, what normalisation may
//! hide, and that tests keep screens only through the shared helper.
use crate::support::{
    self, gate,
    golden::{self, Input, Paths, Set},
    normalise::{self, Volatile},
};
use std::path::{Path, PathBuf};

const SIZE: (u16, u16) = (80, 24);

fn set(test: &str, screens: &[(&str, Input, &str)]) -> Set {
    let mut frames = Vec::new();
    for (step, input, text) in screens {
        golden::add(&mut frames, step, SIZE, *input, (*text).into()).unwrap();
    }
    Set::from([(test.into(), frames)])
}

#[test]
fn a_frame_shared_by_both_input_paths_is_stored_once_and_round_trips() {
    let recorded = set(
        "case.test",
        &[
            ("opened", Input::Keyboard, "same\n"),
            ("chosen", Input::Keyboard, "keys\n"),
            ("opened", Input::Mouse, "same\n"),
            ("chosen", Input::Mouse, "click\n"),
            ("a · label", Input::Mouse, "dotted\n"),
        ],
    );
    let frames = &recorded["case.test"];
    let paths: Vec<_> = frames.iter().map(|f| (f.step.as_str(), f.paths)).collect();
    assert_eq!(
        paths,
        [
            ("opened", Paths::Both),
            ("chosen", Paths::Keyboard),
            ("chosen", Paths::Mouse),
            ("a · label", Paths::Mouse)
        ]
    );
    let file = golden::render(frames);
    assert!(file.starts_with("@@ opened · 80x24 · keyboard+mouse\nsame\n"));
    assert_eq!(golden::parse(&file).unwrap(), *frames);
}

#[test]
fn a_repeated_step_is_numbered_so_no_screen_replaces_another() {
    let mut frames = Vec::new();
    let mut add = |input, text: &str, size| {
        golden::add(&mut frames, "step", size, input, text.into()).unwrap();
    };
    add(Input::Keyboard, "one\n", SIZE);
    add(Input::Keyboard, "two\n", SIZE);
    add(Input::Mouse, "one\n", SIZE);
    add(Input::Mouse, "other\n", SIZE);
    add(Input::Keyboard, "one\n", (120, 36));
    let stored: Vec<_> = frames
        .iter()
        .map(|f| (f.step.as_str(), f.size, f.paths, f.text.as_str()))
        .collect();
    assert_eq!(
        stored,
        [
            ("step", SIZE, Paths::Both, "one\n"),
            ("step #2", SIZE, Paths::Keyboard, "two\n"),
            ("step #2", SIZE, Paths::Mouse, "other\n"),
            ("step", (120, 36), Paths::Keyboard, "one\n"),
        ]
    );
    assert!(golden::add(&mut frames, "two\nlines", SIZE, Input::Mouse, "x\n".into()).is_err());
}

#[test]
fn changed_new_and_unproduced_screens_each_fail_with_a_readable_diff() {
    let stored = set("t", &[("a", Input::Keyboard, "title\nold line\nfoot\n")]);
    assert_eq!(golden::verify(&stored, &stored), Ok(1));

    let changed = set("t", &[("a", Input::Keyboard, "title\nnew line\nfoot\n")]);
    let error = golden::verify(&stored, &changed).unwrap_err();
    assert!(
        error.contains("CHANGED t · a · 80x24 · keyboard differs from its golden on lines 2")
            && error.contains("-   2 old line\n+   2 new line")
            && error.contains("make golden"),
        "{error}"
    );

    let error = golden::verify(&Set::new(), &changed).unwrap_err();
    assert!(
        error.contains("NEW t · a · 80x24 · keyboard has no golden") && error.contains("new line"),
        "{error}"
    );

    let error = golden::verify(&stored, &Set::new()).unwrap_err();
    assert!(
        error.contains("REMOVED t · a · 80x24 · keyboard is golden but no test produced it")
            && error.contains("old line"),
        "{error}"
    );
}

#[test]
fn screens_in_another_order_are_not_the_golden_file() {
    let one = [("a", Input::Keyboard, "a\n"), ("b", Input::Keyboard, "b\n")];
    let other = [one[1], one[0]];
    let error = golden::verify(&set("t", &one), &set("t", &other)).unwrap_err();
    assert!(error.contains("REORDERED t"), "{error}");
}

#[test]
fn the_report_holds_only_changed_screens_with_both_texts_and_counts_the_rest() {
    let approved = set(
        "t",
        &[
            ("kept", Input::Keyboard, "unchanged screen\n"),
            ("edited", Input::Keyboard, "before text\n"),
            ("gone", Input::Keyboard, "removed text\n"),
        ],
    );
    let produced = set(
        "t",
        &[
            ("kept", Input::Keyboard, "unchanged screen\n"),
            ("edited", Input::Keyboard, "after text\n"),
            ("added", Input::Mouse, "added text\n"),
        ],
    );
    let (comparison, report) = golden::report(&approved, &produced, "abc123");
    assert_eq!(
        comparison.summary(),
        "1 matched, 1 new, 1 changed, 1 removed"
    );
    let report = report.unwrap();
    for part in [
        "tests/golden at abc123",
        "1 matched, 1 new, 1 changed, 1 removed",
        "=== CHANGED t · edited · 80x24 · keyboard ===\n--- before\nbefore text\n",
        "+++ after\nafter text\n",
        "=== NEW t · added · 80x24 · mouse ===\n+++ after\nadded text\n",
        "=== REMOVED t · gone · 80x24 · keyboard ===\n--- before\nremoved text\n",
    ] {
        assert!(report.contains(part), "{part:?} missing:\n{report}");
    }
    assert!(!report.contains("unchanged screen"), "{report}");
    assert_eq!(golden::report(&approved, &approved, "abc123").1, None);
}

/// The gate of `make check` and `make golden` against a private repository: what it
/// writes, when it fails and that only `update` touches golden files.
#[test]
fn the_gate_reports_changes_against_the_committed_set_and_only_update_rewrites_it() {
    let sandbox = support::sandbox().unwrap();
    let project = sandbox.path().join("project");
    let recording = sandbox.path().join("recorded");
    let reports = sandbox.path().join("reports");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&recording).unwrap();
    support::empty_repository(&project).unwrap();
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    let record = |step: &str, text: &str| {
        let frames = set("ui", &[(step, Input::Keyboard, text)]);
        std::fs::write(recording.join("ui.screens"), golden::render(&frames["ui"])).unwrap();
    };
    let check = || gate::check(&project, &recording, &reports, "HEAD");
    let golden_file = project.join("tests/golden/ui.screens");

    // The first golden set is new against a commit that has none.
    record("opened", "first\n");
    let missing = check().unwrap_err();
    assert!(
        missing.contains("NEW ui · opened") && !golden_file.exists(),
        "{missing}"
    );
    gate::update(&project, &recording).unwrap();
    let said = check().unwrap().join("\n");
    assert!(said.contains("0 matched, 1 new"), "{said}");
    let report = std::fs::read_to_string(reports.join(gate::REPORT)).unwrap();
    assert!(report.contains("+++ after\nfirst\n"), "{report}");

    // Committed, it is approved: an unchanged tree leaves no report.
    git(&["add", "-f", "tests/golden"]);
    git(&["commit", "-qm", "approve"]);
    let said = check().unwrap().join("\n");
    assert!(
        said.contains("1 matched, 0 new, 0 changed, 0 removed"),
        "{said}"
    );
    assert_eq!(std::fs::read_dir(&reports).unwrap().count(), 0);

    // A changed screen fails, is reported, and the golden file stays as it was.
    record("opened", "second\n");
    let approved = std::fs::read_to_string(&golden_file).unwrap();
    let failed = check().unwrap_err();
    assert!(
        failed.contains("-   1 first\n+   1 second") && failed.contains("1 changed"),
        "{failed}"
    );
    assert_eq!(std::fs::read_to_string(&golden_file).unwrap(), approved);
    let report = std::fs::read_to_string(reports.join(gate::REPORT)).unwrap();
    assert!(
        report.contains("--- before\nfirst\n") && report.contains("+++ after\nsecond\n"),
        "{report}"
    );

    // Regenerated, the check passes and the change is still reported until it is committed.
    gate::update(&project, &recording).unwrap();
    check().unwrap();
    assert!(reports.join(gate::REPORT).is_file());

    // A golden no test produces any more fails until it is regenerated away.
    std::fs::rename(
        recording.join("ui.screens"),
        recording.join("other.screens"),
    )
    .unwrap();
    let gone = check().unwrap_err();
    assert!(gone.contains("REMOVED ui · opened"), "{gone}");
    gate::update(&project, &recording).unwrap();
    assert!(!golden_file.exists() && project.join("tests/golden/other.screens").is_file());
}

fn sources(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// Fixture shell commands run inside a fixture supervisor and may write to its artifact
/// directory as `$NAME`. Test code itself must never learn the real one.
#[test]
fn no_test_writes_evidence_outside_the_screen_helper() {
    // Spelled apart so this file does not match itself.
    let name = ["KTASK", "ARTIFACT", "DIR"].join("_");
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests"),
        &mut files,
    );
    assert!(files.len() > 20, "{files:?}");
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for (at, _) in text.match_indices(&name) {
            assert!(
                text[..at].ends_with('$'),
                "{}:{} reads {name}; keep screens with support::record and print anything else",
                file.display(),
                text[..at].lines().count()
            );
        }
    }
}

/// `make check` hands the suite no artifact directory, so a test cannot write there even
/// by accident; only the gate after it writes the report.
#[test]
fn the_suite_runs_without_the_artifact_directory() {
    let makefile =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Makefile")).unwrap();
    let name = ["KTASK", "ARTIFACT", "DIR"].join("_");
    assert!(makefile.contains(&format!("env -u {name} ")), "{makefile}");
    assert_eq!(makefile.matches("cargo test").count(), 1, "{makefile}");
    // Only `golden` rewrites the set, and `check` runs every test and then verifies.
    let (check, golden) = makefile
        .split_once("\ncheck:\n")
        .and_then(|(_, rest)| rest.split_once("\ngolden:\n"))
        .expect("check and golden targets");
    assert!(
        check.contains("$(SUITE) -- --show-output\n") && check.contains("-- verify \"$(SCREENS)\""),
        "{check}"
    );
    assert!(!check.contains("-- update"), "{check}");
    assert!(golden.contains("-- update \"$(SCREENS)\""), "{golden}");
}

fn committed() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// How many size and input path combinations of `step`, numbered repeats included, a
/// test's golden file holds.
fn covered(frames: &[golden::Frame], step: &str) -> usize {
    let repeat = format!("{step} #");
    frames
        .iter()
        .filter(|frame| {
            frame.step == step
                || frame
                    .step
                    .strip_prefix(&repeat)
                    .is_some_and(|n| n.parse::<u32>().is_ok())
        })
        .map(|frame| if frame.paths == Paths::Both { 2 } else { 1 })
        .sum()
}

/// `recorded-before.tsv` lists what the suite showed before the golden set existed: test,
/// step and how often. Delete a line only when its screen is deliberately gone.
#[test]
fn every_screen_recorded_before_the_golden_set_is_in_it() {
    let golden = golden::load(&committed()).unwrap();
    let list = std::fs::read_to_string(committed().join("recorded-before.tsv")).unwrap();
    let mut rows = 0;
    let mut missing = Vec::new();
    for row in list.lines().filter(|row| !row.starts_with('#')) {
        let fields: Vec<&str> = row.split('\t').collect();
        let [test, step, times] = fields[..] else {
            panic!("bad row {row:?}");
        };
        rows += 1;
        let file = golden::file_name(test);
        let have = golden
            .get(file.trim_end_matches(".screens"))
            .map_or(0, |frames| covered(frames, step));
        if have < times.parse().unwrap() {
            missing.push(format!("{test} · {step}: {have} of {times}"));
        }
    }
    assert!(rows > 900, "{rows}");
    assert!(
        missing.is_empty(),
        "screens dropped:\n{}",
        missing.join("\n")
    );
}

/// One frame as two runs would show it: everything volatile differs, nothing else does.
struct Run {
    temporary: &'static str,
    sandbox: &'static str,
    elapsed: &'static str,
    date: &'static str,
    hash: &'static str,
    epoch: &'static str,
    free: &'static str,
    version: &'static str,
    checkout: &'static str,
    pid: &'static str,
}

impl Run {
    fn normalised(&self) -> String {
        let sandbox = format!(
            "{}/{}/{}",
            self.temporary,
            normalise::sandboxes(self.temporary),
            self.sandbox
        );
        let pane = |text: String| format!("│{text:<46}│\n");
        let path: Vec<char> = format!("{sandbox}/project").chars().collect();
        let mut screen = String::new();
        // Whole in a wide pane, then wrapped and cut in a narrow one.
        screen.push_str(&format!("│{sandbox}/cache│\n"));
        for chunk in path.chunks(46) {
            screen.push_str(&pane(chunk.iter().collect()));
        }
        screen.push_str(&pane(path[..30].iter().collect()));
        screen.push_str(&pane(format!("Project: {}", self.sandbox)));
        screen.push_str(&pane(format!("Project: {}", &self.sandbox[..6])));
        screen.push_str(&pane(format!("Active {} · waited 2s", self.elapsed)));
        screen.push_str(&pane(format!(
            "Current #1 · Running · {}   |",
            self.elapsed
        )));
        screen.push_str(&pane(format!("took {} · time limit 30m 00s", self.elapsed)));
        screen.push_str(&pane(format!("Started {} · retry after 1s", self.date)));
        screen.push_str(&pane(format!("tree {} · {}", self.hash, self.epoch)));
        screen.push_str(&pane(format!("free {}", self.free)));
        // A dialog over the pane: a cut duration, and the end of a path past its border.
        screen.push_str(&format!("│waited {}│dialog│\n", &self.elapsed[..1]));
        // A scratch name wrapped inside its pane, and an installer operation.
        screen.push_str(&pane("cache/workers/r".into()));
        screen.push_str(&pane(format!("un-{}/sdk", &self.sandbox[4..])));
        screen.push_str(&pane(format!(
            "\"operation\": \"1-{}-{}-0\",",
            self.pid, self.epoch
        )));
        screen.push_str(&pane(format!("free bytes Ok({})", self.pid)));
        screen.push_str(&pane(format!("ktask RS · {} · ktask-dev", self.version)));
        let supervisor = format!("supervisor {}/target/debug/ktask-de", self.checkout);
        screen.push_str(&pane(supervisor.chars().take(46).collect()));
        normalise::normalise(
            &screen,
            &Volatile {
                paths: &normalise::sandbox_paths(&sandbox),
                checkout: self.checkout,
                version: self.version,
            },
        )
    }
}

#[test]
fn two_runs_of_one_screen_normalise_to_the_same_bytes() {
    let one = Run {
        temporary: "/home/etf/.cache/ktask/workers/run-AbCdEf",
        sandbox: "run-Zy98Xw",
        elapsed: "3s",
        date: "Nov 27 01:19",
        hash: "b7145bc34d3a17d2ba828c6433feca3c22ccee07",
        epoch: "1795771333141",
        free: "603.4 GiB of 951.3 GiB",
        version: "0.62.0",
        checkout: "/home/etf/Projects/ktask-rs",
        pid: "3307045",
    };
    let other = Run {
        temporary: "/tmp",
        sandbox: "run-0a4oob",
        elapsed: "1m 05s",
        date: "Dec 01 23:59",
        hash: "9200344be256f6ada601179b75d2e049933a054e",
        epoch: "1801111111111",
        free: "1.2 TiB of 1.8 TiB",
        version: "0.100.12",
        checkout: "/home/etf/work/a/longer/checkout/ktask-rs",
        pid: "41",
    };
    let (one, other) = (one.normalised(), other.normalised());
    assert_eq!(one, other, "{:?}", golden::line_diff(&one, &other).1);
    for kept in [
        "/ktask-test-scratch/run-000000/cache│",
        "Project: run-000000",
        "Project: run-00 ",
        "Active <t> · waited <t>",
        "Running · <t> |",
        "took <t> · time limit 30m 00s",
        "Started <date> · retry after 1s",
        "tree xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx · <epoch>",
        "free <size> of <size>",
        "waited 0│dialog",
        "cache/workers/r ",
        "│un-000000/sdk",
        "\"operation\": \"<id>\",",
        "free bytes Ok(<size>)",
        "ktask RS · <ver> · ktask-dev",
        "supervisor <checkout>",
    ] {
        assert!(one.contains(kept), "{kept:?} missing:\n{one}");
    }
}

#[test]
fn only_the_demo_timer_progress_is_read_as_progress() {
    let none = Volatile {
        paths: &[],
        checkout: "/nowhere/to/be/seen",
        version: "9.9.9",
    };
    let screen = "│Attempt 2 · step 1/6      │\n│Cost $0.09 (mock)         │\n";
    assert_eq!(
        normalise::normalise(screen, &none),
        "│Attempt 2 · step <n>/6    │\n│Cost <cost> (mock)        │\n"
    );
}

#[test]
fn a_duration_wrapped_by_its_width_reads_like_a_short_one() {
    let none = Volatile {
        paths: &[],
        checkout: "/nowhere/to/be/seen",
        version: "9.9.9",
    };
    let short = "│Active 1s · waited 2s │\n│3 calls               │\n│done                  │\n│                      │\n╰──────────────────────╯\n";
    let long = "│Active 12s · waited   │\n│10s                   │\n│3 calls               │\n│done                  │\n╰──────────────────────╯\n";
    let short = normalise::normalise(short, &none);
    assert_eq!(normalise::normalise(long, &none), short);
    assert!(short.starts_with("│Active <t> · waited <t>"), "{short}");

    // Beside a dialog, which hides the start of the line the duration wrapped onto.
    let short = "│dialog │ctive 1s · waited 2s│\n│       │3 calls             │\n│       │                    │\n╰───────╯\n";
    let long = "│dialog │ctive 12s · waited  │\n│       │                    │\n│       │3 calls             │\n╰───────╯\n";
    assert_eq!(
        normalise::normalise(long, &none),
        normalise::normalise(short, &none)
    );
}

#[test]
fn only_what_a_storage_pause_measured_reads_as_zeros() {
    let none = Volatile {
        paths: &[],
        checkout: "/nowhere/to/be/seen",
        version: "9.9.9",
    };
    let pause = |needed: u64, proposal: u64| {
        normalise::normalise(
            &format!(
                "│storage_max_bytes is 1048576 bytes, needed {needed} bytes │\n\
                 │+ Raise to {proposal} and resume · needed 16…│\n\
                 │{proposal}▏   │\n"
            ),
            &none,
        )
    };
    assert_eq!(pause(1_616_555, 4_604_998), pause(1_622_017, 4_621_382));
    assert_eq!(
        pause(1_616_555, 4_604_998),
        "│storage_max_bytes is 1048576 bytes, needed 0000000 bytes │\n\
         │+ Raise to 0000000 and resume · needed 00…│\n\
         │0000000▏   │\n"
    );
    let configured = "│artifact_file_bytes is 1024 bytes, needed 3000000 bytes │\n";
    assert_eq!(normalise::normalise(configured, &none), configured);
}

#[test]
fn normalisation_keeps_what_a_change_could_alter() {
    // A sandbox whose random name ends like the text of a help line.
    let paths =
        normalise::sandbox_paths(&format!("/tmp/{}/run-AbCdEj", normalise::sandboxes("/tmp")));
    let none = Volatile {
        paths: &paths,
        checkout: "/nowhere/to/be/seen",
        version: "9.9.9",
    };
    for screen in [
        "│Model: codex/operator · 15 min · $2.00     │\n",
        "│time limit 30m 00s · cost limit $5.00      │\n",
        "│automatic retry 1/2 after 1s               │\n",
        "│Check saved: build (timeout 600000 ms)     │\n",
        "│Defaults 17179869184 · #12 defaced         │\n",
        "│Resume refused                             │\n",
        "│Attempt 1 · step 6/6 · Cost $2.00          │\n",
        "│Help    ╯j/k or arrows: move               │\n",
        "│0123456789 is task output                  │\n",
        "│abcdefghij is task output                  │\n",
    ] {
        assert_eq!(normalise::normalise(screen, &none), screen);
    }
}
