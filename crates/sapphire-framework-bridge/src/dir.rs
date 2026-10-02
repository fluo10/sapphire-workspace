//! The bridge's directory: what it holds, and the guard that keeps it to one process.

use std::path::{Path, PathBuf};

use grain_id::GrainId;

use crate::error::{Error, Result};

/// Overrides the bridge directory outright.
pub const BRIDGE_DIR_ENV: &str = "SAPPHIRE_BRIDGE_DIR";

/// On-disk format version of the bridge directory.
pub const BRIDGE_FORMAT_VERSION: u32 = 1;

/// The bridge's directory.
///
/// Framework-wide: no app name and no kind, because one host is one device. It sits outside
/// the per-app layout deliberately.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeDir {
    /// The directory itself.
    pub root: PathBuf,
}

impl BridgeDir {
    /// The default directory, created if absent.
    pub fn open() -> Result<BridgeDir> {
        let root = match std::env::var_os(BRIDGE_DIR_ENV).filter(|v| !v.is_empty()) {
            Some(v) => PathBuf::from(v),
            None => dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("sapphire-bridge"),
        };
        BridgeDir::at(root)
    }

    /// A specific directory, created if absent.
    pub fn at(root: PathBuf) -> Result<BridgeDir> {
        let dir = BridgeDir { root };
        dir.prepare()?;
        Ok(dir)
    }

    fn prepare(&self) -> Result<()> {
        private_dir(&self.root)?;
        private_dir(&self.log_dir())?;
        private_dir(&self.workgroups_dir())?;

        let format = self.root.join("format");
        match std::fs::read_to_string(&format) {
            Ok(text) => {
                let found: u32 = text.trim().parse().map_err(|_| {
                    Error::Format(format!(
                        "{}: {:?} is not a version",
                        format.display(),
                        text.trim()
                    ))
                })?;
                if found > BRIDGE_FORMAT_VERSION {
                    // A newer build has been here. Running against a format we do not
                    // understand could corrupt it; leave it to the newer bridge.
                    return Err(Error::Format(format!(
                        "the bridge directory is format {found}, which is newer than this \
                         build understands ({BRIDGE_FORMAT_VERSION}); upgrade sapphire-bridge"
                    )));
                }
                if found < BRIDGE_FORMAT_VERSION {
                    // No older format exists yet. When one does, migrate here — idempotently
                    // — before writing the new stamp.
                    std::fs::write(&format, BRIDGE_FORMAT_VERSION.to_string())?;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::write(&format, BRIDGE_FORMAT_VERSION.to_string())?;
            }
            Err(e) => return Err(Error::Io(e)),
        }
        Ok(())
    }

    /// The iroh secret key, from which this device's node id follows.
    pub fn node_key(&self) -> PathBuf {
        self.root.join("node.key")
    }

    /// The single-instance guard.
    pub fn lock_path(&self) -> PathBuf {
        self.root.join("bridge.lock")
    }

    /// Host-local network configuration.
    pub fn net_toml(&self) -> PathBuf {
        self.root.join("net.toml")
    }

    /// The routing table.
    pub fn routes_toml(&self) -> PathBuf {
        self.root.join("routes.toml")
    }

    /// Runtime state, rewritten as it changes.
    pub fn status_json(&self) -> PathBuf {
        self.root.join("status.json")
    }

    /// Where the bridge's log goes.
    pub fn log_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// One directory per workgroup this host belongs to.
    pub fn workgroups_dir(&self) -> PathBuf {
        self.root.join("workgroups")
    }

    /// One workgroup's directory.
    pub fn workgroup_dir(&self, id: GrainId) -> PathBuf {
        self.workgroups_dir().join(id.to_string())
    }

    /// A workgroup's device ledger directory, as `sapphire-framework-registry` wants it.
    pub fn devices_dir(&self, id: GrainId) -> PathBuf {
        self.workgroup_dir(id).join("root").join("devices")
    }
}

/// Create `dir` if needed and make it private to the current user.
///
/// On Unix the directory is created with mode `0700` directly, so a freshly created
/// directory is never briefly readable by group or other — which the brief's
/// `create_dir_all` followed by a chmod would leave open, and this directory holds the
/// device's iroh secret key. A pre-existing directory whose mode drifted is tightened.
/// Same convention as `sapphire-framework-ipc`'s runtime directory.
fn private_dir(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(path)?;

        let mut perms = std::fs::metadata(path)?.permissions();
        if perms.mode() & 0o777 != 0o700 {
            perms.set_mode(0o700);
            std::fs::set_permissions(path, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)?;
    }
    Ok(())
}

/// Guarantees there is only one bridge for this directory.
///
/// Not an election. A process that cannot take this connects to the running bridge instead;
/// there is no handoff and no follower role.
#[derive(Debug)]
pub struct InstanceLock {
    path: PathBuf,
}

impl InstanceLock {
    /// Take the lock, or report who holds it.
    pub fn acquire(dir: &BridgeDir) -> Result<InstanceLock> {
        let path = dir.lock_path();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    use std::io::Write;
                    write!(file, "{}", std::process::id())?;
                    return Ok(InstanceLock { path });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    let holder = std::fs::read_to_string(&path).unwrap_or_default();
                    let pid: u32 = holder.trim().parse().unwrap_or(0);
                    if pid != 0 && process_is_alive(pid) {
                        return Err(Error::AlreadyRunning(pid));
                    }
                    // Nobody is behind it: a crash or a reboot left it. Clear and retry.
                    tracing::debug!(path = %path.display(), "clearing an abandoned bridge lock");
                    std::fs::remove_file(&path)?;
                }
                Err(e) => return Err(Error::Io(e)),
            }
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
fn process_is_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 only tests for the process; it has no other effect.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: the handle is closed on every path.
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = GetExitCodeProcess(handle, &raw mut code) != 0;
        CloseHandle(handle);
        ok && code == STILL_ACTIVE as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opening_creates_the_layout_and_stamps_the_format() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();

        assert!(dir.root.is_dir());
        assert!(dir.log_dir().is_dir());
        assert!(dir.workgroups_dir().is_dir());
        assert_eq!(
            std::fs::read_to_string(dir.root.join("format"))
                .unwrap()
                .trim(),
            BRIDGE_FORMAT_VERSION.to_string()
        );
    }

    #[test]
    fn opening_twice_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge");
        BridgeDir::at(path.clone()).unwrap();
        BridgeDir::at(path).unwrap();
    }

    #[test]
    fn a_newer_format_stops_the_bridge() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bridge");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("format"), (BRIDGE_FORMAT_VERSION + 1).to_string()).unwrap();

        let err = BridgeDir::at(path).unwrap_err();
        assert!(err.to_string().contains("newer"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let mode = std::fs::metadata(&dir.root).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode was {:o}", mode & 0o777);
    }

    // issue #165: `BRIDGE_DIR_ENV` is process-global, and `cargo test` runs a binary's
    // tests in parallel — this test raced `command::test_env`'s own tests over the same
    // variable, each clobbering the other's value mid-test, and failed one of them (never
    // this one, since it never checks anything in between) 20 times out of 20 run
    // together. `command::test_env::lock` is the one lock every test touching this
    // variable anywhere in the crate holds.
    #[tokio::test]
    async fn the_environment_variable_replaces_the_whole_path() {
        let _guard = crate::command::test_env::lock().await;
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: `_guard` holds the lock every other test touching this variable holds too.
        unsafe { std::env::set_var(BRIDGE_DIR_ENV, tmp.path()) };
        let dir = BridgeDir::open().unwrap();
        unsafe { std::env::remove_var(BRIDGE_DIR_ENV) };
        assert_eq!(dir.root, tmp.path());
    }

    #[test]
    fn a_second_instance_cannot_take_the_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();

        let first = InstanceLock::acquire(&dir).unwrap();
        let err = InstanceLock::acquire(&dir).unwrap_err();
        assert!(err.to_string().contains("already running"), "{err}");

        drop(first);
        InstanceLock::acquire(&dir).expect("the lock must be free once the holder drops it");
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_reclaimed() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        // A pid that is certainly not running: pid 0 is never a user process.
        std::fs::write(dir.lock_path(), "0").unwrap();

        InstanceLock::acquire(&dir).expect("a lock naming a dead process must be reclaimed");
    }

    #[test]
    fn the_lock_records_the_holder_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = BridgeDir::at(tmp.path().join("bridge")).unwrap();
        let _lock = InstanceLock::acquire(&dir).unwrap();
        let text = std::fs::read_to_string(dir.lock_path()).unwrap();
        assert_eq!(text.trim(), std::process::id().to_string());
    }
}
