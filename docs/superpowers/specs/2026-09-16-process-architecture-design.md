# Process architecture: app servers over a shared bridge

- Date: 2026-09-16
- Scope: `sapphire-framework` — new crates `sapphire-framework-ipc`, `-server`, `-bridge`;
  new binary `apps/sapphire-bridge/`; changes to `-backend`, `-workspace`, the facade;
  `sapphire-framework-net` is never created
- Depends on: [`2026-09-15-p2p-sync-iroh-design.md`](./2026-09-15-p2p-sync-iroh-design.md)
  ("the sync spec" below). Its §2 (replication core) and §6.1 (its tests) stay authoritative
  and are already implemented; this spec replaces its §3–§5.
- Also revises: [`2026-09-15-sapphire-sync-design.md`](./2026-09-15-sapphire-sync-design.md)
- Follow-ups (separate specs, in their own repositories): `sapphire-journal`,
  `sapphire-ledger`, `sapphire-timer`, `sapphire-agent` migrations
- Related: `sapphire-agent` issue #257 (the shell / fs tool policy whose facility here was
  removed, issue #145)

## Background

Local-first collaboration between a human and an agent means both may drive the same
workspace at the same time, as the same OS user. Today they cannot: the cache is a redb
database opened with `Database::create`, which takes an exclusive file lock and fails
immediately for the second process (`-retrieve/src/redb_store.rs`,
`-track/src/redb_store.rs`, `-sync/src/store.rs`).

#129 split the cache, data and config directories per distribution kind
(`<app>/<kind>/<uuid>/`, `kind` = `cli` / `server` / `desktop`) so that a desktop app and a
server would not fight over one database. That split does not help the case that matters
here: `sapphire-journal add` and the journal's **stdio MCP server** are both `cli`, resolve
to the same path, and collide. Any two CLI invocations collide. Adding more kinds would only
move the problem.

The fix is to stop having several processes open the same database. One process owns the
state; everything else talks to it. That turns the server from one distribution form among
three into a **dependency** of the other two.

This interacts with the sync spec, which is mid-implementation: its replication core is
finished, and `-net` (its §3–§5) has not been started. The sync spec answered the same
"who owns the state" question differently — whichever process wins a lock runs the sync
node, everyone else is a follower, and processes talk to each other only through files.
Keeping both models would leave the framework with two notions of ownership. This spec
unifies them.

## Decisions

Agreed during brainstorming on 2026-09-16:

1. **One ownership model, not two.** The sync spec's holder / follower election (§3.3) is
   replaced. Whatever owns a piece of state is a single, named process.
2. **The app server is the only writer of its workspace files.** A sync update received from
   a peer is applied by the app server that owns that workspace, so writing the file,
   updating the cache and recording the version happen in one process. External-edit
   detection is demoted to what it was meant for — a user editing a file with another tool —
   instead of being the framework's main path.
3. **The replica store lives with the app server, not the bridge.** The bridge carries no
   application replication state. Merging and materialising therefore never cross a process
   boundary, so no crash can leave the replica state and the files disagreeing.
4. **No app-agnostic host.** To sync an app's workspace on a host, run that app's server
   there. A host that only wants an opaque copy of the files uses rsync or git — the origin
   is files, so that already works. `sapphire-sync` therefore loses the "always-on peer for
   any app" and "dedicated background sync service" roles and becomes one app among many;
   the bridge takes the core slot.
5. **App servers start on demand, and can also be installed as a service.** A CLI that finds
   no server spawns one from `current_exe()`; hosts that want an always-on server install it.
   Because start-on-demand re-executes the same binary, one app still ships as one binary.
6. **The framework owns a generic `workspace.*` namespace; apps add their own.** The IPC
   surface is a JSON-RPC router, not a blank transport, so `WorkspaceBackend` crosses the
   boundary once instead of being reinvented per app.
7. **The host-wide daemon is `sapphire-bridge`.** "Node" is already the sync spec's word for
   the abstract peer; naming a process after it would make the word mean two things.
   `bridge` names the job — local apps to the mesh — after Proton Bridge.
8. **The bridge is a standalone binary.** Embedding it in app servers would make "which
   app's build is running the bridge" a question to answer at every version mismatch. A
   separate binary lets the bridge version move with the framework while apps move
   independently.
9. **Privilege separation was removed** (`sapphire-agent` #257 withdrawn). Per-node permission
   separation had to be enforced on every syncing node — hardest on Windows — and one node
   missing it re-opened the injection hole, so the mechanism paid per-OS maintenance for
   partial protection. Shell / generic fs tools are restricted by policy instead: admin
   devices and admin rooms only (a `sapphire-agent` concern).

## 1. Process layering and ownership

```
+- one OS user --------------------------------------------------+
|                                                                |
|  journal CLI   journal stdio MCP   journal desktop             |
|       +--------------+-----------------+                       |
|                      | UDS / named pipe (JSON-RPC)             |
|                      v                                         |
|            sapphire-journal-server ---+                        |
|              . workspace files        |                        |
|              . retrieve / track DB    | UDS / named pipe       |
|              . app-specific cache     |                        |
|              . replica store (sync)   |                        |
|              . HTTP /mcp              |                        |
|                                       v                        |
|            sapphire-ledger-server --> sapphire-bridge          |
|                                         . node.key / NodeId    |
|                                         . workgroup + authz    |
|                                         . pairing              |
|                                         . iroh endpoint, relay |
|                                         . workspace -> owner   |
+------------------------------------------+---------------------+
                                           | iroh (QUIC)
                                     other devices
```

One rule governs the whole design: **every piece of state has exactly one writing process.**

| State | Owner |
|---|---|
| Workspace files | that app's server |
| retrieve / track / app-specific cache | same |
| Replica store (`sync.redb`) | same |
| Device identity (`node.key`), workgroup membership, pairing | bridge |
| The workgroup workspace (device list, workspace list) | bridge |

The last row matters. Workgroup metadata is itself a workspace replicated between devices,
so **the bridge is the app server of one app, and that app is the workgroup.** It uses
`-sync` exactly like any other owner; there is no second mechanism. Decision 3 reads more
precisely as: the bridge holds no *application* replica.

**Where the line falls for users.** The bridge knows devices and workgroups; app servers
know workspaces. The CLIs follow: `sapphire-bridge pair` and `device list` are device-level;
`journal sync enable` and `journal sync map` are workspace-level and reach the bridge through
the owning app server. The sync spec's plan to embed `NodeCommand` in every app's CLI is
dropped.

**One server per app, not per workspace.** A single `sapphire-journal-server` serves every
journal workspace on the host, opening a `WorkspaceState` per root and closing it on an LRU
/ idle policy. This keeps the process count and the socket namespace flat: one `<app>.sock`,
whatever the user's workspaces are.

## 2. IPC layer (`sapphire-framework-ipc`)

The crate carries transport, framing, the router and process startup. It does **not**
implement `workspace.*`: depending on `-backend` or `-workspace` would make it unusable by
the bridge, which has no search stack.

### 2.1 Transports

`trait Transport` exchanges frames. Three implementations:

| Implementation | Used by | Endpoint |
|---|---|---|
| `UnixTransport` | Linux, macOS | `<runtime dir>/<app>.sock` (§2.5) |
| `PipeTransport` | Windows | `\\.\pipe\sapphire.<user-sid>.<app>`, `PIPE_REJECT_REMOTE_CLIENTS` |
| `ChannelTransport` | mobile, tests | in-process `tokio::sync::mpsc` |

`ChannelTransport` still serialises. Paying that cost keeps mobile on the same code path as
every other platform, rather than on a shortcut exercised only there.

### 2.2 Wire format

JSON-RPC 2.0, one message per line (NDJSON). A connection multiplexes requests and matches
responses by `id`, which the desktop needs. Server-to-client events are notifications.

**Cancellation in v1 is disconnection.** An explicit `$/cancel` is added when something needs
it.

**Large payloads** travel as JSON strings. Sending many megabytes through the socket is
merely inefficient, and chunking is deferred to the same moment `-sync` stops doing
whole-file transfer.

### 2.3 Trust model

**Same OS user, and nothing else.** No tokens. Unix checks the peer's uid (`SO_PEERCRED` /
`getpeereid`) and closes the connection on a mismatch; Windows restricts the pipe's DACL to
the current user.

This boundary does **not** separate a human from an agent. An agent running as the same user
can do what the user can do. That is the premise of the whole design — the two are the same
user — so "restrict it because it came through MCP" cannot be enforced here.

### 2.4 Handshake

The first exchange on every connection:

```
-> {"protocol": 1, "app": "sapphire-journal", "client": {"kind": "cli", "version": "0.3.1", "pid": 4312}}
<- {"protocol": 1, "server": {"version": "0.3.1", "pid": 991, "managed_by": "service"}}
```

`protocol` is a single integer for the framework IPC layer, bumped on a breaking change.
`managed_by` is `service` or `spawned`, and decides what happens on a mismatch (§2.6).

### 2.5 Runtime directory

`<platform data root>/sapphire/run/`, mode `0700`, overridden by `SAPPHIRE_RUNTIME_DIR`.

`$XDG_RUNTIME_DIR` and `/run/user/<uid>` are deliberately **not** used. A server running as
a service has neither, while a CLI in a login session has both — the two would resolve
different paths and never meet. A persistent directory is the same for both. The cost is a
socket file surviving a reboot, which §2.6 already handles as a stale socket.

Unix sockets do not work on an NFS home directory. This is the same constraint the sync spec
records for its node directory.

### 2.6 Starting the server

```
1. connect to the socket
2. connected -> handshake
     versions match               -> use it
     mismatch, managed_by=spawned -> request shutdown, wait, go to 3
     mismatch, managed_by=service -> fail: "the installed service is version X, this CLI is
                                     version Y; restart the service" (never kill it)
3. not connected
   a. take <runtime dir>/<app>.spawn.lock exclusively (wait up to 10 s)
   b. having taken it, try to connect again      <- the loser of a race joins here
   c. still nothing -> spawn current_exe() with the server subcommand, detached
   d. wait for the socket with bounded backoff (up to 10 s)
   e. release the lock
4. a stale socket (ECONNREFUSED on Unix) is unlinked; go to 3
```

Steps 3a-3b are what makes a simultaneous start by several CLIs produce exactly one server.

**Idle exit** defaults to 15 minutes and is disabled when the process was started as a
service. Idle exit does not strand sync: the bridge can start a stopped owner when a peer
asks for its workspace (§5, `wake_on_sync`), so one start-on-demand mechanism covers all
three tiers.

## 3. Privilege separation (removed — issue #145)

Privilege separation was removed (`sapphire-agent` #257 withdrawn). Per-node permission
separation had to be enforced on every syncing node — hardest on Windows — and one node
missing it re-opened the injection hole, so the mechanism paid per-OS maintenance for
partial protection. Shell / generic fs tools are restricted by policy instead: admin
devices and admin rooms only (a `sapphire-agent` concern).

Two of the removed design's consequences for the whole framework survive:

1. **Every file and directory the framework creates is `0700` / `0600`.** The rule that
   exists today for `keys.toml` is generalised. It stands because it protects the workspace,
   the cache and the keys whatever runs the server, so the rule admits no exceptions.
2. **`ServiceSpec` carries no `run_as` / `helper_as` anymore.** Every install is a per-user
   unit and the app runs as the user who installed it.

## 4. App server (`sapphire-framework-server`)

The skeleton an app builds on. The app writes namespace handlers; everything else is here.

```rust
AppServer::new(ctx)                          // AppContext: app name, directory resolution
    .namespace("journal", journal_handlers)  // app-specific methods
    .http(router)                            // optional: /mcp, /acp, /a2a, protected by -keys
    .run()
```

### 4.1 The `workspace.*` namespace

`WorkspaceBackend` mapped onto JSON-RPC. Every request carries `ws`, the workspace root, so
requests do not depend on connection state.

| Method | Notes |
|---|---|
| `workspace.search` / `read_file` / `write_file` / `append_file` / `delete_file` / `list_dir` | one-to-one with `WorkspaceBackend` |
| `workspace.reindex` | **renamed** from `WorkspaceBackend::sync()`, which means "rebuild the index from disk" and otherwise reads as peer-to-peer sync |
| `workspace.subscribe` | `workspace.event` notifications follow, per connection |
| `sync.enable` / `sync.disable` / `sync.status` | workspace-level registration, relayed to the bridge |
| `server.info` / `server.shutdown` | used by the handshake and by §2.6 |

### 4.2 Responsibilities

- **Many workspaces**: a `WorkspaceState` per root, closed on an LRU / idle policy.
- **Sync runtime**: a watcher and a `Replica` per registered root. The sync spec's `SyncNode`
  lives **here**, not in the bridge.
- **Client side**: `-backend` gains `IpcBackend: WorkspaceBackend`. The journal desktop
  becomes an IPC client by swapping its backend implementation. The sync spec's
  `SyncedBackend` is not needed; `LocalBackend` becomes an implementation detail of this
  crate.

### 4.3 Talking to the bridge: a control plane and a data plane

```
app server --[control: one JSON-RPC connection]--> bridge
   bridge.register {app_name, exe_path, managed_by, workspaces: [{workspace_id, root}]}
   bridge.unregister / bridge.peers / bridge.status

app server ==[data: N raw connections]==> bridge ==iroh stream==> the peer's app server
   one header line {workspace_id, device_id}, then bytes
```

The data plane is not JSON-RPC because putting it there would re-wrap a QUIC stream as base64
inside JSON. Only the control plane — registration, authorization, status — needs structure.

So **the replication protocol runs end to end between two app servers, and the bridge is a
switchboard.** `-sync` asks only for a bidirectional stream, which is exactly what this
provides. On the receiving side the bridge maps `workspace_id` to its owner and, if the owner
is not running, starts it first.

### 4.4 CLI

`ServerCommand`, flattened into the app's clap tree: `run`, `status`, `stop`,
`service install | uninstall | status`.

## 5. Bridge (`sapphire-framework-bridge`, binary `apps/sapphire-bridge/`)

One process per OS user. It knows no app and never looks inside a workspace.

```
<platform data root>/sapphire/           # override: SAPPHIRE_BRIDGE_DIR
    bridge/
        format                           # directory format version
        node.key                         # iroh secret key -> this device's NodeId
        bridge.lock                      # single-instance guard, not a role election
        net.toml                         # discovery, relays, embedded relay, wake_on_sync
        routes.toml                      # workspace_id -> {app_name, exe_path, managed_by, root}
        invites.toml                     # pending pairing invites
        status.json                      # runtime state
        logs/bridge.log
        workgroups/<workgroup-id>/
            replica/sync.redb            # the workgroup workspace's own replica
            root/                        # device list, workspace list
    run/                                 # §2.5
        bridge.sock
        <app>.sock
```

`bridge.lock` changed meaning from the sync spec's §3.3. There it elected who would run the
node; here it only prevents a second bridge. **A process that fails to take it connects to
the existing socket and is done** — no handoff, no holder change, no follower state.

Its jobs:

1. **Identity** — `node.key` / `NodeId`, workgroup membership, issuing and accepting pairing
   invites.
2. **Authorization** — whether a connecting peer's `NodeId` belongs to the workgroup. Only
   the bridge can decide this; an app server holds nothing to decide it with.
3. **Switchboard** — resolve `workspace_id` to its owner and splice the iroh stream to that
   owner's data connection, starting the owner from `routes.toml`'s `exe_path` if it is not
   running (`wake_on_sync`, on by default). An app that wants its workspaces always reachable
   without depending on an incoming sync request installs its server as a service.
4. **iroh** — endpoint, discovery, relay configuration, the `embedded-relay` feature.
5. **App server of the workgroup workspace** — per §1, using `-sync` unchanged.

**CLI (`sapphire-bridge`)**: no subcommand or `run`; `status`; `log [--follow]`;
`pair create | accept | list`; `device list | forget`; `workgroup create | join | list`;
`workspace list` (what the workgroup contains — read-only); `service install | uninstall |
status`.

`workspace list` is read-only by the rule in §1: the bridge knows what exists in the
workgroup, the app server decides what this host keeps. Placing a workspace on this host or
enabling sync for it is the owning app's CLI (`journal sync map <name> <dir>`,
`journal sync enable`).

## 6. Crate layout

| Crate | Status | Role |
|---|---|---|
| `sapphire-framework-ipc` | new | transports, NDJSON framing, JSON-RPC router, handshake, start-on-demand |
| `sapphire-framework-server` | new | app server skeleton, `workspace.*`, many workspaces, `SyncNode` and watcher, `ServerCommand` |
| `sapphire-framework-bridge` | new | iroh endpoint, workgroup authorization, pairing, switchboard, `routes.toml` |
| `apps/sapphire-bridge/` | new | the binary |
| `sapphire-framework-keys` | as the sync spec | for non-sync HTTP endpoints |
| `sapphire-framework-service` | as the sync spec | `service install` — one per-user unit per platform |
| `sapphire-framework-net` | **never created** | iroh side goes to `-bridge`; `SyncNode` and watcher to `-server` |
| `-backend` | changed | `IpcBackend` added; `RemoteBackend` and the planned `SyncedBackend` removed; `LocalBackend` becomes internal to `-server` |
| `-workspace` | changed | per-kind directories removed (§7); `AppKind` no longer resolves paths |
| `-sync`, `-track`, `-retrieve`, `-registry` | unchanged | `-registry` simply lives behind the bridge |
| `-rpc`, `-remote-client`, `-remote-server`, `-blob` | removed | as the sync spec |

Facade features become `ipc` / `server` / `bridge` / `sync` / `keys`.

**`-server` must not pull in iroh.** The control-plane types and the client for talking to
the bridge live behind a dependency-light `client` feature of `-bridge` (serde only, no
iroh), which is what `-server` depends on — the same separation `-rpc` gave the old remote
client and server. Building an app server therefore costs nothing from the networking stack,
and `sapphire-sync`, which has no search index, keeps verifying that `-sync` and `-bridge`
stay clear of `-workspace` and `-retrieve`.

## 7. Directory layout: reverting the per-kind split

#129 split cache, data and config into `<platform root>/<app>/<kind>/`. The split existed to
stop two kinds from opening one database. This design removes the collision itself, so the
split has no remaining purpose: only the server opens a database.

- Cache returns to `<platform cache root>/<app>/<uuid>/`; data and config to
  `<platform data root>/<app>/` and `<platform config root>/<app>/`.
- `AppKind` survives as a description of what a process does. It no longer appears in a path.
- Environment overrides keep the #129 names (`SAPPHIRE_<APP>_<CATEGORY>_DIR`), and what they
  replace is still the platform root — there is simply no `<kind>` segment left to add.
- **A second one-shot migration** moves `<app>/<kind>/<uuid>/` back to `<app>/<uuid>/`, and
  `<app>/<kind>/<uuid>/keys.toml` to `<app>/<uuid>/keys.toml`. It runs inside `init`, is
  idempotent, deletes nothing, and refuses to overwrite an existing destination. Where two
  kinds both left a cache for one workspace, the server's copy wins and the others stay in
  place for the user to delete; a cache is rebuildable, so nothing is lost either way.

Coming so soon after #129, this is a real cost and is listed as such in §12.

## 8. Revisions to existing specs

**`2026-09-15-p2p-sync-iroh-design.md`.** §2 and §6.1 are untouched and remain the authority
on replication. §3–§5 and §6.2–§6.3 are **revised in place, not deleted**: they carry a
substitution table at their head saying how to read them under this architecture (the node
becomes the bridge, followers disappear, `SyncNode` moves to the app server, file-based IPC
becomes a socket, `-net` splits). Their surviving material — the node directory's contents,
`sync-id` and workspace registration, selectors, the missing-root guard, workgroups and
authorization, pairing, protocols and sessions, `net.toml`, dialing and live propagation, app
fix-ups, app migration requirements, the compatibility policy and service installation —
stands, attached to the bridge or the app server. Only §5.6's implementation order is
replaced outright, by §9 below. In the decision list, 8, 11, 13 and 14 gain a note saying
what superseded them:

| Sync-spec decision | Superseded by |
|---|---|
| 8 — one node per host, run by whichever process holds the lock; files-only IPC | §1, §5: one bridge process; real IPC |
| 11 — `service install` per app | still true; the §3 extension (`run_as` / `helper_as`) was itself removed (issue #145) |
| 13 — `sapphire-sync` as the always-on peer and dedicated sync service | decision 4 above: the bridge takes both roles |
| 14 — guardrails for embedding the node in an app | §10: the bridge is a separate process, so isolation is structural |

Specifically dropped from it: the holder / follower election and lock retry (§3.3), the
`embedded_node` switch, files-only inter-process communication, the shared sync log with
holder-change lines, `NodeCommand` embedded in every app's CLI, `workspaces.toml`, and the
`-net` crate.

**`2026-09-15-sapphire-sync-design.md`.** `sapphire-sync` stops being a core component. It
remains the reference implementation and E2E test bed (its role 1) and a Syncthing-like file
sync product — an app on the bridge, the smallest possible app server, with no search stack.
Its roles 2 and 3 move to the bridge, and the "versioned in lockstep because it is core" note
moves with them.

**App migrations** (journal, ledger, timer, agent) get their own specs in their own
repositories, per `CLAUDE.md`.

## 9. Implementation order

```
 1. [done] Sync core
 2. Registry                    users removed, one file per record, node_id
 3. IPC layer (-ipc)            verified on its own, without an app
 4. App server skeleton         workspace.*, many workspaces, IpcBackend, ServerCommand
                                *** the original problem is solved here ***
 5. Privilege separation        removed (issue #145)
 6. Bridge basics               directory, single instance, control plane, data plane, iroh
 7. Sync runtime in the server  watcher, Replica, sync.enable
 8. Pairing and workgroups
 9. Server features             embedded relay, wake_on_sync
10. Service                     per-user unit per platform
11. Cleanup                     extract keys; remove rpc / remote-* / blob; §7 migration;
                                facade features; rewrite ARCHITECTURE.md
```

**Step 4 is the first release-worthy milestone.** With no sync and no bridge yet,
`journal add` and the stdio MCP server stop colliding, which is the problem this work started
from. Everything after it rebuilds sync on the new layering.

`sapphire-sync` starts as an E2E harness from step 6.

## 10. Failure model

| Situation | Result |
|---|---|
| Bridge not running or crashed | **The app server works completely**; only sync stops. It reconnects with backoff and `sync.status` reports the bridge as unavailable |
| App server not running | The CLI reports that the service must be started. An incoming sync request makes the bridge start it |
| Version mismatch | §2.6: replace a spawned server, report an installed service |
| Crash during a write | `-sync`'s staging and interrupted-write recovery already cover it; unchanged |
| Stale socket after a reboot | Unlinked on `ECONNREFUSED` (§2.6) |

The sync spec's decision 14 — a node failure must never take the app down — holds
structurally here rather than through a guard, because the bridge is a separate process.

## 11. Testing

**`-ipc`**: `ChannelTransport` round trips; real Unix sockets and named pipes; several
in-flight requests on one connection; handshake mismatch in both `managed_by` modes; **a
start race — N processes at once must produce exactly one server**; a stale socket; rejection
of a connection from another uid.

**`-server`**: **two CLI processes writing concurrently must both succeed** — the regression
test for the problem this spec exists to fix; many workspaces in one server; idle eviction;
`workspace.subscribe` delivering events to several connections.

**`-bridge`**: the single-instance lock; routing a workspace to its owner; `wake_on_sync`
starting a stopped owner; an unknown `NodeId` rejected before reaching any app server.

**End to end**: two bridges with separate directories inside one test process, standing in for
two hosts, with `sapphire-sync` as the app.

## 12. Risks

1. **A single CLI invocation gets slower.** The first `journal add` waits for the server to
   start and to open redb and tantivy. Later calls are faster than today because the server is
   warm, but a one-shot invocation is a straight regression. This is accepted, not mitigated.
2. **A second one-shot directory migration**, arriving shortly after #129's.
3. **Unix sockets do not work on an NFS home directory** (§2.5).
4. **More processes on a busy host**: a bridge plus one server per app.
