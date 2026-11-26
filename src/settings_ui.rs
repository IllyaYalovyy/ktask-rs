//! Configuration's Export settings and Import settings forms: they write and read the same
//! file as the CLI, through the same application commands, and preview before importing.
use crate::{
    app::{Action, Screen},
    application::{settings::SettingsFile, transfer},
    dialog::{Dialog, DialogKind, Field},
    forms,
    persistent::StoredWorkspace,
    presentation::settings_report,
};
use std::path::Path;

const OVERWRITE_TITLE: &str = "Overwrite file?";

fn export_form(destination: &str) -> Dialog {
    Dialog::new(
        DialogKind::SettingsExport,
        "Export settings · disk",
        "Writes every setting of this project, and the shared settings it uses, to one file. Tasks have their own Export in Queue. Paths inside the project are written relative to it; tools, registrations and credentials stay here.\nKeep the file outside the project: an untracked file stops the next run.",
        vec![Field::text(forms::DESTINATION, destination)],
    )
}

fn import_form(source: &str) -> Dialog {
    Dialog::new(
        DialogKind::SettingsImport,
        "Import settings · disk",
        "Reads a file written by Export settings. A preview shows every setting before anything changes.\nImport replaces the settings the file names and keeps the rest; tasks and history stay.",
        vec![Field::text(forms::SOURCE, source)],
    )
}

fn overwrite_confirmation(dialog: &mut Dialog, path: &str, kind: DialogKind) {
    dialog.title = OVERWRITE_TITLE.into();
    dialog.body = format!("{path}\nConfirm replaces this file. Cancel keeps its contents.");
    dialog.kind = kind;
    dialog.fields.clear();
    dialog.error.clear();
}

fn preview(dialog: &mut Dialog, kind: DialogKind, body: String) {
    dialog.title = "Import settings preview".into();
    dialog.body = body;
    dialog.kind = kind;
    dialog.fields.clear();
    dialog.focus = 0;
    dialog.scroll = 0;
    dialog.error.clear();
}

impl StoredWorkspace {
    /// Beside the project folder, where the file does not dirty its worktree.
    fn default_settings_file(&self) -> Result<String, String> {
        let root = self.service.project_root().map_err(|e| e.to_string())?;
        let folder = root.file_name().unwrap_or_default().to_string_lossy();
        Ok(root
            .with_file_name(format!("{folder}-ktask-settings.json"))
            .to_string_lossy()
            .into_owned())
    }

    pub(crate) fn open_settings(&self, action: Action) -> Result<Dialog, String> {
        let file = self.default_settings_file()?;
        Ok(if action == Action::ExportSettings {
            export_form(&file)
        } else {
            import_form(&file)
        })
    }

    fn export_settings(
        &mut self,
        screen: &mut Screen,
        path: &str,
        expected: Option<&[u8]>,
    ) -> Result<(), String> {
        let paths = screen.instance.paths().map_err(|e| e.to_string())?;
        let export = self
            .service
            .export_settings(&paths)
            .map_err(|e| e.to_string())?;
        transfer::replace(Path::new(path), &export.text, expected).map_err(|e| e.to_string())?;
        screen.message(format!(
            "Exported {} settings to {path}",
            export.written.len()
        ));
        Ok(())
    }

    /// Ok(false) shows the preview or overwrite step.
    pub(crate) fn save_settings(
        &mut self,
        screen: &mut Screen,
        dialog: &mut Dialog,
    ) -> Result<bool, String> {
        match dialog.kind.clone() {
            DialogKind::SettingsExport => {
                let path = dialog.get(forms::DESTINATION).to_owned();
                if Path::new(&path).symlink_metadata().is_ok() {
                    let expected = transfer::read(Path::new(&path)).map_err(|e| e.to_string())?;
                    let kind = DialogKind::SettingsOverwrite {
                        path: path.clone(),
                        expected,
                    };
                    overwrite_confirmation(dialog, &path, kind);
                    return Ok(false);
                }
                self.export_settings(screen, &path, None)?;
            }
            DialogKind::SettingsOverwrite { path, expected } => {
                self.export_settings(screen, &path, Some(&expected))?;
            }
            DialogKind::SettingsImport => {
                let data = transfer::read(Path::new(dialog.get(forms::SOURCE)))
                    .map_err(|e| e.to_string())?;
                // Decoded first so a malformed file is reported on the form that named it.
                SettingsFile::decode(&data).map_err(|e| e.to_string())?;
                let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                let report = self
                    .service
                    .preview_settings(&paths, &data)
                    .map_err(|e| e.to_string())?;
                let body = settings_report(&report).join("\n");
                let kind = DialogKind::SettingsPreview {
                    data,
                    report: Box::new(report),
                };
                preview(dialog, kind, body);
                return Ok(false);
            }
            DialogKind::SettingsPreview { data, report } => {
                let paths = screen.instance.paths().map_err(|e| e.to_string())?;
                self.service
                    .import_settings(&paths, &data, &report)
                    .map_err(|e| e.to_string())?;
                self.load_preferences(screen)?;
                self.committed(screen, "Settings imported");
            }
            _ => return Err("Not a settings form".into()),
        }
        Ok(true)
    }
}
