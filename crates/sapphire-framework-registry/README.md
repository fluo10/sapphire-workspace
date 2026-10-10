# sapphire-framework-registry

The per-app device ledger. One directory, one record file per device —
`<dir>/<grain-id>.toml` — read and written through `Devices`.

```rust
use sapphire_framework::registry::Devices;

let mut devices = Devices::open(&workgroup_dir.join("devices"))?;
let pendant = devices.add("pendant", None, Some("首から下げるやつ".into()))?;
println!("{}", pendant.id); // e.g. "a3f9k2p"
```

## Why one file per device

Each mutation rewrites exactly one record file, so two hosts mutating the
ledger at the same moment never collide — they write different files. The
id is the file name and is never repeated inside the file.

## IDs close inside the app

A `Device` id means something only inside that app's ledger; apps do not
share ids. sapphire-journal / sapphire-ledger / sapphire-agent each appear
to one another as a single client device, so there is nothing to align.

`device.id` is **persisted into content** (a journal entry's `updated_by`,
say). So removal is a tombstone (`retired_at`) by default, and a record is
deleted physically only by an explicit `purge`.

## External devices

`ExternalDevices` is the ledger of clients that reach a workgroup's applications with a
key instead of syncing (#199): one record per file in `external_devices/`, beside
`devices/`, listing the applications each may use. A record stores the SHA-256 of its
token, never the token, which is shown once by `add` and `rotate`. `retire` stops the
token working and keeps the record; `restore` brings it back.

## Migration

`migrate_single_file` converts a legacy single-file `devices.toml` (with its
`[[device]]` tables and optional per-record `id` / `user_id` fields) into
per-device record files, idempotently and without touching the old file.
