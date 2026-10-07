//! `workspace.list`, `workspace.forget`, and the restore a starting server runs.

use std::path::PathBuf;
use std::sync::Arc;

use sapphire_backend::protocol as proto;
use sapphire_ipc::{Router, RpcError};
use sapphire_workspace::AppContext;

use crate::registry::HostRegistry;
use crate::sync::SyncRuntime;

/// Add [`proto::WORKSPACE_LIST`] and [`proto::WORKSPACE_FORGET`] to `router`.
pub(crate) fn listing_methods(
    ctx: &'static AppContext,
    sync: Option<Arc<SyncRuntime>>,
    router: Router,
) -> Router {
    let forget_sync = sync.clone();
    router
        .method(proto::WORKSPACE_LIST, move |_| {
            let sync = sync.clone();
            async move {
                let rows = HostRegistry::for_app(ctx)
                    .entries()
                    .map_err(|e| RpcError::internal(e.to_string()))?;
                let mut workspaces = Vec::with_capacity(rows.len());
                for (id, entry) in rows {
                    let marker = entry.root.join(format!(".{}", ctx.app_name));
                    // Read, never minted: listing must not create a sync id for a workspace
                    // nobody enabled.
                    let workspace_id =
                        std::fs::read_to_string(marker.join(crate::sync::SYNC_ID_FILE))
                            .ok()
                            .and_then(|s| s.trim().parse().ok());
                    let sync = match &sync {
                        Some(runtime) => runtime.status(&entry.root).await.into(),
                        None => proto::SyncStatusResult::not_synced(),
                    };
                    workspaces.push(proto::WorkspaceListEntry {
                        id,
                        name: entry.name,
                        reachable: marker.is_dir(),
                        root: entry.root,
                        workspace_id,
                        sync,
                    });
                }
                serde_json::to_value(proto::WorkspaceListResult { workspaces })
                    .map_err(|e| RpcError::internal(e.to_string()))
            }
        })
        .method(proto::WORKSPACE_FORGET, move |request| {
            let sync = forget_sync.clone();
            async move {
                let p: proto::WorkspaceForgetParams = serde_json::from_value(request.params)
                    .map_err(|e| RpcError::invalid_params(format!("bad parameters: {e}")))?;
                let registry = HostRegistry::for_app(ctx);
                let rows = registry
                    .entries()
                    .map_err(|e| RpcError::internal(e.to_string()))?;
                let Some((_, entry)) = rows.into_iter().find(|(id, _)| *id == p.id) else {
                    return Err(RpcError::invalid_params(format!(
                        "no workspace {} on this host",
                        p.id
                    )));
                };
                if let Some(runtime) = &sync {
                    runtime
                        .disable(&entry.root)
                        .await
                        .map_err(|e| crate::handlers::rpc_error(&e))?;
                }
                registry
                    .forget(&p.id)
                    .map_err(|e| RpcError::internal(e.to_string()))?;
                serde_json::to_value(proto::Ack {}).map_err(|e| RpcError::internal(e.to_string()))
            }
        })
}

/// Re-enable every workspace the registry says was synced. Best-effort per row: a root
/// that is gone, or fails to open, is logged and skipped so the others still come back.
pub(crate) async fn restore(ctx: &'static AppContext, runtime: Arc<SyncRuntime>) {
    restore_rows(&HostRegistry::for_app(ctx), |root| {
        let runtime = Arc::clone(&runtime);
        async move { runtime.enable(&root).await.map(|_| ()) }
    })
    .await;
}

/// [`restore`] over `registry`, with `enable` doing the enabling.
///
/// The registry is read again before each row: restore runs alongside the server's
/// methods, so a `sync.disable` or a `workspace.forget` may land while an earlier row is
/// being enabled. A row that is gone, or no longer synced, is skipped rather than enabled
/// again — which would turn a disable back on, or resurrect a forgotten row.
async fn restore_rows<F, Fut>(registry: &HostRegistry, mut enable: F)
where
    F: FnMut(PathBuf) -> Fut,
    Fut: std::future::Future<Output = crate::error::Result<()>>,
{
    let rows = match registry.entries() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!("could not read the host workspace registry: {err}");
            return;
        }
    };
    for (id, entry) in rows.into_iter().filter(|(_, e)| e.synced) {
        let still_synced = match registry.entries() {
            Ok(now) => now
                .iter()
                .any(|(i, e)| *i == id && e.root == entry.root && e.synced),
            Err(err) => {
                tracing::warn!("could not read the host workspace registry: {err}");
                return;
            }
        };
        if !still_synced {
            continue;
        }
        if let Err(err) = enable(entry.root.clone()).await {
            tracing::warn!(workspace = %id, root = %entry.root.display(), "could not restore sync: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[tokio::test]
    async fn restore_skips_rows_disabled_or_forgotten_while_it_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = HostRegistry::at(tmp.path().join("workspaces.toml"));
        let roots: Vec<PathBuf> = ["a", "b", "c"].iter().map(|n| tmp.path().join(n)).collect();
        for root in &roots {
            registry.set_synced(root, true, true).unwrap();
        }
        let c_id = registry.upsert(&roots[2]).unwrap();

        let enabled = Mutex::new(Vec::new());
        restore_rows(&registry, |root| {
            enabled.lock().unwrap().push(root.clone());
            // While the first row is being enabled, a `sync.disable` lands for the second
            // and a `workspace.forget` for the third.
            if root == roots[0] {
                registry.set_synced(&roots[1], false, false).unwrap();
                registry.forget(&c_id).unwrap();
            }
            async { Ok(()) }
        })
        .await;

        assert_eq!(*enabled.lock().unwrap(), vec![roots[0].clone()]);
        let rows = registry.entries().unwrap();
        assert_eq!(rows.len(), 2, "the forgotten row stays gone");
        assert!(!rows[1].1.synced, "the disabled row stays disabled");
    }
}
