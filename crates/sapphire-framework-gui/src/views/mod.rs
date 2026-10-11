//! Pure renderers: each takes a [`Snapshot`](crate::client::Snapshot) and returns the
//! [`Command`](crate::client::Command) the user asked for this frame, if any.

pub mod model;

mod banner;
mod devices;
mod embedding;
mod external;
mod workgroup;
mod workspaces;

pub use banner::ServiceStatusBanner;
pub use devices::{DeviceList, InviteDialog};
pub use embedding::EmbeddingView;
pub use external::ExternalDeviceList;
pub use workgroup::{JoinDialog, WorkgroupView};
pub use workspaces::WorkspacePicker;

use crate::client::Snapshot;

/// What every view reads.
pub struct ViewCtx<'a> {
    /// The latest snapshot.
    pub snapshot: &'a Snapshot,
    /// This application's name (filters the ledger, names the marker).
    pub app_name: &'a str,
    /// A command this view sent is still running: disable its buttons.
    pub busy: bool,
}

/// The dismissible red error line every view uses.
pub(crate) fn error_line(ui: &mut egui::Ui, error: &mut Option<String>) {
    let Some(msg) = error.clone() else { return };
    ui.horizontal(|ui| {
        ui.colored_label(egui::Color32::LIGHT_RED, msg);
        if ui.small_button("×").clicked() {
            *error = None;
        }
    });
}

/// The one-line hint a view shows when the process it needs is not up.
pub(crate) fn unavailable(ui: &mut egui::Ui, what: &str) {
    ui.add_space(12.0);
    ui.vertical_centered(|ui| {
        ui.label(format!("{what} is not available."));
        ui.small("See the status line above.");
    });
}
