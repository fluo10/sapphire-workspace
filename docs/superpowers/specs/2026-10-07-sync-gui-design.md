# Sync GUI: shared workgroup / device / workspace components, and the sapphire-sync desktop app

- Date: 2026-10-07
- Scope: `sapphire-framework-gui` (new `client` and `views` layers, `SyncPanel`),
  `sapphire-framework-bridge-api` (two control-plane methods, own version),
  `sapphire-framework-bridge` (the methods' implementation, CLI sharing it),
  `sapphire-framework-backend` + `sapphire-framework-server` (host workspace registry,
  `workspace.list`, `workspace.forget`), and a new `desktop` crate in the sapphire-sync
  repository.
- Related: [`2026-09-24-app-command-system-design.md`](./2026-09-24-app-command-system-design.md)
  (the CLI vocabulary this GUI mirrors), [`2026-09-16-process-architecture-design.md`](./2026-09-16-process-architecture-design.md)
  (always-on servers, one `serve` per host).

## Background

Cross-OS, cross-host sync with sapphire-sync works. Everything it offers is CLI-only:
`workgroup create|list|join`, `device list|invite|retire`, `workspace init|list|map`,
`sync disable`. The next step is a desktop GUI. Mobile is out of scope.

The workgroup / device / workspace screens are not sapphire-sync's alone — journal and the
other apps will need the same screens — so the components live in the framework, in the
existing `sapphire-framework-gui` crate (egui 0.36, no eframe). That crate already carries
`WorkspaceManager`, a generalised form of journal's journal-list screen
(`sapphire-journal/desktop/src/screens/journal_list.rs`); its look (header with action
buttons, `Frame::group` rows, dismissible error line, type-the-name confirmation) is the
visual reference for the new views.

Facts from the code that shape the design:

- The GUI talks to two processes: the **bridge** control plane (`BridgeClient`: `status`,
  `peers`, `invite`, `join`, `workspaces`) and the **app server** IPC (`workspace.init`,
  `sync.enable|disable|status|map`, `server.info`).
- One workgroup per host in this release (`control.rs` rejects any other selector).
- `workgroup create` and `device retire` have no control-plane method: the bridge CLI edits
  the bridge directory directly, and the framework CLI only prints `run: sapphire-bridge …`.
- There is no host-wide list of an app's workspaces. The registry (`[workspace.<id>]`) and
  the sync id live inside each workspace's marker; the server's synced roots are in memory
  (`SyncRuntime.synced`); the CLI's `workspace list` walks up from the current directory.
  A GUI has no current directory.

## Decisions

Agreed during brainstorming on 2026-10-07:

1. **The GUI is a pure client.** It connects to the service-managed `sapphire-sync serve`
   and `sapphire-bridge` over IPC, exactly like the one-shot CLI verbs, and never starts
   either in-process. Closing the GUI does not stop sync. An absent process is a state the
   GUI shows, with a button to install/start the service.
2. **`sapphire-framework-gui` stays in the facade** (`gui` feature) on the shared workspace
   version. Apps run on git branch dependencies today, so independent GUI releases would buy
   nothing, and the GUI's correctness is tied to the protocol types that move with the
   workspace. The crate adds `pub use egui;` so an app reaches egui through the framework and
   its egui version cannot drift from the GUI crate's. Revisit (out of the facade *and* on an
   own version, together) when publishing to crates.io or when egui bumps become a burden.
3. **Workgroup create and device retire become control-plane methods**
   (`bridge.workgroup_create`, `bridge.device_retire`). The GUI stays a pure client; the
   bridge CLI and the framework CLI both go through the same implementation.
4. **The app server keeps a host workspace registry** and serves it as `workspace.list`.
   The GUI and the CLI see the same list.
5. **`sapphire-framework-gui` gets two layers plus an assembled panel**: an egui-free
   `client` layer, pure `views`, and `SyncPanel` wiring them. sapphire-sync places
   `SyncPanel`; other apps embed individual views.
6. **`sapphire-framework-bridge-api` carries its own version, whose major is the
   control-plane `API_VERSION`.** It starts at `2.0.0`; `API_VERSION` is derived from
   `CARGO_PKG_VERSION_MAJOR` so the two cannot drift.

## Architecture

```
sapphire-sync-desktop (eframe binary, sapphire-sync repo)
  └─ sapphire_framework::gui
       ├─ client::FrameworkClient ── IPC ──> sapphire-bridge      (bridge.*)
       │                          └── IPC ──> sapphire-sync serve (server.info, workspace.*, sync.*)
       ├─ views::{ServiceStatusBanner, WorkgroupView, DeviceList, InviteDialog,
       │          JoinDialog, WorkspaceList}
       └─ SyncPanel  (banner + left navigation: Workspaces / Devices / Workgroup)
```

Existing `WorkspaceManager` is kept unchanged. Migrating journal onto the new views is out
of scope.

## Framework additions

### Bridge control plane (`sapphire-framework-bridge-api`, `-bridge`)

| Method | Params → Result | Rules |
|---|---|---|
| `bridge.workgroup_create` | `{ name, device_name }` → `{ workgroup_id, name, device_id }` | Error when this host already belongs to a workgroup. The running bridge adopts the new workgroup in-process, the same way `bridge.join` does today. |
| `bridge.device_retire` | `{ selector }` → `{ device_id, name }` | Selector is name or id. Retiring this host's own device is refused. |

`BridgeClient` gains `workgroup_create` and `device_retire`. The bridge CLI's
`workgroup create` / `device retire` keep working on a stopped bridge (they edit the
directory) but share the implementation with the control-plane handlers; when the bridge is
running they go through the control plane so the running process is the single writer.
The framework CLI's `workgroup create` / `device retire` stop printing `run: sapphire-bridge …`
and call the control plane.

**Versioning.** `Cargo.toml` of `sapphire-framework-bridge-api` sets `version = "2.0.0"`
instead of `version.workspace`. `API_VERSION` becomes a const parsed from
`env!("CARGO_PKG_VERSION_MAJOR")` by a `const fn`. Dependents (`-bridge`, `-server`,
sapphire-sync) require `version = "2"`. In `release-plz.toml` the crate is a standalone
package outside `version_group = "framework"`. Accepted trade-off: a Rust-API-breaking but
wire-compatible change bumps the major and therefore fails the handshake between versions;
this errs on the safe side and matches the lockstep update practice.

### App server (`sapphire-framework-backend::protocol`, `-server`)

- **Host workspace registry**: `<app config dir>/workspaces.toml`, its own small format
  (`[workspace.<id>] root, name?, synced`) rather than `WorkspaceRegistry`, because it
  carries the `synced` flag restart restore needs. The server appends on `workspace.init`
  and `sync.map` (idempotent: an existing root keeps its id) and flips `synced` in
  `SyncRuntime::enable` / `disable`. The per-marker registry stays as it is.
- **`workspace.list`** → `{ workspaces: Vec<WorkspaceListEntry> }`, where
  `WorkspaceListEntry = { id, name: Option<String>, root, reachable: bool,
  workspace_id: Option<GrainId>, sync: SyncStatusResult }`. `reachable` is "the marker
  directory exists"; `workspace_id` is the marker's sync id when one has been minted
  (read, never minted by listing), so a disabled-but-known workspace still matches its
  ledger row; `sync` is the existing `sync.status` shape (`enabled = false` when not synced
  or when the server has no sync runtime).
- **`StatusReport` / `StatusRow`** move from `-server` to `backend::protocol` (re-exported
  from `-server` at the old path) so the GUI can decode `server.info` without linking the
  server crate.
- **`workspace.forget`** `{ id }` → `{}`: disable sync if enabled, then remove the row.
  Files are never touched.
- **Restart restore**: established while planning — nothing restores synced roots today
  (`SyncRuntime.synced` starts empty and the server never reads the bridge's `routes.toml`
  back). `serve` therefore re-enables every registry row with `synced = true` once it is
  listening, best-effort per row.
- **Adding an existing folder** reuses `workspace.init` (already idempotent); no new method.
- Backend `API_VERSION` goes 1 → 2.

Both `API_VERSION` bumps mean every CLI, server and bridge on a host is replaced and
restarted together.

## GUI layer (`sapphire-framework-gui`)

### `client` (egui-free)

```rust
let client = FrameworkClient::spawn(
    runtime: tokio::runtime::Handle,
    app: AppIdentity { app_name: &'static str, version: &'static str },
    repaint: Arc<dyn Fn() + Send + Sync>,
);
client.snapshot() -> watch::Ref<'_, Snapshot>;
client.send(Command) -> CommandId;
client.drain_outcomes() -> Vec<Outcome>; // Outcome { id, result: Result<CommandOutput, String> }
```

- `Snapshot { bridge: Conn<BridgeState>, server: Conn<ServerState>, fetched_at }`.
  `Conn<T> = Absent | Incompatible(String) | Error(String) | Up(T)`.
  `BridgeState = { status: StatusResult, peers: Vec<PeerInfo>, ledger: Vec<WorkgroupWorkspaceInfo> }`.
  `ServerState = { info: StatusReport, workspaces: Vec<WorkspaceListEntry> }`.
- `Command = WorkgroupCreate{..} | WorkgroupJoin{..} | DeviceInvite{..} | DeviceRetire{..}
  | WorkspaceInit{dir, sync} | SyncEnable{root} | SyncDisable{root} | WorkspaceMap{workspace_id, dir}
  | WorkspaceForget{id} | ServiceStart{target}`.
  `WorkspaceMap` on a directory that is not yet a workspace runs `workspace.init` first.
- A background task refreshes every ~2 s and right after each command completes. It keeps
  one connection per process and reconnects on the next tick after a failure. Each change
  publishes a new `Snapshot` on a `watch` channel and calls `repaint`.
- Service start: the app supplies, per target (`bridge`, `server`), the executable to
  register — by default the sibling `sapphire-bridge` / `<app>` binary next to the GUI's
  own executable — and the client runs that binary's own `service install` verb as a child
  process. (The framework's `install()` registers `current_exe()`, which inside the GUI
  would be the GUI; running the CLI's verb registers the right binary with no API change.)
  A missing binary or a failing install yields an outcome error carrying the command line
  to run by hand.

### `views` (pure rendering)

Each view takes `&Snapshot` plus its own transient UI state, and returns `Option<Command>`
(or `Vec<Command>`). Pending command ids are held in view state to disable buttons and
show a spinner. Decision logic is factored into pure functions for unit tests.

- **ServiceStatusBanner** — bridge and server each "running vX.Y" / "not running" /
  "incompatible: <handshake message>". A single thin line when both are up. Not running →
  [Install & start service]; on failure, the command to run with a [Copy] button.
- **WorkgroupView** — not joined: two cards, "Create" (workgroup name, this device's name)
  and "Join with ticket" (ticket, device name defaulting to the host name). Joined: name,
  id (copyable), device count, this device's name.
- **DeviceList** — rows: name, short id, online indicator, [Retire] (absent on this host's
  own row; type-the-name confirmation). Header [Invite…] opens **InviteDialog**: name,
  TTL (1 h / 24 h / 7 d) → ticket in monospace with [Copy]. **JoinDialog** is the join card
  as a dialog for embedding elsewhere.
- **WorkspaceList** —
  - *This host* (`workspace.list`): name, root, state badge (syncing · n peers / paused:
    reason / error / not synced / unreachable), [Start/Stop sync] [Open folder]
    [Remove from list]. Header [New…] (pick folder → init) and [Add existing…].
  - *In the workgroup, not on this host*: ledger rows with `app_name` equal to this app,
    minus workspace ids already present locally. [Bring to this host…] → pick folder →
    `WorkspaceMap`.
- Views that need a process that is `Absent` render disabled with a one-line hint.

### `SyncPanel`

Owns a `FrameworkClient` and the views; renders the banner on top and a left navigation
(Workspaces / Devices / Workgroup). sapphire-sync's desktop app is essentially
`SyncPanel::ui(ui)` inside an eframe shell.

### `fonts` (added 2026-10-07)

No font assets are bundled in any repository or binary. `fonts::system_cjk_font()` scans the
OS font directories with `fontdb` (pure Rust; on Linux it follows fontconfig's configuration
without linking libfontconfig) and picks, in order, a preferred family with Japanese glyph
forms (Windows: Yu Gothic UI / Yu Gothic / Meiryo UI / Meiryo / MS Gothic; macOS: Hiragino
Sans / Hiragino Kaku Gothic ProN; Linux: Noto Sans CJK JP / Noto Sans JP / Source Han Sans JP /
IPAexGothic / IPAGothic / …), verified with `ttf-parser` to contain `あ` and `漢`; failing
those, any face that does. `.ttc` face indices map to egui's `FontData.index`.
`fonts::install_system_cjk_fallback(ctx)` registers it behind egui's defaults for both
families and returns the family used; with no such font it logs a warning and leaves egui's
defaults (CJK renders as boxes; nothing fails).

## sapphire-sync desktop crate

New `desktop` member in the sapphire-sync workspace (`sapphire-sync-desktop`, binary
`sapphire-sync-desktop`). eframe 0.36 shell, a multi-thread tokio runtime whose handle goes
to `FrameworkClient`, and `SyncPanel`. Journal's `gpu.rs` (device-lost handling on RDP
reconnect) is copied in, relicensed to `MIT OR Apache-2.0` with the author's consent; fonts
come from the framework's `fonts` module. Moving `gpu.rs` into the framework is a candidate
for later, not part of this work.

## Error handling

- Process absent → `Conn::Absent` → banner "not running" + start button; dependent views
  disabled with a hint.
- API mismatch → `Conn::Incompatible` → banner shows the handshake's message ("update and
  restart …").
- Command failure inside a dialog → inline error; the dialog stays open. Failure of an
  action outside a dialog (sync toggle, remove) → dismissible error line at the top of the
  view, the `WorkspaceManager` pattern.
- Invite tickets are never logged.

## Testing

- **Bridge**: through the existing testing harness — `workgroup_create` errors when already
  joined and is visible in `status` immediately; `device_retire` refuses this host's own
  device and removes a retired device from `peers`; a test pins
  `API_VERSION == CARGO_PKG_VERSION_MAJOR`.
- **Server**: registry append on init and map (idempotent), `workspace.list` shape and
  sync state, `workspace.forget` disables then removes, restart restore.
- **Client**: integration tests against the server crate's `test-util` `StubBridge` and an
  `AppServer` on a temporary IPC endpoint — `Absent` → `Up` transition, a round trip for
  each `Command`.
- **Views**: unit tests for the pure functions (ledger minus local, badge selection, button
  enablement); headless `egui::Context::run` smoke tests rendering sample snapshots.
- **End to end (manual, two hosts)**: create → invite → join → new workspace → bring to the
  other host → files sync → retire.

## Out of scope

- Mobile.
- Multiple workgroups per host (views show the single workgroup).
- Migrating journal / timer / ledger onto the new views.
- Moving `gpu.rs` / `fonts.rs` into the framework.
- Taking `sapphire-framework-gui` out of the facade.
