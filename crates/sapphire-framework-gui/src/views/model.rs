//! The decisions the views make, as plain functions, so they can be tested without egui.

use grain_id::GrainId;
use sapphire_backend::protocol::WorkspaceListEntry;
use sapphire_bridge_api::{PeerInfo, WorkgroupWorkspaceInfo};

/// A workspace row's state, as one badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Synced, with this many other devices in the workgroup.
    Syncing {
        /// Other devices.
        peers: usize,
    },
    /// Synced but paused, for this reason.
    Paused(String),
    /// Synced, and the last attempt failed.
    Error(String),
    /// Not synced.
    NotSynced,
    /// The folder or its marker is gone.
    Unreachable,
}

impl Badge {
    /// The text shown in the row.
    pub fn label(&self) -> String {
        match self {
            Badge::Syncing { peers } => format!(
                "syncing · {peers} peer{}",
                if *peers == 1 { "" } else { "s" }
            ),
            Badge::Paused(why) => format!("paused: {why}"),
            Badge::Error(e) => format!("error: {e}"),
            Badge::NotSynced => "not synced".to_owned(),
            Badge::Unreachable => "unreachable".to_owned(),
        }
    }
}

/// The badge for `entry`: unreachable beats everything, then not-synced, error, paused.
pub fn badge(entry: &WorkspaceListEntry) -> Badge {
    if !entry.reachable {
        Badge::Unreachable
    } else if !entry.sync.enabled {
        Badge::NotSynced
    } else if let Some(e) = &entry.sync.last_error {
        Badge::Error(e.clone())
    } else if let Some(p) = &entry.sync.paused {
        Badge::Paused(p.clone())
    } else {
        Badge::Syncing {
            peers: entry.sync.peers,
        }
    }
}

/// The workgroup's workspaces of `app_name` that this host does not have, by sync id.
pub fn remote_only<'a>(
    ledger: &'a [WorkgroupWorkspaceInfo],
    local: &[WorkspaceListEntry],
    app_name: &str,
) -> Vec<&'a WorkgroupWorkspaceInfo> {
    ledger
        .iter()
        .filter(|w| w.app_name == app_name)
        .filter(|w| !local.iter().any(|l| l.workspace_id == Some(w.workspace_id)))
        .collect()
}

/// Whether `peer` is this host. An empty node id never matches.
pub fn is_this_device(peer: &PeerInfo, this_node_id: &str) -> bool {
    !this_node_id.is_empty() && peer.node_id == this_node_id
}

/// `input` trimmed, or `None` when nothing is left.
pub fn valid_name(input: &str) -> Option<String> {
    let t = input.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

/// Invite lifetimes offered, in seconds.
pub const TTL_CHOICES: [(&str, u64); 3] =
    [("1 hour", 3_600), ("24 hours", 86_400), ("7 days", 604_800)];

/// This host's name: the device-name default when founding a workgroup.
///
/// Not for joining — a join must repeat the name the invite was issued for.
pub fn default_device_name() -> String {
    ["HOSTNAME", "COMPUTERNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "device".to_owned())
}

/// The first eight characters of an id, for a compact column.
pub fn short_id(id: &GrainId) -> String {
    id.to_string().chars().take(8).collect()
}

/// The row's title: its name, else its registry id.
pub fn display_name(entry: &WorkspaceListEntry) -> String {
    entry.name.clone().unwrap_or_else(|| entry.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sapphire_backend::protocol::SyncStatusResult;
    use std::path::PathBuf;

    fn entry(
        id: &str,
        ws: Option<GrainId>,
        reachable: bool,
        sync: SyncStatusResult,
    ) -> WorkspaceListEntry {
        WorkspaceListEntry {
            id: id.into(),
            name: None,
            root: PathBuf::from(format!("/x/{id}")),
            reachable,
            workspace_id: ws,
            sync,
        }
    }

    fn synced(peers: usize) -> SyncStatusResult {
        SyncStatusResult {
            enabled: true,
            peers,
            ..SyncStatusResult::not_synced()
        }
    }

    #[test]
    fn badge_precedence() {
        assert_eq!(
            badge(&entry("a", None, false, synced(2))),
            Badge::Unreachable
        );
        assert_eq!(
            badge(&entry("a", None, true, SyncStatusResult::not_synced())),
            Badge::NotSynced
        );
        let mut s = synced(1);
        s.paused = Some("root missing".into());
        s.last_error = Some("boom".into());
        assert_eq!(
            badge(&entry("a", None, true, s.clone())),
            Badge::Error("boom".into())
        );
        s.last_error = None;
        assert_eq!(
            badge(&entry("a", None, true, s)),
            Badge::Paused("root missing".into())
        );
        assert_eq!(
            badge(&entry("a", None, true, synced(3))),
            Badge::Syncing { peers: 3 }
        );
    }

    #[test]
    fn remote_only_excludes_a_disabled_local_workspace() {
        let mine = GrainId::random();
        let other = GrainId::random();
        let foreign = GrainId::random();
        let ledger = vec![
            WorkgroupWorkspaceInfo {
                workspace_id: mine,
                app_name: "app".into(),
                name: "mine".into(),
            },
            WorkgroupWorkspaceInfo {
                workspace_id: other,
                app_name: "app".into(),
                name: "other".into(),
            },
            WorkgroupWorkspaceInfo {
                workspace_id: foreign,
                app_name: "journal".into(),
                name: "j".into(),
            },
        ];
        let local = vec![entry(
            "mine",
            Some(mine),
            true,
            SyncStatusResult::not_synced(),
        )];
        let left = remote_only(&ledger, &local, "app");
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].workspace_id, other);
    }

    #[test]
    fn this_device_is_matched_by_node_id() {
        let p = PeerInfo {
            device_id: GrainId::random(),
            name: "desk".into(),
            node_id: "aaaa".into(),
            connected: true,
        };
        assert!(is_this_device(&p, "aaaa"));
        assert!(!is_this_device(&p, "bbbb"));
        assert!(!is_this_device(
            &PeerInfo {
                node_id: String::new(),
                ..p
            },
            ""
        ));
    }

    #[test]
    fn names_are_trimmed_and_must_not_be_empty() {
        assert_eq!(valid_name("  home "), Some("home".into()));
        assert_eq!(valid_name("   "), None);
    }

    #[test]
    fn short_id_is_a_prefix() {
        let id = GrainId::random();
        let s = short_id(&id);
        assert!(id.to_string().starts_with(&s));
        assert!(s.len() <= 8);
    }

    #[test]
    fn display_name_falls_back_to_the_folder_name() {
        let e = entry("notes-2", None, true, SyncStatusResult::not_synced());
        assert_eq!(display_name(&e), "notes-2");
        let named = WorkspaceListEntry {
            name: Some("Notes".into()),
            ..e
        };
        assert_eq!(display_name(&named), "Notes");
    }
}
