# External devices: API-key clients of a workgroup's applications

Issue: #199. Related: #92 (labelled keys, editor records), #103 (`KeyStore` split out),
#104 (rotate keeping the id), #117 (no way back from retire).

## Why

Devices sync over p2p and need no key. Some clients cannot or should not sync and talk HTTP
only: a recording pendant posting to sapphire-agent's `/audio/ingest`, an ACP client driving
sapphire-agent from a machine that does not hold the workspace, a phone shortcut, a webhook.
They need an identity that can be listed, retired, restored and rotated — like a device —
and that every host of the workgroup accepts, so a key is set up once rather than on each
headless server. Today each server keeps a `KeyStore` of raw tokens in its own config.

## Decisions

1. **Name: external device.** It sits beside `device` in the ledger, the CLI and the UI.
2. **A synced ledger beside the devices:**
   `<workgroup root>/external_devices/<grain-id>.toml`, one record per file, so two hosts
   editing at once never collide (as `devices/`). The bridge owns it, as it owns the device
   ledger.
3. **Scoped by application, not partitioned by it.** A record lists the applications it may
   use (`apps = ["sapphire-agent"]`); one external device may use several, as one device
   runs several. An empty list allows nothing; there is no "every app" wildcard, so a key
   never gains access to an application installed later.
4. **Only a hash is stored.** The token is 32 random bytes, shown once —
   `sapphire-ed-<base64url>` — and the record keeps its SHA-256 (hex). A random 256-bit
   token needs no slow hash. Comparison is constant-time.
5. **Operations:** `add` (prints the token once), `list`, `retire` (the record stays: an
   id may be referenced forever), `restore` (#117 not repeated), `rotate` (a new token, the
   same id, the old token dead at once; #104), and `apps` (set the application list).
6. **Authentication goes through the bridge.** An app server's HTTP layer sends the token
   and its own app name to `external_device.authenticate`; the bridge answers with the
   external device's id and name, or refuses. A success is cached for 30 seconds (keyed by
   the token's hash), so a request costs no IPC most of the time; a retire, a rotate or an
   app removed reaches every host within the sync delay plus 30 s. When the bridge cannot
   be reached the layer refuses with 503 — closed, never open.
7. **`KeyStore` is removed**, with `KeyEntry`, `AuthConfig` and the raw-token key file.
   Existing keys are re-created as external devices. Nothing has used them in a release
   that syncs.
8. **Editors are not recorded yet.** The layer puts `Authenticated { id, name }` into the
   request; recording it in synced versions needs a sync format change and is its own
   issue.

## Record

```toml
# A sapphire external device: a client that reaches the workgroup's apps with a key.
name        = "pendant"
description = "the recorder on my coat"
apps        = ["sapphire-agent"]
token_sha256 = "<64 hex digits>"
created_at  = 2026-10-11T09:00:00Z
rotated_at  = 2026-10-12T09:00:00Z   # optional
retired_at  = 2026-10-13T09:00:00Z   # optional
```

`name` is unique among external devices and accepted wherever an id is.

## Components

- **registry:** `external_devices` — `ExternalDevice`, `ExternalDevices` (open, add, resolve,
  retire, restore, rotate, set_apps, authenticate), `Token` (generate, hash). `sha2` and
  `getrandom` added.
- **bridge-api (2.3.0, unreleased, additive):** `external_device.list`, `.add`, `.retire`,
  `.restore`, `.rotate`, `.set_apps`, `.authenticate`, their types, client calls, and the
  `external-device` subcommands in the `cli` feature.
- **bridge:** `Workgroup::external_devices()`; the control-plane handlers; the CLI, which
  without a running bridge works on the ledger directly (as `device retire` does).
- **keys:** rewritten around the bridge: `protect(verifier, app, router)` with a
  `Verifier` trait, `BridgeVerifier` (the IPC call and the cache), `Authenticated`. Keeps the
  fail-closed behaviour and the named test bypass.
- **server:** `FrameworkCommand::ExternalDevice`; `add` defaults `--app` to the application
  running it.
- **gui:** an External devices screen: the list, add (the token shown once, with Copy),
  retire / restore, rotate, and the application list.

## Testing

- registry: a round trip of the record; the token is never stored; authenticate by app,
  refusing a retired device, an unlisted app, an old token after rotate; names unique.
- bridge: each RPC; authenticate across a rotate and a retire.
- keys: the layer with a fake verifier — 401 without or with a wrong token, 503 when the
  verifier is unreachable, the cache, the extension set.
- CLI: `add` prints the token once and `list` never does.
