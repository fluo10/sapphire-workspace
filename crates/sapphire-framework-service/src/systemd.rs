//! Rendering and activating systemd **user** units, the one kind of unit this crate
//! installs on Linux.
//!
//! The unit file is the contract between this crate and the machine: what it says is what
//! the service manager does. It turns what an install needs — which unit file to write and
//! what starts it — into systemd's own words.
//! See the process-architecture spec, §3 and §9 step 10.

use std::path::{Path, PathBuf};

use crate::scope::{InstallContext, ServiceSpec};

/// The unit file an install writes: under the invoking user's own `~/.config`, so it is
/// theirs to remove.
pub fn unit_path(app_name: &str, home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
        .join(format!("{app_name}.service"))
}

/// The commands that activate an installed unit: `daemon-reload` first, so the manager sees
/// the file just written; then `enable --now`, which both registers the unit for future boots
/// and starts it now. A user unit goes through the invoking user's own manager.
pub fn activation(app_name: &str) -> Vec<Vec<String>> {
    vec![
        words(&["systemctl", "--user", "daemon-reload"]),
        words(&["systemctl", "--user", "enable", "--now", app_name]),
    ]
}

/// The advice an install should print for a machine meant to run the service without a login.
///
/// A user unit dies with its session; `loginctl enable-linger` lets it survive one.
pub fn linger_hint() -> Option<String> {
    Some("to run without a login, run: loginctl enable-linger".to_owned())
}

/// Turn a [`ServiceSpec`] and a resolved [`InstallContext`] into a systemd unit file.
///
/// A user unit runs as its owner with no network ordering: it starts after the session is
/// up already, and systemd needs no `User=` line to run it as the person who installed it.
pub fn render_unit(spec: &ServiceSpec, ctx: &InstallContext) -> String {
    let mut unit = String::new();
    unit.push_str("[Unit]\n");
    unit.push_str(&format!("Description={}\n", spec.description));
    unit.push_str("\n[Service]\nType=simple\n");
    // A user unit runs as its owner; systemd needs no User= line.
    unit.push_str(&format!(
        "ExecStart={} {}\n",
        ctx.exe.display(),
        spec.args.join(" ")
    ));
    unit.push_str("Restart=on-failure\n");
    unit.push_str("RestartSec=5\n");
    unit.push_str("\n[Install]\n");
    unit.push_str("WantedBy=default.target\n");
    unit
}

/// A command line built from its words.
pub(crate) fn words(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn commands(words: &[&[&str]]) -> Vec<Vec<String>> {
        words
            .iter()
            .map(|c| c.iter().map(|w| (*w).to_owned()).collect())
            .collect()
    }

    #[test]
    fn a_user_unit_lives_under_the_users_own_config() {
        let path = unit_path("sapphire-bridge", Path::new("/home/alice"));
        assert_eq!(
            path,
            PathBuf::from("/home/alice/.config/systemd/user/sapphire-bridge.service")
        );
    }

    #[test]
    fn a_unit_activates_through_the_user_manager() {
        assert_eq!(
            activation("sapphire-bridge"),
            commands(&[
                &["systemctl", "--user", "daemon-reload"],
                &["systemctl", "--user", "enable", "--now", "sapphire-bridge"],
            ])
        );
    }

    #[test]
    fn a_user_install_hints_at_linger() {
        let hint = linger_hint().unwrap();
        assert!(hint.contains("loginctl enable-linger"), "{hint}");
    }
}
