//! Who is a workspace's primary and secondary device, as this host sees it.
//!
//! OSPF's DR/BDR election, minus the consensus: every bridge computes the roles from the
//! Hellos it hears, and two bridges with different views may disagree for a while. That is
//! accepted by design (spec decision 2): the work the primary device does is idempotent.
//! Claims make the election non-preemptive — a device that holds a role keeps it while it
//! is heard, whoever turns up.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use grain_id::GrainId;

use crate::hello::Hello;

/// One device that could take a role for one workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Candidate {
    pub device_id: GrainId,
    pub priority: u8,
    pub availability: Option<u8>,
    pub claims_primary: bool,
    pub claims_secondary: bool,
}

/// The outcome for one workspace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roles {
    pub primary: Option<GrainId>,
    pub secondary: Option<GrainId>,
}

fn rank(c: &Candidate) -> (u8, Option<u8>, GrainId) {
    (c.priority, c.availability, c.device_id)
}

fn best(pool: &[&Candidate], pick: impl Fn(&Candidate) -> bool) -> Option<GrainId> {
    pool.iter()
        .copied()
        .filter(|c| pick(c))
        .max_by_key(|c| rank(c))
        .map(|c| c.device_id)
}

/// Elect the primary and secondary device among `candidates`.
///
/// Primary: the best current claimant; else, if `promote`, the best secondary claimant; else
/// the best candidate. Secondary: the same over the rest, with secondary claims.
///
/// `promote` gates promotion: a secondary claim only shows the primary device is gone if a
/// primary claim was seen before. Otherwise the claim may be provisional (the primary
/// device has not announced yet), and promoting on it lets a lower-ranked device take the
/// role and flap it.
pub(crate) fn elect(candidates: &[Candidate], promote: bool) -> Roles {
    let eligible: Vec<&Candidate> = candidates.iter().filter(|c| c.priority > 0).collect();
    let primary = best(&eligible, |c| c.claims_primary)
        .or_else(|| {
            if promote {
                best(&eligible, |c| c.claims_secondary)
            } else {
                None
            }
        })
        .or_else(|| best(&eligible, |_| true));
    let rest: Vec<&Candidate> = eligible
        .iter()
        .copied()
        .filter(|c| Some(c.device_id) != primary)
        .collect();
    let secondary = best(&rest, |c| c.claims_secondary).or_else(|| best(&rest, |_| true));
    Roles { primary, secondary }
}

/// What this host says about itself, before claims.
pub(crate) struct Own {
    pub priority: u8,
    pub availability: Option<u8>,
    pub hosting: Vec<GrainId>,
}

/// This host's side of the election: its claims, and when it started hosting each workspace.
pub(crate) struct Elector {
    me: GrainId,
    wait: Duration,
    since: BTreeMap<GrainId, Instant>,
    primary: BTreeSet<GrainId>,
    /// Workspaces in which a primary claim has been heard (from anyone, this host included).
    seen_primary: BTreeSet<GrainId>,
    secondary: BTreeSet<GrainId>,
}

impl Elector {
    pub(crate) fn new(me: GrainId, wait: Duration) -> Elector {
        Elector {
            me,
            wait,
            since: BTreeMap::new(),
            primary: BTreeSet::new(),
            seen_primary: BTreeSet::new(),
            secondary: BTreeSet::new(),
        }
    }

    /// Run one round: elect every hosted workspace, update this host's claims, and return
    /// the Hello to send and the roles to report.
    ///
    /// While a workspace is younger than the wait, this host is left out of its election
    /// entirely, so it can neither claim nor be reported as holding a role it has not
    /// announced — the peers' existing claims get `wait` to arrive first.
    ///
    /// The roles reported for a workspace are `Roles::default()` (mesh) until this host has
    /// seen a primary claim in it, its own included. Claims are made and published all
    /// the same.
    pub(crate) fn step(
        &mut self,
        now: Instant,
        own: &Own,
        peers: &[Hello],
    ) -> (Hello, BTreeMap<GrainId, Roles>) {
        self.since.retain(|ws, _| own.hosting.contains(ws));
        for ws in &own.hosting {
            self.since.entry(*ws).or_insert(now);
        }
        self.primary.retain(|ws| own.hosting.contains(ws));
        self.seen_primary.retain(|ws| own.hosting.contains(ws));
        self.secondary.retain(|ws| own.hosting.contains(ws));

        let mut roles = BTreeMap::new();
        for ws in &own.hosting {
            let waited = now.duration_since(self.since[ws]) >= self.wait;
            let mut candidates: Vec<Candidate> = peers
                .iter()
                .filter(|h| h.device_id != self.me && h.hosting.contains(ws))
                .map(|h| Candidate {
                    device_id: h.device_id,
                    priority: h.priority,
                    availability: h.availability,
                    claims_primary: h.primary.contains(ws),
                    claims_secondary: h.secondary.contains(ws),
                })
                .collect();
            if waited {
                candidates.push(Candidate {
                    device_id: self.me,
                    priority: own.priority,
                    availability: own.availability,
                    claims_primary: self.primary.contains(ws),
                    claims_secondary: self.secondary.contains(ws),
                });
            }
            if candidates.iter().any(|c| c.claims_primary) {
                self.seen_primary.insert(*ws);
            }
            let elected = elect(&candidates, self.seen_primary.contains(ws));
            if waited {
                set(&mut self.primary, *ws, elected.primary == Some(self.me));
                set(&mut self.secondary, *ws, elected.secondary == Some(self.me));
            }
            // This host's own claim counts as seen at once.
            if self.primary.contains(ws) {
                self.seen_primary.insert(*ws);
            }
            // No star before a primary claim: until then each waiting host leaves itself
            // out of its own election, so hosts elect different hubs, and a star on those
            // would close working sessions and could strand a host. Once a claim exists,
            // every host agrees on the primary device, and every non-hub keeps a path
            // through it.
            let reported = if self.seen_primary.contains(ws) {
                elected
            } else {
                Roles::default()
            };
            roles.insert(*ws, reported);
        }

        let hello = Hello {
            device_id: self.me,
            priority: own.priority,
            availability: own.availability,
            hosting: own.hosting.clone(),
            primary: self.primary.iter().copied().collect(),
            secondary: self.secondary.iter().copied().collect(),
        };
        (hello, roles)
    }
}

fn set(claims: &mut BTreeSet<GrainId>, ws: GrainId, on: bool) {
    if on {
        claims.insert(ws);
    } else {
        claims.remove(&ws);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<GrainId> {
        let mut v: Vec<GrainId> = (0..n).map(|_| GrainId::random()).collect();
        v.sort();
        v
    }

    fn cand(id: GrainId, priority: u8) -> Candidate {
        Candidate {
            device_id: id,
            priority,
            availability: None,
            claims_primary: false,
            claims_secondary: false,
        }
    }

    #[test]
    fn no_candidates_elect_nobody() {
        assert_eq!(elect(&[], true), Roles::default());
    }

    #[test]
    fn priority_zero_is_never_elected() {
        let id = ids(1);
        assert_eq!(elect(&[cand(id[0], 0)], true), Roles::default());
    }

    #[test]
    fn rank_is_priority_then_availability_then_id() {
        let id = ids(3);
        let mut a = cand(id[0], 2);
        a.availability = Some(0);
        let b = cand(id[1], 2); // no availability: ranks below Some(0)
        let c = cand(id[2], 1); // highest id, lowest priority
        let roles = elect(&[a, b, c], false);
        assert_eq!(roles.primary, Some(id[0]));
        assert_eq!(roles.secondary, Some(id[1]));
    }

    #[test]
    fn an_existing_claim_survives_a_higher_ranked_newcomer() {
        let id = ids(2);
        let mut holder = cand(id[0], 1);
        holder.claims_primary = true;
        let newcomer = cand(id[1], 9);
        let roles = elect(&[holder, newcomer], false);
        assert_eq!(roles.primary, Some(id[0]));
        assert_eq!(roles.secondary, Some(id[1]));
    }

    #[test]
    fn the_secondary_is_promoted_when_the_primary_is_gone() {
        let id = ids(3);
        let mut secondary = cand(id[0], 1);
        secondary.claims_secondary = true;
        let roles = elect(&[secondary, cand(id[1], 1), cand(id[2], 9)], true);
        assert_eq!(roles.primary, Some(id[0]));
        assert_eq!(roles.secondary, Some(id[2]));
    }

    #[test]
    fn two_primary_claims_resolve_to_the_higher_rank() {
        let id = ids(2);
        let mut low = cand(id[0], 1);
        low.claims_primary = true;
        let mut high = cand(id[1], 1);
        high.claims_primary = true;
        assert_eq!(elect(&[low, high], false).primary, Some(id[1]));
    }

    fn hello(id: GrainId, priority: u8, ws: GrainId) -> Hello {
        Hello {
            device_id: id,
            priority,
            availability: None,
            hosting: vec![ws],
            primary: vec![],
            secondary: vec![],
        }
    }

    #[test]
    fn a_fresh_host_waits_before_claiming() {
        let id = ids(2);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[1], Duration::from_secs(40));
        let own = Own {
            priority: 9,
            availability: None,
            hosting: vec![ws],
        };
        let mut peer = hello(id[0], 1, ws);
        peer.primary = vec![ws];

        let (out, roles) = elector.step(t0, &own, &[peer.clone()]);
        assert!(
            out.primary.is_empty() && out.secondary.is_empty(),
            "no claims while waiting"
        );
        assert_eq!(roles[&ws].primary, Some(id[0]), "the peer's claim stands");
        assert_eq!(
            roles[&ws].secondary, None,
            "this host is not a candidate while waiting"
        );

        let (out, roles) = elector.step(t0 + Duration::from_secs(41), &own, &[peer]);
        assert_eq!(
            roles[&ws].primary,
            Some(id[0]),
            "no preemption after the wait either"
        );
        assert_eq!(out.secondary, vec![ws], "it takes the free secondary role");
    }

    #[test]
    fn no_star_is_reported_before_a_primary_claim_is_seen() {
        let id = ids(2);
        let (me, peer) = (id[0], id[1]);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(me, Duration::from_secs(40));
        let own = Own {
            priority: 1,
            availability: None,
            hosting: vec![ws],
        };
        let unclaimed = hello(peer, 1, ws);

        let (_, roles) = elector.step(t0, &own, std::slice::from_ref(&unclaimed));
        assert_eq!(roles[&ws], Roles::default(), "mesh while waiting");

        let (out, roles) = elector.step(t0 + Duration::from_secs(41), &own, &[unclaimed]);
        assert_eq!(
            roles[&ws],
            Roles::default(),
            "mesh after the wait too, while nobody claims primary"
        );
        assert_eq!(
            out.secondary,
            vec![ws],
            "the claim is still made and published"
        );
        assert!(out.primary.is_empty());
    }

    /// Four hosts at the default priority start out of phase. Each steps every interval with
    /// the others' latest Hellos, and `wait` = DEAD, as a real bridge does.
    ///
    /// This pins the simpler property, not full pairwise liveness:
    /// - until a host has seen a primary claim, it reports no star (mesh);
    /// - every host that reports a star names the same primary device, and that device
    ///   has itself claimed the role;
    /// - a host that reports a star and holds no role has a hub, by its own report, that
    ///   either considers itself a hub or reports mesh, so it links to everyone;
    /// - in the end every host reports the same primary device, and exactly one claims it.
    #[test]
    fn a_cold_start_never_strands_a_host() {
        const DEAD: Duration = Duration::from_secs(40);
        const INTERVAL: Duration = Duration::from_secs(10);
        let orders: Vec<[usize; 4]> = (0..256usize)
            .map(|n| [n % 4, n / 4 % 4, n / 16 % 4, n / 64])
            .filter(|o| (0..4).all(|i| o.contains(&i)))
            .collect();
        assert_eq!(orders.len(), 24);

        for order in orders {
            let id = ids(4);
            let ws = GrainId::random();
            let own = Own {
                priority: 1,
                availability: None,
                hosting: vec![ws],
            };
            let t0 = Instant::now();
            let mut electors: Vec<Elector> = id.iter().map(|i| Elector::new(*i, DEAD)).collect();
            // Host `order[k]` starts k * 3 s after t0.
            let mut events: Vec<(Instant, usize)> = Vec::new();
            for (k, &host) in order.iter().enumerate() {
                let start = t0 + Duration::from_secs(3 * k as u64);
                for n in 0..20u32 {
                    events.push((start + INTERVAL * n, host));
                }
            }
            events.sort();

            let mut latest: Vec<Option<Hello>> = vec![None; 4];
            let mut reports: Vec<Option<Roles>> = vec![None; 4];
            let mut seen_claim = [false; 4];
            let index = |d: GrainId| id.iter().position(|i| *i == d).unwrap();
            for (now, h) in events {
                let peers: Vec<Hello> = (0..4)
                    .filter(|&p| p != h)
                    .filter_map(|p| latest[p].clone())
                    .collect();
                let (out, roles) = electors[h].step(now, &own, &peers);
                seen_claim[h] |=
                    peers.iter().any(|p| p.primary.contains(&ws)) || out.primary.contains(&ws);
                latest[h] = Some(out);
                let r = roles[&ws];
                if !seen_claim[h] {
                    assert_eq!(r, Roles::default(), "{order:?}: a star before any claim");
                }
                reports[h] = Some(r);

                let stars: Vec<(usize, Roles)> = reports
                    .iter()
                    .enumerate()
                    .filter_map(|(x, r)| r.filter(|r| r.primary.is_some()).map(|r| (x, r)))
                    .collect();
                for (x, r) in &stars {
                    let d = r.primary.unwrap();
                    assert_eq!(r.primary, stars[0].1.primary, "{order:?}: two stars");
                    assert!(
                        latest[index(d)].as_ref().unwrap().primary.contains(&ws),
                        "{order:?}: a star around a device that has not claimed"
                    );
                    let me = id[*x];
                    if r.primary != Some(me) && r.secondary != Some(me) {
                        let linked = [r.primary, r.secondary].into_iter().flatten().any(|hub| {
                            reports[index(hub)].is_some_and(|hr| {
                                hr == Roles::default()
                                    || hr.primary == Some(hub)
                                    || hr.secondary == Some(hub)
                            })
                        });
                        assert!(linked, "{order:?}: host {x} has no hub that links to it");
                    }
                }
            }

            let primary: Vec<Option<GrainId>> =
                reports.iter().map(|r| r.unwrap().primary).collect();
            assert!(primary[0].is_some(), "{order:?}: no star in the end");
            assert!(
                primary.iter().all(|d| *d == primary[0]),
                "{order:?}: hosts disagree in the end"
            );
            let claimants = latest
                .iter()
                .filter(|h| h.as_ref().unwrap().primary.contains(&ws))
                .count();
            assert_eq!(claimants, 1, "{order:?}");
        }
    }

    #[test]
    fn a_lone_host_claims_primary_after_the_wait() {
        let id = ids(1);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::from_secs(40));
        let own = Own {
            priority: 1,
            availability: None,
            hosting: vec![ws],
        };
        elector.step(t0, &own, &[]);
        let (out, roles) = elector.step(t0 + Duration::from_secs(41), &own, &[]);
        assert_eq!(roles[&ws].primary, Some(id[0]));
        assert_eq!(out.primary, vec![ws]);
    }

    #[test]
    fn a_peer_not_hosting_the_workspace_is_not_a_candidate() {
        let id = ids(2);
        let ws = GrainId::random();
        let other = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::ZERO);
        let own = Own {
            priority: 0,
            availability: None,
            hosting: vec![ws],
        };
        let (_, roles) = elector.step(t0, &own, &[hello(id[1], 5, other)]);
        assert_eq!(roles[&ws], Roles::default());
    }

    #[test]
    fn a_workspace_no_longer_hosted_drops_its_claims() {
        let id = ids(1);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::ZERO);
        elector.step(
            t0,
            &Own {
                priority: 1,
                availability: None,
                hosting: vec![ws],
            },
            &[],
        );
        let (out, roles) = elector.step(
            t0,
            &Own {
                priority: 1,
                availability: None,
                hosting: vec![],
            },
            &[],
        );
        assert!(out.primary.is_empty());
        assert!(roles.is_empty());
    }

    #[test]
    fn a_provisional_secondary_claim_does_not_promote() {
        let id = ids(2);
        let (b_id, a_id) = (id[0], id[1]);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut a = Elector::new(a_id, Duration::ZERO);
        let mut b = Elector::new(b_id, Duration::ZERO);
        let own_a = Own {
            priority: 9,
            availability: None,
            hosting: vec![ws],
        };
        let own_b = Own {
            priority: 1,
            availability: None,
            hosting: vec![ws],
        };
        let a_unclaimed = hello(a_id, 9, ws);
        let mut b_hello = None;
        for _ in 0..2 {
            let (h, roles) = b.step(t0, &own_b, std::slice::from_ref(&a_unclaimed));
            // The reported roles are mesh before any primary claim, so the gate shows in
            // the claims B publishes, not in what it reports.
            assert!(h.primary.is_empty(), "B must not promote itself");
            assert_eq!(roles[&ws], Roles::default(), "no star before a claim");
            b_hello = Some(h);
        }
        let b_hello = b_hello.unwrap();
        assert_eq!(b_hello.secondary, vec![ws]);
        let (a_out, a_roles) = a.step(t0, &own_a, &[b_hello]);
        assert_eq!(a_roles[&ws].primary, Some(a_id));
        assert_eq!(a_roles[&ws].secondary, Some(b_id));
        let (_, b_roles) = b.step(t0, &own_b, &[a_out]);
        assert_eq!(b_roles[&ws].primary, Some(a_id));
        assert_eq!(b_roles[&ws].secondary, Some(b_id));
    }

    #[test]
    fn the_secondary_takes_over_when_the_primary_vanishes() {
        let id = ids(3);
        let (b_id, c_id, a_id) = (id[0], id[1], id[2]);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut a = Elector::new(a_id, Duration::ZERO);
        let mut b = Elector::new(b_id, Duration::ZERO);
        let mut c = Elector::new(c_id, Duration::ZERO);
        let own = |priority| Own {
            priority,
            availability: None,
            hosting: vec![ws],
        };
        let (own_a, own_b, own_c) = (own(9), own(1), own(5));

        // A and B converge first: A primary, B secondary.
        let mut a_hello = hello(a_id, 9, ws);
        let mut b_hello = hello(b_id, 1, ws);
        for _ in 0..4 {
            a_hello = a.step(t0, &own_a, &[b_hello.clone()]).0;
            b_hello = b.step(t0, &own_b, &[a_hello.clone()]).0;
        }
        assert_eq!(a_hello.primary, vec![ws]);
        assert_eq!(b_hello.secondary, vec![ws]);

        // C outranks B but arrives late: B's claim stands, C gets no role.
        let mut c_hello = hello(c_id, 5, ws);
        for _ in 0..3 {
            c_hello = c.step(t0, &own_c, &[a_hello.clone(), b_hello.clone()]).0;
            a_hello = a.step(t0, &own_a, &[b_hello.clone(), c_hello.clone()]).0;
            b_hello = b.step(t0, &own_b, &[a_hello.clone(), c_hello.clone()]).0;
        }
        assert_eq!(a_hello.primary, vec![ws]);
        assert_eq!(b_hello.secondary, vec![ws]);
        assert!(c_hello.primary.is_empty() && c_hello.secondary.is_empty());

        // A vanishes: B takes over although C outranks it.
        let (out, roles) = b.step(t0, &own_b, &[c_hello]);
        assert_eq!(roles[&ws].primary, Some(b_id));
        assert_eq!(out.primary, vec![ws]);
    }
}
