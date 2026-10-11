# One workspace per app server

- Issue: #215
- Crates: `sapphire-framework-server`, `sapphire-framework-backend` (protocol, `IpcBackend`),
  `sapphire-framework-gui`
- Status: accepted

## Why

An app server today holds any number of workspaces: `WorkspaceHost` opens them on demand
(up to 8, closed after 5 idle minutes), every request names one (`ws: PathBuf`), the
`SyncRuntime` syncs each enabled one, and the host keeps a list of them
(`<config dir>/workspaces.toml`) for `workspace.list`, `workspace.forget` and the restore
on start. Clients pick the workspace per request: the CLI by `--workspace`, an env var or
the current directory, the GUI from the list.

That generality costs in every layer — every method and every client carries a root, the
CLI's answer depends on the directory it was run in, the GUI manages a list — and buys
little: most first-party apps (and Apple's) have no account switching, sapphire-agent
already serves exactly one workspace, and the workgroup is already one per bridge.

So: **one app server serves one workspace at a time.** Clients — the CLI, the GUI, MCP
and ACP endpoints — never name a workspace; they talk to the server, and the server knows
which one it is.

## Decisions

1. **The current workspace is the server's.** It lives in
   `<config dir>/workspace.toml`:

   ```toml
   root = "/home/me/notes"   # canonical
   sync = true               # whether sync is on for it
   ```

   No file means no workspace yet. The server opens the workspace at start and keeps it
   open (no idle close, no LRU).

2. **Switching is a server call, not a restart.** `workspace.select { dir }` closes the
   current workspace — its live sessions, its replica, its index — and opens the new one.
   A failed open leaves the old one in place. The process keeps running, so clients only
   see the next answer change.

3. **Sync follows the workspace that has it.** Selecting a workspace turns sync on if the
   workspace has a sync id (it has synced before, here or by `sync.map`), off otherwise.
   `sync.enable` / `sync.disable` change it for the current workspace and record it in
   `workspace.toml`. A disable is not remembered past a switch: switching back to a
   workspace with a sync id syncs it again. (Rare enough not to keep a per-workspace
   memory, which would be the list this design removes.)

4. **Only the current workspace syncs on this host.** The server registers only it with
   the bridge, so the bridge routes nothing else to this app, and `wake_on_sync` never
   starts it for another workspace — a peer's stream for one is refused as an unknown
   route, as for any workspace this host does not have. A workspace that is not selected
   does not catch up here, and this host is not a primary or secondary candidate for it.
   Running several servers (below) is the way to sync several.

5. **Methods drop `ws`.** Every `workspace.*` and `sync.*` method acts on the current
   workspace. With none selected they fail with `INVALID_PARAMS` and a message that says
   to run `<app> workspace select <dir>` (or `init`).

6. **The CLI does not follow the current directory.** File commands act on the server's
   workspace. When the current directory is inside *another* workspace of the same app
   (a `.<app>` marker on the way up that is not the server's root), the command fails and
   names both, rather than silently writing to a workspace the user is not looking at.
   Only `workspace init` and `workspace select` take a path.

7. **The GUI picks, it does not list.** The Workspaces screen shows the current workspace
   (path, sync state, start/stop sync, open folder) and three ways to change it: open an
   existing folder, create a new one, or bring one from the workgroup. The host-side list,
   "Remove from list" and `workspace.forget` go away.

8. **Several workspaces at once, later, means several servers.** Not now. When it comes,
   each server gets its own endpoint (socket path / pipe name) derived from the
   workspace's **id** — not its path, which moves — the way the cache directory is
   derived today.

## Protocol (`sapphire-framework-backend::protocol`), `API_VERSION` 3 → 4

- `PathParams`, `ContentParams`, `SearchParams` lose `ws`. `WsParams` is removed:
  `workspace.reindex`, `workspace.subscribe`, `sync.enable`, `sync.disable` and
  `sync.status` take `{}`.
- `workspace.init { dir }` creates the marker as before **and selects it**. Result
  unchanged.
- New `workspace.select { dir }` → `CurrentWorkspace`. `dir` must hold this app's marker.
- New `workspace.current` → `{ workspace: Option<CurrentWorkspace> }`.

  ```rust
  struct CurrentWorkspace {
      root: PathBuf,
      workspace_id: Option<GrainId>,   // read, never minted
      reachable: bool,                 // the marker is still there
      sync: SyncStatusResult,
  }
  ```

- `sync.map { workspace, dir }` writes the id into `dir` as before, then selects `dir`
  with sync on.
- `workspace.list`, `workspace.forget` and their types are removed.
- `workspace.event` keeps `ws` (the root the event came from), so a subscriber can tell an
  event of a workspace selected after it subscribed. A subscription outlives a switch: it
  follows whichever workspace is current.

## Server (`sapphire-framework-server`)

- `WorkspaceHost` holds at most one open workspace: `root()`, `backend()` (the current
  one), `select(root)`, `close()`. `DEFAULT_MAX_OPEN`, `DEFAULT_IDLE` and
  `AppServer::limits` are removed.
- A new `Current` owns the switch: it holds the host, the optional `SyncRuntime` and the
  selection file, serialises selections, and implements decisions 2–4. The `workspace.*`
  and `sync.*` methods go through it. `sync_router` takes it instead of the runtime.
- `SyncRuntime` keeps its per-root tables (holding at most one entry now) and loses the
  host-registry writes; `Current` records `sync` instead.
- Start: read `workspace.toml`; select its root; turn sync on if `sync`. A root that is
  gone is logged and left selected-but-unreachable, so `workspace.current` can say so.
- `HostRegistry`, `HostEntry`, `HOST_REGISTRY_FILE` and `listing.rs` are removed.
  **Migration:** with no `workspace.toml` and an old `workspaces.toml`, the first entry
  marked synced (else the first whose marker exists) becomes the current workspace, with
  its sync flag. The old file is left in place.
- `AppServer::selection_file(path)` overrides where `workspace.toml` lives — for fixtures
  that run several hosts in one process.

## Clients

- `IpcBackend::connect(endpoint, app, kind, version)` and `from_client(client)` lose the
  root. `IpcBackend::check_cwd()` implements decision 6 for an application's CLI.
- Framework CLI `workspace` verbs: `init [dir] [--sync]`, `select <dir>`, `show` (the
  current workspace, then the workgroup's workspaces of this app; replaces `list`),
  `map <selector> [dir]`.
- GUI: `ServerState.current: Option<CurrentWorkspace>` replaces `workspaces`;
  `Command::WorkspaceSelect` is added, `SyncEnable` / `SyncDisable` lose `root`,
  `WorkspaceForget` is removed; `WorkspaceList` becomes `WorkspacePicker`.

## Out of scope

- The older app-side `WorkspaceRegistry` (`[workspace.<id>]`) and the GUI crate's
  `WorkspaceManager`, which apps on the released framework still use. They become dead
  weight with this change and go in a follow-up.
- Several servers per app (decision 8).

## Testing

- Unit: the selection file round trip and migration from `workspaces.toml`; the sync rule
  on select (with and without a sync id); the cwd check.
- Server: methods without a selection fail with `INVALID_PARAMS`; `init` selects;
  `select` switches files, search and sync (the old workspace is unregistered from the
  bridge, the new one registered); a failed select keeps the old one; a subscription
  survives a switch; a restart restores the selection and its sync.
- The sync integration suites run unchanged in substance: each host selects its one
  workspace before enabling sync.
- GUI: the picker renders the current workspace, and with none; the client test drives
  init → sync → switch.
