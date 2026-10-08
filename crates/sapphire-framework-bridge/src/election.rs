//! Who is a workspace's designated and backup device, as this host sees it.
//!
//! OSPF's DR/BDR election, minus the consensus: every bridge computes the roles from the
//! Hellos it hears, and two bridges with different views may disagree for a while. That is
//! accepted by design (spec decision 2): the work the designated device does is idempotent.
//! Claims make the election non-preemptive — a device that holds a role keeps it while it
//! is heard, whoever turns up.
// Used from Task 5 (the bridge wiring); unused outside tests until then.
#![cfg_attr(not(test), allow(dead_code))]

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
    pub claims_designated: bool,
    pub claims_backup: bool,
}

/// The outcome for one workspace.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roles {
    pub designated: Option<GrainId>,
    pub backup: Option<GrainId>,
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

/// Elect the designated and backup device among `candidates`.
///
/// Designated: the best current claimant; else the best backup claimant (promotion); else
/// the best candidate. Backup: the same over the rest, with backup claims.
pub(crate) fn elect(candidates: &[Candidate]) -> Roles {
    let eligible: Vec<&Candidate> = candidates.iter().filter(|c| c.priority > 0).collect();
    let designated = best(&eligible, |c| c.claims_designated)
        .or_else(|| best(&eligible, |c| c.claims_backup))
        .or_else(|| best(&eligible, |_| true));
    let rest: Vec<&Candidate> = eligible
        .iter()
        .copied()
        .filter(|c| Some(c.device_id) != designated)
        .collect();
    let backup = best(&rest, |c| c.claims_backup).or_else(|| best(&rest, |_| true));
    Roles { designated, backup }
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
    designated: BTreeSet<GrainId>,
    backup: BTreeSet<GrainId>,
}

impl Elector {
    pub(crate) fn new(me: GrainId, wait: Duration) -> Elector {
        Elector { me, wait, since: BTreeMap::new(), designated: BTreeSet::new(), backup: BTreeSet::new() }
    }

    /// Run one round: elect every hosted workspace, update this host's claims, and return
    /// the Hello to send and the roles to report.
    ///
    /// While a workspace is younger than the wait, this host is left out of its election
    /// entirely, so it can neither claim nor be reported as holding a role it has not
    /// announced — the peers' existing claims get `wait` to arrive first.
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
        self.designated.retain(|ws| own.hosting.contains(ws));
        self.backup.retain(|ws| own.hosting.contains(ws));

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
                    claims_designated: h.designated.contains(ws),
                    claims_backup: h.backup.contains(ws),
                })
                .collect();
            if waited {
                candidates.push(Candidate {
                    device_id: self.me,
                    priority: own.priority,
                    availability: own.availability,
                    claims_designated: self.designated.contains(ws),
                    claims_backup: self.backup.contains(ws),
                });
            }
            let elected = elect(&candidates);
            if waited {
                set(&mut self.designated, *ws, elected.designated == Some(self.me));
                set(&mut self.backup, *ws, elected.backup == Some(self.me));
            }
            roles.insert(*ws, elected);
        }

        let hello = Hello {
            device_id: self.me,
            priority: own.priority,
            availability: own.availability,
            hosting: own.hosting.clone(),
            designated: self.designated.iter().copied().collect(),
            backup: self.backup.iter().copied().collect(),
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
        Candidate { device_id: id, priority, availability: None, claims_designated: false, claims_backup: false }
    }

    #[test]
    fn no_candidates_elect_nobody() {
        assert_eq!(elect(&[]), Roles::default());
    }

    #[test]
    fn priority_zero_is_never_elected() {
        let id = ids(1);
        assert_eq!(elect(&[cand(id[0], 0)]), Roles::default());
    }

    #[test]
    fn rank_is_priority_then_availability_then_id() {
        let id = ids(3);
        let mut a = cand(id[0], 2);
        a.availability = Some(0);
        let b = cand(id[1], 2); // no availability: ranks below Some(0)
        let c = cand(id[2], 1); // highest id, lowest priority
        let roles = elect(&[a, b, c]);
        assert_eq!(roles.designated, Some(id[0]));
        assert_eq!(roles.backup, Some(id[1]));
    }

    #[test]
    fn an_existing_claim_survives_a_higher_ranked_newcomer() {
        let id = ids(2);
        let mut holder = cand(id[0], 1);
        holder.claims_designated = true;
        let newcomer = cand(id[1], 9);
        let roles = elect(&[holder, newcomer]);
        assert_eq!(roles.designated, Some(id[0]));
        assert_eq!(roles.backup, Some(id[1]));
    }

    #[test]
    fn the_backup_is_promoted_when_the_designated_is_gone() {
        let id = ids(3);
        let mut backup = cand(id[0], 1);
        backup.claims_backup = true;
        let roles = elect(&[backup, cand(id[1], 1), cand(id[2], 9)]);
        assert_eq!(roles.designated, Some(id[0]));
        assert_eq!(roles.backup, Some(id[2]));
    }

    #[test]
    fn two_designated_claims_resolve_to_the_higher_rank() {
        let id = ids(2);
        let mut low = cand(id[0], 1);
        low.claims_designated = true;
        let mut high = cand(id[1], 1);
        high.claims_designated = true;
        assert_eq!(elect(&[low, high]).designated, Some(id[1]));
    }

    fn hello(id: GrainId, priority: u8, ws: GrainId) -> Hello {
        Hello { device_id: id, priority, availability: None, hosting: vec![ws], designated: vec![], backup: vec![] }
    }

    #[test]
    fn a_fresh_host_waits_before_claiming() {
        let id = ids(2);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[1], Duration::from_secs(40));
        let own = Own { priority: 9, availability: None, hosting: vec![ws] };
        let mut peer = hello(id[0], 1, ws);
        peer.designated = vec![ws];

        let (out, roles) = elector.step(t0, &own, &[peer.clone()]);
        assert!(out.designated.is_empty() && out.backup.is_empty(), "no claims while waiting");
        assert_eq!(roles[&ws].designated, Some(id[0]), "the peer's claim stands");
        assert_eq!(roles[&ws].backup, None, "this host is not a candidate while waiting");

        let (out, roles) = elector.step(t0 + Duration::from_secs(41), &own, &[peer]);
        assert_eq!(roles[&ws].designated, Some(id[0]), "no preemption after the wait either");
        assert_eq!(out.backup, vec![ws], "it takes the free backup role");
    }

    #[test]
    fn a_lone_host_claims_designated_after_the_wait() {
        let id = ids(1);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::from_secs(40));
        let own = Own { priority: 1, availability: None, hosting: vec![ws] };
        elector.step(t0, &own, &[]);
        let (out, roles) = elector.step(t0 + Duration::from_secs(41), &own, &[]);
        assert_eq!(roles[&ws].designated, Some(id[0]));
        assert_eq!(out.designated, vec![ws]);
    }

    #[test]
    fn a_peer_not_hosting_the_workspace_is_not_a_candidate() {
        let id = ids(2);
        let ws = GrainId::random();
        let other = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::ZERO);
        let own = Own { priority: 0, availability: None, hosting: vec![ws] };
        let (_, roles) = elector.step(t0, &own, &[hello(id[1], 5, other)]);
        assert_eq!(roles[&ws], Roles::default());
    }

    #[test]
    fn a_workspace_no_longer_hosted_drops_its_claims() {
        let id = ids(1);
        let ws = GrainId::random();
        let t0 = Instant::now();
        let mut elector = Elector::new(id[0], Duration::ZERO);
        elector.step(t0, &Own { priority: 1, availability: None, hosting: vec![ws] }, &[]);
        let (out, roles) = elector.step(t0, &Own { priority: 1, availability: None, hosting: vec![] }, &[]);
        assert!(out.designated.is_empty());
        assert!(roles.is_empty());
    }
}
