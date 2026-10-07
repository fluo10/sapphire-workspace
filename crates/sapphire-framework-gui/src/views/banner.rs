use crate::client::{Command, CommandOutput, Conn, ServiceTarget};

use super::{ViewCtx, error_line};

/// The top status line: is the bridge up, is the app server up.
#[derive(Default)]
pub struct ServiceStatusBanner {
    error: Option<String>,
}

impl ServiceStatusBanner {
    /// Render. Returns a [`Command::ServiceInstall`] when a start button is clicked.
    pub fn ui(&mut self, ui: &mut egui::Ui, cx: &ViewCtx) -> Option<Command> {
        let mut out = None;
        if !cx.snapshot.fetched {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Connecting…");
            });
            return None;
        }
        let bridge = describe(&cx.snapshot.bridge, |b| format!("v{}", b.status.version));
        let server = describe(&cx.snapshot.server, |s| {
            format!("v{}", s.info.version.clone().unwrap_or_default())
        });
        ui.horizontal_wrapped(|ui| {
            for (label, state, absent, target) in [
                (
                    "sapphire-bridge",
                    &bridge,
                    matches!(cx.snapshot.bridge, Conn::Absent),
                    ServiceTarget::Bridge,
                ),
                (
                    cx.app_name,
                    &server,
                    matches!(cx.snapshot.server, Conn::Absent),
                    ServiceTarget::App,
                ),
            ] {
                ui.strong(label);
                ui.label(state);
                if absent
                    && ui
                        .add_enabled(!cx.busy, egui::Button::new("Install & start service"))
                        .clicked()
                {
                    out = Some(Command::ServiceInstall(target));
                }
                ui.separator();
            }
        });
        error_line(ui, &mut self.error);
        out
    }

    /// Show an install failure (it carries the command to run by hand).
    pub fn on_outcome(&mut self, result: &Result<CommandOutput, String>) {
        self.error = result.as_ref().err().cloned();
    }
}

fn describe<T>(conn: &Conn<T>, up: impl Fn(&T) -> String) -> String {
    match conn {
        Conn::Absent => "not running".to_owned(),
        Conn::Incompatible(msg) => format!("incompatible: {msg}"),
        Conn::Error(msg) => format!("error: {msg}"),
        Conn::Up(t) => format!("running {}", up(t)),
    }
}
