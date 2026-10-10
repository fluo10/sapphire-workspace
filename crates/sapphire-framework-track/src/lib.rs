//! File-type-agnostic mtime-based file-change detection.
//!
//! `sapphire-track` owns the "what files changed since last time" concern,
//! independent of *why* a caller cares about those files. It persists a
//! `path -> (mtime, len)` snapshot and diffs the current filesystem state
//! against it.
//!
//! The crate deliberately knows nothing about file types: the caller decides
//! which paths to track by supplying an `accept` predicate to [`scan`] (or by
//! feeding [`Observed`] entries directly to [`diff`]). This keeps retrieval
//! concerns (handled by `sapphire-retrieve`) separate from change detection —
//! an application can track updates to files that are *not* retrievable (e.g.
//! audio assets) without involving the retrieve index at all.
//!
//! ## Backends
//!
//! - [`open_redb`] — persistent, pure-Rust [redb] store (the default).
//! - [`open_in_memory`] — ephemeral [`InMemoryTrackStore`] for tests and
//!   no-persistence builds.
//!
//! The store is treated as a rebuildable cache: if the on-disk format becomes
//! incompatible the file is simply recreated (callers version the filename).
//!
//! [redb]: https://docs.rs/redb

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
};

mod error;
mod redb_store;

pub use error::{Error, Result};
pub use redb_store::RedbTrackStore;

/// The per-path bookkeeping a [`TrackStore`] keeps per file: the last-seen
/// mtime (nanoseconds since the UNIX epoch) plus the last-seen file size.
///
/// The size is part of change detection because not every filesystem records
/// nanosecond-resolution timestamps (some network and embedded filesystems
/// store coarser timestamps than ext4's nanoseconds). A rewrite that lands
/// inside the filesystem's timestamp granularity is caught by `mtime_ns`
/// wherever the filesystem records sub-second changes, and by `len` whenever
/// the size differs — second-only resolution alone no longer hides
/// same-second edits (#118).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileStamp {
    /// Last-seen mtime, **nanoseconds** since the UNIX epoch.
    pub mtime_ns: i64,
    /// Last-seen file size in bytes.
    pub len: u64,
}

/// One observed path plus its current [`FileStamp`] (nanosecond mtime and
/// file size).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub path: PathBuf,
    /// The path's current change-detection stamp.
    pub stamp: FileStamp,
}

/// The result of diffing the current filesystem state against a stored
/// snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// Observed now, absent from the stored snapshot.
    pub added: Vec<PathBuf>,
    /// Present in the snapshot but with a different mtime **or** file size.
    pub modified: Vec<PathBuf>,
    /// Present in the snapshot but not observed now.
    pub removed: Vec<PathBuf>,
}

impl Changes {
    /// `true` when nothing was added, modified, or removed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    /// The set of paths a caller must re-process: `added` followed by
    /// `modified`.
    pub fn upserted(&self) -> impl Iterator<Item = &Path> {
        self.added
            .iter()
            .chain(self.modified.iter())
            .map(PathBuf::as_path)
    }
}

/// Persistent store mapping each file path to its [`FileStamp`].
///
/// All methods are synchronous, mirroring the style of
/// `sapphire_retrieve::RetrieveStore`. Paths are stored as their string
/// representation; the caller is responsible for passing consistent
/// (e.g. canonicalized) paths.
pub trait TrackStore: Send + Sync {
    /// Return the full snapshot mapping each path to its [`FileStamp`].
    fn mtimes(&self) -> Result<HashMap<String, FileStamp>>;

    /// Insert or update the stamp for `path`.
    fn upsert(&self, path: &str, stamp: FileStamp) -> Result<()>;

    /// Remove the entry for `path` (no-op if absent).
    fn remove(&self, path: &str) -> Result<()>;

    /// Number of tracked paths.
    fn count(&self) -> Result<u64>;

    /// Forget every path, so the next sync treats every file as changed. Backends
    /// override this to clear in one transaction.
    fn clear(&self) -> Result<()> {
        for path in self.mtimes()?.keys() {
            self.remove(path)?;
        }
        Ok(())
    }

    /// Insert or update many entries. Backends override this to commit the
    /// whole batch in a single transaction.
    fn upsert_many(&self, entries: &[(String, FileStamp)]) -> Result<()> {
        for (path, stamp) in entries {
            self.upsert(path, *stamp)?;
        }
        Ok(())
    }
}

/// Diff `observed` (the current filesystem state) against `stored` (the
/// previous snapshot). Pure: performs no I/O.
///
/// A stored path is `modified` when either recorded value differs — its
/// mtime **or** its file size (see [`FileStamp`], #118).
///
/// Caller-supplied filtering is assumed to already be applied to `observed`.
pub fn diff(stored: &HashMap<String, FileStamp>, observed: &[Observed]) -> Changes {
    let mut changes = Changes::default();
    let mut seen: HashSet<String> = HashSet::with_capacity(observed.len());

    for obs in observed {
        let key = obs.path.to_string_lossy().into_owned();
        match stored.get(&key) {
            None => changes.added.push(obs.path.clone()),
            Some(prev) if prev != &obs.stamp => changes.modified.push(obs.path.clone()),
            Some(_) => {}
        }
        seen.insert(key);
    }

    for path in stored.keys() {
        if !seen.contains(path) {
            changes.removed.push(PathBuf::from(path));
        }
    }

    changes
}

/// Read the stored snapshot from `store` and diff `observed` against it.
///
/// Does **not** mutate the store — the caller commits new stamps (via
/// [`TrackStore::upsert_many`]) only after successfully processing the
/// changes, so an interrupted run re-detects the work rather than dropping it.
pub fn detect_changes(store: &dyn TrackStore, observed: &[Observed]) -> Result<Changes> {
    let stored = store.mtimes()?;
    Ok(diff(&stored, observed))
}

/// Read both [`FileStamp`] fields of `path` from a **single** `metadata()`
/// call, so the pair cannot be torn into an (old mtime, new len) mismatch by
/// a concurrent write, and return the zero stamp on error.
///
/// This is the hot path used by [`scan`]; callers that need only one field can
/// use [`mtime_ns`] or [`file_len`] directly.
pub fn file_stamp(path: &Path) -> FileStamp {
    match path.metadata() {
        Ok(meta) => FileStamp {
            mtime_ns: meta
                .modified()
                .map(|t| {
                    t.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_nanos() as i64
                })
                .unwrap_or(0),
            len: meta.len(),
        },
        Err(_) => FileStamp {
            mtime_ns: 0,
            len: 0,
        },
    }
}

/// Return the mtime of `path` in **nanoseconds** since the UNIX epoch, or 0
/// on error.
///
/// i64 nanoseconds since the epoch cover 1678-04-11 to 2262-04-11 — the
/// nanosecond resolution is what catches same-second rewrites (#118), and
/// i64 still carries every realistic file timestamp with centuries to spare.
///
/// This stats the file on its own; [`file_stamp`] reads mtime and size from
/// one `metadata()` call when both fields are needed.
pub fn mtime_ns(path: &Path) -> i64 {
    file_stamp(path).mtime_ns
}

/// Return the size of `path` in bytes, or 0 on error.
///
/// Paired with [`mtime_ns`] for change detection: on a filesystem whose
/// timestamp granularity hides a same-interval rewrite, a size change still
/// flags the file as modified (#118). See the single-stat note on
/// [`file_stamp`].
pub fn file_len(path: &Path) -> u64 {
    file_stamp(path).len
}

/// Walk `root` recursively and return an [`Observed`] entry for every file for
/// which `accept(path)` is `true`.
///
/// `accept` also governs directories, so it must return `true` for every
/// directory the caller wants descended into — a predicate written to accept
/// only files (say, `path.extension() == Some("md")`) prunes every
/// subdirectory and silently observes nothing below `root`. A directory for
/// which `accept` returns `false` is not descended into, so its contents are
/// never observed (and never even visited — this is a pruning decision, not
/// just a filter on the final file list). `root` itself is always entered
/// regardless of what `accept(root)` would return, so callers don't need a
/// special case for a root that happens to fail their own predicate (e.g. a
/// hidden directory used as `root` directly). Symlinks are not followed.
pub fn scan<F: Fn(&Path) -> bool>(root: &Path, accept: F) -> Result<Vec<Observed>> {
    let mut out = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        // `accept` is the single decision point: it decides both which
        // directories to descend into and which files end up in `out` (the
        // loop body below just picks files back out, it does not re-check
        // `accept`). Root (depth 0) is exempt so a root that itself fails
        // `accept` — an allowed hidden directory, say — is still entered.
        .filter_entry(|e| e.depth() == 0 || accept(e.path()))
    {
        let entry = entry?;
        if entry.file_type().is_file() {
            let path = entry.path();
            out.push(Observed {
                path: path.to_path_buf(),
                // One `metadata()` call for both fields: no torn
                // (old mtime, new len) pair under concurrent writes.
                stamp: file_stamp(path),
            });
        }
    }
    Ok(out)
}

/// Open (or create) a persistent redb-backed [`TrackStore`] at `path`.
pub fn open_redb(path: &Path) -> Result<RedbTrackStore> {
    RedbTrackStore::open(path)
}

/// Create an ephemeral in-memory [`TrackStore`].
pub fn open_in_memory() -> InMemoryTrackStore {
    InMemoryTrackStore::default()
}

/// In-memory [`TrackStore`] backed by a `Mutex<HashMap>`. Used for tests and
/// no-persistence builds.
#[derive(Default)]
pub struct InMemoryTrackStore {
    inner: std::sync::Mutex<HashMap<String, FileStamp>>,
}

impl TrackStore for InMemoryTrackStore {
    fn mtimes(&self) -> Result<HashMap<String, FileStamp>> {
        Ok(self.inner.lock().unwrap().clone())
    }

    fn upsert(&self, path: &str, stamp: FileStamp) -> Result<()> {
        self.inner.lock().unwrap().insert(path.to_owned(), stamp);
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        self.inner.lock().unwrap().remove(path);
        Ok(())
    }

    fn count(&self) -> Result<u64> {
        Ok(self.inner.lock().unwrap().len() as u64)
    }

    fn clear(&self) -> Result<()> {
        self.inner.lock().unwrap().clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(path: &str, mtime_ns: i64, len: u64) -> Observed {
        Observed {
            path: PathBuf::from(path),
            stamp: FileStamp { mtime_ns, len },
        }
    }

    fn stored(pairs: &[(&str, i64, u64)]) -> HashMap<String, FileStamp> {
        pairs
            .iter()
            .map(|(p, m, l)| {
                (
                    p.to_string(),
                    FileStamp {
                        mtime_ns: *m,
                        len: *l,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn diff_classifies_added_modified_removed_and_skips_unchanged() {
        let prev = stored(&[("a", 1, 1), ("b", 2, 1), ("gone", 9, 1)]);
        let now = [obs("a", 1, 1), obs("b", 5, 1), obs("c", 3, 1)];

        let changes = diff(&prev, &now);

        assert_eq!(changes.added, vec![PathBuf::from("c")]);
        assert_eq!(changes.modified, vec![PathBuf::from("b")]);
        assert_eq!(changes.removed, vec![PathBuf::from("gone")]);
        // "a" is unchanged → in none of the buckets.
        assert!(changes.upserted().eq([Path::new("c"), Path::new("b")]));
    }

    #[test]
    fn diff_treats_a_same_mtime_length_change_as_modified() {
        // #118: stored mtime identical, size different → modified.
        let prev = stored(&[("a", 1, 1)]);
        let now = [obs("a", 1, 2)];
        assert_eq!(diff(&prev, &now).modified, vec![PathBuf::from("a")]);
    }

    #[test]
    fn diff_empty_when_nothing_changed() {
        let prev = stored(&[("a", 1, 1)]);
        let now = [obs("a", 1, 1)];
        assert!(diff(&prev, &now).is_empty());
    }

    #[test]
    fn in_memory_store_round_trips() {
        let store = open_in_memory();
        let x = FileStamp {
            mtime_ns: 10,
            len: 1,
        };
        store.upsert("x", x).unwrap();
        store
            .upsert_many(&[
                (
                    "y".into(),
                    FileStamp {
                        mtime_ns: 20,
                        len: 2,
                    },
                ),
                (
                    "z".into(),
                    FileStamp {
                        mtime_ns: 30,
                        len: 3,
                    },
                ),
            ])
            .unwrap();
        assert_eq!(store.count().unwrap(), 3);

        let m = store.mtimes().unwrap();
        assert_eq!(m.get("x"), Some(&x));
        assert_eq!(
            m.get("y"),
            Some(&FileStamp {
                mtime_ns: 20,
                len: 2
            })
        );

        store.remove("x").unwrap();
        assert_eq!(store.count().unwrap(), 2);
        assert!(!store.mtimes().unwrap().contains_key("x"));
    }

    #[test]
    fn detect_changes_reads_store_then_diffs() {
        let store = open_in_memory();
        store
            .upsert(
                "a",
                FileStamp {
                    mtime_ns: 1,
                    len: 1,
                },
            )
            .unwrap();
        let now = [obs("a", 1, 1), obs("b", 2, 2)];
        let changes = detect_changes(&store, &now).unwrap();
        assert_eq!(changes.added, vec![PathBuf::from("b")]);
        assert!(changes.modified.is_empty());
        assert!(changes.removed.is_empty());
    }

    #[test]
    fn scan_prunes_a_directory_the_predicate_rejects() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("keep")).unwrap();
        std::fs::write(tmp.path().join("keep").join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(tmp.path().join("skip")).unwrap();
        std::fs::write(tmp.path().join("skip").join("b.txt"), "b").unwrap();

        // Rejects the "skip" directory itself, so its contents must never be
        // visited — not merely filtered out of the result.
        let observed = scan(tmp.path(), |p| {
            !p.components().any(|c| c.as_os_str() == "skip")
        })
        .unwrap();

        let names: Vec<_> = observed
            .iter()
            .map(|o| o.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt"]);
    }

    #[test]
    fn scan_allows_a_differently_named_dot_directory_when_the_predicate_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join(".allowed")).unwrap();
        std::fs::write(tmp.path().join(".allowed").join("a.txt"), "a").unwrap();
        std::fs::create_dir_all(tmp.path().join(".blocked")).unwrap();
        std::fs::write(tmp.path().join(".blocked").join("b.txt"), "b").unwrap();

        // No built-in "skip anything starting with `.`" rule any more: the
        // predicate alone decides, so one dot-directory can be let through
        // while a differently-named one is still pruned.
        let observed = scan(tmp.path(), |p| {
            !p.components().any(|c| c.as_os_str() == ".blocked")
        })
        .unwrap();

        let names: Vec<_> = observed
            .iter()
            .map(|o| o.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt"]);
    }

    #[test]
    fn scan_always_enters_the_root_even_if_the_predicate_would_reject_it() {
        let tmp = tempfile::tempdir().unwrap();
        let hidden_root = tmp.path().join(".hidden_root");
        std::fs::create_dir_all(&hidden_root).unwrap();
        std::fs::write(hidden_root.join("a.txt"), "a").unwrap();

        // A predicate that rejects anything hidden would reject the root
        // itself if applied there — the walk must not let that empty out the
        // whole scan.
        let observed = scan(&hidden_root, |p| {
            !p.file_name().unwrap().to_string_lossy().starts_with('.')
        })
        .unwrap();

        let names: Vec<_> = observed
            .iter()
            .map(|o| o.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a.txt"]);
    }

    #[test]
    fn scan_observes_nanosecond_mtime_and_len() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "hello").unwrap();

        let observed = scan(tmp.path(), |_| true).unwrap();
        assert_eq!(observed.len(), 1);
        let meta = std::fs::metadata(&file).unwrap();
        assert_eq!(
            observed[0].stamp.mtime_ns,
            meta.modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as i64
        );
        assert_eq!(observed[0].stamp.len, meta.len());
    }

    #[test]
    fn detect_changes_detects_same_second_rewrite_same_length() {
        // Regression #118: a rewrite that lands in the same *second* as the
        // last scan, with the same file length, must still be `modified`.
        // The recorded mtime is pinned to a whole second first, so an
        // implementation storing second-resolution mtimes would see the
        // rewrite as a no-op.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "aaaa").unwrap();
        let whole_second = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_secs(
                std::fs::metadata(&file)
                    .unwrap()
                    .modified()
                    .unwrap()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
            );
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(whole_second)
            .unwrap();

        let store = open_in_memory();
        let first = scan(tmp.path(), |_| true).unwrap();
        store
            .upsert_many(
                &first
                    .iter()
                    .map(|o| (o.path.to_string_lossy().into_owned(), o.stamp))
                    .collect::<Vec<_>>(),
            )
            .unwrap();

        // Plain rewrite: same length, same second — the recorded mtime is now
        // the write's own (nanosecond-precision) timestamp, which differs
        // from the pinned whole-second value in its sub-second part.
        std::fs::write(&file, "bbbb").unwrap();

        let changes = detect_changes(&store, &scan(tmp.path(), |_| true).unwrap()).unwrap();
        assert_eq!(
            changes.modified,
            vec![file.clone()],
            "a same-second, same-length rewrite must count as modified"
        );
    }

    #[test]
    fn detect_changes_detects_length_change_with_identical_mtime() {
        // Regression #118 (coarse-granularity filesystems): even when the
        // recorded mtime is pinned to be bit-identical, a length change alone
        // must be detected as `modified`.
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.txt");
        std::fs::write(&file, "aaa").unwrap();
        let mtime = std::fs::metadata(&file).unwrap().modified().unwrap();

        let store = open_in_memory();
        store
            .upsert(
                &file.to_string_lossy(),
                FileStamp {
                    mtime_ns: mtime
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos() as i64,
                    len: std::fs::metadata(&file).unwrap().len(),
                },
            )
            .unwrap();

        std::fs::write(&file, "aaaaaa").unwrap();
        // Pin the mtime back to the recorded value: only `len` differs now.
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let changes = detect_changes(&store, &scan(tmp.path(), |_| true).unwrap()).unwrap();
        assert_eq!(
            changes.modified,
            vec![file.clone()],
            "a same-mtime length change must count as modified"
        );
    }
}
