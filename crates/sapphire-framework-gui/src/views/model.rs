//! The decisions the views make, as plain functions, so they can be tested without egui.

use std::path::{Path, PathBuf};

use grain_id::GrainId;
use sapphire_backend::protocol::{Topology, WorkspaceListEntry};
use sapphire_bridge_api::{PeerInfo, WorkgroupWorkspaceInfo, WorkspaceRoles};

/// A workspace row's state, as one badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Synced, with this many other devices in the workgroup.
    Syncing {
        /// Other devices.
        peers: usize,
        /// Whether the workspace syncs through a designated device.
        star: bool,
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
            Badge::Syncing { peers, star } => format!(
                "syncing · {peers} peer{}{}",
                if *peers == 1 { "" } else { "s" },
                if *star { " · star" } else { "" }
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
            star: matches!(entry.sync.topology, Topology::Star { .. }),
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

/// Whether `dir` is already a workspace of `app_name` (holds its `.{app_name}` marker).
fn has_marker(dir: &Path, app_name: &str) -> bool {
    dir.join(format!(".{app_name}")).is_dir()
}

/// `name` made safe as one folder name on every platform: path separators and the
/// characters Windows forbids become `-`, leading and trailing dots and spaces go, and
/// nothing left becomes `workspace`.
fn folder_name(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '-',
            c if c.is_control() => '-',
            c => c,
        })
        .collect();
    let trimmed = replaced.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() {
        "workspace".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// Where "Bring to this host…" puts `workspace_name` when the user picked `picked`.
///
/// A picked folder that already is a workspace of `app_name` is used as it is; any other
/// folder gets a new one inside it, named after the workspace — picking `~/Documents` must
/// not turn all of `~/Documents` into a synced workspace.
pub fn bring_target(picked: &Path, workspace_name: &str, app_name: &str) -> PathBuf {
    if has_marker(picked, app_name) {
        picked.to_owned()
    } else {
        picked.join(folder_name(workspace_name))
    }
}

/// Whether "New…" may create a workspace in `picked`: it must already be a workspace of
/// `app_name`, or be empty. The error is the message to show.
pub fn new_target_ok(picked: &Path, app_name: &str) -> Result<(), String> {
    if has_marker(picked, app_name) {
        return Ok(());
    }
    let mut entries =
        std::fs::read_dir(picked).map_err(|e| format!("{}: {e}", picked.display()))?;
    if entries.next().is_none() {
        Ok(())
    } else {
        Err(format!(
            "Choose an empty folder ({} is not empty)",
            display_path(picked).display()
        ))
    }
}

/// `p` as a person writes it: without the Windows verbatim prefix `\\?\` that
/// canonicalisation adds (`\\?\UNC\server\share` becomes `\\server\share`).
///
/// For showing a path and handing it to the file manager only; the registry keeps the
/// canonical form. Plain string logic, the same on every platform; a path that is not
/// UTF-8 is returned unchanged.
pub fn display_path(p: &Path) -> PathBuf {
    let Some(s) = p.to_str() else {
        return p.to_owned();
    };
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        p.to_owned()
    }
}

/// The roles `peer` holds in any workspace, each named once: what its device row shows.
pub fn role_badges(peer: &PeerInfo, roles: &[WorkspaceRoles]) -> Vec<&'static str> {
    let mut out = Vec::new();
    if roles.iter().any(|r| r.designated == Some(peer.device_id)) {
        out.push("designated");
    }
    if roles.iter().any(|r| r.backup == Some(peer.device_id)) {
        out.push("backup");
    }
    out
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
    use sapphire_backend::protocol::{SyncStatusResult, Topology};
    use sapphire_bridge_api::WorkspaceRoles;
    use std::path::{Path, PathBuf};

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
            Badge::Syncing {
                peers: 3,
                star: false
            }
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
            priority: 1,
            availability: None,
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
    fn bring_target_nests_a_folder_named_after_the_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            bring_target(tmp.path(), "Notes", "app"),
            tmp.path().join("Notes")
        );
        assert_eq!(
            bring_target(tmp.path(), "a/b\\c:d*e?f\"g<h>i|j", "app"),
            tmp.path().join("a-b-c-d-e-f-g-h-i-j")
        );
        assert_eq!(
            bring_target(tmp.path(), " ..x.. ", "app"),
            tmp.path().join("x")
        );
        assert_eq!(
            bring_target(tmp.path(), " . ", "app"),
            tmp.path().join("workspace")
        );
        assert_eq!(
            bring_target(tmp.path(), "", "app"),
            tmp.path().join("workspace")
        );
    }

    #[test]
    fn bring_target_uses_a_picked_workspace_folder_as_is() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".app")).unwrap();
        assert_eq!(bring_target(tmp.path(), "Notes", "app"), tmp.path());
    }

    #[test]
    fn a_new_workspace_needs_an_empty_folder_or_a_workspace() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(new_target_ok(tmp.path(), "app"), Ok(()));
        std::fs::write(tmp.path().join("file.txt"), "x").unwrap();
        let err = new_target_ok(tmp.path(), "app").unwrap_err();
        assert!(err.starts_with("Choose an empty folder"), "{err}");
        assert!(err.contains("is not empty"), "{err}");
        std::fs::create_dir(tmp.path().join(".app")).unwrap();
        assert_eq!(new_target_ok(tmp.path(), "app"), Ok(()));
    }

    #[test]
    fn display_path_drops_the_verbatim_prefix() {
        assert_eq!(
            display_path(Path::new(r"\\?\C:\Users\me\notes")),
            PathBuf::from(r"C:\Users\me\notes")
        );
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\notes")),
            PathBuf::from(r"\\server\share\notes")
        );
        assert_eq!(
            display_path(Path::new("/home/me/notes")),
            PathBuf::from("/home/me/notes")
        );
        assert_eq!(
            display_path(Path::new(r"C:\plain")),
            PathBuf::from(r"C:\plain")
        );
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

    #[test]
    fn role_badges_name_each_role_once() {
        let me = GrainId::random();
        let p = PeerInfo {
            device_id: me,
            name: "a".into(),
            node_id: String::new(),
            connected: true,
            priority: 1,
            availability: None,
        };
        let roles = vec![
            WorkspaceRoles {
                workspace_id: GrainId::random(),
                designated: Some(me),
                backup: None,
            },
            WorkspaceRoles {
                workspace_id: GrainId::random(),
                designated: Some(me),
                backup: None,
            },
            WorkspaceRoles {
                workspace_id: GrainId::random(),
                designated: None,
                backup: Some(me),
            },
        ];
        assert_eq!(role_badges(&p, &roles), vec!["designated", "backup"]);
    }

    #[test]
    fn a_star_workspace_says_so() {
        let mut s = synced(2);
        s.topology = Topology::Star {
            designated: GrainId::random(),
            backup: None,
        };
        assert_eq!(
            badge(&entry("a", None, true, s)).label(),
            "syncing · 2 peers · star"
        );
    }
}
