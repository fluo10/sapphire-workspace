# Central device: one designated device per workgroup, and star-shaped sync while it is reachable

- Date: 2026-10-08
- Issue: #182 (tracking: #189)
- Scope: `sapphire-framework-bridge` (`workgroup.toml`, the ledger, the CLI, the control-plane
  method), `sapphire-framework-bridge-api` (one method, one field, own version 2.0.0 → 2.1.0),
  `sapphire-framework-server` (`SyncRuntime`'s dial loop, `sync_now`, inbound sessions),
  `sapphire-framework-gui` (`SyncPanel`'s device list).
- Related: [`2026-09-15-p2p-sync-iroh-design.md`](./2026-09-15-p2p-sync-iroh-design.md)
  (replication), [`2026-09-16-process-architecture-design.md`](./2026-09-16-process-architecture-design.md)
  (who owns what), [`2026-10-07-sync-gui-design.md`](./2026-10-07-sync-gui-design.md) (`SyncPanel`).

## Background

Every app server today opens a live session with every other device that hosts the same
workspace: a full mesh, with one dial direction per pair (the lower device id dials). The
number of sessions grows with the square of the device count, and every device does the
same work on every change.

Two upcoming features also need *one* device to act on behalf of the workgroup. Background
work that writes files into a workspace — embedding files that have no vector yet, and
collecting vectors nobody references (#188) — must run on exactly one device, or two devices
write the same file and produce conflicts.

This spec gives a workgroup an optional **central device**. While it is reachable, every other
device syncs a workspace through it alone (a star). The central device is also the device
that later features use for their background work, through `SyncRuntime::is_central()`.

**The central device is not a server.** This project once had a central sync server (#90,
HTTP JSON-RPC), and that design was replaced by p2p sync. Nothing about that comes back
here: every device still holds a full replica and works offline, every device can write,
and when the central device is unreachable the others fall back to the mesh they use
today. "Central" names a position in the topology, as in Bluetooth LE's central /
peripheral, not an authority.

## Decisions

1. **One central device per workgroup, not per workspace.** It is recorded once, in the
   workgroup's own synced state. Picking the always-on host is how people will use it, and
   one setting is what the UI can explain.
2. **Whether the star is in force is decided per workspace.** The central device may not host
   every workspace (it may run journal but not agent). For a given workspace, a non-central
   device uses the star only while it holds a live session *for that workspace* with the
   central device. Otherwise that workspace syncs as a mesh. No workspace is ever left
   without a path.
3. **The word is "central".** "Master" was avoided deliberately; "hub" undersold the role of
   doing the workgroup's background work; "primary" suggests the others are read-only.
4. **No new consensus.** The setting lives in `workgroup.toml`, which the workgroup root already
   replicates with last-writer-wins. Two devices setting different central devices at once
   converge on whichever write wins, like any other workgroup file.

## Data

`<workgroup root>/workgroup.toml` gains an optional key:

```toml
name = "home"
central = "<device grain-id>"   # absent: no central device, full mesh as today
```

`WorkgroupFile` gets `central: Option<GrainId>` with `#[serde(default, skip_serializing_if =
"Option::is_none")]`. The bridge resolves it against the ledger every time it reads it:

- the id is not in the ledger, or names a retired device → treated as **no central device**.
  The file is not rewritten; retiring the central device needs no second step, and an id
  that arrives before its device record does starts working when the record lands.

A bridge older than this change ignores the key when reading. It drops the key only if it
rewrites `workgroup.toml`, which today happens only at `create` and `join`.

## Control plane (`sapphire-framework-bridge-api` 2.1.0)

Both changes only add things, so `API_VERSION` (the major version) stays 2.

- New method `bridge.workgroup_central_set` (`WORKGROUP_CENTRAL_SET`):
  `{ workgroup: Option<String>, device: Option<String> }` → `{ central: Option<GrainId> }`.
  `device` is a name or id; `None` clears the setting. `workgroup` is the same selector the
  other workgroup methods take. A retired or unknown device is an error.
- `PeersResult` gains `central: Option<GrainId>` (`#[serde(default)]`): the resolved central
  device, or `None`. An app server talking to an older bridge reads `None` and stays a mesh.
- `WorkgroupStatus` gains the same field, so `status` and the GUI can show it.

## CLI (`sapphire-bridge`)

```
sapphire-bridge workgroup central             # show the central device, or "none"
sapphire-bridge workgroup central set <device>
sapphire-bridge workgroup central unset
```

Like `device retire`: with a bridge running it goes through the control-plane method; with
none running it edits the bridge directory directly. Either way the change is a write to the
synced workgroup root, so it reaches the other devices at their next workgroup session. The
apps' flat `workgroup` directive gains the same verb, pointing at the bridge as the other
bridge-owned verbs do. `device list` marks the central device.

## Sync behaviour (`sapphire-framework-server`)

A pure function decides each pair, so the rule is testable without a network:

```rust
enum Link { Dial, Skip }

fn link(me: GrainId, peer: GrainId, central: Option<GrainId>, star: bool) -> Link
```

- `star` is true when `central` is `Some(c)`, `c != me`, and this host has a live session with
  `c` for the workspace in question (or had one within the grace period below).
- When `star` holds, a pair in which neither side is the central device is `Skip`.
- Otherwise the existing rule applies unchanged: dial only if `me < peer`.

The central device itself never sees `star` and never skips anyone. Whether it dials a
device or is dialled by it still follows the id rule.

Where the rule applies:

- **`dial_loop` and `sync_now`** filter peers through `link`. `central` comes from the same
  `bridge.peers()` call they already make.
- **Entering the star.** When a workspace's live session with the central device opens, live
  sessions with other non-central devices for that workspace are closed
  (`LivePeers::drop_except(central)`). Nothing is lost: the central device holds and relays
  everything those sessions would have carried.
- **Inbound sessions.** `run()` drops an announced stream from a non-central peer while
  `star` holds for that workspace. Both ends compute the same rule from the same ledger, so
  this is only a safety net for the moment when they disagree (one has seen the new
  `workgroup.toml` and the other has not yet).
- **Leaving the star.** When the session with the central device closes, `star` stays true
  for a **grace period of 30 seconds** (`CENTRAL_GRACE`), so a brief reconnect does not
  tear the mesh up and down. After it, the next dial pass dials the mesh as today.
- **Relay.** Unchanged. The central device's reader already forwards what one peer sends to
  every other session (`fan_out(.., Some(from))`).

`SyncRuntime::is_central() -> bool` reports whether this device is the resolved central
device. It reads the same `peers()` answer, cached for one dial interval. #188 uses it.

Status: the per-workspace sync status gains `topology: "mesh" | "star"`, and
`sapphire-<app> status` prints it, so "why is this device not talking to that one" has a
visible answer.

## GUI (`sapphire-framework-gui`)

- `views`: the device rows carry `is_central`. The central device's row shows a "central"
  badge.
- `SyncPanel`: each non-retired device row gets "Make central". The central device's row gets
  "Clear central". Both go through `FrameworkClient` → `bridge.workgroup_central_set`, with the
  same command timeout and error line as the other commands. No confirmation dialog: the
  change can be undone at once and loses nothing.

## Error handling

- Bridge down, or no workgroup: `peers()` fails, and nobody is dialled, as today.
- `central` names an unknown or retired device: no central device, full mesh.
- The central device does not host the workspace: it ignores the announcement (existing
  behaviour), no session forms, and that workspace stays a mesh.
- An older bridge (no `central` field): `None`, full mesh.

## Testing

- **Unit:** `link` over every combination — no central; this host is central; peer is
  central; star on and off; both id orders.
- **Unit:** resolving `central` against the ledger — unknown id, retired id, valid id.
- **Bridge:** `workgroup central set/unset` both with and without a running bridge. A retired
  device is refused. The setting round-trips through `workgroup.toml`.
- **Integration (three devices, existing sync test harness):**
  1. With C central, an edit on A reaches B, and A and B hold no session with each other.
  2. With C stopped, after the grace period A and B open a session and the edit still arrives.
  3. When C comes back, the A–B session is closed and edits still arrive.
  4. A workspace C does not host syncs A↔B directly while another workspace is in a star.

## Out of scope

- Electing the central device automatically, or failing over to another one. The fallback
  is the mesh, not a second central device.
- Background work itself (embedding backfill, GC) — #188.
- More than one workgroup per host (still rejected by `control.rs`).
