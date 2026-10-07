//! Carrying out one [`Command`].

use sapphire_backend::protocol as proto;
use sapphire_bridge_api::{DeviceRetireParams, InviteParams, JoinParams, WorkgroupCreateParams};

use super::conn::Connections;
use super::types::{ClientConfig, Command, CommandOutput, ServiceTarget};

pub(crate) async fn execute(
    cfg: &ClientConfig,
    conns: &mut Connections,
    command: Command,
) -> Result<CommandOutput, String> {
    match command {
        Command::ServiceInstall(target) => install(cfg, target).await,
        Command::WorkgroupCreate { .. }
        | Command::WorkgroupJoin { .. }
        | Command::DeviceInvite { .. }
        | Command::DeviceRetire { .. } => {
            let result = bridge_command(cfg, conns, command).await;
            if result.is_err() {
                conns.drop_bridge();
            }
            result
        }
        _ => {
            let result = app_command(cfg, conns, command).await;
            if result.is_err() {
                conns.drop_app();
            }
            result
        }
    }
}

async fn bridge_command(
    cfg: &ClientConfig,
    conns: &mut Connections,
    command: Command,
) -> Result<CommandOutput, String> {
    let c = conns
        .bridge(cfg)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "sapphire-bridge is not running".to_owned())?;
    let out = match command {
        Command::WorkgroupCreate { name, device_name } => c
            .workgroup_create(WorkgroupCreateParams { name, device_name })
            .await
            .map(|_| CommandOutput::Done),
        Command::WorkgroupJoin {
            ticket,
            device_name,
        } => c
            .join(JoinParams {
                ticket,
                device_name,
            })
            .await
            .map(|_| CommandOutput::Done),
        Command::DeviceInvite { name, ttl_secs } => c
            .invite(InviteParams {
                name,
                ttl: ttl_secs,
                workgroup: None,
            })
            .await
            .map(|r| CommandOutput::Ticket(r.ticket)),
        Command::DeviceRetire { selector } => c
            .device_retire(DeviceRetireParams { selector })
            .await
            .map(|_| CommandOutput::Done),
        _ => unreachable!("execute routes only bridge commands here"),
    };
    out.map_err(|e| e.to_string())
}

async fn app_command(
    cfg: &ClientConfig,
    conns: &mut Connections,
    command: Command,
) -> Result<CommandOutput, String> {
    let app = cfg.app.app_name;
    let c = conns
        .app(cfg)
        .await
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("the {app} server is not running"))?;
    let r: sapphire_ipc::Result<()> = async {
        match command {
            Command::WorkspaceInit { dir, sync } => {
                let init: proto::WorkspaceInitResult = c
                    .call(proto::WORKSPACE_INIT, proto::WorkspaceInitParams { dir })
                    .await?;
                if sync {
                    let _: proto::SyncEnableResult = c
                        .call(proto::SYNC_ENABLE, proto::WsParams { ws: init.root })
                        .await?;
                }
            }
            Command::SyncEnable { root } => {
                let _: proto::SyncEnableResult = c
                    .call(proto::SYNC_ENABLE, proto::WsParams { ws: root })
                    .await?;
            }
            Command::SyncDisable { root } => {
                let _: proto::Ack = c
                    .call(proto::SYNC_DISABLE, proto::WsParams { ws: root })
                    .await?;
            }
            Command::WorkspaceMap { workspace_id, dir } => {
                if !dir.join(format!(".{app}")).is_dir() {
                    let _: proto::WorkspaceInitResult = c
                        .call(
                            proto::WORKSPACE_INIT,
                            proto::WorkspaceInitParams { dir: dir.clone() },
                        )
                        .await?;
                }
                let _: proto::SyncEnableResult = c
                    .call(
                        proto::SYNC_MAP,
                        proto::SyncMapParams {
                            workspace: workspace_id.to_string(),
                            dir,
                        },
                    )
                    .await?;
            }
            Command::WorkspaceForget { id } => {
                let _: proto::Ack = c
                    .call(proto::WORKSPACE_FORGET, proto::WorkspaceForgetParams { id })
                    .await?;
            }
            _ => unreachable!("execute routes only app commands here"),
        }
        Ok(())
    }
    .await;
    r.map(|()| CommandOutput::Done).map_err(|e| e.to_string())
}

/// Run `<exe> service install`. The CLI's own verb registers the CLI binary, which is what
/// the service manager must start.
async fn install(cfg: &ClientConfig, target: ServiceTarget) -> Result<CommandOutput, String> {
    let exe = match target {
        ServiceTarget::Bridge => &cfg.service_exes.bridge,
        ServiceTarget::App => &cfg.service_exes.app,
    };
    let manual = format!("{} service install", exe.display());
    if !exe.is_file() {
        return Err(format!("{} was not found; run: {manual}", exe.display()));
    }
    let output = tokio::process::Command::new(exe)
        .args(["service", "install"])
        .output()
        .await
        .map_err(|e| format!("{e}; run: {manual}"))?;
    if output.status.success() {
        Ok(CommandOutput::Done)
    } else {
        let text = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(format!("{text}; run: {manual}"))
    }
}
