use std::collections::HashMap;

use egui::{Align, Layout};
use grain_id::GrainId;
use sapphire_bridge_api::{
    ExternalDeviceAddParams, ExternalDeviceInfo, ExternalDeviceRequest, ExternalDeviceSetAppsParams,
};

use crate::client::{Command, CommandOutput};

use super::{ViewCtx, error_line, unavailable};

/// The workgroup's external devices: clients that reach its apps with a key (#199).
#[derive(Default)]
pub struct ExternalDeviceList {
    name: String,
    description: String,
    /// The new external device's applications, comma-separated; this app's when empty.
    apps: String,
    /// Applications being edited, per record, comma-separated.
    editing: HashMap<GrainId, String>,
    /// A token to show once, with whose it is.
    token: Option<(String, String)>,
    /// The name a token-producing command was sent for.
    pending_name: Option<String>,
    error: Option<String>,
}

/// `a, b ,, c` → `["a", "b", "c"]`.
pub fn parse_apps(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

impl ExternalDeviceList {
    /// Render. Returns the change the user asked for.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        ui.horizontal(|ui| {
            ui.heading("External devices");
            if cx.busy {
                ui.spinner();
            }
        });
        ui.small("Clients that reach the workgroup's apps with a key instead of syncing.");
        ui.separator();
        let Some(bridge) = cx.snapshot.bridge.up() else {
            unavailable(ui, "The bridge");
            return None;
        };
        if bridge.status.workgroup.is_none() {
            ui.label("Found or join a workgroup first.");
            return None;
        }
        let Some(list) = &bridge.external_devices else {
            ui.label("This bridge is too old to manage external devices; update sapphire-bridge.");
            return None;
        };
        error_line(ui, &mut self.error);
        let mut out = None;

        if let Some((name, token)) = self.token.clone() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(format!("The token for «{name}». It is shown only now."));
                ui.add(
                    egui::TextEdit::singleline(&mut token.as_str())
                        .font(egui::TextStyle::Monospace)
                        .desired_width(f32::INFINITY),
                );
                ui.horizontal(|ui| {
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(token.clone());
                    }
                    if ui.button("Done").clicked() {
                        self.token = None;
                    }
                });
            });
            ui.add_space(8.0);
        }

        egui::Grid::new("external_devices")
            .num_columns(3)
            .striped(true)
            .show(ui, |ui| {
                for d in list {
                    if let Some(c) = self.row(ui, cx, d) {
                        out = Some(c);
                    }
                    ui.end_row();
                }
            });
        if list.is_empty() {
            ui.label("No external devices.");
        }

        ui.add_space(8.0);
        ui.separator();
        ui.label("Add an external device");
        egui::Grid::new("external_add")
            .num_columns(2)
            .show(ui, |ui| {
                ui.label("Name");
                ui.text_edit_singleline(&mut self.name);
                ui.end_row();
                ui.label("Apps");
                ui.add(egui::TextEdit::singleline(&mut self.apps).hint_text(cx.app_name));
                ui.end_row();
                ui.label("Note");
                ui.text_edit_singleline(&mut self.description);
                ui.end_row();
            });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let name = self.name.trim().to_owned();
            if ui
                .add_enabled(!cx.busy && !name.is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                let mut apps = parse_apps(&self.apps);
                if apps.is_empty() {
                    apps.push(cx.app_name.to_owned());
                }
                let description =
                    Some(self.description.trim().to_owned()).filter(|d| !d.is_empty());
                self.pending_name = Some(name.clone());
                out = Some(Command::ExternalDevice(ExternalDeviceRequest::Add(
                    ExternalDeviceAddParams {
                        name,
                        description,
                        apps,
                    },
                )));
            }
        });
        out
    }

    fn row(&mut self, ui: &mut egui::Ui, cx: &ViewCtx, d: &ExternalDeviceInfo) -> Option<Command> {
        let mut out = None;
        ui.vertical(|ui| {
            ui.strong(&d.name);
            if d.retired_at.is_some() {
                ui.small("retired");
            }
        });
        let editing = self
            .editing
            .entry(d.id)
            .or_insert_with(|| d.apps.join(", "));
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(editing).desired_width(180.0));
            let apps = parse_apps(editing);
            if apps != d.apps
                && ui
                    .add_enabled(!cx.busy, egui::Button::new("Save apps"))
                    .clicked()
            {
                out = Some(Command::ExternalDevice(ExternalDeviceRequest::SetApps(
                    ExternalDeviceSetAppsParams {
                        selector: d.id.to_string(),
                        apps,
                    },
                )));
            }
        });
        ui.horizontal(|ui| {
            let id = d.id.to_string();
            if d.retired_at.is_some() {
                if ui
                    .add_enabled(!cx.busy, egui::Button::new("Restore"))
                    .clicked()
                {
                    out = Some(Command::ExternalDevice(ExternalDeviceRequest::Restore(id)));
                }
            } else {
                if ui
                    .add_enabled(!cx.busy, egui::Button::new("New token"))
                    .clicked()
                {
                    self.pending_name = Some(d.name.clone());
                    out = Some(Command::ExternalDevice(ExternalDeviceRequest::Rotate(
                        id.clone(),
                    )));
                }
                if ui
                    .add_enabled(!cx.busy, egui::Button::new("Retire"))
                    .clicked()
                {
                    out = Some(Command::ExternalDevice(ExternalDeviceRequest::Retire(id)));
                }
            }
        });
        out
    }

    /// Show the token once, or the error.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match result {
            Ok(CommandOutput::Token(token)) => {
                let name = self.pending_name.take().unwrap_or_default();
                self.token = Some((name, token.clone()));
                self.name.clear();
                self.description.clear();
                self.apps.clear();
                self.error = None;
            }
            Ok(_) => {
                self.pending_name = None;
                // The snapshot carries the new applications; edit from those.
                self.editing.clear();
                self.error = None;
            }
            Err(e) => {
                self.pending_name = None;
                self.error = Some(e.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apps_parse_from_a_comma_list() {
        assert_eq!(parse_apps(" a, b ,, c "), vec!["a", "b", "c"]);
        assert!(parse_apps("  ").is_empty());
    }

    #[test]
    fn a_token_is_shown_with_the_name_it_was_made_for() {
        let mut v = ExternalDeviceList {
            pending_name: Some("pendant".into()),
            name: "pendant".into(),
            ..Default::default()
        };
        v.on_outcome(&Ok(CommandOutput::Token("sapphire-ed-x".into())));
        assert_eq!(
            v.token,
            Some(("pendant".to_owned(), "sapphire-ed-x".to_owned()))
        );
        assert!(v.name.is_empty(), "the form is cleared");
    }
}
