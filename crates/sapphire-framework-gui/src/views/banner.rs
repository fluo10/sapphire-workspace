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
        if cx.busy {
            ui.spinner();
        }
        error_line(ui, &mut self.error);
        if let Some(command) = self.error.as_deref().and_then(manual_command) {
            let command = command.to_owned();
            ui.horizontal(|ui| {
                ui.small("To install by hand:");
                ui.monospace(&command);
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(command.clone());
                }
            });
        }
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

/// The command an install failure says to run by hand: whatever follows its last `run: `.
fn manual_command(message: &str) -> Option<&str> {
    let (_, command) = message.rsplit_once("run: ")?;
    let command = command.trim();
    (!command.is_empty()).then_some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_command_is_what_follows_run() {
        assert_eq!(
            manual_command(r"access denied; run: C:\x\sapphire-bridge.exe service install"),
            Some(r"C:\x\sapphire-bridge.exe service install")
        );
        assert_eq!(
            manual_command("/a/b was not found; run: /a/b service install\n"),
            Some("/a/b service install")
        );
        assert_eq!(manual_command("closed"), None);
        assert_eq!(manual_command("run: "), None);
    }

    #[test]
    fn an_install_failure_renders_with_its_copy_row() {
        let mut banner = ServiceStatusBanner::default();
        banner.on_outcome(&Err("exit status 1; run: /a/b service install".into()));
        let snapshot = crate::client::Snapshot {
            fetched: true,
            ..Default::default()
        };
        let ctx = egui::Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let cx = ViewCtx {
                snapshot: &snapshot,
                app_name: "app",
                busy: false,
            };
            assert!(banner.ui(ui, &cx).is_none());
        });
        output.textures_delta.clear();
        assert!(banner.error.is_some());
    }
}
