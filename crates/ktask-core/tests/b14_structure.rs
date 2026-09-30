//! The structural rules of docs/CODER.md's task B-14: core exposes one function for adding a
//! task, and each project setting is described once — by [`ktask_core::settings::setting_specs`]
//! (private to that module) — which `show_settings` and `set_setting` both walk rather than
//! listing settings by hand.

use std::path::Path;

/// `ktask-core/src/task/use_cases.rs`'s own source, where `add_task` is defined.
fn task_source() -> std::io::Result<String> {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/task/use_cases.rs"))
}

/// `ktask-core/src/settings/mod.rs`'s own source.
fn settings_source() -> std::io::Result<String> {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/settings/mod.rs"))
}

/// The body of the first `#[cfg(test)]` module in `text`, cut off, so a check does not trip
/// over a name a test itself happens to mention.
fn without_tests(text: &str) -> &str {
    text.find("#[cfg(test)]").map_or(text, |at| &text[..at])
}

/// Core exposes exactly one *public* function for adding a task: `add_task`. An earlier
/// version also had `add_task_listing_problems`, doing the same thing with a different error
/// shape; that name must not come back. `add_tasks` (plural), which both `add_task` and
/// `import` share to add a batch of drafts together, is `pub(crate)` — internal, not part of
/// this surface.
#[test]
fn core_exposes_one_function_for_adding_a_task() -> std::io::Result<()> {
    let source = without_tests(&task_source()?).to_owned();
    let public_add_task_fns: Vec<&str> = source
        .lines()
        .filter(|line| line.trim_start().starts_with("pub fn add_task"))
        .collect();
    assert_eq!(
        public_add_task_fns,
        vec!["pub fn add_task("],
        "expected exactly one public add-task function, named add_task: {public_add_task_fns:?}"
    );
    Ok(())
}

/// Every setting's name, as [`ktask_core::settings`] declares it as a constant.
fn setting_name_constants() -> Vec<&'static str> {
    vec![
        "ATTEMPT_TIMEOUT",
        "HEALTH_CHECK",
        "TRACKED_BRANCH",
        "STEP_SYNC",
        "STEP_HEALTH_CHECK",
        "STEP_REVIEW",
        "STEP_TESTING",
        "STEP_COMMIT",
        "STEP_PUSH",
    ]
}

/// The body of the function `source` declares as `pub fn <name>(`, from its signature to its
/// closing brace — matched by the first line back at the same indentation as a bare `}`,
/// which is how every top-level function in this file ends.
fn function_body<'a>(source: &'a str, name: &str) -> &'a str {
    let signature = format!("pub fn {name}(");
    let Some(start) = source.find(&signature) else {
        return "";
    };
    let Some(relative_end) = source[start..].find("\n}\n") else {
        return &source[start..];
    };
    &source[start..start + relative_end]
}

/// `show_settings` and `set_setting` each describe every setting once, in
/// `settings::setting_specs` — neither hardcodes a setting's own name constant itself, which
/// would mean the setting was described a second time.
#[test]
fn show_and_set_settings_do_not_hardcode_a_settings_name() -> std::io::Result<()> {
    let source = without_tests(&settings_source()?).to_owned();
    let show_body = function_body(&source, "show_settings");
    let set_body = function_body(&source, "set_setting");
    assert!(!show_body.is_empty(), "show_settings not found");
    assert!(!set_body.is_empty(), "set_setting not found");

    let mut offenders = Vec::new();
    for name in setting_name_constants() {
        if show_body.contains(name) {
            offenders.push(format!("show_settings mentions {name}"));
        }
        if set_body.contains(name) {
            offenders.push(format!("set_setting mentions {name}"));
        }
    }
    assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    Ok(())
}
