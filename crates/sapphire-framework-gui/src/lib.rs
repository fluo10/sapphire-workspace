//! Shared egui components for sapphire-framework apps.
//!
//! Currently: [`WorkspaceManager`], a workspace list + management screen
//! (create local, open existing, delete) that operates on the shared
//! [`WorkspaceRegistry`]. It is app-agnostic — timer / journal / ledger
//! render it and implement [`WorkspaceHost`] for the app-specific bits
//! (marker name, how to initialize a new local workspace).
//!
//! The component owns only transient UI state; the app owns the registry
//! (passed `&mut`) and persists it after an action.
//!
//! The [`client`] module is the egui-free half: a background task that keeps a snapshot of
//! the bridge and the app server current and carries out the commands the views (workspace,
//! workgroup and device panels) send it.

/// The egui this crate renders with. Apps should use this re-export so their egui is
/// always the one the components were built against.
pub use egui;

pub mod client;

use std::path::{Path, PathBuf};

use egui::{Align, Color32, Layout};
use sapphire_backend::{WorkspaceEntry, WorkspaceRegistry};

/// App-specific hooks the [`WorkspaceManager`] needs.
pub trait WorkspaceHost {
    /// The workspace marker app name (e.g. `"sapphire-timer"` → `.sapphire-timer/`).
    fn app_name(&self) -> &str;

    /// Directory under which newly-created local workspaces are placed
    /// (`<dir>/<id>`).
    fn default_workspaces_dir(&self) -> PathBuf;

    /// Initialize a new local workspace at `path` (create the marker + any
    /// starter content). `name` is the user-facing display name.
    fn create_local(&self, path: &Path, name: &str) -> Result<(), String>;

    /// Whether an entry looks reachable (for the list badge). The default
    /// checks a local entry's marker directory.
    fn is_reachable(&self, entry: &WorkspaceEntry) -> bool {
        match &entry.path {
            Some(p) => p.join(format!(".{}", self.app_name())).is_dir(),
            None => true,
        }
    }
}

/// What the user did this frame, for the app to act on (open a workspace, or
/// persist the registry after a create/delete).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    /// Open the workspace with this registry id.
    Open(String),
    /// A workspace was created/registered under this id (registry changed).
    Created(String),
    /// A workspace was removed from the registry (registry changed).
    Deleted(String),
}

enum Dialog {
    NewLocal {
        name: String,
    },
    ConfirmDelete {
        id: String,
        name: String,
        typed: String,
    },
}

/// The workspace list + management screen. Hold one per app; call [`ui`](Self::ui)
/// each frame.
#[derive(Default)]
pub struct WorkspaceManager {
    dialog: Option<Dialog>,
    error: Option<String>,
}

impl WorkspaceManager {
    /// Create an empty manager.
    pub fn new() -> Self {
        Self::default()
    }

    /// Render the manager into `ui`, operating on `registry` and `host`.
    ///
    /// Returns an action when the user opens a workspace or mutates the registry
    /// (create/delete); the app should open the workspace and/or persist.
    pub fn ui(
        &mut self,
        ui: &mut egui::Ui,
        registry: &mut WorkspaceRegistry,
        host: &dyn WorkspaceHost,
    ) -> Option<WorkspaceAction> {
        let mut action = None;

        ui.horizontal(|ui| {
            ui.heading("Workspaces");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Open Existing…").clicked()
                    && self.dialog.is_none()
                    && let Some(created) = self.open_existing(registry, host)
                {
                    action = Some(created);
                }
                if ui.button("New").clicked() && self.dialog.is_none() {
                    self.dialog = Some(Dialog::NewLocal {
                        name: String::new(),
                    });
                }
            });
        });

        if let Some(msg) = self.error.clone() {
            ui.horizontal(|ui| {
                ui.colored_label(Color32::LIGHT_RED, msg);
                if ui.small_button("×").clicked() {
                    self.error = None;
                }
            });
        }

        ui.separator();

        // Snapshot ids so we can mutate the registry while iterating.
        let ids: Vec<String> = registry.ids().cloned().collect();
        if ids.is_empty() {
            ui.add_space(20.0);
            ui.vertical_centered(|ui| {
                ui.label("No workspaces yet.");
                ui.label("Create a new one or open an existing folder.");
            });
        } else {
            egui::ScrollArea::vertical().show(ui, |ui| {
                for id in &ids {
                    let Some(entry) = registry.get(id).cloned() else {
                        continue;
                    };
                    if let Some(a) = self.entry_row(ui, id, &entry, host) {
                        action = Some(a);
                    }
                }
            });
        }

        // Modal dialogs may mutate the registry / produce an action.
        if let Some(a) = self.show_dialog(ui, registry, host) {
            action = Some(a);
        }

        action
    }

    fn entry_row(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        entry: &WorkspaceEntry,
        host: &dyn WorkspaceHost,
    ) -> Option<WorkspaceAction> {
        let mut action = None;
        let name = entry.name.clone().unwrap_or_else(|| id.to_owned());
        let subtitle = match &entry.path {
            Some(p) => p.display().to_string(),
            None => "(invalid: no path)".to_owned(),
        };
        let reachable = host.is_reachable(entry);

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.horizontal(|ui| {
                        ui.strong(&name);
                        if !reachable {
                            ui.colored_label(Color32::YELLOW, "unreachable");
                        }
                    });
                    ui.small(subtitle);
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Delete").clicked() && self.dialog.is_none() {
                        self.dialog = Some(Dialog::ConfirmDelete {
                            id: id.to_owned(),
                            name: name.clone(),
                            typed: String::new(),
                        });
                    }
                    if ui.button("Open").clicked() {
                        action = Some(WorkspaceAction::Open(id.to_owned()));
                    }
                });
            });
        });
        action
    }

    fn show_dialog(
        &mut self,
        ui: &egui::Ui,
        registry: &mut WorkspaceRegistry,
        host: &dyn WorkspaceHost,
    ) -> Option<WorkspaceAction> {
        let ctx = ui.ctx().clone();
        let mut action = None;
        let mut close = false;

        match self.dialog.as_mut() {
            Some(Dialog::NewLocal { name }) => {
                let mut open = true;
                egui::Window::new("New Workspace")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut open)
                    .show(&ctx, |ui| {
                        ui.set_min_width(360.0);
                        ui.label("Name");
                        ui.add(
                            egui::TextEdit::singleline(name)
                                .hint_text("e.g. My Timer")
                                .desired_width(f32::INFINITY),
                        );
                        ui.add_space(8.0);
                        let trimmed = name.trim().to_owned();
                        let can = !trimmed.is_empty();
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.add_enabled(can, egui::Button::new("Create")).clicked() {
                                let id = unique_id(&trimmed, registry);
                                let path = host.default_workspaces_dir().join(&id);
                                match host.create_local(&path, &trimmed) {
                                    Ok(()) => {
                                        let mut e = WorkspaceEntry::local(path);
                                        e.name = Some(trimmed.clone());
                                        registry.insert(id.clone(), e);
                                        action = Some(WorkspaceAction::Created(id));
                                        close = true;
                                    }
                                    Err(msg) => self.error = Some(msg),
                                }
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open {
                    close = true;
                }
            }
            Some(Dialog::ConfirmDelete { id, name, typed }) => {
                let id = id.clone();
                let expected = name.clone();
                let mut open = true;
                egui::Window::new("Remove Workspace")
                    .collapsible(false)
                    .resizable(false)
                    .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                    .open(&mut open)
                    .show(&ctx, |ui| {
                        ui.set_min_width(360.0);
                        ui.label("This removes the workspace from the list.");
                        ui.small("Files on disk are not deleted.");
                        ui.add_space(8.0);
                        ui.horizontal_wrapped(|ui| {
                            ui.label("Type");
                            ui.strong(&expected);
                            ui.label("to confirm:");
                        });
                        ui.add(
                            egui::TextEdit::singleline(typed)
                                .hint_text(&expected)
                                .desired_width(f32::INFINITY),
                        );
                        let matches = typed.trim() == expected;
                        ui.add_space(8.0);
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(matches, egui::Button::new("Remove"))
                                .clicked()
                            {
                                registry.remove(&id);
                                action = Some(WorkspaceAction::Deleted(id.clone()));
                                close = true;
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    });
                if !open {
                    close = true;
                }
            }
            None => {}
        }

        if close {
            self.dialog = None;
        }
        action
    }

    /// Register an existing local workspace folder chosen via a native dialog.
    fn open_existing(
        &mut self,
        registry: &mut WorkspaceRegistry,
        host: &dyn WorkspaceHost,
    ) -> Option<WorkspaceAction> {
        let path = rfd::FileDialog::new()
            .set_title("Open workspace folder")
            .pick_folder()?;
        if !path.join(format!(".{}", host.app_name())).is_dir() {
            self.error = Some(format!(
                "'{}' is not a {} workspace",
                path.display(),
                host.app_name()
            ));
            return None;
        }
        let base = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workspace".to_owned());
        let id = unique_id(&base, registry);
        let mut e = WorkspaceEntry::local(path);
        e.name = Some(base);
        registry.insert(id.clone(), e);
        Some(WorkspaceAction::Created(id))
    }
}

/// Slugify `name` into a registry id unique within `registry`.
fn unique_id(name: &str, registry: &WorkspaceRegistry) -> String {
    let base: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let base = base.trim_matches('-').to_owned();
    let base = if base.is_empty() {
        "workspace".to_owned()
    } else {
        base
    };
    if registry.get(&base).is_none() {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base}-{n}");
        if registry.get(&candidate).is_none() {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_id_slugifies_and_dedupes() {
        let mut reg = WorkspaceRegistry::default();
        assert_eq!(unique_id("My Timer!", &reg), "my-timer");
        reg.insert("my-timer", WorkspaceEntry::local("/a"));
        assert_eq!(unique_id("My Timer!", &reg), "my-timer-2");
        assert_eq!(unique_id("", &reg), "workspace");
    }
}
