//! The decisions the views make, as plain functions, so they can be tested without egui.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use grain_id::GrainId;
use sapphire_backend::protocol::{Topology, WorkspaceListEntry};
use sapphire_bridge_api::{
    EmbedSettingsResult, LOCAL_MODEL, LocalModel, ModelSettings, PeerInfo, RemoteModel, Slot,
    WorkgroupWorkspaceInfo, WorkspaceRoles,
};

/// A workspace row's state, as one badge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Badge {
    /// Synced, with this many other devices in the workgroup.
    Syncing {
        /// Other devices.
        peers: usize,
        /// Whether the workspace syncs through a primary device.
        star: bool,
        /// Files this host has yet to give a vector (#188); 0 when none or not embedding.
        embedding_pending: u64,
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
            Badge::Syncing {
                peers,
                star,
                embedding_pending,
            } => format!(
                "syncing · {peers} peer{}{}{}",
                if *peers == 1 { "" } else { "s" },
                if *star { " · star" } else { "" },
                if *embedding_pending > 0 {
                    format!(" · embedding {embedding_pending} pending")
                } else {
                    String::new()
                }
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
            embedding_pending: entry.sync.embedding.as_ref().map_or(0, |e| e.pending),
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

/// Keep `value`, the priority shown in `peer`'s row, as an edit only while it differs from
/// the bridge's priority. An unchanged row then follows changes made elsewhere.
pub fn record_edit(edits: &mut HashMap<GrainId, u8>, peer: &PeerInfo, value: u8) {
    if value == peer.priority {
        edits.remove(&peer.device_id);
    } else {
        edits.insert(peer.device_id, value);
    }
}

/// Drop the edits the bridge now agrees with, and those for devices no longer listed.
pub fn prune_edits(edits: &mut HashMap<GrainId, u8>, peers: &[PeerInfo]) {
    edits.retain(|id, p| peers.iter().any(|x| x.device_id == *id && x.priority != *p));
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
    if roles.iter().any(|r| r.primary == Some(peer.device_id)) {
        out.push("primary");
    }
    if roles.iter().any(|r| r.secondary == Some(peer.device_id)) {
        out.push("secondary");
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

// ── embedding ───────────────────────────────────────────────────────────────

/// The embedding screen's status line, and whether it is a warning.
pub fn embedding_status(s: &EmbedSettingsResult) -> (String, bool) {
    let note = s.info.note.as_ref();
    match (s.active, &s.info.model) {
        (Some(slot), Some(model)) => {
            let state = if s.info.loaded {
                "loaded"
            } else {
                "not loaded"
            };
            let line = format!(
                "Embedding with the {slot} model {} ({} dimensions), {state}",
                model.model, model.dimension
            );
            match note {
                Some(n) => (format!("{line} — {n}"), true),
                None => (line, false),
            }
        }
        (Some(slot), None) => (
            format!("The {slot} model is configured, but this bridge cannot run it"),
            true,
        ),
        (None, _) => (
            format!(
                "Embedding is off{}",
                note.map(|n| format!(": {n}")).unwrap_or_default()
            ),
            matches!(note, Some(sapphire_bridge_api::EmbedNote::Invalid(_))),
        ),
    }
}

/// A slot's three-way switch on this device: `None` is Auto.
pub const SWITCH_CHOICES: [(&str, Option<bool>); 3] =
    [("Auto", None), ("On", Some(true)), ("Off", Some(false))];

/// What Auto resolves to for `slot`, and why, for the label beside the switch.
pub fn auto_resolution(s: &EmbedSettingsResult, slot: Slot) -> &'static str {
    match slot {
        Slot::Local if !s.avx2 => "Auto: off — this CPU has no AVX2",
        Slot::Local => "Auto: on",
        Slot::Remote => "Auto: on",
    }
}

/// The local slot's form, as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LocalForm {
    pub dimension: String,
    pub max_tokens: String,
}

/// The remote slot's form, as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RemoteForm {
    pub endpoint: String,
    pub model: String,
    pub dimension: String,
}

impl LocalForm {
    /// The form for `model`, or the defaults when the slot is empty.
    pub fn from(model: Option<&LocalModel>) -> LocalForm {
        let m = model.cloned().unwrap_or_default();
        LocalForm {
            dimension: m.dimension.to_string(),
            max_tokens: m.max_tokens.to_string(),
        }
    }

    /// The model this form describes, checked as the bridge would.
    pub fn parse(&self) -> Result<LocalModel, String> {
        let model = LocalModel {
            model: LOCAL_MODEL.to_owned(),
            dimension: number(&self.dimension, "dimension")?,
            max_tokens: number(&self.max_tokens, "max tokens")?,
        };
        ModelSettings {
            local: Some(model.clone()),
            remote: None,
        }
        .validate()?;
        Ok(model)
    }
}

impl RemoteForm {
    /// The form for `model`, empty when the slot is.
    pub fn from(model: Option<&RemoteModel>) -> RemoteForm {
        match model {
            Some(m) => RemoteForm {
                endpoint: m.endpoint.clone(),
                model: m.model.clone(),
                dimension: m.dimension.to_string(),
            },
            None => RemoteForm::default(),
        }
    }

    /// The model this form describes, checked as the bridge would.
    pub fn parse(&self) -> Result<RemoteModel, String> {
        let model = RemoteModel {
            endpoint: self.endpoint.trim().to_owned(),
            model: self.model.trim().to_owned(),
            dimension: number(&self.dimension, "dimension")?,
        };
        ModelSettings {
            local: None,
            remote: Some(model.clone()),
        }
        .validate()?;
        Ok(model)
    }
}

fn number<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, String> {
    text.trim()
        .parse()
        .map_err(|_| format!("{what}: `{}` is not a number", text.trim()))
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
                star: false,
                embedding_pending: 0,
            }
        );
        let mut s = synced(1);
        s.embedding = Some(sapphire_backend::protocol::EmbeddingProgress {
            vectors: 10,
            pending: 4,
            running: true,
        });
        assert_eq!(
            badge(&entry("a", None, true, s)).label(),
            "syncing · 1 peer · embedding 4 pending"
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

    fn peer(id: GrainId, priority: u8) -> PeerInfo {
        PeerInfo {
            device_id: id,
            name: "desk".into(),
            node_id: "aaaa".into(),
            connected: true,
            priority,
            availability: None,
        }
    }

    /// One frame of the device row, as the view runs it: prune, show, record. `typed` is what
    /// the user dials in this frame, if anything. Returns the value shown.
    fn frame(edits: &mut HashMap<GrainId, u8>, p: &PeerInfo, typed: Option<u8>) -> u8 {
        prune_edits(edits, std::slice::from_ref(p));
        let shown = typed.unwrap_or(edits.get(&p.device_id).copied().unwrap_or(p.priority));
        record_edit(edits, p, shown);
        shown
    }

    #[test]
    fn a_change_made_elsewhere_shows_in_an_untouched_row() {
        let id = GrainId::random();
        let mut edits = HashMap::new();
        assert_eq!(frame(&mut edits, &peer(id, 1), None), 1);
        assert_eq!(frame(&mut edits, &peer(id, 3), None), 3, "the row follows");
        assert!(edits.is_empty(), "nothing to Set");
    }

    #[test]
    fn a_pending_edit_survives_until_set() {
        let id = GrainId::random();
        let mut edits = HashMap::new();
        frame(&mut edits, &peer(id, 1), Some(5));
        assert_eq!(frame(&mut edits, &peer(id, 1), None), 5);
        assert_eq!(
            frame(&mut edits, &peer(id, 3), None),
            5,
            "outlives other changes"
        );
        assert_eq!(edits.get(&id), Some(&5));
        // Once the bridge agrees, the edit is done.
        assert_eq!(frame(&mut edits, &peer(id, 5), None), 5);
        assert!(edits.is_empty());
        // Dialling back to the bridge's value is no edit either.
        frame(&mut edits, &peer(id, 5), Some(7));
        frame(&mut edits, &peer(id, 5), Some(5));
        assert!(edits.is_empty());
    }

    #[test]
    fn an_edit_for_a_vanished_device_is_dropped() {
        let (gone, kept) = (GrainId::random(), GrainId::random());
        let mut edits = HashMap::from([(gone, 4), (kept, 4)]);
        prune_edits(&mut edits, &[peer(kept, 1)]);
        assert_eq!(edits, HashMap::from([(kept, 4)]));
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
                primary: Some(me),
                secondary: None,
            },
            WorkspaceRoles {
                workspace_id: GrainId::random(),
                primary: Some(me),
                secondary: None,
            },
            WorkspaceRoles {
                workspace_id: GrainId::random(),
                primary: None,
                secondary: Some(me),
            },
        ];
        assert_eq!(role_badges(&p, &roles), vec!["primary", "secondary"]);
    }

    #[test]
    fn a_star_workspace_says_so() {
        let mut s = synced(2);
        s.topology = Topology::Star {
            primary: GrainId::random(),
            secondary: None,
        };
        assert_eq!(
            badge(&entry("a", None, true, s)).label(),
            "syncing · 2 peers · star"
        );
    }

    // ── embedding ───────────────────────────────────────────────────────────

    fn embed_settings() -> sapphire_bridge_api::EmbedSettingsResult {
        sapphire_bridge_api::EmbedSettingsResult {
            avx2: true,
            ..Default::default()
        }
    }

    #[test]
    fn embedding_status_names_the_model_or_why_it_is_off() {
        use sapphire_bridge_api::{EmbedInfoResult, EmbedModelInfo, EmbedNote};

        let mut s = embed_settings();
        s.info.note = Some(EmbedNote::NotConfigured);
        assert_eq!(
            embedding_status(&s),
            ("Embedding is off: no model is configured".to_owned(), false)
        );

        s.active = Some(Slot::Remote);
        s.info = EmbedInfoResult {
            enabled: true,
            model: Some(EmbedModelInfo {
                model: "m".into(),
                dimension: 8,
                template_version: 0,
                revision: None,
                max_tokens: None,
            }),
            loaded: true,
            note: Some(EmbedNote::KeyMissing),
        };
        let (line, warn) = embedding_status(&s);
        assert!(warn);
        assert!(line.starts_with("Embedding with the remote model m (8 dimensions), loaded"));
        assert!(
            line.ends_with("no API key is set for the remote model"),
            "{line}"
        );
    }

    #[test]
    fn auto_says_why_the_local_slot_is_off() {
        let mut s = embed_settings();
        assert_eq!(auto_resolution(&s, Slot::Local), "Auto: on");
        s.avx2 = false;
        assert_eq!(
            auto_resolution(&s, Slot::Local),
            "Auto: off — this CPU has no AVX2"
        );
        assert_eq!(auto_resolution(&s, Slot::Remote), "Auto: on");
    }

    #[test]
    fn the_forms_parse_and_validate_like_the_bridge() {
        let local = LocalForm::from(None);
        assert_eq!(local.parse().unwrap(), LocalModel::default());
        let too_big = LocalForm {
            dimension: "4096".into(),
            ..local.clone()
        };
        assert!(too_big.parse().unwrap_err().contains("dimension"));
        let nan = LocalForm {
            max_tokens: "lots".into(),
            ..local
        };
        assert!(nan.parse().unwrap_err().contains("not a number"));

        let remote = RemoteForm {
            endpoint: " https://e ".into(),
            model: "m".into(),
            dimension: "8".into(),
        };
        let parsed = remote.parse().unwrap();
        assert_eq!(parsed.endpoint, "https://e");
        assert_eq!(RemoteForm::from(Some(&parsed)).dimension, "8");
        assert!(RemoteForm::default().parse().is_err());
    }
}
