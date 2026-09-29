//! Registering a sapphire application with the OS service manager.
//!
//! An application describes itself with a [`ServiceSpec`]; this crate turns that into a unit
//! file and hands it to the OS service manager — one real implementation per platform, plus
//! a recording one for tests, so no test ever touches the host's service manager. See the
//! process-architecture spec, §3 and §9 step 10.
//!
//! The one unit kind each platform offers, and how this crate installs it:
//!
//! | Platform | Unit | Activation |
//! |---|---|---|
//! | Linux | `~/.config/systemd/user/<app>.service` | `systemctl --user enable --now` |
//! | macOS | `~/Library/LaunchAgents/<label>.plist` | `launchctl bootstrap gui/<uid>` |
//! | Windows | a scheduled task, handed over from a temporary XML | `schtasks /create` |
//!
//! Every install is user level: the service runs as the user who installed it, with their
//! own home directory and their own directories. LaunchDaemons and real Windows services
//! are per-machine services this crate does not install — a service the machine runs
//! without a user is outside what it offers.
//!
//! The install flow itself lives in [`manager`]: [`install`], [`uninstall`] and [`status`]
//! drive everything through the [`ServiceManager`] trait, whose real implementation is the
//! only thing in the crate that writes to the machine or runs a command.

#![warn(missing_docs)]

pub mod error;
pub mod launchd;
pub mod manager;
pub mod scope;
pub mod systemd;
pub mod windows;

pub use error::{Error, Result};
pub use launchd::{agent_path, label, render_launch_agent};
pub use manager::{
    Calls, RecordingManager, ServiceCommand, ServiceManager, SystemManager, install, status,
    uninstall,
};
pub use scope::{Environment, InstallContext, Os, PostInstall, ServiceSpec};
pub use systemd::{activation, linger_hint, render_unit, unit_path};
pub use windows::render_task;
