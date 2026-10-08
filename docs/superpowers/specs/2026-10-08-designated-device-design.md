# Designated devices: electing a designated and a backup device per workspace, and star-shaped sync around them

- Date: 2026-10-08
- Issues: #182 (election and topology), #190 (availability measurement); tracking: #189
- Scope: `sapphire-framework-registry` (a `priority` on the device record),
  `sapphire-framework-bridge` (a bridge-to-bridge Hello protocol, the election, the CLI,
  availability tracking), `sapphire-framework-bridge-api` (one method, new fields, own
  version 2.0.0 → 2.1.0), `sapphire-framework-server` (`SyncRuntime`'s dial loop,
  `sync_now`, inbound sessions), `sapphire-framework-gui` (`SyncPanel`).
- Related: [`2026-09-15-p2p-sync-iroh-design.md`](./2026-09-15-p2p-sync-iroh-design.md)
  (replication), [`2026-09-16-process-architecture-design.md`](./2026-09-16-process-architecture-design.md)
  (who owns what), [`2026-10-07-sync-gui-design.md`](./2026-10-07-sync-gui-design.md) (`SyncPanel`).

## Background

Every app server today opens a live session with every other device that hosts the same
workspace: a full mesh, with one dial direction per pair (the lower device id dials). The
number of sessions grows with the square of the device count, and every device does the
same work on every change.

Upcoming features also need *one* device to act for the workgroup. Background work that
writes files into a workspace — embedding files that have no vector yet, and collecting
vectors nobody references (#188) — should run on one device, not on all of them.

The first draft of this spec named a single, manually chosen "central device". That was
dropped: a workgroup that moved from a central server (#90) to p2p sync should not grow a
fixed centre again. This spec instead follows OSPF's designated router (DR) and backup
designated router (BDR): every device computes, from what it can see, which reachable
device is the **designated device** and which is the **backup device** for each workspace.
No device is fixed in the role, and losing one costs nothing but a re-election.

Unlike routers, desktops sleep, and laptops (and, later, phones) come and go. So the
election weighs two things: a manual **priority** (0 opts a device out) and a measured
**availability**.

## Decisions

1. **Elected, not appointed.** No setting names a device as designated. Users influence the
   election only through priority. Setting one device's priority above everyone else's is
   how to pin the role in practice.
2. **The election tolerates disagreement; the work it guards must be idempotent.** There is
   no consensus protocol. During a partition, each side elects its own designated device,
   and both run background work. That is accepted. The requirement moves to the work instead:
   everything #188 does must be idempotent and converge when two devices do it at once
   (content-addressed vector paths, last-writer-wins with no conflict copies under
   `embedded/`, GC that only deletes what a backfill would recreate). The cost of a
   disagreement is wasted compute, never lost or conflicting data.
3. **Per workspace.** Candidates for a workspace are the devices whose app server for that
   workspace is online. A device that does not run agent is never agent's designated device.
   So no workspace is ever left without a path.
4. **Non-preemptive, like OSPF.** A designated device keeps the role while it stays
   reachable, even when a higher-ranked device appears. The role moves only when its holder
   disappears. This is what keeps a sleeping-and-waking desktop from making the topology
   flap.
5. **Availability travels in Hello, not in the ledger.** Only reachable devices are
   candidates, and every reachable device can say its own availability. The ledger (synced)
   holds only what a person sets, the priority. Nothing measured is written into synced
   files, so measuring adds no sync traffic and cannot race a priority change.
6. **Words.** "Designated device" and "backup device" (表示: 代表デバイス / 予備デバイス).
   "Master" and "central" were both considered and rejected.

## Data

The device record (`<workgroup root>/devices/<id>.toml`) gains:

```toml
priority = 1    # 0..=255; 0: never designated or backup. Absent: 1.
```

`Device` gets `priority: u8` with `#[serde(default = "default_priority")]` (1). Default 1
means every device takes part. With #190, availability then keeps devices that sleep a lot
out of the role without anyone setting anything. Before #190 ships, ties on priority fall to
the device id.

## Hello protocol (bridge ↔ bridge)

A new ALPN, `sapphire/hello/1`, authorized by the device ledger exactly like the workspace
ALPN. Each bridge holds one long-lived Hello stream per connected peer and sends a message
every `HELLO_INTERVAL` (10 s) and whenever its content changes. Lines are capped at 16 KiB.
A dialer does not open a Hello stream before it has its own `Some(Hello)` to send (on iroh
the acceptor's loop is serial). A link that ends forgets its peer, including when its task
is aborted.

```rust
struct Hello {
    device_id: GrainId,
    priority: u8,                       // as this bridge reads its own ledger record
    availability: Option<Tier>,         // None until #190
    hosting: Vec<GrainId>,              // workspaces whose owning app server is online here
    designated: Vec<GrainId>,           // workspaces this device holds as designated
    backup: Vec<GrainId>,               // workspaces this device holds as backup
}
```

A peer is **reachable** while a Hello from it is less than `DEAD_INTERVAL` (40 s) old. When
the stream closes, the peer becomes unreachable at once.

A peer whose bridge predates this protocol never sends a Hello. Its device is not a
candidate. Its app server syncs as before (see "Mixed versions").

## Election (`sapphire-framework-bridge`, `election.rs`)

A pure function, run per workspace and driven by a stateful `Elector::step(now, &own,
&peers)`. The election re-runs once per Hello interval, not on every Hello arrival, so
failover lags by up to one interval plus the app server's dial interval.

```rust
fn elect(candidates: &[Candidate], promote: bool) -> Roles

struct Roles { designated: Option<GrainId>, backup: Option<GrainId> }
```

- **Candidates:** this host and every reachable peer that lists `workspace` in `hosting`,
  with `priority > 0`.
- **Rank:** `(priority, availability tier, device id)`, highest first. A missing tier ranks
  below every tier.
- **Designated:** if candidates already claim it, the highest-ranked claimant keeps it, and
  the others drop their claims. Otherwise, if a candidate claims backup and `promote` is
  set, the backup is promoted. Otherwise, the highest-ranked candidate.
- **Promotion gate:** `promote` is true only for a workspace in which this `Elector` has
  previously seen a designated claim. Before that, designated is chosen by rank. A
  provisional backup claim must not promote itself before the rank-elected designated has
  claimed, which would make the role flap.
- **Backup:** the same rule over the candidates other than the designated device, using
  `backup` claims.
- **Wait timer:** a bridge that has just started (or just started hosting a workspace)
  claims nothing for `DEAD_INTERVAL`, and while it waits it is left out of its own election
  entirely (it still hears its peers). This gives existing claims time to arrive, so a
  device that wakes up does not seize a role that is already held.

Each bridge then publishes its own claims in its next Hello. Two bridges with different
views can transiently disagree. The claim rule makes them converge in one Hello round once
they see each other, and decision 2 makes the disagreement harmless meanwhile.

## Control plane (`sapphire-framework-bridge-api` 2.1.0)

These changes only add things, so `API_VERSION` stays 2.

- `PeersResult` gains `roles: Vec<WorkspaceRoles>` (`#[serde(default)]`), with
  `WorkspaceRoles { workspace_id, designated: Option<GrainId>, backup: Option<GrainId> }` for
  every hosted workspace, including entries where both roles are `None`. An app server talking to an older bridge reads
  an empty list and stays a mesh.
- `PeerInfo` gains `priority: u8` and `availability: Option<Tier>` (`#[serde(default)]`), for
  display.
- New method `bridge.device_priority_set` (`DEVICE_PRIORITY_SET`):
  `{ selector: String, priority: u8 }` → `{ device_id, name, priority }`, mirroring
  `device_retire`. An unknown or retired device is an error.

## CLI (`sapphire-bridge`)

```
sapphire-bridge device priority <device>            # show
sapphire-bridge device priority <device> <0-255>    # set
```

Like `device retire`: with a bridge running it goes through the control-plane method; with
none running it edits the ledger directly. The change is a write to the synced workgroup
root, so every device sees it at its next workgroup session. `device list` gains `priority`
column, plus the workspaces each device is designated or backup for. The `availability`
column of `device list` belongs to #190.
The apps' flat `device` directive gains the same verb, pointing at the bridge as the other
bridge-owned verbs do.

## Sync behaviour (`sapphire-framework-server`)

The server gets roles from `bridge.peers()`, the call it already makes on every dial pass.
A pure function decides each pair:

```rust
enum Link { Dial, Await, Skip }

fn link(me: GrainId, peer: GrainId, roles: &Roles) -> Link
```

- A star requires a `designated` device. Roles with only `backup` set, or with nothing set,
  mean mesh, and the existing rule applies: `Dial` only if `me < peer`, otherwise `Await`
  the peer's dial.
- In a star, if this host is the designated or backup device, the same id rule applies to
  every peer. They therefore stay linked to everyone, and to each other.
- Otherwise (this host is neither), a peer that is neither designated nor backup is `Skip`.
  The designated and backup devices follow the id rule.

Where the rule applies:

- **`dial_loop` and `sync_now`** filter peers through `link`.
- **Entering the star.** When roles appear for a workspace and this host is neither, live
  sessions with peers that `link` now skips are closed (`LivePeers::retain(..)`). Nothing is
  lost: the designated and backup devices hold and relay everything.
- **Inbound sessions.** `run()` drops an announced stream from a peer that `link` would skip.
  Both ends compute from the same Hellos, so this is only a safety net for the moment their
  views differ.
- **Losing the designated device.** The backup already holds a session with every device, so
  sync does not pause. The election promotes it within `DEAD_INTERVAL`, a new backup is
  elected, and the next dial pass links to it. If no candidate is left, `roles` empties and
  the mesh returns.
- **Relay.** Unchanged. A reader already forwards what one peer sends to every other session
  (`fan_out(.., Some(from))`).

`SyncRuntime::is_designated(workspace) -> bool` is the hook #188 uses. It reads only the
cached device id and the last roles the bridge reported, and never registers anything.

Status: each workspace's sync status gains `topology: mesh | star { designated, backup }`,
and `sapphire-<app> status` prints it.

## Availability (#190)

- The bridge records, once a minute, that it is running, in `<bridge dir>/availability.toml`
  as per-day minute counts for the last 7 days. If the wall clock has advanced much further
  than the monotonic clock between two ticks, the host slept, and those minutes are not
  counted.
- `availability = minutes up / minutes in the window`, mapped to a `Tier`: `≥ 99 %` → 3,
  `≥ 95 %` → 2, `≥ 80 %` → 1, below → 0. Tiers, not percentages, so small swings do not
  reorder candidates.
- A device with less than 7 days of history reports one tier lower than its measurement
  (floor 0). A freshly installed host does not outrank one with a long record.
- The tier is announced in Hello and shown in `device list` and the GUI. It is never written
  to the ledger (decision 5).

## GUI (`sapphire-framework-gui`)

- Device rows: priority (editable, 0–255, through `bridge.device_priority_set`), the
  availability tier, and badges for the workspaces the device is designated or backup for.
- Workspace rows: the topology line (`mesh`, or `star: designated X, backup Y`).
- No confirmation dialog for priority changes: they can be undone at once and lose nothing.

## Mixed versions

- An old bridge sends no Hello. Its device is not a candidate.
- An old app server ignores `roles` and dials the mesh. New non-elected devices drop its
  inbound sessions while a star is up, but the designated and backup devices accept them. So
  the old device still syncs, through them.
- An old bridge under a new app server: `roles` is empty, and the mesh is used.

## Error handling

- Bridge down, or no workgroup: `peers()` fails, and nobody is dialled, as today.
- No candidates for a workspace (all priority 0, or nobody else hosts it): `roles` is empty,
  and the mesh is used.
- A Hello that does not parse: logged once per peer, and that peer is treated as having sent
  none. It is never a reason to drop the peer's workspace streams.

## Testing

- **Unit, `elect`:** no candidates; priority 0 excluded; rank order; an existing claim
  survives a higher-ranked newcomer; backup promotion; two conflicting claims resolve to the
  higher rank; the wait timer suppresses claims.
- **Unit, `link`:** every combination of me / peer × designated / backup / neither, roles
  empty and present, both id orders.
- **Unit, availability:** tick counting, sleep detection (wall clock jumps past monotonic),
  tier mapping, the young-history penalty, the 7-day window rolling over.
- **Bridge:** Hello over the loopback transport — reachability, `DEAD_INTERVAL`, claims
  converging between two bridges with different starting views. `device priority` with and
  without a running bridge. A retired device is refused.
- **Integration (three devices, existing sync test harness):**
  1. With A designated, an edit on B reaches C, and B and C hold no session with each other.
  2. With A stopped, the backup takes over, and edits still arrive with no gap longer than
     a dial pass. (Not enforced by a test.)
  3. A returns and does **not** take the role back (non-preemption).
  4. B and C, which the top-priority device does not host, elect among themselves and sync
     directly. Cross-workspace isolation is covered by the `Elector` unit test
     `a_peer_not_hosting_the_workspace_is_not_a_candidate`, because the test harness runs
     one workspace per host.
  5. With every priority at 0, the mesh is used.

## Out of scope

- Background work itself (embedding backfill, GC): #188, under decision 2's idempotency
  requirement.
- Picking candidates by anything other than priority and availability (network quality,
  disk space, battery).
- More than one workgroup per host (still rejected by `control.rs`).
