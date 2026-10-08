//! [`SyncPanel`]: the status line, a left navigation and the three screens, wired to a
//! [`FrameworkClient`].

use std::collections::HashMap;

use crate::client::{CommandId, FrameworkClient};
use crate::views::{DeviceList, ServiceStatusBanner, ViewCtx, WorkgroupView, WorkspaceList};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Screen {
    Workspaces,
    Devices,
    Workgroup,
}

/// Which view sent a command, so its outcome goes back there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Origin {
    Banner,
    Screen(Screen),
}

/// The whole sync management UI. Hold one; call [`ui`](Self::ui) every frame.
pub struct SyncPanel {
    client: FrameworkClient,
    screen: Screen,
    banner: ServiceStatusBanner,
    workspaces: WorkspaceList,
    devices: DeviceList,
    workgroup: WorkgroupView,
    pending: HashMap<CommandId, Origin>,
}

impl SyncPanel {
    /// A panel over `client`, opening on the workspace screen.
    pub fn new(client: FrameworkClient) -> SyncPanel {
        SyncPanel {
            client,
            screen: Screen::Workspaces,
            banner: ServiceStatusBanner::default(),
            workspaces: WorkspaceList::default(),
            devices: DeviceList::default(),
            workgroup: WorkgroupView::default(),
            pending: HashMap::new(),
        }
    }

    /// Render into `ui` (typically the eframe root).
    pub fn ui(&mut self, ui: &mut egui::Ui) {
        for outcome in self.client.drain_outcomes() {
            match self.pending.remove(&outcome.id) {
                Some(Origin::Banner) => self.banner.on_outcome(&outcome.result),
                Some(Origin::Screen(Screen::Workspaces)) => {
                    self.workspaces.on_outcome(&outcome.result)
                }
                Some(Origin::Screen(Screen::Devices)) => self.devices.on_outcome(&outcome.result),
                Some(Origin::Screen(Screen::Workgroup)) => {
                    self.workgroup.on_outcome(&outcome.result)
                }
                None => {}
            }
        }

        let snapshot = self.client.snapshot().clone();
        let app_name = self.client.app().app_name;
        let busy =
            |o: Origin, pending: &HashMap<CommandId, Origin>| pending.values().any(|p| *p == o);
        let mut sent = None;

        egui::Panel::top("sync_status").show(ui, |ui| {
            let cx = ViewCtx {
                snapshot: &snapshot,
                app_name,
                busy: busy(Origin::Banner, &self.pending),
            };
            if let Some(c) = self.banner.ui(ui, &cx) {
                sent = Some((c, Origin::Banner));
            }
        });
        egui::Panel::left("sync_nav")
            .resizable(false)
            .exact_size(150.0)
            .show(ui, |ui| {
                for (screen, label) in [
                    (Screen::Workspaces, "Workspaces"),
                    (Screen::Devices, "Devices"),
                    (Screen::Workgroup, "Workgroup"),
                ] {
                    ui.selectable_value(&mut self.screen, screen, label);
                }
            });
        egui::CentralPanel::default().show(ui, |ui| {
            let origin = Origin::Screen(self.screen);
            let cx = ViewCtx {
                snapshot: &snapshot,
                app_name,
                busy: busy(origin, &self.pending),
            };
            let c = match self.screen {
                Screen::Workspaces => self.workspaces.ui(ui, &cx),
                Screen::Devices => self.devices.ui(ui, &cx),
                Screen::Workgroup => self.workgroup.ui(ui, &cx),
            };
            if let Some(c) = c {
                sent = Some((c, origin));
            }
        });

        if let Some((command, origin)) = sent {
            let id = self.client.send(command);
            self.pending.insert(id, origin);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{BridgeState, Conn, ServerState, Snapshot};
    use sapphire_backend::protocol::{StatusReport, SyncStatusResult, WorkspaceListEntry};
    use sapphire_bridge_api::{
        GrainId, PeerInfo, StatusResult, WorkgroupStatus, WorkgroupWorkspaceInfo,
    };

    fn rich() -> Snapshot {
        let wg = GrainId::random();
        Snapshot {
            fetched: true,
            bridge: Conn::Up(BridgeState {
                status: StatusResult {
                    version: "1".into(),
                    node_id: "aaaa".into(),
                    workgroup: Some(WorkgroupStatus {
                        workgroup_id: wg,
                        name: "home".into(),
                        devices: 2,
                    }),
                    routes: vec![],
                },
                peers: vec![
                    PeerInfo {
                        device_id: GrainId::random(),
                        name: "desk".into(),
                        node_id: "aaaa".into(),
                        connected: true,
                        priority: 1,
                        availability: None,
                    },
                    PeerInfo {
                        device_id: GrainId::random(),
                        name: "laptop".into(),
                        node_id: "bbbb".into(),
                        connected: false,
                        priority: 1,
                        availability: None,
                    },
                ],
                peer_roles: vec![],
                ledger: vec![WorkgroupWorkspaceInfo {
                    workspace_id: GrainId::random(),
                    app_name: "app".into(),
                    name: "remote".into(),
                }],
            }),
            server: Conn::Up(ServerState {
                info: StatusReport {
                    running: true,
                    version: Some("1".into()),
                    pid: Some(1),
                    managed_by: None,
                    app: vec![],
                },
                workspaces: vec![WorkspaceListEntry {
                    id: "notes".into(),
                    name: None,
                    root: "/x/notes".into(),
                    reachable: true,
                    workspace_id: None,
                    sync: SyncStatusResult::not_synced(),
                }],
            }),
        }
    }

    fn render(snapshot: &Snapshot) {
        let ctx = egui::Context::default();
        let mut banner = ServiceStatusBanner::default();
        let mut wg = WorkgroupView::default();
        let mut devices = DeviceList::default();
        let mut workspaces = WorkspaceList::default();
        for _ in 0..2 {
            let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                let cx = ViewCtx {
                    snapshot,
                    app_name: "app",
                    busy: false,
                };
                assert!(banner.ui(ui, &cx).is_none());
                assert!(wg.ui(ui, &cx).is_none());
                assert!(devices.ui(ui, &cx).is_none());
                assert!(workspaces.ui(ui, &cx).is_none());
            });
            // Unapplied texture deltas panic on drop; there is no renderer here.
            output.textures_delta.clear();
        }
    }

    #[test]
    fn every_view_renders_a_full_snapshot_without_emitting_commands() {
        render(&rich());
    }

    #[test]
    fn every_view_renders_absent_and_incompatible_processes() {
        render(&Snapshot::default());
        render(&Snapshot {
            fetched: true,
            bridge: Conn::Incompatible("update sapphire-bridge".into()),
            server: Conn::Error("closed".into()),
        });
    }
}
