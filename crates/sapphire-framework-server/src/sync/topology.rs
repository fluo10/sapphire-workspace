//! Which peers a host syncs a workspace with, given the bridge's elected roles.

use grain_id::GrainId;
use sapphire_backend::protocol as proto;
use sapphire_bridge_api::WorkspaceRoles;

/// What this host does about one peer for one workspace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Link {
    /// Dial it (this host has the lower id).
    Dial,
    /// Let it dial (it has the lower id), and accept its stream.
    Await,
    /// Neither: both are outside the star's hub, so they sync through it.
    Skip,
}

/// The link rule. With no designated or backup device for the workspace, the mesh rule
/// applies unchanged. Otherwise only a pair with at least one hub in it links.
pub(crate) fn link(me: GrainId, peer: GrainId, roles: Option<&WorkspaceRoles>) -> Link {
    let hub = |d: GrainId| roles.is_some_and(|r| r.designated == Some(d) || r.backup == Some(d));
    let star = roles.is_some_and(|r| r.designated.is_some() || r.backup.is_some());
    if star && !hub(me) && !hub(peer) {
        Link::Skip
    } else if me < peer {
        Link::Dial
    } else {
        Link::Await
    }
}

/// What `sync.status` reports for these roles.
pub(crate) fn topology(roles: Option<&WorkspaceRoles>) -> proto::Topology {
    match roles.and_then(|r| r.designated.map(|d| (d, r.backup))) {
        Some((designated, backup)) => proto::Topology::Star { designated, backup },
        None => proto::Topology::Mesh,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(n: usize) -> Vec<GrainId> {
        let mut v: Vec<GrainId> = (0..n).map(|_| GrainId::random()).collect();
        v.sort();
        v
    }

    fn roles(ws: GrainId, d: Option<GrainId>, b: Option<GrainId>) -> WorkspaceRoles {
        WorkspaceRoles {
            workspace_id: ws,
            designated: d,
            backup: b,
        }
    }

    #[test]
    fn no_roles_is_the_mesh() {
        let id = sorted(2);
        assert_eq!(link(id[0], id[1], None), Link::Dial);
        assert_eq!(link(id[1], id[0], None), Link::Await);
        let empty = roles(GrainId::random(), None, None);
        assert_eq!(link(id[0], id[1], Some(&empty)), Link::Dial);
    }

    #[test]
    fn an_entry_with_no_roles_is_the_mesh() {
        // The bridge reports an entry with both fields `None` while it waits to elect, or
        // when no device is a candidate. That must read exactly as no entry.
        let id = sorted(2);
        let empty = roles(GrainId::random(), None, None);
        assert_eq!(link(id[0], id[1], Some(&empty)), Link::Dial);
        assert_eq!(link(id[1], id[0], Some(&empty)), Link::Await);
        assert_eq!(topology(Some(&empty)), proto::Topology::Mesh);
    }

    #[test]
    fn two_non_hubs_skip_each_other() {
        let id = sorted(4); // id[0] designated, id[1] backup, id[2] and id[3] neither
        let r = roles(GrainId::random(), Some(id[0]), Some(id[1]));
        assert_eq!(link(id[2], id[3], Some(&r)), Link::Skip);
        assert_eq!(link(id[3], id[2], Some(&r)), Link::Skip);
    }

    #[test]
    fn a_hub_accepts_everyone() {
        let id = sorted(4);
        let r = roles(GrainId::random(), Some(id[3]), Some(id[2]));
        // Non-hub to hub follows the id rule, both ways.
        assert_eq!(link(id[0], id[3], Some(&r)), Link::Dial);
        assert_eq!(link(id[3], id[0], Some(&r)), Link::Await);
        // The two hubs link to each other.
        assert_eq!(link(id[2], id[3], Some(&r)), Link::Dial);
    }

    #[test]
    fn an_old_server_that_dials_a_hub_is_kept() {
        // An old server dials everyone. Its stream reaches a hub, and the hub's guard keeps
        // it, because `link` from the hub's side is never `Skip`.
        let id = sorted(3);
        let r = roles(GrainId::random(), Some(id[2]), None);
        assert_ne!(link(id[2], id[0], Some(&r)), Link::Skip);
    }

    #[test]
    fn topology_is_star_only_with_a_designated_device() {
        let d = GrainId::random();
        assert_eq!(topology(None), proto::Topology::Mesh);
        assert_eq!(
            topology(Some(&roles(GrainId::random(), Some(d), None))),
            proto::Topology::Star {
                designated: d,
                backup: None
            }
        );
    }
}
