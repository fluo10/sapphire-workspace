use egui::{Align, Color32, Layout};

use crate::client::{Command, CommandOutput};

use super::model::{TTL_CHOICES, is_this_device, short_id, valid_name};
use super::{ViewCtx, error_line, unavailable};

/// Issue an invite: a name and a lifetime in, a ticket out.
#[derive(Default)]
pub struct InviteDialog {
    open: bool,
    name: String,
    ttl: usize,
    ticket: Option<String>,
    error: Option<String>,
}

impl InviteDialog {
    /// Show the dialog, fresh.
    pub fn open(&mut self) {
        *self = InviteDialog {
            open: true,
            ttl: 1,
            ..InviteDialog::default()
        };
    }

    /// Render as a centred window while open. Returns [`Command::DeviceInvite`] on submit.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        if !self.open {
            return None;
        }
        let mut out = None;
        let mut open = true;
        egui::Window::new("Invite a device")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.set_min_width(380.0);
                if let Some(ticket) = &self.ticket {
                    ui.label("Give this ticket to the new device. It works once.");
                    ui.add(
                        egui::TextEdit::multiline(&mut ticket.as_str())
                            .font(egui::TextStyle::Monospace)
                            .desired_width(f32::INFINITY),
                    );
                    if ui.button("Copy").clicked() {
                        ui.ctx().copy_text(ticket.clone());
                    }
                    return;
                }
                ui.label("The new device's name");
                ui.text_edit_singleline(&mut self.name);
                ui.horizontal(|ui| {
                    ui.label("Valid for");
                    for (i, (label, _)) in TTL_CHOICES.iter().enumerate() {
                        ui.selectable_value(&mut self.ttl, i, *label);
                    }
                });
                error_line(ui, &mut self.error);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let name = valid_name(&self.name);
                    if ui
                        .add_enabled(
                            !cx.busy && name.is_some(),
                            egui::Button::new("Create ticket"),
                        )
                        .clicked()
                        && let Some(name) = name
                    {
                        out = Some(Command::DeviceInvite {
                            name,
                            ttl_secs: Some(TTL_CHOICES[self.ttl].1),
                        });
                    }
                    if cx.busy {
                        ui.spinner();
                    }
                });
            });
        if !open {
            self.open = false;
        }
        out
    }

    /// Show the ticket, or the error.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match result {
            Ok(CommandOutput::Ticket(t)) => self.ticket = Some(t.clone()),
            Ok(CommandOutput::Done) => {}
            Err(e) => self.error = Some(e.clone()),
        }
    }
}

/// The retire confirmation: the name to type, and the last failure.
#[derive(Default)]
struct RetireConfirm {
    name: String,
    typed: String,
    error: Option<String>,
}

/// Which command the last send was, so its outcome goes back to the right place.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Last {
    Invite,
    Retire,
}

/// The device screen: the workgroup's devices, invite, retire.
#[derive(Default)]
pub struct DeviceList {
    invite: InviteDialog,
    confirm: Option<RetireConfirm>,
    error: Option<String>,
    last: Option<Last>,
}

impl DeviceList {
    /// Render.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let mut out = None;
        ui.horizontal(|ui| {
            ui.heading("Devices");
            if cx.busy {
                ui.spinner();
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let joined = cx
                    .snapshot
                    .bridge
                    .up()
                    .is_some_and(|b| b.status.workgroup.is_some());
                if ui
                    .add_enabled(joined, egui::Button::new("Invite…"))
                    .clicked()
                {
                    self.invite.open();
                }
            });
        });
        ui.separator();
        error_line(ui, &mut self.error);
        let Some(bridge) = cx.snapshot.bridge.up() else {
            unavailable(ui, "The bridge");
            return None;
        };
        if bridge.status.workgroup.is_none() {
            ui.label("This host has not joined a workgroup yet. See the Workgroup screen.");
            return None;
        }
        egui::ScrollArea::vertical().show(ui, |ui| {
            for peer in &bridge.peers {
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        let (dot, colour) = if peer.connected {
                            ("●", Color32::GREEN)
                        } else {
                            ("○", Color32::GRAY)
                        };
                        ui.colored_label(colour, dot);
                        ui.strong(&peer.name);
                        ui.small(short_id(&peer.device_id));
                        if is_this_device(peer, &bridge.status.node_id) {
                            ui.small("(this device)");
                            return;
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui
                                .add_enabled(!cx.busy, egui::Button::new("Retire"))
                                .clicked()
                            {
                                self.confirm = Some(RetireConfirm {
                                    name: peer.name.clone(),
                                    ..RetireConfirm::default()
                                });
                            }
                        });
                    });
                });
            }
        });

        if let Some(c) = self.invite.ui(ui, cx) {
            self.last = Some(Last::Invite);
            out = Some(c);
        }
        if let Some(c) = self.confirm_ui(ui, cx) {
            self.last = Some(Last::Retire);
            out = Some(c);
        }
        out
    }

    fn confirm_ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let confirm = self.confirm.as_mut()?;
        let mut out = None;
        let mut open = true;
        let mut cancel = false;
        egui::Window::new("Retire device")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ui.ctx(), |ui| {
                ui.label("A retired device can no longer connect. Its record stays.");
                ui.horizontal_wrapped(|ui| {
                    ui.label("Type");
                    ui.strong(confirm.name.as_str());
                    ui.label("to confirm:");
                });
                ui.text_edit_singleline(&mut confirm.typed);
                error_line(ui, &mut confirm.error);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let matches = confirm.typed.trim() == confirm.name;
                    if ui
                        .add_enabled(matches && !cx.busy, egui::Button::new("Retire"))
                        .clicked()
                    {
                        confirm.error = None;
                        out = Some(Command::DeviceRetire {
                            selector: confirm.name.clone(),
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                    if cx.busy {
                        ui.spinner();
                    }
                });
            });
        if cancel || !open {
            self.confirm = None;
        }
        out
    }

    /// Route the result: to the invite dialog or the retire confirmation, whichever sent it.
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        match self.last.take() {
            Some(Last::Invite) => self.invite.on_outcome(result),
            Some(Last::Retire) => match (result, self.confirm.as_mut()) {
                (Ok(_), _) => self.confirm = None,
                (Err(e), Some(c)) => c.error = Some(e.clone()),
                (Err(e), None) => self.error = Some(e.clone()),
            },
            None => {
                if let Err(e) = result {
                    self.error = Some(e.clone());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invite_ticket_is_routed_even_after_a_retire_was_opened() {
        let mut list = DeviceList::default();
        list.invite.open();
        list.last = Some(Last::Invite);
        list.on_outcome(&Ok(CommandOutput::Ticket("t".into())));
        assert_eq!(list.invite.ticket.as_deref(), Some("t"));
        assert!(list.last.is_none());
    }

    #[test]
    fn retire_failure_stays_in_the_open_confirm() {
        let mut list = DeviceList {
            confirm: Some(RetireConfirm {
                name: "laptop".into(),
                typed: "laptop".into(),
                error: None,
            }),
            last: Some(Last::Retire),
            ..DeviceList::default()
        };
        list.on_outcome(&Err("boom".into()));
        let c = list.confirm.as_ref().expect("confirm stays open");
        assert_eq!(c.error.as_deref(), Some("boom"));
        assert!(list.error.is_none());
    }

    #[test]
    fn retire_success_closes_the_confirm() {
        let mut list = DeviceList {
            confirm: Some(RetireConfirm::default()),
            last: Some(Last::Retire),
            ..DeviceList::default()
        };
        list.on_outcome(&Ok(CommandOutput::Done));
        assert!(list.confirm.is_none());
    }

    #[test]
    fn open_resets_a_previous_ticket_and_error() {
        let mut d = InviteDialog::default();
        d.open();
        d.on_outcome(&Ok(CommandOutput::Ticket("old".into())));
        d.on_outcome(&Err("bad".into()));
        d.open();
        assert!(d.open && d.ticket.is_none() && d.error.is_none());
    }
}
