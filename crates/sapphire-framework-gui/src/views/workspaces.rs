use egui::{Align, Color32, Layout};

use crate::client::{Command, CommandOutput};

use super::model::{Badge, badge, bring_target, display_name, display_path, new_target_ok, others};
use super::{ViewCtx, error_line, unavailable};

/// The workspace screen: the one workspace the server serves (#215), and the ways to
/// switch it — open another folder, create one, or bring one from the workgroup.
#[derive(Default)]
pub struct WorkspacePicker {
    error: Option<String>,
}

impl WorkspacePicker {
    /// Render.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let mut out = None;
        ui.horizontal(|ui| {
            ui.heading("Workspace");
            if cx.busy {
                ui.spinner();
            }
        });
        ui.separator();
        error_line(ui, &mut self.error);
        let Some(server) = cx.snapshot.server.up() else {
            unavailable(ui, &format!("The {} server", cx.app_name));
            return out;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            match &server.current {
                None => {
                    ui.label(
                        "No workspace yet. Open an existing folder, create a new one, or bring one from the workgroup.",
                    );
                }
                Some(current) => {
                    let b = badge(current);
                    egui::Frame::group(ui.style()).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.horizontal(|ui| {
                                    ui.strong(display_name(current));
                                    let colour = match b {
                                        Badge::Syncing { .. } => Color32::GREEN,
                                        Badge::NotSynced => Color32::GRAY,
                                        _ => Color32::YELLOW,
                                    };
                                    ui.colored_label(colour, b.label());
                                });
                                ui.small(display_path(&current.root).display().to_string());
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add_enabled(current.reachable, egui::Button::new("Open folder"))
                                    .clicked()
                                {
                                    open_folder(&display_path(&current.root));
                                }
                                let (label, cmd) = if current.sync.enabled {
                                    ("Stop sync", Command::SyncDisable)
                                } else {
                                    ("Start sync", Command::SyncEnable)
                                };
                                if ui
                                    .add_enabled(
                                        current.reachable && !cx.busy,
                                        egui::Button::new(label),
                                    )
                                    .clicked()
                                {
                                    out = Some(cmd);
                                }
                            });
                        });
                    });
                }
            }

            ui.add_space(12.0);
            ui.strong("Switch to another workspace");
            ui.small("Only the selected workspace syncs on this device.");
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!cx.busy, egui::Button::new("Open folder…"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Open a workspace folder")
                        .pick_folder()
                {
                    out = Some(if dir.join(format!(".{}", cx.app_name)).is_dir() {
                        Command::WorkspaceSelect { dir }
                    } else {
                        // A folder that is not a workspace yet becomes one.
                        Command::WorkspaceInit { dir, sync: false }
                    });
                }
                if ui
                    .add_enabled(!cx.busy, egui::Button::new("New…"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Choose an empty folder for the new workspace")
                        .pick_folder()
                {
                    match new_target_ok(&dir, cx.app_name) {
                        Ok(()) => out = Some(Command::WorkspaceInit { dir, sync: true }),
                        Err(e) => self.error = Some(e),
                    }
                }
            });

            ui.add_space(12.0);
            ui.strong("From the workgroup");
            match cx.snapshot.bridge.up() {
                None => {
                    ui.small("The bridge is not available.");
                }
                Some(bridge) => {
                    let elsewhere = others(&bridge.ledger, server.current.as_ref(), cx.app_name);
                    if elsewhere.is_empty() {
                        ui.small("No other workspaces in the workgroup.");
                    }
                    for w in elsewhere {
                        egui::Frame::group(ui.style()).show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.strong(&w.name);
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui
                                        .add_enabled(
                                            !cx.busy,
                                            egui::Button::new("Bring to this host…"),
                                        )
                                        .clicked()
                                        && let Some(dir) = rfd::FileDialog::new()
                                            .set_title(format!(
                                                "Choose where to put {0} (a folder named {0} is created inside)",
                                                w.name
                                            ))
                                            .pick_folder()
                                    {
                                        out = Some(Command::WorkspaceMap {
                                            workspace_id: w.workspace_id,
                                            dir: bring_target(&dir, &w.name, cx.app_name),
                                        });
                                    }
                                });
                            });
                        });
                    }
                }
            }
        });
        out
    }

    /// Show a failure on the error line.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        if let Err(e) = result {
            self.error = Some(e.clone());
        }
    }
}

/// Open `path` in the platform's file manager. Failures are ignored: it is a convenience.
fn open_folder(path: &std::path::Path) {
    #[cfg(windows)]
    let program = "explorer";
    #[cfg(target_os = "macos")]
    let program = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let program = "xdg-open";
    let _ = std::process::Command::new(program).arg(path).spawn();
}
