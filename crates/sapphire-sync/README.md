# sapphire-sync

Git-based workspace synchronisation library for [sapphire-journal](https://github.com/fluo10/sapphire-journal).

## What this crate provides

- **`SyncBackend` trait** — abstraction over sync strategies
- **Git backend** *(feature: `git`)* — commit, pull, and push changes via `git2`
- **Config types** — `SyncConfig` and `SyncBackendKind` in `sapphire_sync::config`

## Features

| Feature | Default | Description |
|---|---|---|
| `git` | yes | Git-based sync via `git2` |

## Configuration

```toml
[sync]
backend  = "auto"   # "auto" | "git" | "none"
remote   = "origin"
branch   = "main"
```

`backend = "auto"` (default) enables git sync when the workspace is inside a git repository and falls back to no-op otherwise.

## License

MIT OR Apache-2.0
