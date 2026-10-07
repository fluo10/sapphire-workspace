use egui::{Align, Layout};

use crate::client::{Command, CommandOutput};

use super::model::{default_device_name, is_this_device, valid_name};
use super::{ViewCtx, error_line, unavailable};

/// Join with a ticket: the ticket and this device's name.
pub struct JoinDialog {
    ticket: String,
    device_name: String,
    error: Option<String>,
}

impl Default for JoinDialog {
    fn default() -> Self {
        JoinDialog {
            ticket: String::new(),
            device_name: default_device_name(),
            error: None,
        }
    }
}

impl JoinDialog {
    /// Reset the form.
    pub fn open(&mut self) {
        *self = JoinDialog::default();
    }

    /// Render the form inline. Returns [`Command::WorkgroupJoin`] on submit.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let mut out = None;
        ui.label("Ticket");
        ui.add(
            egui::TextEdit::multiline(&mut self.ticket)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );
        ui.label("This device's name");
        ui.text_edit_singleline(&mut self.device_name);
        let ticket = self.ticket.trim().to_owned();
        if ui
            .add_enabled(!cx.busy && !ticket.is_empty(), egui::Button::new("Join"))
            .clicked()
        {
            out = Some(Command::WorkgroupJoin {
                ticket,
                device_name: valid_name(&self.device_name),
            });
        }
        if cx.busy {
            ui.spinner();
        }
        error_line(ui, &mut self.error);
        out
    }

    /// Clear on success, show the error otherwise.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match result {
            Ok(_) => *self = JoinDialog::default(),
            Err(e) => self.error = Some(e.clone()),
        }
    }
}

/// The workgroup screen: create or join when there is none, details when there is.
#[derive(Default)]
pub struct WorkgroupView {
    name: String,
    device_name: Option<String>,
    join: JoinDialog,
    error: Option<String>,
    last: Option<Last>,
}

#[derive(Clone, Copy)]
enum Last {
    Create,
    Join,
}

impl WorkgroupView {
    /// Render.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        ui.horizontal(|ui| {
            ui.heading("Workgroup");
            if cx.busy {
                ui.spinner();
            }
        });
        ui.separator();
        let Some(bridge) = cx.snapshot.bridge.up() else {
            unavailable(ui, "The bridge");
            return None;
        };
        if let Some(wg) = &bridge.status.workgroup {
            egui::Grid::new("workgroup").num_columns(2).show(ui, |ui| {
                ui.label("Name");
                ui.strong(&wg.name);
                ui.end_row();
                ui.label("Id");
                ui.horizontal(|ui| {
                    ui.monospace(wg.workgroup_id.to_string());
                    if ui.small_button("Copy").clicked() {
                        ui.ctx().copy_text(wg.workgroup_id.to_string());
                    }
                });
                ui.end_row();
                ui.label("Devices");
                ui.label(wg.devices.to_string());
                ui.end_row();
                if let Some(me) = bridge
                    .peers
                    .iter()
                    .find(|p| is_this_device(p, &bridge.status.node_id))
                {
                    ui.label("This device");
                    ui.label(&me.name);
                    ui.end_row();
                }
            });
            return None;
        }

        let mut out = None;
        let device_name = self.device_name.get_or_insert_with(default_device_name);
        ui.columns(2, |cols| {
            egui::Frame::group(cols[0].style()).show(&mut cols[0], |ui| {
                ui.strong("Create a workgroup");
                ui.label("Name");
                ui.text_edit_singleline(&mut self.name);
                ui.label("This device's name");
                ui.text_edit_singleline(device_name);
                let ready = valid_name(&self.name).zip(valid_name(device_name));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add_enabled(!cx.busy && ready.is_some(), egui::Button::new("Create"))
                        .clicked()
                        && let Some((name, device_name)) = ready
                    {
                        self.last = Some(Last::Create);
                        out = Some(Command::WorkgroupCreate { name, device_name });
                    }
                });
            });
            egui::Frame::group(cols[1].style()).show(&mut cols[1], |ui| {
                ui.strong("Join with a ticket");
                if let Some(c) = self.join.ui(ui, cx) {
                    self.last = Some(Last::Join);
                    out = Some(c);
                }
            });
        });
        error_line(ui, &mut self.error);
        out
    }

    /// Route the result to the form that sent it.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match self.last.take() {
            Some(Last::Join) => self.join.on_outcome(result),
            _ => {
                if let Err(e) = result {
                    self.error = Some(e.clone());
                } else {
                    self.name.clear();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_join_failure_goes_to_the_join_dialog() {
        let mut view = WorkgroupView {
            last: Some(Last::Join),
            ..WorkgroupView::default()
        };
        view.on_outcome(&Err("bad ticket".into()));
        assert_eq!(view.join.error.as_deref(), Some("bad ticket"));
        assert!(view.error.is_none());
    }

    #[test]
    fn a_create_failure_goes_to_the_view_error_line() {
        let mut view = WorkgroupView {
            last: Some(Last::Create),
            ..WorkgroupView::default()
        };
        view.on_outcome(&Err("exists".into()));
        assert_eq!(view.error.as_deref(), Some("exists"));
        assert!(view.join.error.is_none());
    }
}
