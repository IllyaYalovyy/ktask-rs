//! Rendering a project's settings, and the one just changed by `settings set`.

use std::io::Write;

use ktask_core::SettingView;
use serde::Serialize;

/// One setting as `settings --json` shows it.
#[derive(Debug, Serialize)]
struct SettingJson<'a> {
    name: &'a str,
    value: &'a str,
    default: bool,
}

/// Writes `views`: one `name<TAB>value<TAB>default|custom` line each, or a JSON array with
/// `json`.
pub(crate) fn settings(
    views: &[SettingView],
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown: Vec<_> = views
            .iter()
            .map(|view| SettingJson {
                name: view.name,
                value: &view.value,
                default: view.is_default,
            })
            .collect();
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        views.iter().try_for_each(|view| {
            let kind = if view.is_default { "default" } else { "custom" };
            writeln!(out, "{}\t{}\t{kind}", view.name, view.value).map_err(|e| e.to_string())
        })
    }
}

/// Writes `view`, the setting `settings set` has just changed: a `name<TAB>value` line, or
/// a JSON object with `json`.
pub(crate) fn setting_set(
    view: &SettingView,
    json: bool,
    out: &mut impl Write,
) -> Result<(), String> {
    if json {
        let shown = SettingJson {
            name: view.name,
            value: &view.value,
            default: view.is_default,
        };
        serde_json::to_writer(&mut *out, &shown).map_err(|e| e.to_string())?;
        writeln!(out).map_err(|e| e.to_string())
    } else {
        writeln!(out, "{}\t{}", view.name, view.value).map_err(|e| e.to_string())
    }
}
