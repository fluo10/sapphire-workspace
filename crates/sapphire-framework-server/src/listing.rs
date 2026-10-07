//! `workspace.list`, `workspace.forget`, and the restore a starting server runs.

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
    let rows = match HostRegistry::for_app(ctx).entries() {
        Ok(rows) => rows,
        Err(err) => {
            tracing::warn!("could not read the host workspace registry: {err}");
            return;
        }
    };
    for (id, entry) in rows.into_iter().filter(|(_, e)| e.synced) {
        if let Err(err) = runtime.enable(&entry.root).await {
            tracing::warn!(workspace = %id, root = %entry.root.display(), "could not restore sync: {err}");
        }
    }
}
