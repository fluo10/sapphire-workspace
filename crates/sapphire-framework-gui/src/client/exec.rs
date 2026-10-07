//! Carrying out one [`Command`].

use sapphire_backend::protocol as proto;
use sapphire_bridge_api::{DeviceRetireParams, InviteParams, JoinParams, WorkgroupCreateParams};

use super::conn::{Connections, unanswered};
use super::types::{ClientConfig, Command, CommandOutput, ServiceTarget};

/// How long `command` may take.
fn limit(cfg: &ClientConfig, command: &Command) -> std::time::Duration {
    match command {
        // An install may wait on an elevation prompt a person has to answer.
        Command::ServiceInstall(_) => cfg.install_timeout,
        // A join dials peers, so it may take far longer than any other command.
        Command::WorkgroupJoin { .. } => cfg.command_timeout * 3,
        _ => cfg.command_timeout,
    }
}

pub(crate) async fn execute(
    cfg: &ClientConfig,
    conns: &mut Connections,
    command: Command,
) -> Result<CommandOutput, String> {
    let limit = limit(cfg, &command);
    match command {
        Command::ServiceInstall(target) => {
            let name = match target {
                ServiceTarget::Bridge => "sapphire-bridge service install",
                ServiceTarget::App => "the app's service install",
            };
            tokio::time::timeout(limit, install(cfg, target))
                .await
                .unwrap_or_else(|_| Err(unanswered(name, limit)))
        }
        Command::WorkgroupCreate { .. }
        | Command::WorkgroupJoin { .. }
        | Command::DeviceInvite { .. }
        | Command::DeviceRetire { .. } => {
            let result = tokio::time::timeout(limit, bridge_command(cfg, conns, command))
                .await
                .unwrap_or_else(|_| Err(unanswered("sapphire-bridge", limit)));
            if result.is_err() {
                conns.drop_bridge();
            }
            result
        }
        _ => {
            let result = tokio::time::timeout(limit, app_command(cfg, conns, command))
                .await
                .unwrap_or_else(|_| Err(unanswered(cfg.app.app_name, limit)));
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
                // "Bring to this host…" names a folder inside the one picked, which does
                // not exist yet. The GUI runs on the same host as the server, so it makes it.
                std::fs::create_dir_all(&dir).map_err(|e| {
                    sapphire_ipc::Error::Io(std::io::Error::new(
                        e.kind(),
                        format!("could not create {}: {e}", dir.display()),
                    ))
                })?;
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
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("{e}; run: {manual}"))?;
    if output.status.success() {
        Ok(CommandOutput::Done)
    } else {
        Err(install_failure(
            &String::from_utf8_lossy(&output.stderr),
            &String::from_utf8_lossy(&output.stdout),
            output.status.code(),
            &manual,
        ))
    }
}

/// The message for a failed `service install`: what it said on stderr, else on stdout,
/// else its exit status — never empty before the `; run:` hint.
fn install_failure(stderr: &str, stdout: &str, code: Option<i32>, manual: &str) -> String {
    let said = [stderr.trim(), stdout.trim()]
        .into_iter()
        .find(|s| !s.is_empty())
        .map(str::to_owned);
    let what = said.unwrap_or_else(|| match code {
        Some(code) => format!("exit status {code}"),
        None => "it was stopped by a signal".to_owned(),
    });
    format!("{what}; run: {manual}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_install_gets_its_own_limit_and_a_join_three_times_the_command_limit() {
        use std::time::Duration;
        let app = super::super::AppIdentity {
            app_name: "app",
            version: "0",
        };
        let dir = std::path::PathBuf::from("/run");
        let cfg = ClientConfig {
            app,
            endpoints: super::super::Endpoints {
                bridge: sapphire_ipc::Endpoint::in_dir("b", dir.clone()),
                app: sapphire_ipc::Endpoint::in_dir("a", dir),
            },
            service_exes: super::super::ServiceExes {
                bridge: "/b".into(),
                app: "/a".into(),
            },
            refresh: Duration::from_secs(2),
            fetch_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(30),
            install_timeout: Duration::from_secs(300),
        };
        assert_eq!(
            limit(&cfg, &Command::ServiceInstall(ServiceTarget::App)),
            Duration::from_secs(300)
        );
        assert_eq!(
            limit(
                &cfg,
                &Command::WorkgroupJoin {
                    ticket: "t".into(),
                    device_name: None
                }
            ),
            Duration::from_secs(90)
        );
        assert_eq!(
            limit(&cfg, &Command::SyncEnable { root: "/x".into() }),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn an_install_failure_prefers_stderr_then_stdout_then_the_status() {
        let manual = "x service install";
        assert_eq!(
            install_failure(" denied \n", "ignored", Some(1), manual),
            "denied; run: x service install"
        );
        assert_eq!(
            install_failure("  ", "need admin\n", Some(1), manual),
            "need admin; run: x service install"
        );
        assert_eq!(
            install_failure("", "", Some(3), manual),
            "exit status 3; run: x service install"
        );
        let killed = install_failure("", "", None, manual);
        assert!(!killed.starts_with(';'), "{killed}");
        assert!(killed.ends_with("; run: x service install"), "{killed}");
    }
}
