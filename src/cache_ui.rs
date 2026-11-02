//! Configuration → Build cache: inventory, limits, cleanup and opt-in adoption through the
//! same `build_cache` commands as the CLI.
use crate::{
    app::App,
    build_cache::{self, Policy},
    dialog::{BuildCachePlan, Dialog, DialogKind, Field},
};

const SAVE: &str = "Save limits";
const CLEAN: &str = "Clean obsolete";
const ADOPT: &str = "Adopt project target";

fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

impl App {
    pub(crate) fn open_build_cache(&mut self) -> Result<(), String> {
        let paths = self.instance.paths().map_err(|e| e.to_string())?;
        let root = self
            .service
            .as_ref()
            .unwrap()
            .project_root()
            .map_err(|e| e.to_string())?;
        let inv = build_cache::inventory(&paths, &root).map_err(|e| e.to_string())?;
        let mut body = format!(
            "Managed: {} in {} files (limit {} / {} files per cache) · free {} of {}\n",
            size(inv.managed_bytes),
            inv.managed_files,
            size(inv.policy.max_bytes),
            inv.policy.max_files,
            size(inv.free_bytes),
            size(inv.total_bytes),
        );
        for (title, entries) in [
            ("Owned", &inv.owned),
            ("Not managed (opt in to clean)", &inv.unowned),
            ("Provider storage (never cleaned)", &inv.provider),
        ] {
            if entries.is_empty() {
                continue;
            }
            body.push_str(&format!("{title}:\n"));
            for e in entries {
                body.push_str(&format!(
                    "  {} · {} · {} files · {}\n",
                    e.path.display(),
                    size(e.bytes),
                    e.files,
                    e.note
                ));
            }
        }
        for line in &inv.pressure {
            body.push_str(&format!("Pressure: {line}\n"));
        }
        body.push_str(&format!("Never touched: {}\n", inv.exclusions.join("; ")));
        let policy = inv.policy;
        self.dialog = Some(Dialog::new(
            DialogKind::BuildCache(policy),
            "Build cache",
            &body,
            vec![
                Field::text("Maximum bytes per cache", &policy.max_bytes.to_string()),
                Field::text("Maximum files per cache", &policy.max_files.to_string()),
                Field::text(
                    "Warm generations kept",
                    &policy.keep_generations.to_string(),
                ),
                Field::choice("Action", &[SAVE, CLEAN, ADOPT], SAVE),
            ],
        ));
        Ok(())
    }

    /// Returns true when the dialog is finished.
    pub(crate) fn save_build_cache(&mut self, dialog: &mut Dialog) -> Result<bool, String> {
        let paths = self.instance.paths().map_err(|e| e.to_string())?;
        let root = self
            .service
            .as_ref()
            .unwrap()
            .project_root()
            .map_err(|e| e.to_string())?;
        match &dialog.kind {
            DialogKind::BuildCache(expected) => {
                let number = |index: usize, what: &str| -> Result<u64, String> {
                    dialog
                        .value(index)
                        .trim()
                        .parse()
                        .map_err(|_| format!("{what} must be a positive integer"))
                };
                let next = Policy {
                    max_bytes: number(0, "Bytes")?,
                    max_files: number(1, "Files")?,
                    keep_generations: number(2, "Generations")?,
                    ..*expected
                };
                match dialog.value(3) {
                    CLEAN | ADOPT if next != *expected => {
                        Err("Save the changed limits first, then reopen Build cache".into())
                    }
                    CLEAN => {
                        let preview =
                            build_cache::clean(&paths, false).map_err(|e| e.to_string())?;
                        let mut body = String::from(
                            "Retire obsolete generations and abandoned scratch in owned caches. Active builds, open or running files, fresh output, final binaries and the installed supervisor are kept.\n\n",
                        );
                        for r in &preview.reports {
                            body.push_str(&match &r.deferred {
                                Some(why) => format!("{}: deferred — {why}\n", r.root.display()),
                                None => format!(
                                    "{}: {} generation(s), {} to free ({} → {}){}\n",
                                    r.root.display(),
                                    r.removed_generations,
                                    size(r.freed_bytes),
                                    size(r.before_bytes),
                                    size(r.after_bytes),
                                    if r.over_capacity {
                                        " · still over limit; only warm output remains"
                                    } else {
                                        ""
                                    },
                                ),
                            });
                        }
                        if preview.reports.is_empty() {
                            body.push_str(
                                "No cache is owned yet; adopt the project target first.\n",
                            );
                        }
                        confirm(dialog, BuildCachePlan::Clean, "Clean build caches?", body);
                        Ok(false)
                    }
                    ADOPT => {
                        let target = root.join("target");
                        let body = format!(
                            "Take ownership of {}?\nktask will write an ownership marker there and may delete obsolete build generations from it. It must be a Cargo target directory; the supervisor executable, sources and Git data are never touched.",
                            target.display()
                        );
                        confirm(
                            dialog,
                            BuildCachePlan::Adopt(target),
                            "Adopt build cache?",
                            body,
                        );
                        Ok(false)
                    }
                    _ => {
                        build_cache::set_policy(&paths, *expected, next)
                            .map_err(|e| e.to_string())?;
                        self.message("Build cache limits saved");
                        Ok(true)
                    }
                }
            }
            DialogKind::BuildCacheConfirm(plan) => {
                let message = match plan {
                    BuildCachePlan::Clean => {
                        let done = build_cache::clean(&paths, true).map_err(|e| e.to_string())?;
                        let freed: u64 = done.reports.iter().map(|r| r.freed_bytes).sum();
                        format!(
                            "Build cache cleaned: {} freed, {} abandoned scratch run(s) reclaimed",
                            size(freed),
                            done.scratch_reclaimed
                        )
                    }
                    BuildCachePlan::Adopt(path) => {
                        let claim =
                            build_cache::adopt(&paths, &root, path).map_err(|e| e.to_string())?;
                        format!("Build cache owned: {}", claim.root.display())
                    }
                };
                self.message(message);
                Ok(true)
            }
            _ => Ok(true),
        }
    }
}

fn confirm(dialog: &mut Dialog, plan: BuildCachePlan, title: &str, body: String) {
    dialog.kind = DialogKind::BuildCacheConfirm(plan);
    dialog.title = title.into();
    dialog.body = body;
    dialog.fields.clear();
    dialog.focus = 0;
    dialog.scroll = 0;
    dialog.error.clear();
}
