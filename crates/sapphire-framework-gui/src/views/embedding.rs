use egui::Color32;
use sapphire_bridge_api::{
    ApiKey, EmbedDeviceSetParams, EmbedModelSetParams, EmbedRequest, EmbedSettingsResult,
    LOCAL_MODEL, ModelSettings, ModelSource, Slot, SlotModel,
};

use crate::client::{Command, CommandOutput};

use super::model::{LocalForm, RemoteForm, SWITCH_CHOICES, auto_resolution, embedding_status};
use super::{ViewCtx, error_line, unavailable};

/// The embedding models — the workgroup's local and remote one — this device's switch for
/// each, and its API key.
#[derive(Default)]
pub struct EmbeddingView {
    local: LocalForm,
    remote: RemoteForm,
    key: String,
    /// The settings the forms were filled from: refilled when the bridge's change.
    filled_from: Option<ModelSettings>,
    /// A key was sent: clear the field once it is stored.
    key_sent: bool,
    error: Option<String>,
}

impl EmbeddingView {
    /// Render. Returns the settings change the user asked for.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        ui.horizontal(|ui| {
            ui.heading("Embedding");
            if cx.busy {
                ui.spinner();
            }
        });
        ui.separator();
        let Some(bridge) = cx.snapshot.bridge.up() else {
            unavailable(ui, "The bridge");
            return None;
        };
        let Some(s) = &bridge.embedding else {
            ui.label("This bridge is too old to configure embedding; update sapphire-bridge.");
            return None;
        };
        if self.filled_from.as_ref() != Some(&s.models) {
            self.local = LocalForm::from(s.models.local.as_ref());
            self.remote = RemoteForm::from(s.models.remote.as_ref());
            self.filled_from = Some(s.models.clone());
        }

        let (line, warn) = embedding_status(s);
        if warn {
            ui.colored_label(Color32::from_rgb(230, 160, 40), line);
        } else {
            ui.label(line);
        }
        let whose = match s.source {
            Some(ModelSource::Device) => "this device",
            _ if bridge.status.workgroup.is_some() => "the workgroup",
            _ => "this device",
        };
        if s.shadowed_device_models {
            ui.small("This device's own model settings are ignored: the workgroup's apply.");
        }
        error_line(ui, &mut self.error);
        ui.add_space(8.0);

        let mut out = None;
        egui::CollapsingHeader::new(format!("Remote model ({whose})"))
            .default_open(true)
            .show(ui, |ui| {
                ui.small("An OpenAI-compatible endpoint. Used first whenever it is on.");
                egui::Grid::new("embed_remote")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Endpoint");
                        ui.text_edit_singleline(&mut self.remote.endpoint);
                        ui.end_row();
                        ui.label("Model");
                        ui.text_edit_singleline(&mut self.remote.model);
                        ui.end_row();
                        ui.label("Dimension");
                        ui.text_edit_singleline(&mut self.remote.dimension);
                        ui.end_row();
                    });
                if let Some(c) = self.slot_buttons(ui, cx, s, Slot::Remote) {
                    out = Some(c);
                }
                if let Some(c) = self.switch(ui, cx, s, Slot::Remote) {
                    out = Some(c);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    ui.label("API key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.key)
                            .password(true)
                            .hint_text(if s.key_set { "set" } else { "not set" }),
                    );
                    if ui
                        .add_enabled(
                            !cx.busy && !self.key.trim().is_empty(),
                            egui::Button::new("Set"),
                        )
                        .clicked()
                    {
                        self.key_sent = true;
                        out = Some(Command::Embedding(EmbedRequest::KeySet(ApiKey::new(
                            self.key.clone(),
                        ))));
                    }
                    if s.key_set
                        && ui
                            .add_enabled(!cx.busy, egui::Button::new("Clear"))
                            .clicked()
                    {
                        out = Some(Command::Embedding(EmbedRequest::KeyClear));
                    }
                });
                ui.small("The key stays on this device; it is never synced.");
            });

        egui::CollapsingHeader::new(format!("Local model ({whose})"))
            .default_open(true)
            .show(ui, |ui| {
                ui.small("Computed on each device's CPU; needs AVX2 to be usable.");
                egui::Grid::new("embed_local")
                    .num_columns(2)
                    .show(ui, |ui| {
                        ui.label("Model");
                        ui.monospace(LOCAL_MODEL);
                        ui.end_row();
                        ui.label("Dimension");
                        ui.text_edit_singleline(&mut self.local.dimension);
                        ui.end_row();
                        ui.label("Max tokens");
                        ui.text_edit_singleline(&mut self.local.max_tokens);
                        ui.end_row();
                    });
                if let Some(c) = self.slot_buttons(ui, cx, s, Slot::Local) {
                    out = Some(c);
                }
                if let Some(c) = self.switch(ui, cx, s, Slot::Local) {
                    out = Some(c);
                }
            });
        out
    }

    /// Save and Clear for one slot's model.
    fn slot_buttons(
        &mut self,
        ui: &mut egui::Ui,
        cx: &ViewCtx,
        s: &EmbedSettingsResult,
        slot: Slot,
    ) -> Option<Command> {
        let configured = match slot {
            Slot::Local => s.models.local.is_some(),
            Slot::Remote => s.models.remote.is_some(),
        };
        let mut out = None;
        ui.horizontal(|ui| {
            if ui
                .add_enabled(!cx.busy, egui::Button::new("Save"))
                .clicked()
            {
                let parsed = match slot {
                    Slot::Local => self.local.parse().map(SlotModel::Local),
                    Slot::Remote => self.remote.parse().map(SlotModel::Remote),
                };
                match parsed {
                    Ok(model) => {
                        out = Some(Command::Embedding(EmbedRequest::ModelSet(
                            EmbedModelSetParams {
                                slot,
                                model: Some(model),
                            },
                        )));
                    }
                    Err(e) => self.error = Some(e),
                }
            }
            if configured
                && ui
                    .add_enabled(!cx.busy, egui::Button::new("Clear"))
                    .clicked()
            {
                out = Some(Command::Embedding(EmbedRequest::ModelSet(
                    EmbedModelSetParams { slot, model: None },
                )));
            }
        });
        out
    }

    /// This device's Auto / On / Off for one slot.
    fn switch(
        &mut self,
        ui: &mut egui::Ui,
        cx: &ViewCtx,
        s: &EmbedSettingsResult,
        slot: Slot,
    ) -> Option<Command> {
        let current = s.device.enabled(slot);
        let mut out = None;
        ui.horizontal(|ui| {
            ui.label("On this device");
            for (label, value) in SWITCH_CHOICES {
                if ui
                    .add_enabled(!cx.busy, egui::Button::selectable(current == value, label))
                    .clicked()
                    && current != value
                {
                    out = Some(Command::Embedding(EmbedRequest::DeviceSet(
                        EmbedDeviceSetParams {
                            slot,
                            enabled: value,
                        },
                    )));
                }
            }
            if current.is_none() {
                ui.small(auto_resolution(s, slot));
            }
        });
        out
    }

    /// Show the failure, or clear a key that was stored.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match result {
            Ok(_) => {
                if std::mem::take(&mut self.key_sent) {
                    self.key.clear();
                }
                self.error = None;
            }
            Err(e) => {
                self.key_sent = false;
                self.error = Some(e.clone());
            }
        }
    }
}
