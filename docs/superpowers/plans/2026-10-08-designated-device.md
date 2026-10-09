# Designated Devices Implementation Plan (#182)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every bridge elects, per workspace, a designated and a backup device from Hello messages, ranked by a manual priority. App servers that are neither sync only with those two (a star). With no candidates, they fall back to the existing mesh.

**Architecture:** The device ledger gains `priority`. Bridges keep one long-lived Hello stream per peer on a new ALPN (`sapphire/hello/1`) and run a pure, non-preemptive election (`election.rs`) over what they hear. The result goes out through `bridge.peers()` as `roles`. The app server's `SyncRuntime` filters dials, closes sessions it no longer needs and drops inbound streams through a pure `link()` rule. Availability (#190) is out of scope: `availability` is always `None` here.

**Tech Stack:** Rust 2024, tokio, serde/serde_json (NDJSON lines), iroh (ALPN), egui (GUI), the existing loopback transport for tests.

**Spec:** `docs/superpowers/specs/2026-10-08-primary-device-design.md`

## Global Constraints

- `priority` is a `u8`, `0..=255`. `0` = never designated or backup. Absent in a record = `1` (`DEFAULT_PRIORITY`).
- Hello defaults: `HELLO_INTERVAL` = 10 s, `DEAD_INTERVAL` = 40 s. The wait timer equals `DEAD_INTERVAL`.
- Rank key: `(priority, availability, device_id)`, highest first. `None` availability ranks below every `Some`.
- Non-preemptive: an existing designated claim survives a higher-ranked newcomer.
- `sapphire-framework-bridge-api` goes from `2.0.0` to `2.1.0`. `API_VERSION` stays `2`. Every new field is `#[serde(default)]`, so old peers deserialize new messages and vice versa.
- Availability is never written to the ledger.
- Display words: "designated" / "backup" (in Japanese docs: 代表デバイス / 予備デバイス). The words "master" and "central" do not appear in code.
- On the loopback transport, Hello streams are **not** counted by `frames_sent`. The quiet tests in `crates/sapphire-framework-server/tests/live.rs` depend on that.
- Every file the framework creates stays `0600` / `0700` (no new files are created by this plan beyond source files).
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **A host that just woke up** must not seize a role that is already held. Pinned by `a_fresh_host_waits_before_claiming` in Task 3 and by integration test 3 in Task 8.
2. **Two hosts with different views** (one has heard a peer the other has not) must not leave a non-hub pair with no path: the inbound guard drops only streams `link()` says `Skip`, and the hubs accept everyone. Pinned by `a_hub_accepts_everyone` in Task 6.
3. **An older app server** (no `roles` awareness) dialling a new non-hub during a star is dropped there, but still reaches the hubs. Pinned by `an_old_server_that_dials_a_hub_is_kept` in Task 6.
4. **A peer that sends garbage on the Hello stream** must not take down its workspace streams or the Hello loop of others. Pinned by `a_garbage_hello_drops_only_that_link` in Task 4.
5. **Setting priority while the bridge is stopped** must write the ledger directly, like `device retire`. Pinned by `priority_set_without_a_bridge_edits_the_ledger` in Task 5.

---

## File Structure

| File | Change | Responsibility |
|---|---|---|
| `crates/sapphire-framework-registry/src/devices.rs` | modify | `Device.priority`, `DEFAULT_PRIORITY`, `Devices::set_priority` |
| `crates/sapphire-framework-registry/src/lib.rs` | modify | re-export `DEFAULT_PRIORITY` |
| `crates/sapphire-framework-bridge-api/Cargo.toml` | modify | version `2.1.0` |
| `crates/sapphire-framework-bridge-api/src/lib.rs` | modify | `DEVICE_PRIORITY_SET`, params/result, `PeerInfo.priority/availability`, `WorkspaceRoles`, `PeersResult.roles`, `PeersResult::roles_for` |
| `crates/sapphire-framework-bridge-api/src/client.rs` | modify | `BridgeClient::device_priority_set` |
| `crates/sapphire-framework-bridge/src/election.rs` | **create** | `Candidate`, `Roles`, `elect()`, `Elector` (claims + wait timer) |
| `crates/sapphire-framework-bridge/src/hello.rs` | **create** | `Hello`, `HelloTiming`, `Neighbours`, line I/O, `exchange()`, `run()` (dial + elect loop) |
| `crates/sapphire-framework-bridge/src/peer.rs` | modify | `Inbound::Hello`, `PeerTransport::open_hello`, loopback hello inbox |
| `crates/sapphire-framework-bridge/src/iroh.rs` | modify | `HELLO_ALPN` on the endpoint, `open_hello`, accept branch |
| `crates/sapphire-framework-bridge/src/data.rs` | modify | route `Inbound::Hello` to `hello::serve_inbound` |
| `crates/sapphire-framework-bridge/src/lib.rs` | modify | `Bridge` fields + `hello_timing()` builder, run `hello::run`, `roles()` accessor |
| `crates/sapphire-framework-bridge/src/control.rs` | modify | `peers()` fills roles/priority, `device_priority_set` method, `Owners::is_online` un-gated |
| `crates/sapphire-framework-bridge/src/workgroup.rs` | modify | `Workgroup::set_priority` |
| `crates/sapphire-framework-bridge/src/command.rs` | modify | `device priority` verb, `device list` priority column |
| `crates/sapphire-framework-server/src/command.rs` | modify | the app CLI's `device priority` verb, `(star)` marker in `workspace list` |
| `crates/sapphire-framework-backend/src/protocol.rs` | modify | `Topology`, `SyncStatusResult.topology` |
| `crates/sapphire-framework-server/src/sync/topology.rs` | **create** | `Link`, `link()`, `topology()` |
| `crates/sapphire-framework-server/src/sync/live.rs` | modify | `LivePeers::retain` |
| `crates/sapphire-framework-server/src/sync/mod.rs` | modify | cached peers, dial filtering, inbound guard, `is_designated`, status topology |
| `crates/sapphire-framework-server/src/sync/methods.rs` | modify | carry `topology` into `SyncStatusResult` |
| `crates/sapphire-framework-server/tests/common/mod.rs` | modify | short Hello timing, priority 0 by default, `NODE_C`, `star_hosts`, `Host::bridge()` |
| `crates/sapphire-framework-server/tests/star.rs` | **create** | the integration scenarios |
| `crates/sapphire-framework-gui/src/client/types.rs`, `client/exec.rs` | modify | `Command::DevicePrioritySet` |
| `crates/sapphire-framework-gui/src/views/devices.rs` | modify | priority editor and role badges |
| `crates/sapphire-framework-gui/src/views/model.rs` | modify | `role_badges()`, topology in `Badge::Syncing` |
| `docs/ARCHITECTURE.md`, `CHANGELOG.md` | modify | document the feature |

---

### Task 1: `priority` on the device record

**Files:**
- Modify: `crates/sapphire-framework-registry/src/devices.rs`
- Modify: `crates/sapphire-framework-registry/src/lib.rs`

**Interfaces:**
- Produces: `pub const DEFAULT_PRIORITY: u8 = 1;`, `Device.priority: u8`, `Devices::set_priority(&mut self, selector: &str, priority: u8) -> Result<Device>`.

- [ ] **Step 1: Write the failing tests** (append inside `mod tests` in `devices.rs`)

```rust
    #[test]
    fn a_record_without_priority_reads_as_the_default() {
        let (_d, path) = hand_written("abcdefg", "name = \"pendant\"\n");

        let devices = Devices::open(&path).unwrap();

        assert_eq!(devices.entries()[0].priority, DEFAULT_PRIORITY);
    }

    #[test]
    fn set_priority_round_trips_and_leaves_the_default_unwritten() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        let added = devices.add("desk", None, None).unwrap();
        let text = std::fs::read_to_string(path.join(added.file_name())).unwrap();
        assert!(!text.contains("priority"), "the default is not written: {text}");

        let updated = devices.set_priority("desk", 0).unwrap();

        assert_eq!(updated.priority, 0);
        let reloaded = Devices::open(&path).unwrap();
        assert_eq!(reloaded.entries()[0].priority, 0);
    }

    #[test]
    fn set_priority_refuses_a_retired_device() {
        let (_d, path) = tmp();
        let mut devices = Devices::open(&path).unwrap();
        devices.add("gone", None, None).unwrap();
        devices.retire("gone").unwrap();

        let err = devices.set_priority("gone", 5).unwrap_err();

        assert!(err.to_string().contains("retired"), "{err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-registry priority`
Expected: compile error, `no field priority` / `no method set_priority`.

- [ ] **Step 3: Implement**

In `devices.rs`:

1. Add this line to `HEADER`, after the `description` line:
```
# priority    optional, 0-255, default 1. Higher is preferred as the designated
#             device. 0: never designated or backup.
```
2. Add the constant and the field:
```rust
/// The priority a record has when it names none: every device takes part in the election.
pub const DEFAULT_PRIORITY: u8 = 1;
```
On `Device`, after `description`:
```rust
    /// How strongly this device is preferred as a workspace's designated device. `0` opts
    /// it out of the election entirely.
    pub priority: u8,
```
3. On `RawDevice`, after `description`:
```rust
    #[serde(default, skip_serializing_if = "Option::is_none")]
    priority: Option<u8>,
```
4. In `Devices::open`, build the `Device` with `priority: raw.priority.unwrap_or(DEFAULT_PRIORITY),`. In `Devices::add`, use `priority: DEFAULT_PRIORITY,`.
5. Wherever a `RawDevice` is built from a `Device` (in `write_record` / `save_one`), write `priority: (device.priority != DEFAULT_PRIORITY).then_some(device.priority),`.
6. Add the method next to `retire`:
```rust
    /// Set a device's priority. A retired device has no say in anything, so it is refused.
    pub fn set_priority(&mut self, selector: &str, priority: u8) -> Result<Device> {
        let i = self.index_of(selector)?;
        if self.entries[i].is_retired() {
            return Err(Error::File(format!(
                "the device {:?} is retired",
                self.entries[i].name
            )));
        }
        if self.entries[i].priority == priority {
            return Ok(self.entries[i].clone());
        }
        let mut updated = self.entries[i].clone();
        updated.priority = priority;
        self.save_one(&updated)?;
        self.entries[i] = updated.clone();
        Ok(updated)
    }
```
7. In `crates/sapphire-framework-registry/src/migrate.rs:71`, the `crate::Device { .. }` literal gets `priority: crate::DEFAULT_PRIORITY,`.
8. In `lib.rs`, extend the `devices` re-export with `DEFAULT_PRIORITY`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p sapphire-framework-registry`
Expected: all pass. Then run `cargo check --workspace --all-targets` and fix any other `Device { .. }` literal it reports (add `priority: sapphire_registry::DEFAULT_PRIORITY`).

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-framework-registry
git commit -m "feat(registry): a priority on the device record, default 1" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 2: bridge-api 2.1.0 — priority, roles, `device_priority_set`

**Files:**
- Modify: `crates/sapphire-framework-bridge-api/Cargo.toml` (`version = "2.1.0"`)
- Modify: `crates/sapphire-framework-bridge-api/src/lib.rs`
- Modify: `crates/sapphire-framework-bridge-api/src/client.rs`

**Interfaces:**
- Produces:
  - `pub const DEVICE_PRIORITY_SET: &str = "bridge.device_priority_set";`
  - `pub const DEFAULT_PRIORITY: u8 = 1;`
  - `pub struct DevicePrioritySetParams { pub selector: String, pub priority: u8 }`
  - `pub struct DevicePrioritySetResult { pub device_id: GrainId, pub name: String, pub priority: u8 }`
  - `PeerInfo { .., pub priority: u8, pub availability: Option<u8> }`
  - `pub struct WorkspaceRoles { pub workspace_id: GrainId, pub designated: Option<GrainId>, pub backup: Option<GrainId> }`
  - `PeersResult { pub peers: Vec<PeerInfo>, pub roles: Vec<WorkspaceRoles> }`
  - `impl PeersResult { pub fn roles_for(&self, workspace_id: GrainId) -> Option<&WorkspaceRoles> }`
  - `BridgeClient::device_priority_set(&self, params: DevicePrioritySetParams) -> sapphire_ipc::Result<DevicePrioritySetResult>`

- [ ] **Step 1: Write the failing tests** (in the existing `#[cfg(test)] mod tests` of `lib.rs`)

```rust
    #[test]
    fn a_2_0_peers_answer_reads_with_no_roles_and_default_priority() {
        let id = GrainId::random();
        let old = serde_json::json!({
            "peers": [{ "device_id": id, "name": "desk", "node_id": "", "connected": true }]
        });

        let read: PeersResult = serde_json::from_value(old).unwrap();

        assert!(read.roles.is_empty());
        assert_eq!(read.peers[0].priority, DEFAULT_PRIORITY);
        assert_eq!(read.peers[0].availability, None);
        assert!(read.roles_for(id).is_none());
    }

    #[test]
    fn roles_for_finds_the_workspace() {
        let ws = GrainId::random();
        let d = GrainId::random();
        let result = PeersResult {
            peers: Vec::new(),
            roles: vec![WorkspaceRoles { workspace_id: ws, designated: Some(d), backup: None }],
        };

        assert_eq!(result.roles_for(ws).unwrap().designated, Some(d));
    }

    #[test]
    fn device_priority_set_params_are_selector_and_priority() {
        let p = DevicePrioritySetParams { selector: "desk".into(), priority: 0 };
        assert_eq!(
            serde_json::to_value(&p).unwrap(),
            serde_json::json!({ "selector": "desk", "priority": 0 })
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-bridge-api`
Expected: compile errors for the missing items.

- [ ] **Step 3: Implement**

In `lib.rs`, after `DEVICE_RETIRE`:
```rust
/// Set a device's election priority.
pub const DEVICE_PRIORITY_SET: &str = "bridge.device_priority_set";

/// A device's priority when its record names none. Mirrors the registry's own constant;
/// this crate does not depend on the registry.
pub const DEFAULT_PRIORITY: u8 = 1;

fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

/// Parameters of [`DEVICE_PRIORITY_SET`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicePrioritySetParams {
    /// The device's name or id.
    pub selector: String,
    /// `0..=255`. `0` takes the device out of the election.
    pub priority: u8,
}

/// Result of [`DEVICE_PRIORITY_SET`].
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DevicePrioritySetResult {
    /// The device's id.
    pub device_id: GrainId,
    /// Its name.
    pub name: String,
    /// Its priority now.
    pub priority: u8,
}
```
On `PeerInfo`, after `connected`:
```rust
    /// Its election priority, as its ledger record says.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// Its availability tier, as its own Hello reports it (#190). `None` until measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<u8>,
```
New type and field:
```rust
/// Who the bridge elected for one workspace this host's app servers own.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct WorkspaceRoles {
    /// The workspace.
    pub workspace_id: GrainId,
    /// The designated device, if any candidate exists.
    #[serde(default)]
    pub designated: Option<GrainId>,
    /// The backup device, if a second candidate exists.
    #[serde(default)]
    pub backup: Option<GrainId>,
}
```
On `PeersResult`, after `peers`:
```rust
    /// The elected roles of every workspace this host's app servers own. Empty from a
    /// bridge older than 2.1, which means a full mesh.
    #[serde(default)]
    pub roles: Vec<WorkspaceRoles>,
```
and
```rust
impl PeersResult {
    /// The roles for `workspace_id`, if the bridge elected any.
    pub fn roles_for(&self, workspace_id: GrainId) -> Option<&WorkspaceRoles> {
        self.roles.iter().find(|r| r.workspace_id == workspace_id)
    }
}
```
In `client.rs`, next to `device_retire`, following its exact shape:
```rust
    /// Set a device's election priority.
    pub async fn device_priority_set(
        &self,
        params: DevicePrioritySetParams,
    ) -> sapphire_ipc::Result<DevicePrioritySetResult> {
        self.client.call(DEVICE_PRIORITY_SET, params).await
    }
```
(Match whatever field name `device_retire` uses for the inner client; copy its body and change only the constant and types.)

Fix the struct literals the new fields break, so the workspace compiles:
- `crates/sapphire-framework-server/src/sync/testing.rs:96`: `PeersResult { peers: vec![], roles: vec![] }`.
- `crates/sapphire-framework-bridge/src/control.rs` `peers()` and `peer_infos()`: add `roles: Vec::new()` and `priority: d.priority, availability: None` for now. Task 5 fills them properly.
- `crates/sapphire-framework-bridge/src/command.rs:357`: add `priority: sapphire_bridge_api::DEFAULT_PRIORITY, availability: None`.
- `crates/sapphire-framework-gui/src/panel.rs:141,147` and `views/model.rs:276,285`: add `priority: 1, availability: None`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sapphire-framework-bridge-api && cargo check --workspace --all-targets`
Expected: tests pass, the workspace compiles.

- [ ] **Step 5: Commit**

```bash
git add -A crates
git commit -m "feat(bridge-api): 2.1.0 — device priority, elected roles in peers" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 3: the election (`election.rs`)

**Files:**
- Create: `crates/sapphire-framework-bridge/src/election.rs`
- Modify: `crates/sapphire-framework-bridge/src/lib.rs` (add `mod election;`)

**Interfaces:**
- Consumes: `hello::Hello` from Task 4 — **define `Hello` in this task** in `election.rs`'s sibling `hello.rs` as just the struct below, so Task 4 only adds behaviour around it:
```rust
// crates/sapphire-framework-bridge/src/hello.rs (this task creates the file with only this)
use grain_id::GrainId;
use serde::{Deserialize, Serialize};

/// What a bridge tells each peer about itself, every interval and on change.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct Hello {
    pub device_id: GrainId,
    pub priority: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<u8>,
    #[serde(default)]
    pub hosting: Vec<GrainId>,
    #[serde(default)]
    pub designated: Vec<GrainId>,
    #[serde(default)]
    pub backup: Vec<GrainId>,
}
```
(`GrainId` has no `Default`, so `Hello` has none either: build it field by field.)
- Produces:
```rust
pub(crate) struct Candidate { pub device_id: GrainId, pub priority: u8, pub availability: Option<u8>, pub claims_designated: bool, pub claims_backup: bool }
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Roles { pub designated: Option<GrainId>, pub backup: Option<GrainId> }
pub(crate) fn elect(candidates: &[Candidate]) -> Roles
pub(crate) struct Own { pub priority: u8, pub availability: Option<u8>, pub hosting: Vec<GrainId> }
pub(crate) struct Elector { .. }
impl Elector {
    pub(crate) fn new(me: GrainId, wait: Duration) -> Elector;
    pub(crate) fn step(&mut self, now: Instant, own: &Own, peers: &[Hello]) -> (Hello, BTreeMap<GrainId, Roles>);
}
```

- [ ] **Step 1: Write the failing tests** (bottom of `election.rs`)

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-bridge election`
Expected: compile errors (nothing defined yet).

- [ ] **Step 3: Implement** (top of `election.rs`)

```rust
//! Who is a workspace's designated and backup device, as this host sees it.
//!
//! OSPF's DR/BDR election, minus the consensus: every bridge computes the roles from the
//! Hellos it hears, and two bridges with different views may disagree for a while. That is
//! accepted by design (spec decision 2): the work the designated device does is idempotent.
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
```

Add `mod election;` and `mod hello;` to `lib.rs` (after `mod error;`, alphabetical).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sapphire-framework-bridge election`
Expected: all 10 pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-framework-bridge/src/election.rs crates/sapphire-framework-bridge/src/hello.rs crates/sapphire-framework-bridge/src/lib.rs
git commit -m "feat(bridge): a non-preemptive DR/BDR election over Hellos" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 4: the Hello transport and exchange

**Files:**
- Modify: `crates/sapphire-framework-bridge/src/hello.rs`
- Modify: `crates/sapphire-framework-bridge/src/peer.rs`
- Modify: `crates/sapphire-framework-bridge/src/iroh.rs`
- Modify: `crates/sapphire-framework-bridge/src/lib.rs` (exports)

**Interfaces:**
- Consumes: `Hello` (Task 3).
- Produces:
```rust
pub const HELLO_ALPN: &[u8] = b"sapphire/hello/1";              // hello.rs, re-exported from lib.rs
#[derive(Clone, Copy, Debug)] pub struct HelloTiming { pub interval: Duration, pub dead: Duration } // Default: 10 s / 40 s
pub(crate) struct Neighbours;                                       // Default
impl Neighbours {
    pub(crate) fn heard(&self, hello: Hello, at: Instant);
    pub(crate) fn forget(&self, device: GrainId);
    pub(crate) fn reachable(&self, now: Instant, dead: Duration) -> Vec<Hello>;
    pub(crate) fn get(&self, device: GrainId) -> Option<Hello>;
    pub(crate) fn begin_link(&self, device: GrainId) -> bool;   // false if already linked
    pub(crate) fn end_link(&self, device: GrainId);
}
pub(crate) async fn exchange(stream: BoxedStream, peer: GrainId, local: watch::Receiver<Option<Hello>>, neighbours: Arc<Neighbours>, timing: HelloTiming);
// peer.rs
pub enum Inbound { Workspace(..), Pairing(..), Hello(String, BoxedStream) }
trait PeerTransport { async fn open_hello(&self, node_id: &str) -> Result<BoxedStream>; }  // default: Err(Error::Peer("this transport has no hello protocol"))
```

- [ ] **Step 1: Write the failing tests** (in `hello.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::peer::{LoopbackNetwork, PeerTransport, Inbound};

    fn timing() -> HelloTiming {
        HelloTiming { interval: Duration::from_millis(50), dead: Duration::from_millis(300) }
    }

    fn hello(id: GrainId) -> Hello {
        Hello { device_id: id, priority: 1, availability: None, hosting: vec![], designated: vec![], backup: vec![] }
    }

    #[test]
    fn a_neighbour_is_reachable_until_dead() {
        let n = Neighbours::default();
        let id = GrainId::random();
        let t0 = Instant::now();
        n.heard(hello(id), t0);
        assert_eq!(n.reachable(t0 + Duration::from_millis(100), Duration::from_millis(300)).len(), 1);
        assert!(n.reachable(t0 + Duration::from_millis(400), Duration::from_millis(300)).is_empty());
    }

    #[test]
    fn a_link_is_claimed_once() {
        let n = Neighbours::default();
        let id = GrainId::random();
        assert!(n.begin_link(id));
        assert!(!n.begin_link(id));
        n.end_link(id);
        assert!(n.begin_link(id));
    }

    #[tokio::test]
    async fn two_ends_hear_each_other_and_forget_on_close() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let (ida, idb) = (GrainId::random(), GrainId::random());
        let (na, nb) = (Arc::new(Neighbours::default()), Arc::new(Neighbours::default()));
        let (_ta, ra) = watch::channel(Some(hello(ida)));
        let (_tb, rb) = watch::channel(Some(hello(idb)));

        let out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(from, inb) = b.accept().await.unwrap() else { panic!("not a hello") };
        assert_eq!(from, "node-a");
        let ja = tokio::spawn(exchange(out, idb, ra, Arc::clone(&na), timing()));
        let jb = tokio::spawn(exchange(inb, ida, rb, Arc::clone(&nb), timing()));

        let deadline = Instant::now() + Duration::from_secs(5);
        while na.get(idb).is_none() || nb.get(ida).is_none() {
            assert!(Instant::now() < deadline, "the Hellos never arrived");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        ja.abort();
        let _ = ja.await;
        tokio::time::timeout(Duration::from_secs(5), jb).await.unwrap().unwrap();
        assert!(nb.get(ida).is_none(), "the closed link is forgotten");
    }

    #[tokio::test]
    async fn a_garbage_hello_drops_only_that_link() {
        use tokio::io::AsyncWriteExt;
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let nb = Arc::new(Neighbours::default());
        let (_tb, rb) = watch::channel(Some(hello(GrainId::random())));
        let mut out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else { panic!() };
        let job = tokio::spawn(exchange(inb, GrainId::random(), rb, Arc::clone(&nb), timing()));

        out.write_all(b"not json\n").await.unwrap();

        tokio::time::timeout(Duration::from_secs(5), job).await.unwrap().unwrap();
        // The transport still serves workspace streams.
        let _ws = a.open("node-b", GrainId::random()).await.unwrap();
        assert!(matches!(b.accept().await.unwrap(), Inbound::Workspace(..)));
    }

    #[tokio::test]
    async fn a_hello_naming_another_device_is_refused() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let b = net.transport("node-b");
        let nb = Arc::new(Neighbours::default());
        let (_ta, ra) = watch::channel(Some(hello(GrainId::random()))); // not the id b expects
        let (_tb, rb) = watch::channel(Some(hello(GrainId::random())));
        let out = a.open_hello("node-b").await.unwrap();
        let Inbound::Hello(_, inb) = b.accept().await.unwrap() else { panic!() };
        let expected = GrainId::random();
        let _ja = tokio::spawn(exchange(out, GrainId::random(), ra, Arc::new(Neighbours::default()), timing()));
        tokio::time::timeout(Duration::from_secs(5), exchange(inb, expected, rb, Arc::clone(&nb), timing()))
            .await
            .unwrap();
        assert!(nb.get(expected).is_none());
    }

    #[tokio::test]
    async fn hello_streams_are_not_counted_as_frames() {
        let net = LoopbackNetwork::new();
        let a = net.transport("node-a");
        let _b = net.transport("node-b");
        let mut out = a.open_hello("node-b").await.unwrap();
        tokio::io::AsyncWriteExt::write_all(&mut out, b"x\n").await.unwrap();
        assert_eq!(net.frames_sent("node-a"), 0);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-bridge hello`
Expected: compile errors (`open_hello`, `Inbound::Hello`, `Neighbours`, `exchange` are missing).

- [ ] **Step 3: Implement `peer.rs`**

1. Add the variant to `Inbound`:
```rust
    /// A Hello stream, on its own ALPN: the caller's node id and the stream. Authorized with
    /// the ledger like a workspace stream; see [`crate::hello`].
    Hello(String, BoxedStream),
```
2. In `accept_pairing`, change `Inbound::Workspace(..) => continue,` to `Inbound::Workspace(..) | Inbound::Hello(..) => continue,`. In `accept_workspace`, add the arm `Inbound::Hello(_, stream) => drop(stream),`.
3. Add to the trait, after `open_pairing`:
```rust
    /// Open a Hello stream to `node_id`.
    ///
    /// Defaults to an error, so a transport written before the Hello protocol still
    /// compiles. Its bridge then simply hears nobody, and nobody's election counts it.
    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        Err(crate::error::Error::Peer(format!(
            "this transport cannot open a hello stream to {node_id}"
        )))
    }
```
4. Loopback: add `hello: Arc<Mutex<HashMap<String, PairingInbox>>>` to `LoopbackNetwork` (initialize it in `new` via `Default`, and in `partitioned` as `hello: Arc::default(),`). Add `hello_nodes` and `hello_inbox` fields to `LoopbackTransport`, set up in `transport()` exactly as the pairing pair is. Implement:
```rust
    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        if !self.reachable(node_id) {
            return Err(Error::Peer(format!("no such node on the loopback network: {node_id}")));
        }
        let inbox = { self.hello_nodes.lock().expect("loopback network").get(node_id).cloned() };
        let Some(inbox) = inbox else {
            return Err(Error::Peer(format!("no such node on the loopback network: {node_id}")));
        };
        let (mine, theirs) = tokio::io::duplex(LOOPBACK_BUFFER);
        inbox
            .send((self.node_id.clone(), theirs))
            .map_err(|_| Error::Peer(format!("{node_id} is no longer listening")))?;
        // Not a `CountingStream`: Hellos are a steady background, and the frame counters
        // exist for tests asking whether *sync* has gone quiet.
        Ok(Box::new(mine))
    }
```
In `accept`, add a third `select!` branch:
```rust
            item = next_hello(&self.hello_inbox) => item,
```
with
```rust
#[cfg(any(test, feature = "test-util"))]
/// The next Hello stream, from the Hello inbox. Uncounted, like `open_hello`'s end.
async fn next_hello(
    inbox: &tokio::sync::Mutex<mpsc::UnboundedReceiver<(String, tokio::io::DuplexStream)>>,
) -> Result<Inbound> {
    let mut inbox = inbox.lock().await;
    match inbox.recv().await {
        Some((from, stream)) => Ok(Inbound::Hello(from, Box::new(stream))),
        None => Err(Error::Peer("the loopback network is gone".to_owned())),
    }
}
```

- [ ] **Step 4: Implement `iroh.rs`**

1. `use crate::hello::HELLO_ALPN;`
2. `let alpns = vec![ALPN.to_vec(), PAIR_ALPN.to_vec(), HELLO_ALPN.to_vec()];` and update the comment above it to say "three protocols".
3. Implement `open_hello` like `open`, but with no request line:
```rust
    async fn open_hello(&self, node_id: &str) -> Result<BoxedStream> {
        let id: EndpointId = node_id
            .parse()
            .map_err(|e| Error::Peer(format!("{node_id}: not a node id: {e}")))?;
        let conn = self
            .endpoint
            .connect(id, HELLO_ALPN)
            .await
            .map_err(|e| Error::Peer(format!("could not reach {node_id}: {e}")))?;
        let (send, recv) = conn
            .open_bi()
            .await
            .map_err(|e| Error::Peer(format!("could not open a hello stream to {node_id}: {e}")))?;
        Ok(Box::new(tokio::io::join(recv, send)))
    }
```
4. In `accept`, after the `PAIR_ALPN` branch:
```rust
            if alpn == HELLO_ALPN {
                return Ok(Inbound::Hello(
                    from.to_string(),
                    Box::new(AcceptedStream { io: tokio::io::join(recv, send), conn }),
                ));
            }
```
A QUIC stream is only seen by `accept_bi` once the opener writes to it. `exchange` writes its first Hello immediately, so this holds.

- [ ] **Step 5: Implement `hello.rs`** (above the `Hello` struct from Task 3)

```rust
//! Hello: what each bridge tells its peers about itself, and the loop that keeps it said.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::watch;

use crate::peer::BoxedStream;

/// The ALPN Hello streams speak.
pub const HELLO_ALPN: &[u8] = b"sapphire/hello/1";

/// How often a Hello is sent, and how long silence takes to mean "gone".
#[derive(Clone, Copy, Debug)]
pub struct HelloTiming {
    /// Between two Hellos on one stream.
    pub interval: Duration,
    /// Silence after which a peer is unreachable. Also the wait before claiming.
    pub dead: Duration,
}

impl Default for HelloTiming {
    fn default() -> HelloTiming {
        HelloTiming { interval: Duration::from_secs(10), dead: Duration::from_secs(40) }
    }
}

/// The peers this bridge has heard, and which ones it holds a Hello link to.
#[derive(Debug, Default)]
pub(crate) struct Neighbours {
    heard: Mutex<HashMap<GrainId, (Hello, Instant)>>,
    links: Mutex<HashSet<GrainId>>,
}

impl Neighbours {
    pub(crate) fn heard(&self, hello: Hello, at: Instant) {
        self.heard.lock().expect("neighbours").insert(hello.device_id, (hello, at));
    }

    pub(crate) fn forget(&self, device: GrainId) {
        self.heard.lock().expect("neighbours").remove(&device);
    }

    pub(crate) fn reachable(&self, now: Instant, dead: Duration) -> Vec<Hello> {
        self.heard
            .lock()
            .expect("neighbours")
            .values()
            .filter(|(_, at)| now.duration_since(*at) < dead)
            .map(|(h, _)| h.clone())
            .collect()
    }

    pub(crate) fn get(&self, device: GrainId) -> Option<Hello> {
        self.heard.lock().expect("neighbours").get(&device).map(|(h, _)| h.clone())
    }

    pub(crate) fn begin_link(&self, device: GrainId) -> bool {
        self.links.lock().expect("neighbours").insert(device)
    }

    pub(crate) fn end_link(&self, device: GrainId) {
        self.links.lock().expect("neighbours").remove(&device);
    }
}

/// Speak Hello with `peer` over `stream` until either side goes away.
///
/// Sends this host's current Hello every `interval` and whenever it changes. Records each
/// Hello read, provided it names `peer`. Anything unreadable, or a Hello that names a
/// different device, ends this link and only this link. The peer is forgotten when the
/// link ends, so it stops counting at once instead of after `dead`.
pub(crate) async fn exchange(
    stream: BoxedStream,
    peer: GrainId,
    mut local: watch::Receiver<Option<Hello>>,
    neighbours: Arc<Neighbours>,
    timing: HelloTiming,
) {
    let (read, mut write) = tokio::io::split(stream);
    let reader = async {
        let mut lines = BufReader::new(read).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => match serde_json::from_str::<Hello>(&line) {
                    Ok(hello) if hello.device_id == peer => neighbours.heard(hello, Instant::now()),
                    Ok(hello) => {
                        tracing::debug!(%peer, claimed = %hello.device_id, "a Hello named another device");
                        return;
                    }
                    Err(err) => {
                        tracing::debug!(%peer, "an unreadable Hello: {err}");
                        return;
                    }
                },
                Ok(None) | Err(_) => return,
            }
        }
    };
    let writer = async {
        let mut tick = tokio::time::interval(timing.interval);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let current = local.borrow_and_update().clone();
            if let Some(hello) = current {
                let mut line = match serde_json::to_vec(&hello) {
                    Ok(line) => line,
                    Err(_) => return,
                };
                line.push(b'\n');
                if write.write_all(&line).await.is_err() || write.flush().await.is_err() {
                    return;
                }
            }
            tokio::select! {
                _ = tick.tick() => {}
                changed = local.changed() => if changed.is_err() { return },
            }
        }
    };
    tokio::select! {
        _ = reader => {}
        _ = writer => {}
    }
    neighbours.forget(peer);
}
```
Add `use grain_id::GrainId;` once at the top (Task 3 already has it). In `lib.rs`, add `pub use hello::{HELLO_ALPN, HelloTiming};`.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p sapphire-framework-bridge hello && cargo test -p sapphire-framework-bridge peer && cargo check -p sapphire-framework-bridge --features node`
Expected: pass. The node build compiles with the third ALPN.

- [ ] **Step 7: Commit**

```bash
git add crates/sapphire-framework-bridge
git commit -m "feat(bridge): a Hello protocol on its own ALPN, uncounted on the loopback" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 5: wire the election into the bridge (`peers` roles, priority method)

**Files:**
- Modify: `crates/sapphire-framework-bridge/src/hello.rs` (add `run`, `serve_inbound`)
- Modify: `crates/sapphire-framework-bridge/src/lib.rs`
- Modify: `crates/sapphire-framework-bridge/src/data.rs`
- Modify: `crates/sapphire-framework-bridge/src/control.rs`
- Modify: `crates/sapphire-framework-bridge/src/workgroup.rs`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:
  - `Bridge::hello_timing(self, timing: HelloTiming) -> Bridge` (builder)
  - `pub(crate) fn Bridge::roles(&self) -> BTreeMap<GrainId, Roles>`
  - `pub(crate) fn Bridge::neighbours(&self) -> &Arc<Neighbours>`
  - `Workgroup::set_priority(&self, selector: &str, priority: u8) -> Result<Device>`
  - `bridge.device_priority_set` on the control plane
  - `PeersResult.roles`, `PeerInfo.priority` filled in

- [ ] **Step 1: Write the failing tests** (in `control.rs`'s `mod tests`, reusing its `bridge(&tmp)`, `connect` and `call` helpers)

```rust
    #[tokio::test]
    async fn device_priority_set_changes_the_ledger_and_peers_reports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let (client, _serving) = connect(&bridge).await;

        let set: sapphire_bridge_api::DevicePrioritySetResult = serde_json::from_value(
            call(&client, sapphire_bridge_api::DEVICE_PRIORITY_SET,
                 serde_json::json!({ "selector": "host-a", "priority": 7 })).await,
        )
        .unwrap();
        assert_eq!(set.priority, 7);

        let peers: PeersResult =
            serde_json::from_value(call(&client, PEERS, serde_json::json!({})).await).unwrap();
        assert_eq!(peers.peers.iter().find(|p| p.name == "host-a").unwrap().priority, 7);
    }

    #[tokio::test]
    async fn device_priority_set_refuses_a_retired_device() {
        let tmp = tempfile::tempdir().unwrap();
        let bridge = bridge(&tmp);
        let wg = bridge.workgroup().unwrap().unwrap();
        wg.devices().unwrap().add("laptop", Some("bbbb".into()), None).unwrap();
        wg.retire_device("laptop", "aaaa").unwrap();
        let (client, _serving) = connect(&bridge).await;

        let err = client
            .call::<_, Value>(sapphire_bridge_api::DEVICE_PRIORITY_SET,
                              serde_json::json!({ "selector": "laptop", "priority": 3 }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("retired"), "{err}");
    }
```
Also in `control.rs`'s tests, a two-bridge election over one loopback network. It runs only the Hello loop and the inbound loop, not the IPC listeners, which `connect` replaces:

```rust
    #[tokio::test(flavor = "multi_thread")]
    async fn two_bridges_elect_the_same_designated_device() {
        let net = LoopbackNetwork::new();
        let (ta, tb) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let dir_a = BridgeDir::at(ta.path().join("bridge")).unwrap();
        let dir_b = BridgeDir::at(tb.path().join("bridge")).unwrap();
        let wa = crate::workgroup::Workgroup::create(&dir_a, "test", "host-a", "aaaa").unwrap();
        wa.devices().unwrap().add("host-b", Some("bbbb".into()), None).unwrap();
        wa.set_priority("host-a", 5).unwrap();
        crate::testing::adopt_workgroup(&dir_b, &wa).unwrap();
        let timing = crate::hello::HelloTiming {
            interval: std::time::Duration::from_millis(50),
            dead: std::time::Duration::from_millis(300),
        };
        let make = |dir: BridgeDir, node: &str| {
            Arc::new(
                Bridge::new(dir, Arc::new(net.transport(node)), "0.0.0")
                    .unwrap()
                    .net(NetConfig::default())
                    .hello_timing(timing),
            )
        };
        let (a, b) = (make(dir_a, "aaaa"), make(dir_b, "bbbb"));
        let ws = grain_id::GrainId::random();
        let mut keep = Vec::new();
        for bridge in [&a, &b] {
            tokio::spawn(crate::hello::run(Arc::clone(bridge)));
            tokio::spawn(crate::data::inbound(Arc::clone(bridge), NetConfig::default()));
            let (client, serving) = connect(bridge).await;
            call(&client, REGISTER, serde_json::to_value(registration(ws)).unwrap()).await;
            keep.push((client, serving)); // the registration lasts as long as the connection
        }
        let a_id = wa.this_device("aaaa").unwrap().id;

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (ra, rb) = (a.roles(), b.roles());
            if ra.get(&ws).and_then(|r| r.designated) == Some(a_id)
                && rb.get(&ws).and_then(|r| r.designated) == Some(a_id)
            {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "no agreement: {ra:?} / {rb:?}");
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }
```
Add `use crate::peer::LoopbackNetwork;` and `use crate::dir::BridgeDir;` to the test module if they are not imported already. `bridge()` in the same module shows the existing imports. `crate::testing` exists under `#[cfg(test)]` (see `lib.rs`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-bridge priority`
Expected: FAIL. The method is unknown, and `priority` is still the Task 2 placeholder.

- [ ] **Step 3: Implement `Workgroup::set_priority`** (in `workgroup.rs`, next to `retire_device`)

```rust
    /// Set a device's election priority in the ledger.
    pub fn set_priority(&self, selector: &str, priority: u8) -> Result<Device> {
        Ok(self.devices()?.set_priority(selector, priority)?)
    }
```

- [ ] **Step 4: Implement the bridge state** (in `lib.rs`)

Add fields to `Bridge`:
```rust
    hello_timing: hello::HelloTiming,
    neighbours: Arc<hello::Neighbours>,
    hello_tx: tokio::sync::watch::Sender<Option<hello::Hello>>,
    roles: Mutex<std::collections::BTreeMap<GrainId, election::Roles>>,
```
Initialize them in `new`: `HelloTiming::default()`, `Arc::default()`, `watch::channel(None).0`, `Mutex::default()`. Add the builder and accessors:
```rust
    /// How often this bridge says Hello, and how long silence means gone.
    pub fn hello_timing(mut self, timing: hello::HelloTiming) -> Bridge {
        self.hello_timing = timing;
        self
    }

    pub(crate) fn roles(&self) -> std::collections::BTreeMap<GrainId, election::Roles> {
        self.roles.lock().expect("roles").clone()
    }

    pub(crate) fn neighbours(&self) -> &Arc<hello::Neighbours> {
        &self.neighbours
    }
```
In `serve_loops`, add a branch to the `select!`:
```rust
            result = hello::run(Arc::clone(&bridge)) => result,
```
(Put it before `data::inbound(bridge, net)`, because that one moves `bridge`.)

Remove the `#[cfg(any(test, feature = "test-util"))]` from `Owners::is_online` in `control.rs`. `hello::run` needs it in production.

- [ ] **Step 5: Implement `hello::run` and `hello::serve_inbound`** (append to `hello.rs`)

```rust
use crate::Bridge;
use crate::election::{Elector, Own};

/// Keep Hello links to every peer, and re-run the election, every interval.
///
/// Only the lower device id of a pair dials, as everywhere else. Never fails: a bridge with
/// no workgroup yet simply has nobody to greet, and asks again next tick.
pub(crate) async fn run(bridge: Arc<Bridge>) -> crate::Result<()> {
    let timing = bridge.hello_timing;
    let mut tick = tokio::time::interval(timing.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut elector: Option<Elector> = None;
    loop {
        tick.tick().await;
        let Ok(Some(workgroup)) = bridge.workgroup() else { continue };
        let Ok(me) = workgroup.this_device(&bridge.transport().node_id()) else { continue };
        let elector = elector.get_or_insert_with(|| Elector::new(me.id, timing.dead));

        let hosting: Vec<GrainId> = bridge
            .route_entries()
            .into_iter()
            .filter(|r| r.app_name != crate::wgsync::WORKSPACE_APP_NAME)
            .filter(|r| bridge.owners().is_online(&r.app_name))
            .map(|r| r.workspace_id)
            .collect();
        let own = Own { priority: me.priority, availability: None, hosting };
        let now = Instant::now();
        let heard = bridge.neighbours.reachable(now, timing.dead);
        let (hello, roles) = elector.step(now, &own, &heard);
        *bridge.roles.lock().expect("roles") = roles;
        bridge.hello_tx.send_if_modified(|current| {
            let changed = current.as_ref() != Some(&hello);
            *current = Some(hello);
            changed
        });

        let Ok(devices) = workgroup.devices() else { continue };
        for device in devices.entries() {
            if device.id <= me.id || device.is_retired() {
                continue;
            }
            let Some(node_id) = device.node_id.clone() else { continue };
            if !bridge.neighbours.begin_link(device.id) {
                continue;
            }
            let bridge = Arc::clone(&bridge);
            let peer = device.id;
            tokio::spawn(async move {
                match bridge.transport().open_hello(&node_id).await {
                    Ok(stream) => {
                        exchange(stream, peer, bridge.hello_tx.subscribe(),
                                 Arc::clone(&bridge.neighbours), bridge.hello_timing).await;
                    }
                    Err(err) => tracing::debug!(%peer, "no hello link: {err}"),
                }
                bridge.neighbours.end_link(peer);
            });
        }
    }
}

/// Answer an inbound Hello stream from an authorized device.
pub(crate) fn serve_inbound(bridge: &Arc<Bridge>, peer: GrainId, stream: BoxedStream) {
    let rx = bridge.hello_tx.subscribe();
    let neighbours = Arc::clone(&bridge.neighbours);
    let timing = bridge.hello_timing;
    tokio::spawn(exchange(stream, peer, rx, neighbours, timing));
}
```
A failed dial is retried on the next tick (every `interval`), with no backoff. That is the same pace the Hellos themselves keep, so it adds no traffic beyond what a working link would.

- [ ] **Step 6: Route inbound Hellos** (in `data.rs` `inbound`)

Replace the match with one that also handles `Inbound::Hello`:
```rust
            Inbound::Hello(from, stream) => {
                // Authorized exactly like a workspace stream: a stranger learns nothing, and
                // a retired device is not heard.
                match bridge.workgroup()?.map(|wg| wg.authorize(&from)) {
                    Some(Ok(device)) => crate::hello::serve_inbound(&bridge, device.id, stream),
                    _ => {
                        tracing::debug!(peer = %from, "hung up on a hello stream");
                        drop(stream);
                    }
                }
                continue;
            }
```

- [ ] **Step 7: Fill `peers` and add the method** (in `control.rs`)

`peers()`:
```rust
fn peers(bridge: &Bridge) -> Result<PeersResult> {
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let roles = bridge
        .roles()
        .into_iter()
        .map(|(workspace_id, r)| WorkspaceRoles { workspace_id, designated: r.designated, backup: r.backup })
        .collect();
    Ok(PeersResult { peers: peer_infos(bridge, &workgroup)?, roles })
}
```
In `peer_infos`, set `priority: d.priority` and `availability: bridge.neighbours().get(d.id).and_then(|h| h.availability)`.

The handler:
```rust
fn device_priority_set(bridge: &Bridge, ctx: RequestCtx) -> Result<DevicePrioritySetResult> {
    let params: DevicePrioritySetParams = serde_json::from_value(ctx.params)
        .map_err(|e| Error::Config(format!("malformed device_priority_set: {e}")))?;
    let workgroup = bridge.workgroup()?.ok_or(Error::NoWorkgroup)?;
    let device = workgroup.set_priority(&params.selector, params.priority)?;
    Ok(DevicePrioritySetResult { device_id: device.id, name: device.name, priority: device.priority })
}
```
Register it in `router` after `DEVICE_RETIRE`, with the same shape as the `DEVICE_RETIRE` entry. Extend the `use sapphire_bridge_api::{..}` list with `DEVICE_PRIORITY_SET, DevicePrioritySetParams, DevicePrioritySetResult, WorkspaceRoles`.

- [ ] **Step 8: Run the tests**

Run: `cargo test -p sapphire-framework-bridge`
Expected: all pass, including the existing suite. If a status/logging test is flaky, it is #165/#172 and pre-existing: rerun that test alone to confirm, and do not "fix" it here.

- [ ] **Step 9: Commit**

```bash
git add crates/sapphire-framework-bridge
git commit -m "feat(bridge): run the election and report roles and priority in peers" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 6: the app server follows the roles

**Files:**
- Create: `crates/sapphire-framework-server/src/sync/topology.rs`
- Modify: `crates/sapphire-framework-server/src/sync/mod.rs`
- Modify: `crates/sapphire-framework-server/src/sync/live.rs`
- Modify: `crates/sapphire-framework-server/src/sync/methods.rs`
- Modify: `crates/sapphire-framework-backend/src/protocol.rs`

**Interfaces:**
- Consumes: `PeersResult::roles_for`, `WorkspaceRoles` (Task 2).
- Produces:
```rust
// sync/topology.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)] pub(crate) enum Link { Dial, Await, Skip }
pub(crate) fn link(me: GrainId, peer: GrainId, roles: Option<&WorkspaceRoles>) -> Link
pub(crate) fn topology(roles: Option<&WorkspaceRoles>) -> proto::Topology
// backend protocol
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Topology { #[default] Mesh, Star { designated: GrainId, backup: Option<GrainId> } }
SyncStatusResult { .., #[serde(default)] pub topology: Topology }
// live.rs
pub(crate) async fn LivePeers::retain(&self, keep: impl Fn(&GrainId) -> bool)
// mod.rs
pub async fn SyncRuntime::is_designated(&self, workspace_id: GrainId) -> bool
SyncStatus { .., pub topology: proto::Topology }
```

- [ ] **Step 1: Write the failing tests** (in `topology.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sorted(n: usize) -> Vec<GrainId> {
        let mut v: Vec<GrainId> = (0..n).map(|_| GrainId::random()).collect();
        v.sort();
        v
    }

    fn roles(ws: GrainId, d: Option<GrainId>, b: Option<GrainId>) -> WorkspaceRoles {
        WorkspaceRoles { workspace_id: ws, designated: d, backup: b }
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
            proto::Topology::Star { designated: d, backup: None }
        );
    }
}
```
`LivePeers::retain` has no unit test: a `LiveSession` exists only after a real exchange. Integration test 1 in Task 8 covers it: the two non-hubs end up with no session to each other.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-server topology`
Expected: compile errors.

- [ ] **Step 3: Implement the protocol type** (in `backend/src/protocol.rs`, above `SyncStatusResult`)

```rust
/// How a synced workspace is wired to its peers right now.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Topology {
    /// Every device syncs with every other. Also the answer when no device was elected.
    #[default]
    Mesh,
    /// Devices that are neither designated nor backup sync only with those two.
    Star {
        /// The designated device.
        designated: grain_id::GrainId,
        /// The backup device, if there is a second candidate.
        backup: Option<grain_id::GrainId>,
    },
}
```
Add `#[serde(default)] pub topology: Topology,` to `SyncStatusResult`, and `topology: Topology::Mesh,` to `not_synced()`.

- [ ] **Step 4: Implement `topology.rs`**

```rust
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
```
(Use the extern names the crate already uses for the backend and bridge-api crates. Check `use` lines at the top of `sync/mod.rs`.) Add `mod topology;` in `sync/mod.rs`.

- [ ] **Step 5: Implement `LivePeers::retain`** (in `live.rs`, next to `drop_connections`)

```rust
    /// Close every session whose device `keep` rejects, as `drop_connections` does for all.
    pub(crate) async fn retain(&self, keep: impl Fn(&GrainId) -> bool) {
        self.sessions.lock().await.retain(|device, _| keep(device));
    }
```

- [ ] **Step 6: Wire `SyncRuntime`** (in `sync/mod.rs`)

1. Add a field `last_peers: Mutex<Option<PeersResult>>` (tokio `Mutex`, like the others) and initialize it to `None`. After each successful `self.bridge.peers()` in `dial_loop`, `sync_now` and `status`, store a clone in it.
2. `dial_loop`, inside `for root in roots`, after `workspace_id` is known:
```rust
                let roles = peers.roles_for(workspace_id);
                // Entering a star closes what it no longer needs. The hubs carry it all.
                table.retain(|d| topology::link(me, *d, roles) != topology::Link::Skip).await;
```
and change the peer filter from `.filter(|p| p.connected && me < p.device_id)` to
```rust
                    .filter(|p| p.connected && topology::link(me, p.device_id, roles) == topology::Link::Dial)
```
3. `sync_now`: change `.filter(|p| me < p.device_id)` to
```rust
        let roles = peers.roles_for(workspace_id).cloned();
        for peer in peers.peers.iter().filter(|p| topology::link(me, p.device_id, roles.as_ref()) == topology::Link::Dial).cloned().collect::<Vec<_>>() {
```
(Keep the loop body unchanged.)
4. The inbound guard in `run()`, inside the spawned task, before `open_live_session`:
```rust
                if driver.should_skip(workspace_id, peer).await {
                    tracing::debug!(%peer, "dropped an inbound session: both ends are outside this workspace's star");
                    return;
                }
```
with
```rust
    /// Whether the last roles the bridge reported say this pair does not link.
    async fn should_skip(&self, workspace_id: GrainId, peer: GrainId) -> bool {
        let Ok(me) = self.device_id().await else { return false };
        let cached = self.last_peers.lock().await;
        let roles = cached.as_ref().and_then(|p| p.roles_for(workspace_id));
        topology::link(me, peer, roles) == topology::Link::Skip
    }
```
5. The hook #188 uses:
```rust
    /// Whether this host is `workspace_id`'s designated device, by the last roles the
    /// bridge reported. `false` when the bridge has not answered yet.
    pub async fn is_designated(&self, workspace_id: GrainId) -> bool {
        let Ok(me) = self.device_id().await else { return false };
        let cached = self.last_peers.lock().await;
        cached
            .as_ref()
            .and_then(|p| p.roles_for(workspace_id))
            .is_some_and(|r| r.designated == Some(me))
    }
```
6. `SyncStatus` gains `pub topology: proto::Topology`. Rewrite the top of `status()` so it keeps the whole answer:
```rust
        let answer = self.bridge.peers().await.ok();
        if let Some(answer) = &answer {
            *self.last_peers.lock().await = Some(answer.clone());
        }
        let peers = answer.as_ref().map_or(0, |p| p.peers.len().saturating_sub(1));
```
In the `Some(entry)` arm, set
```rust
                topology: topology::topology(answer.as_ref().and_then(|p| p.roles_for(entry.workspace_id))),
```
and `topology: proto::Topology::Mesh` in the two other arms. In `methods.rs`'s `From<SyncStatus>`, add `topology: status.topology,`.

- [ ] **Step 7: Run the tests**

Run: `cargo test -p sapphire-framework-server --lib && cargo test -p sapphire-framework-backend`
Expected: pass. Existing integration tests are run in Task 8, after the harness is updated.

- [ ] **Step 8: Commit**

```bash
git add crates/sapphire-framework-server crates/sapphire-framework-backend
git commit -m "feat(server): sync through the designated and backup devices when elected" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 7: CLI verbs

**Files:**
- Modify: `crates/sapphire-framework-bridge/src/command.rs`
- Modify: `crates/sapphire-framework-server/src/command.rs`
- Test: `crates/sapphire-framework-server/tests/cli_bridge.rs` (existing CLI-against-bridge tests)

**Interfaces:**
- Consumes: `BridgeClient::device_priority_set`, `Workgroup::set_priority`.
- Produces: `DeviceCommand::Priority { selector: String, priority: Option<u8> }` in **both** CLIs.

- [ ] **Step 1: Write the failing test**

In `crates/sapphire-framework-bridge/src/command.rs`'s test module (or alongside the existing `device_retire` CLI tests; find them with `grep -n "device_retire" crates/sapphire-framework-bridge/src/command.rs`), add:
```rust
    #[tokio::test]
    async fn priority_set_without_a_bridge_edits_the_ledger() {
        // Point SAPPHIRE_BRIDGE_DIR at a temp dir holding a workgroup founded by
        // `Workgroup::create(&dir, "home", "desk", "aaaa")`, with no bridge running, exactly
        // as the existing offline `device retire` test does. Then:
        let code = device_priority(VERSION, "desk", Some(0)).await.unwrap();
        assert_eq!(code, 0);
        let wg = Workgroup::open(&dir).unwrap().unwrap();
        assert_eq!(wg.this_device("aaaa").unwrap().priority, 0);
    }
```
Copy the environment setup verbatim from the existing offline retire test. If there is none, use `BridgeDir::at(tmp.path().join("bridge"))` and set `BRIDGE_DIR_ENV` for the test, guarded the same way other env-mutating tests in the crate are.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p sapphire-framework-bridge priority_set_without`
Expected: compile error, `device_priority` is not defined.

- [ ] **Step 3: Implement the bridge CLI**

Add the variant to the bridge's `DeviceCommand`:
```rust
    /// Show or set a device's election priority (0-255; 0 = never designated or backup).
    Priority {
        /// The device's name or id.
        selector: String,
        /// The new priority. Omit to show the current one.
        priority: Option<u8>,
    },
```
Dispatch it: `DeviceCommand::Priority { selector, priority } => device_priority(version, &selector, priority).await,`. Implement it next to `device_retire`, with the same online/offline split:
```rust
/// Show or set a device's priority. Through the running bridge when there is one, else
/// straight into the ledger, like `device retire`.
async fn device_priority(version: &str, selector: &str, priority: Option<u8>) -> Result<i32> {
    match priority {
        Some(priority) => {
            if let Some(client) = connect(version).await? {
                let set = client
                    .device_priority_set(sapphire_bridge_api::DevicePrioritySetParams {
                        selector: selector.to_owned(),
                        priority,
                    })
                    .await?;
                println!("{} ({}) priority {}", set.name, set.device_id, set.priority);
                return Ok(0);
            }
            let dir = BridgeDir::open()?;
            let Some(workgroup) = Workgroup::open(&dir)? else {
                println!("this host has not joined a workgroup");
                return Ok(1);
            };
            let device = workgroup.set_priority(selector, priority)?;
            println!("{} ({}) priority {}", device.name, device.id, device.priority);
            Ok(0)
        }
        None => {
            let dir = BridgeDir::open()?;
            let Some(workgroup) = Workgroup::open(&dir)? else {
                println!("this host has not joined a workgroup");
                return Ok(1);
            };
            let devices = workgroup.devices()?;
            let device = devices.resolve(selector)?;
            println!("{} ({}) priority {}", device.name, device.id, device.priority);
            Ok(0)
        }
    }
}
```
In `device_list`, print the priority, plus the roles each device holds:
```rust
    let roles = peers.roles.clone();
    for peer in peers.peers {
        let held: Vec<String> = roles
            .iter()
            .filter_map(|r| {
                if r.designated == Some(peer.device_id) { Some(format!("designated:{}", r.workspace_id)) }
                else if r.backup == Some(peer.device_id) { Some(format!("backup:{}", r.workspace_id)) }
                else { None }
            })
            .collect();
        println!(
            "{} {} {} p{}{}{}",
            peer.name,
            peer.device_id,
            if peer.node_id.is_empty() { "-".to_owned() } else { peer.node_id },
            peer.priority,
            if peer.connected { " (online)" } else { "" },
            if held.is_empty() { String::new() } else { format!(" [{}]", held.join(" ")) },
        );
    }
```

- [ ] **Step 4: Implement the app CLI** (in `crates/sapphire-framework-server/src/command.rs`)

Add the same `Priority` variant to its `DeviceCommand`. Dispatch it through the running bridge only, as its `Retire` does:
```rust
            DeviceCommand::Priority { selector, priority } => {
                let client = connect_running(version).await?;
                match priority {
                    Some(priority) => {
                        let set = client
                            .device_priority_set(DevicePrioritySetParams { selector, priority })
                            .await?;
                        println!("{} ({}) priority {}", set.name, set.device_id, set.priority);
                    }
                    None => {
                        let peers = client.peers().await?;
                        match peers.peers.iter().find(|p| p.name == selector || p.device_id.to_string() == selector) {
                            Some(p) => println!("{} ({}) priority {}", p.name, p.device_id, p.priority),
                            None => {
                                println!("no device {selector:?}");
                                return Ok(1);
                            }
                        }
                    }
                }
                Ok(0)
            }
```
In `render_workspace_list`, append `" (star)"` to the state when `row.sync.topology` is `proto::Topology::Star { .. }`. A mesh line stays exactly as it was, because existing tests compare it.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p sapphire-framework-bridge command && cargo test -p sapphire-framework-server --test cli_bridge`
Expected: pass.

- [ ] **Step 6: Commit**

```bash
git add crates/sapphire-framework-bridge/src/command.rs crates/sapphire-framework-server/src/command.rs
git commit -m "feat(cli): device priority, and roles in device list" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 8: integration tests and harness

**Files:**
- Modify: `crates/sapphire-framework-server/tests/common/mod.rs`
- Create: `crates/sapphire-framework-server/tests/star.rs`

**Interfaces:**
- Consumes: everything above.
- Produces (harness):
  - `pub const NODE_C: &str = "d1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90";`
  - `pub async fn start_host_with_priority(net, node_id, device_name, priority: u8) -> Host`
  - `pub async fn star_hosts(net, spec: &[(&str, &str, u8)]) -> Vec<Host>` (introduced with `introduce_all`, sync enabled on each)
  - `pub fn Host::bridge(&self) -> &BridgeClient`
  - `pub async fn Host::device_id(&self) -> GrainId`

- [ ] **Step 1: Update the harness**

1. In `build`, give every bridge short Hello timing:
```rust
    .hello_timing(HelloTiming { interval: Duration::from_millis(100), dead: Duration::from_millis(600) })
```
2. `build` takes a `priority: u8`. Right after the workgroup is created or opened, write it:
```rust
    workgroup.set_priority(device_name, priority).unwrap();
```
`start_host` passes **`0`**. Existing tests then keep the full mesh they were written for: nobody is a candidate, so `roles` is empty. `restart` keeps the priority already on disk: it passes the current value read back with `Workgroup::open(..).this_device(node_id).priority`.
3. Add `start_host_with_priority`, `NODE_C`, `Host::bridge()` (returns `&self.bridge_client`), and `Host::device_id()`:
```rust
    pub async fn device_id(&self) -> GrainId {
        Workgroup::open(&self.bridge_dir).unwrap().unwrap().this_device(&self.node_id).unwrap().id
    }
```
4. `star_hosts`:
```rust
pub async fn star_hosts(net: &LoopbackNetwork, spec: &[(&str, &str, u8)]) -> Vec<Host> {
    let mut hosts = Vec::new();
    for (node, name, priority) in spec {
        hosts.push(start_host_with_priority(net, node, name, *priority).await);
    }
    let refs: Vec<&Host> = hosts.iter().collect();
    introduce_all(&refs);
    for host in &hosts {
        enable_sync(host).await;
    }
    hosts
}
```
5. Add a fixture that waits for every host to report the same designated device:
```rust
/// Wait until every host's bridge reports `want` as the designated device of its workspace.
pub async fn await_designated(hosts: &[&Host], want: GrainId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let mut all = true;
        for host in hosts {
            let peers = host.bridge().peers().await.expect("peers");
            let ws = host.workspace_id().await;
            if peers.roles_for(ws).and_then(|r| r.designated) != Some(want) {
                all = false;
            }
        }
        if all { return; }
        assert!(Instant::now() < deadline, "the hosts never agreed on {want}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
```
`Host::workspace_id()` reads the shared id from `sync_id_path(self)` and parses it as a `GrainId`.

- [ ] **Step 2: Write the integration tests** (`tests/star.rs`)

```rust
mod common;

use std::time::{Duration, Instant};
use sapphire_framework_bridge::LoopbackNetwork;
use common::{NODE_A, NODE_B, NODE_C, NODE_S};

// Copy the `write` / `await_file` helpers from `tests/live.rs` into this file. They are
// private there.

async fn await_no_session(host: &common::Host, peer: grain_id::GrainId) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let open = host.runtime().unwrap().live_session_devices(&host.ws).await;
        if !open.contains(&peer) { return; }
        assert!(Instant::now() < deadline, "a session to {peer} stayed open");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edit_between_two_non_hubs_travels_through_the_hub() {
    let net = LoopbackNetwork::new();
    let hosts = common::star_hosts(&net, &[
        (NODE_A, "host-a", 3), (NODE_S, "host-s", 2), (NODE_B, "host-b", 1), (NODE_C, "host-c", 1),
    ]).await;
    let (a, b, c) = (&hosts[0], &hosts[2], &hosts[3]);
    let ids = [a.device_id().await, b.device_id().await, c.device_id().await];
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), ids[0]).await;
    await_no_session(b, ids[2]).await;
    await_no_session(c, ids[1]).await;

    write(b, "via-hub.md", "x").await;

    assert_eq!(await_file(c, "via-hub.md").await, "x");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_backup_takes_over_when_the_designated_device_stops() {
    let net = LoopbackNetwork::new();
    let mut hosts = common::star_hosts(&net, &[
        (NODE_A, "host-a", 3), (NODE_S, "host-s", 2), (NODE_B, "host-b", 1), (NODE_C, "host-c", 1),
    ]).await;
    let s_id = hosts[1].device_id().await;
    let a_id = hosts[0].device_id().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), a_id).await;

    hosts[0].stop().await;
    let rest: Vec<&common::Host> = hosts[1..].iter().collect();
    common::await_designated(&rest, s_id).await;

    write(&hosts[2], "after.md", "y").await;
    assert_eq!(await_file(&hosts[3], "after.md").await, "y");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_returning_device_does_not_take_the_role_back() {
    let net = LoopbackNetwork::new();
    let mut hosts = common::star_hosts(&net, &[
        (NODE_A, "host-a", 3), (NODE_S, "host-s", 2), (NODE_B, "host-b", 1),
    ]).await;
    let a_id = hosts[0].device_id().await;
    let s_id = hosts[1].device_id().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), a_id).await;

    let a = hosts.remove(0);
    let mut a = a; a.stop().await;
    common::await_designated(&hosts.iter().collect::<Vec<_>>(), s_id).await;
    let a = a.restart(&net).await;
    // `restart` leaves sync disabled. Enable it the way `star_hosts` does: expose
    // `enable_sync` as `pub` in common/mod.rs for this.
    common::enable_sync(&a).await;

    // Several dead intervals later, S still holds the role.
    tokio::time::sleep(Duration::from_secs(3)).await;
    let mut all: Vec<&common::Host> = hosts.iter().collect();
    all.push(&a);
    common::await_designated(&all, s_id).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_workspace_the_hub_does_not_host_stays_a_mesh() {
    // B and C share a workspace that A (priority 3) does not host. Their roles come only
    // from each other, so B and C are designated and backup, and they hold a session.
    let net = LoopbackNetwork::new();
    let a = common::start_host_with_priority(&net, NODE_A, "host-a", 3).await;
    let b = common::start_host_with_priority(&net, NODE_B, "host-b", 1).await;
    let c = common::start_host_with_priority(&net, NODE_C, "host-c", 1).await;
    common::introduce_all(&[&a, &b, &c]);       // ledgers
    common::introduce(&b, &c);                  // a workspace identity A does not share
    common::enable_sync(&b).await;
    common::enable_sync(&c).await;

    write(&b, "pair.md", "z").await;
    assert_eq!(await_file(&c, "pair.md").await, "z");
}

#[tokio::test(flavor = "multi_thread")]
async fn with_every_priority_zero_the_mesh_is_used() {
    let net = LoopbackNetwork::new();
    let (a, s, b) = common::synced_triple(&net).await; // harness default: priority 0
    common::settle(&[&a, &s, &b]).await;               // the mesh: everyone to everyone
    let status = a.runtime().unwrap().status(&a.ws).await;
    assert_eq!(status.topology, sapphire_backend::protocol::Topology::Mesh);
}
```
Make `enable_sync` `pub` in `common/mod.rs`. In `a_workspace_the_hub_does_not_host_stays_a_mesh`, `introduce_all` writes a shared sync-id to all three, and then `introduce(&b, &c)` overwrites B's and C's with a new one, so A ends up with a different workspace. Leave A's sync disabled.

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p sapphire-framework-server --test star -- --test-threads=1`
Expected: 5 pass. If `await_designated` times out, print `host.bridge().peers()` for every host before failing. The usual cause is that a Hello link never formed: check that the harness's `introduce_all` ran before the first tick that dials.

- [ ] **Step 4: Run the whole server suite** (the harness change touches every test)

Run: `cargo test -p sapphire-framework-server`
Expected: same pass/fail set as before this branch. Compare with a run on the parent commit if anything fails. The known flakes #159/#141 are not regressions, so rerun them alone before investigating.

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-framework-server/tests
git commit -m "test(server): star topology end to end — relay, takeover, no preemption, mesh fallbacks" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 9: GUI

**Files:**
- Modify: `crates/sapphire-framework-gui/src/client/types.rs`
- Modify: `crates/sapphire-framework-gui/src/client/exec.rs`
- Modify: `crates/sapphire-framework-gui/src/views/model.rs`
- Modify: `crates/sapphire-framework-gui/src/views/devices.rs`

**Interfaces:**
- Consumes: `PeerInfo.priority`, `PeersResult.roles`, `SyncStatusResult.topology`.
- Produces:
  - `Command::DevicePrioritySet { selector: String, priority: u8 }`
  - `pub fn role_badges(peer: &PeerInfo, roles: &[WorkspaceRoles]) -> Vec<&'static str>` (`"designated"` / `"backup"`, each at most once)
  - `Badge::Syncing { peers: usize, star: bool }`, with `label()` appending `" · star"` when `star`

- [ ] **Step 1: Write the failing tests** (in `views/model.rs`'s tests)

```rust
    #[test]
    fn role_badges_name_each_role_once() {
        let me = GrainId::random();
        let p = PeerInfo { device_id: me, name: "a".into(), node_id: String::new(), connected: true, priority: 1, availability: None };
        let roles = vec![
            WorkspaceRoles { workspace_id: GrainId::random(), designated: Some(me), backup: None },
            WorkspaceRoles { workspace_id: GrainId::random(), designated: Some(me), backup: None },
            WorkspaceRoles { workspace_id: GrainId::random(), designated: None, backup: Some(me) },
        ];
        assert_eq!(role_badges(&p, &roles), vec!["designated", "backup"]);
    }

    #[test]
    fn a_star_workspace_says_so() {
        let mut s = SyncStatusResult::not_synced();
        s.enabled = true;
        s.peers = 2;
        s.topology = Topology::Star { designated: GrainId::random(), backup: None };
        assert_eq!(badge(&entry_with(s)).label(), "syncing · 2 peers · star");
    }
```
(`entry_with` is whatever helper the existing badge tests in this module use to build a `WorkspaceListEntry`. Reuse it.)

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p sapphire-framework-gui model`
Expected: compile errors.

- [ ] **Step 3: Implement**

`types.rs`, after `DeviceRetire`:
```rust
    /// Set a device's election priority.
    DevicePrioritySet {
        /// The device's name or id.
        selector: String,
        /// `0..=255`.
        priority: u8,
    },
```
`exec.rs`: add `| Command::DevicePrioritySet { .. }` to the bridge-command arm at line 36–39, and an arm next to `DeviceRetire`:
```rust
        Command::DevicePrioritySet { selector, priority } => c
            .device_priority_set(DevicePrioritySetParams { selector, priority })
            .await
            .map(|r| CommandOutput::Message(format!("{} priority {}", r.name, r.priority))),
```
(Match the `CommandOutput` variant and the error mapping the `DeviceRetire` arm uses. Copy that arm's shape exactly.)

`model.rs`:
```rust
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
```
`Badge::Syncing` gets `star: bool`. `badge()` sets `star: matches!(entry.sync.topology, Topology::Star { .. })`, and `label()` appends `" · star"` when it is true. Update every other construction of `Badge::Syncing` the compiler points at.

`devices.rs`: in each device row, after `ui.small(short_id(&peer.device_id));`:
```rust
                        for role in role_badges(peer, &bridge.peers_roles) {
                            ui.small(format!("[{role}]"));
                        }
```
(`bridge.peers_roles` is whatever field the snapshot keeps the `PeersResult.roles` in. Add it to the snapshot type where `peers` is filled from `client.peers()`: `client/mod.rs` or `client/types.rs`, at the place `bridge.peers` is assigned.)

Then add a priority editor to **every** device row, this device included. Restructure the row so the `(this device)` label no longer `return`s before the right-to-left layout. Inside that layout, the editor comes first, and the Retire button stays only for other devices:
```rust
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if !this {
                                if ui.add_enabled(!cx.busy, egui::Button::new("Retire")).clicked() {
                                    self.confirm = Some(RetireConfirm { name: peer.name.clone(), ..RetireConfirm::default() });
                                }
                            }
                            let mut p = self.priority_edit.get(&peer.device_id).copied().unwrap_or(peer.priority);
                            if ui
                                .add_enabled(!cx.busy && p != peer.priority, egui::Button::new("Set"))
                                .clicked()
                            {
                                out = Some(Command::DevicePrioritySet { selector: peer.name.clone(), priority: p });
                            }
                            ui.add_enabled(!cx.busy, egui::DragValue::new(&mut p).range(0..=255).prefix("priority "));
                            self.priority_edit.insert(peer.device_id, p);
                        });
```
Here `this` is `is_this_device(peer, &bridge.status.node_id)`, computed once at the top of the row. Show `ui.small("(this device)")` without returning. Add `priority_edit: HashMap<GrainId, u8>` to the screen's state struct. Remove an entry when the snapshot's `peer.priority` equals it, so the editor follows changes made elsewhere. `out` is the function's existing `let mut out = None;`. The row closure already borrows `self` mutably for `confirm`, so `priority_edit` follows the same pattern. No confirmation dialog (spec).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p sapphire-framework-gui && cargo check --workspace --all-targets`
Expected: pass.

- [ ] **Step 5: Commit**

```bash
git add crates/sapphire-framework-gui
git commit -m "feat(gui): device priority editor, role badges, star in the workspace badge" -m "Refs #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```

---

### Task 10: docs and changelog

**Files:**
- Modify: `docs/ARCHITECTURE.md`
- Modify: `CHANGELOG.md`
- Modify: `docs/superpowers/specs/2026-10-08-primary-device-design.md` (one correction)

- [ ] **Step 1: Correct the spec**

In the spec's "Control plane" section, the method's parameters are `{ selector: String, priority: u8 }` (matching `device_retire`'s `selector`), not `{ workgroup, device, priority }`. In "Sync behaviour", `Link` has three values: `Dial`, `Await`, `Skip`. Edit both lines to match the code.

- [ ] **Step 2: ARCHITECTURE.md**

Under "ワークスペース同期（iroh・2 仕様）", add a subsection `### 代表デバイスとスター型同期（#182）` with 5–8 lines of Japanese prose:
- What decides the roles: priority (台帳、手動), Hello (`sapphire/hello/1`, 10 s / 40 s), the non-preemptive election per workspace.
- That the roles are not consensus, and that work done by the designated device must therefore be idempotent.
- The fallback to the mesh.
- A link to the spec.

In the bridge's role list ("bridge はホスト常駐の交換台"), add a 4th item: **選出** — Hello を交換し、ワークスペースごとに代表 / 予備デバイスを選ぶ.

- [ ] **Step 3: CHANGELOG.md**

Under the unreleased section, add an entry in the file's existing style:
- **Added:** designated / backup devices elected per workspace. `sapphire-bridge device priority`. bridge-api 2.1.0 (`bridge.device_priority_set`, `PeersResult.roles`, `PeerInfo.priority`).
- **Changed:** with two or more devices at priority ≥ 1 (the default), a workspace syncs as a star around its designated and backup devices. Set every device's priority to 0 to keep the full mesh.

- [ ] **Step 4: Verify the whole workspace**

Run: `cargo test --workspace && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass. Flakes listed in #159/#165/#172/#141 can be rerun alone.

- [ ] **Step 5: Commit**

```bash
git add docs CHANGELOG.md
git commit -m "docs: designated devices in ARCHITECTURE and CHANGELOG" -m "Closes #182." -m "Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
```
