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

/// The link rule. With no designated device for the workspace, the mesh rule applies
/// unchanged, as [`topology`] reports. Otherwise only a pair with at least one hub (the
/// designated or the backup device) in it links.
pub(crate) fn link(me: GrainId, peer: GrainId, roles: Option<&WorkspaceRoles>) -> Link {
    let hub = |d: GrainId| roles.is_some_and(|r| r.designated == Some(d) || r.backup == Some(d));
    let star = roles.is_some_and(|r| r.designated.is_some());
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
    fn backup_only_roles_are_the_mesh() {
        let id = sorted(3);
        let r = roles(GrainId::random(), None, Some(id[2]));
        assert_eq!(link(id[0], id[1], Some(&r)), Link::Dial);
        assert_eq!(link(id[1], id[0], Some(&r)), Link::Await);
        assert_eq!(topology(Some(&r)), proto::Topology::Mesh);
    }

    /// Every pair of me / peer drawn from designated, backup and two non-hubs, under every
    /// assignment of ids to those roles, so both id orders are covered for each pair.
    #[test]
    fn link_covers_every_role_pair_in_both_id_orders() {
        // Role slots: 0 designated, 1 backup, 2 and 3 neither.
        let perms: Vec<[usize; 4]> = {
            let mut out = Vec::new();
            for a in 0..4 {
                for b in 0..4 {
                    for c in 0..4 {
                        for d in 0..4 {
                            let p = [a, b, c, d];
                            let mut s = p;
                            s.sort();
                            if s == [0, 1, 2, 3] {
                                out.push(p);
                            }
                        }
                    }
                }
            }
            out
        };
        assert_eq!(perms.len(), 24);
        let ids = sorted(4);
        let mut seen = Vec::new();
        for perm in perms {
            // `perm[slot]` is the rank of the id that slot gets.
            let id = |slot: usize| ids[perm[slot]];
            let r = roles(GrainId::random(), Some(id(0)), Some(id(1)));
            for me in 0..4 {
                for peer in 0..4 {
                    if me == peer {
                        continue;
                    }
                    let expected = if me >= 2 && peer >= 2 {
                        Link::Skip
                    } else if id(me) < id(peer) {
                        Link::Dial
                    } else {
                        Link::Await
                    };
                    assert_eq!(
                        link(id(me), id(peer), Some(&r)),
                        expected,
                        "me slot {me}, peer slot {peer}, ranks {perm:?}"
                    );
                    seen.push((me.min(2), peer.min(2), expected));
                }
            }
        }
        // Each (me role, peer role) with a hub in it was seen as both Dial and Await, and
        // the non-hub pair only as Skip.
        for me in 0..3 {
            for peer in 0..3 {
                if me == peer && me < 2 {
                    continue;
                }
                if me == 2 && peer == 2 {
                    assert!(seen.contains(&(2, 2, Link::Skip)));
                } else {
                    assert!(
                        seen.contains(&(me, peer, Link::Dial)),
                        "{me} -> {peer} Dial"
                    );
                    assert!(
                        seen.contains(&(me, peer, Link::Await)),
                        "{me} -> {peer} Await"
                    );
                }
            }
        }
    }

    #[test]
    fn named_cases_against_the_hubs() {
        let id = sorted(4);
        // Non-hub against the backup, both orders.
        let r = roles(GrainId::random(), Some(id[3]), Some(id[1]));
        assert_eq!(link(id[0], id[1], Some(&r)), Link::Dial);
        assert_eq!(link(id[2], id[1], Some(&r)), Link::Await);
        // A non-hub whose id is greater than the designated's awaits it.
        let r = roles(GrainId::random(), Some(id[0]), Some(id[1]));
        assert_eq!(link(id[3], id[0], Some(&r)), Link::Await);
        // A designated whose id is lower than a non-hub's dials it.
        assert_eq!(link(id[0], id[3], Some(&r)), Link::Dial);
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
