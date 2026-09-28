//! What an application wants installed, and the facts about the machine an install needs.
//!
//! Every rule here turns on the effective uid and the platform; they arrive bundled in an
//! [`Environment`] so all combinations are testable on one machine, and so the rules are
//! functions of values rather than of the world they run in.

use crate::error::Result;

/// What an application wants installed.
///
/// Written by the application (`AppServer::service_spec` builds one); the rest of the crate
/// turns it into files and manager calls.
pub struct ServiceSpec {
    /// The application's name, as the manager and the paths name it: `sapphire-bridge`.
    pub app_name: &'static str,
    /// One line for the unit's `Description=` (or the platform's equivalent).
    pub description: String,
    /// Arguments handed to the executable, after the executable's own absolute path.
    pub args: Vec<String>,
    /// Runs after activation, with the resolved context.
    pub post_install: Option<PostInstall>,
}

impl std::fmt::Debug for ServiceSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `PostInstall` is a boxed closure; name it rather than trying to show it.
        f.debug_struct("ServiceSpec")
            .field("app_name", &self.app_name)
            .field("description", &self.description)
            .field("args", &self.args)
            .field("post_install", &self.post_install.as_ref().map(|_| "set"))
            .finish()
    }
}

/// A hook that runs after activation, with everything the install resolved.
pub type PostInstall = Box<dyn Fn(&InstallContext) -> Result<()> + Send + Sync>;

/// Everything an install resolved, handed to [`PostInstall`].
#[derive(Clone, Debug)]
pub struct InstallContext {
    /// Where the unit was written.
    pub unit_path: std::path::PathBuf,
    /// The absolute path of the running executable, the unit's `ExecStart` head.
    pub exe: std::path::PathBuf,
}

/// Every environment fact an install needs.
///
/// Read through a value rather than from the machine, so every combination is testable
/// without another operating system. The effective uid is a fact about the invoking
/// user's session — on macOS, the `gui/<uid>` domain a LaunchAgent activates into.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Environment {
    /// The effective uid of the process doing the installing, which names the `gui/<uid>`
    /// session a LaunchAgent activates into.
    pub euid: u32,
    /// The operating system, which decides which file an install writes.
    pub os: Os,
}

impl Environment {
    /// Read this machine's own facts: the effective uid and the platform.
    ///
    /// The one place these are read from the world. Every rule in this module turns on the
    /// value this returns, which is exactly why a test builds one itself instead: the rules
    /// are testable because they are functions of values, not of the machine.
    pub fn detect() -> Environment {
        Environment {
            euid: effective_uid(),
            os: Os::current(),
        }
    }
}

/// The effective uid of this process.
#[cfg(unix)]
fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

/// Not zero: there is no root to report, and a zero would read as one — refusing the
/// user-level install every Windows machine should be getting.
#[cfg(not(unix))]
fn effective_uid() -> u32 {
    1
}

/// The platforms an install can run on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Os {
    /// Linux, with systemd.
    Linux,
    /// macOS, with launchd.
    MacOs,
    /// Windows, with the Task Scheduler.
    Windows,
}

impl Os {
    /// The platform this build runs on.
    ///
    /// A platform that is neither Linux nor Windows is treated as macOS, the most
    /// restrictive of the three: it offers user-level units only.
    pub const fn current() -> Os {
        if cfg!(target_os = "linux") {
            Os::Linux
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::MacOs
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_detected_environment_describes_this_machine() {
        let env = Environment::detect();
        assert_eq!(env.os, Os::current(), "the platform is this build's own");
        #[cfg(unix)]
        assert_eq!(
            env.euid,
            // SAFETY: geteuid has no preconditions.
            unsafe { libc::geteuid() },
            "the effective uid is this process's own"
        );
    }

    #[test]
    fn a_detected_environment_is_not_root_where_there_is_no_root() {
        // A platform without an effective uid must not report one that reads as root.
        #[cfg(not(unix))]
        assert_ne!(Environment::detect().euid, 0);
    }
}
