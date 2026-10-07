use egui::{Align, Color32, Layout};

use crate::client::{Command, CommandOutput};

use super::model::{Badge, badge, display_name, remote_only};
use super::{ViewCtx, error_line, unavailable};

/// The workspace screen: this host's workspaces, then the workgroup's that are not here.
#[derive(Default)]
pub struct WorkspaceList {
    error: Option<String>,
}

impl WorkspaceList {
    /// Render.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let mut out = None;
        let server_up = cx.snapshot.server.up().is_some();
        ui.horizontal(|ui| {
            ui.heading("Workspaces");
            if cx.busy {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui
                    .add_enabled(server_up && !cx.busy, egui::Button::new("Add existing…"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Add a workspace folder")
                        .pick_folder()
                {
                    out = Some(Command::WorkspaceInit { dir, sync: false });
                }
                if ui
                    .add_enabled(server_up && !cx.busy, egui::Button::new("New…"))
                    .clicked()
                    && let Some(dir) = rfd::FileDialog::new()
                        .set_title("Choose a folder for the new workspace")
                        .pick_folder()
                {
                    out = Some(Command::WorkspaceInit { dir, sync: true });
                }
            });
        });
        ui.separator();
        error_line(ui, &mut self.error);
        let Some(server) = cx.snapshot.server.up() else {
            unavailable(ui, &format!("The {} server", cx.app_name));
            return out;
        };

        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.strong("This host");
            if server.workspaces.is_empty() {
                ui.label(
                    "No workspaces yet. Create one, add an existing folder, or bring one from the workgroup.",
                );
            }
            for entry in &server.workspaces {
                let b = badge(entry);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.horizontal(|ui| {
                                ui.strong(display_name(entry));
                                let colour = match b {
                                    Badge::Syncing { .. } => Color32::GREEN,
                                    Badge::NotSynced => Color32::GRAY,
                                    _ => Color32::YELLOW,
                                };
                                ui.colored_label(colour, b.label());
                            });
                            ui.small(entry.root.display().to_string());
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(!cx.busy, egui::Button::new("Remove from list"))
                                .clicked()
                            {
                                out = Some(Command::WorkspaceForget {
                                    id: entry.id.clone(),
                                });
                            }
                            if ui
                                .add_enabled(entry.reachable, egui::Button::new("Open folder"))
                                .clicked()
                            {
                                open_folder(&entry.root);
                            }
                            let (label, cmd) = if entry.sync.enabled {
                                (
                                    "Stop sync",
                                    Command::SyncDisable {
                                        root: entry.root.clone(),
                                    },
                                )
                            } else {
                                (
                                    "Start sync",
                                    Command::SyncEnable {
                                        root: entry.root.clone(),
                                    },
                                )
                            };
                            if ui
                                .add_enabled(entry.reachable && !cx.busy, egui::Button::new(label))
                                .clicked()
                            {
                                out = Some(cmd);
                            }
                        });
                    });
                });
            }

            ui.add_space(12.0);
            ui.strong("In the workgroup, not on this host");
            match cx.snapshot.bridge.up() {
                None => {
                    ui.small("The bridge is not available.");
                }
                Some(bridge) => {
                    let missing = remote_only(&bridge.ledger, &server.workspaces, cx.app_name);
                    if missing.is_empty() {
                        ui.small("Nothing to bring here.");
                    }
                    for w in missing {
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
                                            .set_title(format!("Choose where {} goes", w.name))
                                            .pick_folder()
                                    {
                                        out = Some(Command::WorkspaceMap {
                                            workspace_id: w.workspace_id,
                                            dir,
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
