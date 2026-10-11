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

/// The link rule. With no primary device for the workspace, the mesh rule applies
/// unchanged, as [`topology`] reports. Otherwise only a pair with at least one hub (the
/// primary or the secondary device) in it links.
pub(crate) fn link(me: GrainId, peer: GrainId, roles: Option<&WorkspaceRoles>) -> Link {
    let hub = |d: GrainId| roles.is_some_and(|r| r.primary == Some(d) || r.secondary == Some(d));
    let star = roles.is_some_and(|r| r.primary.is_some());
    if star && !hub(me) && !hub(peer) {
        Link::Skip
    } else if me < peer {
        Link::Dial
    } else {
        Link::Await
    }
}

/// Whether a host may close the sessions [`link`] now says to skip.
///
/// Not until it is a hub itself or holds a session with one of these roles' hubs. During a
/// handover two hosts can name different hubs for a moment (#192): a host that closed its
/// session with the device just promoted — a non-hub in its stale view — while the hub it
/// does name still refused it would have no path at all. Keeping the old session until a
/// hub answers costs one redundant session for a Hello round.
pub(crate) fn may_close_skipped(
    me: GrainId,
    roles: Option<&WorkspaceRoles>,
    open: &[GrainId],
) -> bool {
    let Some(r) = roles else {
        return true;
    };
    let hub = |d: &GrainId| r.primary == Some(*d) || r.secondary == Some(*d);
    hub(&me) || open.iter().any(hub)
}

/// What `sync.status` reports for these roles.
pub(crate) fn topology(roles: Option<&WorkspaceRoles>) -> proto::Topology {
    match roles.and_then(|r| r.primary.map(|d| (d, r.secondary))) {
        Some((primary, secondary)) => proto::Topology::Star { primary, secondary },
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
            primary: d,
            secondary: b,
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
    fn skipped_sessions_close_only_with_a_hub_session_in_hand() {
        let id = sorted(4);
        let r = roles(GrainId::random(), Some(id[0]), Some(id[1]));
        assert!(may_close_skipped(id[0], Some(&r), &[]), "a hub");
        assert!(!may_close_skipped(id[2], Some(&r), &[id[3]]));
        assert!(may_close_skipped(id[2], Some(&r), &[id[3], id[1]]));
        assert!(may_close_skipped(id[2], None, &[]), "the mesh skips no one");
    }

    /// The handover of #192, host by host, with each host acting on its own (stale) view:
    /// four hosts B < X < Y < D, D primary and B secondary; D goes away. Every host that is
    /// up must hold a session at every step.
    #[test]
    fn a_handover_with_stale_views_never_strands_a_host() {
        use std::collections::{BTreeMap, BTreeSet};

        let id = sorted(4);
        let (b, x, y, d) = (id[0], id[1], id[2], id[3]);
        let ws = GrainId::random();
        let view = |p, s| roles(ws, Some(p), s);

        type Pair = (GrainId, GrainId);
        let pair = |a: GrainId, c: GrainId| (a.min(c), a.max(c));
        // Sessions before D goes away: the star around D and B.
        let mut sessions: BTreeSet<Pair> =
            [pair(x, d), pair(y, d), pair(b, d), pair(x, b), pair(y, b)]
                .into_iter()
                .collect();

        // What each live host believes, step by step after D is gone.
        let steps: Vec<BTreeMap<GrainId, WorkspaceRoles>> = vec![
            // B lost D and promoted itself; X and Y still hold D's claim.
            [
                (b, view(b, None)),
                (x, view(d, Some(b))),
                (y, view(d, Some(b))),
            ]
            .into(),
            // X heard B's claim beside D's stale one: (D, Y). Y has not caught up.
            [
                (b, view(b, None)),
                (x, view(d, Some(y))),
                (y, view(d, Some(b))),
            ]
            .into(),
            // Y's next tick: (D, Y) too.
            [
                (b, view(b, Some(y))),
                (x, view(d, Some(y))),
                (y, view(d, Some(y))),
            ]
            .into(),
            // Everyone forgot D.
            [
                (b, view(b, Some(y))),
                (x, view(b, Some(y))),
                (y, view(b, Some(y))),
            ]
            .into(),
        ];
        sessions.retain(|(p, q)| *p != d && *q != d);

        for (n, views) in steps.iter().enumerate() {
            let open = |h: GrainId, s: &BTreeSet<Pair>| -> Vec<GrainId> {
                s.iter()
                    .filter_map(|&(p, q)| (p == h).then_some(q).or((q == h).then_some(p)))
                    .collect()
            };
            // Each host closes what its view skips, if it may.
            for (&me, r) in views {
                let mine = open(me, &sessions);
                if may_close_skipped(me, Some(r), &mine) {
                    sessions.retain(|&(p, q)| {
                        let other = if p == me {
                            q
                        } else if q == me {
                            p
                        } else {
                            return true;
                        };
                        link(me, other, Some(r)) != Link::Skip
                    });
                }
            }
            // Each host dials what its view says to; the other end accepts unless its view
            // skips the dialer.
            for (&me, r) in views {
                for (&peer, theirs) in views {
                    if peer != me
                        && link(me, peer, Some(r)) == Link::Dial
                        && link(peer, me, Some(theirs)) != Link::Skip
                    {
                        sessions.insert(pair(me, peer));
                    }
                }
            }
            for &h in views.keys() {
                assert!(
                    !open(h, &sessions).is_empty(),
                    "step {n}: host {h} has no session; sessions {sessions:?}"
                );
            }
        }
    }

    #[test]
    fn two_non_hubs_skip_each_other() {
        let id = sorted(4); // id[0] primary, id[1] secondary, id[2] and id[3] neither
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
    fn secondary_only_roles_are_the_mesh() {
        let id = sorted(3);
        let r = roles(GrainId::random(), None, Some(id[2]));
        assert_eq!(link(id[0], id[1], Some(&r)), Link::Dial);
        assert_eq!(link(id[1], id[0], Some(&r)), Link::Await);
        assert_eq!(topology(Some(&r)), proto::Topology::Mesh);
    }

    /// Every pair of me / peer drawn from primary, secondary and two non-hubs, under every
    /// assignment of ids to those roles, so both id orders are covered for each pair.
    #[test]
    fn link_covers_every_role_pair_in_both_id_orders() {
        // Role slots: 0 primary, 1 secondary, 2 and 3 neither.
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
        // Non-hub against the secondary, both orders.
        let r = roles(GrainId::random(), Some(id[3]), Some(id[1]));
        assert_eq!(link(id[0], id[1], Some(&r)), Link::Dial);
        assert_eq!(link(id[2], id[1], Some(&r)), Link::Await);
        // A non-hub whose id is greater than the primary's awaits it.
        let r = roles(GrainId::random(), Some(id[0]), Some(id[1]));
        assert_eq!(link(id[3], id[0], Some(&r)), Link::Await);
        // A primary whose id is lower than a non-hub's dials it.
        assert_eq!(link(id[0], id[3], Some(&r)), Link::Dial);
    }

    #[test]
    fn topology_is_star_only_with_a_primary_device() {
        let d = GrainId::random();
        assert_eq!(topology(None), proto::Topology::Mesh);
        assert_eq!(
            topology(Some(&roles(GrainId::random(), Some(d), None))),
            proto::Topology::Star {
                primary: d,
                secondary: None
            }
        );
    }
}
