//! Where a server listens (spec §2.5).
//!
//! The runtime directory is persistent, not `/run/user/<uid>`. A server started as a system
//! service and dropped to a user has neither `$XDG_RUNTIME_DIR` nor a login session's
//! `/run/user/<uid>`, while a CLI in a login session has both — the two would resolve
//! different paths and never meet.

use std::fs;

use std::path::{Path, PathBuf};

use crate::error::Result;

/// The endpoint name the host-wide bridge listens under.
pub const BRIDGE_NAME: &str = "bridge";

/// Environment variable that replaces the runtime directory outright.
pub const RUNTIME_DIR_ENV: &str = "SAPPHIRE_RUNTIME_DIR";

/// Create `dir` if needed and make it private to the current user.
///
/// On Unix the directory is created with mode `0700` directly, so a freshly created
/// directory is never briefly readable by group or other — which is what a plain
/// `create_dir_all` followed by a chmod would leave open. A pre-existing directory whose
/// mode drifted is tightened to `0700`.
pub(crate) fn ensure_private_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(dir)?;

        let mut perms = std::fs::metadata(dir)?.permissions();
        if perms.mode() & 0o777 != 0o700 {
            perms.set_mode(0o700);
            std::fs::set_permissions(dir, perms)?;
        }
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// The directory holding this user's sockets, created if absent.
pub fn runtime_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os(RUNTIME_DIR_ENV).filter(|v| !v.is_empty()) {
        Some(v) => PathBuf::from(v),
        None => dirs::data_dir()
            .unwrap_or_else(std::env::temp_dir)
            .join("sapphire-bridge")
            .join("run"),
    };
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Identifies one server's listening address.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The app name, or [`BRIDGE_NAME`].
    pub name: String,
    /// The directory the socket and lock live in.
    pub dir: PathBuf,
}

impl Endpoint {
    /// The endpoint of `app_name`'s server, creating the runtime directory if needed.
    pub fn for_app(app_name: &str) -> Result<Endpoint> {
        Ok(Endpoint {
            name: app_name.to_owned(),
            dir: runtime_dir()?,
        })
    }

    /// The endpoint of the host-wide bridge.
    pub fn for_bridge() -> Result<Endpoint> {
        Endpoint::for_app(BRIDGE_NAME)
    }

    /// An endpoint in an explicit directory. Used by tests and by callers that resolved
    /// the directory themselves.
    pub fn in_dir(name: impl Into<String>, dir: PathBuf) -> Endpoint {
        Endpoint {
            name: name.into(),
            dir,
        }
    }

    /// The Unix domain socket path.
    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(format!("{}.sock", self.name))
    }

    /// The Windows named pipe name.
    ///
    /// Scoped to the current user so two users on one machine get separate pipes, and
    /// salted with a hash of the endpoint directory so two same-named endpoints in
    /// different directories get separate pipes too. The directory is part of an
    /// endpoint's identity on both platforms: on Unix it *is* the socket path, and here
    /// it shapes the pipe name (see `dir_salt`).
    pub fn pipe_name(&self) -> String {
        format!(
            "\\\\.\\pipe\\sapphire.{}.{}.{}",
            user_scope(),
            dir_salt(&self.dir),
            self.name
        )
    }
}

/// The directory component of the Windows pipe name: a hash of the directory
/// canonical path's bytes, so that same-name endpoints in different directories get
/// distinct pipes.
///
/// Canonicalized first: two processes naming the same directory through different
/// path spellings (case, separators, a `.` prefix) must still derive the same pipe
/// name. Falls back to the path as given when canonicalization fails (the directory
/// may not exist yet); two endpoints that agree on their spelling still agree on
/// their salt, which is what matters.
///
/// The salt is an identifier any other user on the machine can observe in the pipe
/// namespace; it is not a secret and not access control. The security descriptor set
/// on the pipe (see `windows.rs`) is what actually restricts who may connect.
fn dir_salt(dir: &Path) -> String {
    use sha2::{Digest as _, Sha256};
    let canonical = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    // Same shape as `ContentHash::of_bytes` in the sync crate: digest, then slice.
    let digest: [u8; 32] = Sha256::digest(canonical.as_os_str().as_encoded_bytes()).into();
    // 16 hex characters: collisions are not worth worrying about at this scale.
    let mut salt = String::with_capacity(16);
    for byte in &digest[..8] {
        salt.push(char::from_digit(u32::from(byte >> 4), 16).expect("a hex digit"));
        salt.push(char::from_digit(u32::from(byte & 0xF), 16).expect("a hex digit"));
    }
    salt
}

/// A stable, per-user string used to scope the Windows pipe name.
///
/// The security descriptor is what actually restricts access (see `windows.rs`); this only
/// keeps two users' pipes from colliding by name.
fn user_scope() -> String {
    #[cfg(windows)]
    {
        crate::windows::current_user_sid().unwrap_or_else(|_| "unknown".to_owned())
    }
    #[cfg(unix)]
    {
        // SAFETY: getuid is always successful and has no preconditions.
        unsafe { libc::getuid() }.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    /// Serialises every test in this module that reads or writes the process environment.
    ///
    /// The environment is process-global while the test harness runs tests on parallel
    /// threads: `std::env::temp_dir()` (used by the endpoint-name tests) and
    /// `tempfile::tempdir()` read it, and `set_var`/`remove_var` write it (which is why
    /// Rust 2024 made them `unsafe`). Holding this lock across both the read and the write
    /// is the whole safety argument.
    static ENV: Mutex<()> = Mutex::new(());

    /// Lock the process environment. Hold the guard for the whole test, including the
    /// env reads (`temp_dir`, `tempdir`) that would otherwise race a sibling's mutation.
    #[must_use = "the guard must be held while the test reads env vars"]
    fn lock_env() -> MutexGuard<'static, ()> {
        // A poisoned lock only means some other test panicked while holding it; the env
        // state is restored by `EnvGuard::drop`, so it is not invariant-critical here.
        ENV.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Sets `SAPPHIRE_RUNTIME_DIR` while holding `lock`, restoring the previous value
    /// (present or absent) when dropped — including while unwinding from a panic.
    ///
    /// Without the unconditional restore, a panicking assertion would leave the variable
    /// pointing at a temp directory that the test then deletes, and every later
    /// `runtime_dir()` in the process would resolve into the void.
    struct EnvGuard {
        previous: Option<OsString>,
        _lock: MutexGuard<'static, ()>,
    }

    impl EnvGuard {
        fn set_runtime_dir(lock: MutexGuard<'static, ()>, dir: &Path) -> EnvGuard {
            let previous = std::env::var_os(RUNTIME_DIR_ENV);
            // SAFETY: `lock` serialises every read and write of the process environment in
            // this test binary, and it is held until `drop` has restored the old value.
            unsafe { std::env::set_var(RUNTIME_DIR_ENV, dir) };
            EnvGuard {
                previous,
                _lock: lock,
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                // SAFETY: `self._lock` still serialises the environment; it is dropped
                // only after this method returns.
                Some(value) => unsafe { std::env::set_var(RUNTIME_DIR_ENV, value) },
                None => unsafe { std::env::remove_var(RUNTIME_DIR_ENV) },
            }
        }
    }

    #[test]
    fn an_endpoint_names_its_socket_and_lock_under_its_directory() {
        let _env = lock_env();
        let dir = std::env::temp_dir().join("sapphire-endpoint-test");
        let ep = Endpoint::in_dir("sapphire-journal", dir.clone());
        assert_eq!(ep.socket_path(), dir.join("sapphire-journal.sock"));
    }

    #[test]
    fn the_bridge_endpoint_is_named_bridge() {
        let _env = lock_env();
        let ep = Endpoint::in_dir(BRIDGE_NAME, std::env::temp_dir());
        assert_eq!(ep.socket_path().file_name().unwrap(), "bridge.sock");
    }

    #[test]
    fn a_pipe_name_is_scoped_to_the_user_and_the_directory() {
        let _env = lock_env();
        let ep = Endpoint::in_dir("sapphire-journal", std::env::temp_dir());
        let name = ep.pipe_name();
        let body = name.trim_start_matches(r"\\.\pipe\sapphire.");
        // Shape: <user scope>.<16 hex chars>.<endpoint name>.
        let segments: Vec<&str> = body.split(".").collect();
        assert_eq!(segments.len(), 3, "{name}");
        assert_eq!(segments[2], "sapphire-journal", "{name}");
        let salt = segments[1];
        assert_eq!(salt.len(), 16, "{name}");
        assert!(
            salt.chars().all(|c| c.is_ascii_hexdigit()),
            "salt {salt} should be 16 hex characters",
        );
    }

    #[test]
    fn the_runtime_directory_env_var_replaces_the_whole_path() {
        // The guard locks the environment for the whole test, including `tempdir()`.
        let lock = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let _override = EnvGuard::set_runtime_dir(lock, tmp.path());
        let dir = runtime_dir().unwrap();
        assert_eq!(dir, tmp.path());
        assert!(dir.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn the_runtime_directory_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode was {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn runtime_dir_creates_a_missing_directory_private() {
        use std::os::unix::fs::PermissionsExt;
        // The guard locks the environment for the whole test, including `tempdir()`.
        let lock = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("run");
        let _override = EnvGuard::set_runtime_dir(lock, &dir);
        let resolved = runtime_dir().unwrap();
        assert_eq!(resolved, dir);
        let mode = std::fs::metadata(&resolved).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "mode was {:o}", mode & 0o777);
    }

    /// Two endpoints with the same name in different directories must get different
    /// pipes (issue #151), and the same name in the same directory must keep getting
    /// the same one — the salt is a function of the directory, not of the instance.
    #[cfg(windows)]
    #[test]
    fn same_name_in_different_directories_gets_a_different_pipe() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        let a = Endpoint::in_dir("sapphire-journal", tmp.path().join("host-a"));
        let b = Endpoint::in_dir("sapphire-journal", tmp.path().join("host-b"));
        assert_ne!(a.pipe_name(), b.pipe_name(), "{:?} vs {:?}", a.dir, b.dir);
        // Deterministic in the directory, not in the instance: same dir, same name.
        assert_eq!(
            a.pipe_name(),
            Endpoint::in_dir("sapphire-journal", tmp.path().join("host-a")).pipe_name(),
        );
    }

    /// The salt is a function of the *canonical* directory, so the same directory spelled
    /// differently (a `.` component, mixed separators, case) must still hash to one pipe
    /// name — otherwise two processes that merely disagree about spelling would silently
    /// talk past each other. This pins the canonicalization the `dir_salt` docs promise.
    #[cfg(windows)]
    #[test]
    fn the_same_directory_spelled_differently_gets_the_same_pipe() {
        let _env = lock_env();
        let tmp = tempfile::tempdir().unwrap();
        // The directory must exist: `dir_salt` canonicalizes only what it can resolve.
        let plain_dir = tmp.path().join("host-a");
        std::fs::create_dir_all(&plain_dir).unwrap();
        // Same directory, four spellings a caller might plausibly hand us: a `.`
        // component, a `..` round-trip, an upper-cased name, and the plain form.
        let spellings = [
            plain_dir.clone(),
            tmp.path().join(".").join("host-a"),
            tmp.path().join("host-a").join("..").join("host-a"),
            tmp.path().join("HOST-A"),
        ];
        let plain = Endpoint::in_dir("sapphire-journal", plain_dir);
        let expected = plain.pipe_name();
        for spelling in spellings {
            let other = Endpoint::in_dir("sapphire-journal", spelling);
            assert_eq!(expected, other.pipe_name(), "{:?}", other.dir);
        }
    }
}
