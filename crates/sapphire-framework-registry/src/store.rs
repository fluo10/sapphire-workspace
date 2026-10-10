//! The write path shared by every ledger.
//!
//! Each ledger module (`devices.rs`) assembles its own content; this module only
//! guarantees the write itself lands without corruption.

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};

/// Write `header` + a blank line + `body` via **a temp file, then rename**.
///
/// Truncating in place would destroy the ledger if we crashed mid-write. A
/// device id is baked into journal frontmatter, so losing the ledger leaves past
/// references unresolvable.
///
/// The file is not created 0600: no ledger holds a secret (an external device's record
/// keeps only its token's hash) and the ledgers are meant to sync, so an owner-only
/// permission would be pointless.
///
/// The temp file name carries the process id and a per-process counter. Two
/// processes writing the same ledger at once (or two threads in one process)
/// would each `File::create` (truncate) a fixed `*.tmp` name, interleaving both
/// writes into one inode and publishing a half-written file on the first
/// rename. This only narrows that window, never closes it — there is no lock,
/// so whoever renames last wins.
pub(crate) fn write_atomic(path: &Path, header: &str, body: &str) -> Result<()> {
    use std::io::Write as _;

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;

    let file_name = path
        .file_name()
        .ok_or_else(|| Error::File(format!("{} is not a file path", path.display())))?;
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut tmp_name = file_name.to_os_string();
    tmp_name.push(format!(".tmp.{}.{unique}", std::process::id()));
    let tmp = parent.join(tmp_name);

    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(header.as_bytes())?;
        file.write_all(b"\n")?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}
