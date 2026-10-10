use std::{collections::HashMap, path::Path};

use redb::{
    Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition, TableError,
};

use crate::{Error, FileStamp, Result, TrackStore};

/// Single table mapping a file path to its last-seen stamp, stored as the
/// `(mtime_ns, len)` tuple — nanoseconds since the UNIX epoch plus the
/// file size in bytes (#118).
const TABLE: TableDefinition<&str, (i64, u64)> = TableDefinition::new("tracked");

/// Persistent [`TrackStore`] backed by a pure-Rust [redb] database.
///
/// The database is a rebuildable cache: callers version the filename (e.g.
/// `track_v2.redb`) so that an incompatible value format — like the pre-#118
/// seconds-only `i64` — simply orphans the old file and a fresh one is created
/// here. As a belt-and-suspenders safety net, [`RedbTrackStore::open`] also
/// discards-and-recreates a file whose `tracked` table carries an
/// incompatible schema, so a leftover v1 file degrades to one full re-scan
/// instead of a hard open failure (#118 review finding).
///
/// [redb]: https://docs.rs/redb
pub struct RedbTrackStore {
    db: Database,
}

impl RedbTrackStore {
    /// Open (or create) the database at `path`, ensuring the table exists so
    /// that read transactions never fail on a brand-new file.
    ///
    /// If the file exists but cannot be opened with this build’s schema — an
    /// old on-disk format version, or a `tracked` table written with a
    /// different value type (the pre-#118 `i64` vs. today’s `(i64, u64)`) —
    /// the file is discarded and recreated empty (see [`create_or_reset`]).
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        Ok(Self {
            db: create_or_reset(path)?,
        })
    }
}

impl TrackStore for RedbTrackStore {
    fn mtimes(&self) -> Result<HashMap<String, FileStamp>> {
        let rtx = self.db.begin_read()?;
        let table = rtx.open_table(TABLE)?;
        let mut out = HashMap::new();
        for entry in table.iter()? {
            let (k, v) = entry?;
            let (mtime_ns, len) = v.value();
            out.insert(k.value().to_owned(), FileStamp { mtime_ns, len });
        }
        Ok(out)
    }

    fn upsert(&self, path: &str, stamp: FileStamp) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE)?;
            table.insert(path, (stamp.mtime_ns, stamp.len))?;
        }
        wtx.commit()?;
        Ok(())
    }

    fn remove(&self, path: &str) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE)?;
            table.remove(path)?;
        }
        wtx.commit()?;
        Ok(())
    }

    fn count(&self) -> Result<u64> {
        let rtx = self.db.begin_read()?;
        let table = rtx.open_table(TABLE)?;
        Ok(table.len()?)
    }

    fn clear(&self) -> Result<()> {
        let wtx = self.db.begin_write()?;
        wtx.open_table(TABLE)?.retain(|_, _| false)?;
        wtx.commit()?;
        Ok(())
    }

    fn upsert_many(&self, entries: &[(String, FileStamp)]) -> Result<()> {
        let wtx = self.db.begin_write()?;
        {
            let mut table = wtx.open_table(TABLE)?;
            for (path, stamp) in entries {
                table.insert(path.as_str(), (stamp.mtime_ns, stamp.len))?;
            }
        }
        wtx.commit()?;
        Ok(())
    }
}

/// Open `path` and materialise the `tracked` table once up front (a write
/// transaction’s `open_table` creates it when missing), so read
/// transactions never fail on a brand-new file.
fn open_db(path: &Path) -> Result<Database> {
    let db = Database::create(path)?;
    let wtx = db.begin_write()?;
    wtx.open_table(TABLE)?;
    wtx.commit()?;
    Ok(db)
}

/// Open the database at `path`, discarding it first if it cannot be opened
/// with this build’s schema.
///
/// Two failure shapes are tolerated, both handled by the same
/// orphan-and-rebuild path — this store only holds a snapshot of file stamps
/// that the next scan rebuilds from the filesystem, so throwing it away costs
/// one full re-scan and nothing else (the same as a cache miss):
///
/// - [`redb::DatabaseError::UpgradeRequired`]: the file is in an on-disk
///   *format* version this redb version can no longer read.
/// - A table *schema* mismatch (`TableError::TableTypeMismatch` /
///   `TypeDefinitionChanged`, surfacing from the `open_table` call in [`open_db`]): the file opens fine but its `tracked` table
///   was written with a different value type — e.g. the pre-#118
///   seconds-only `i64` vs. today’s `(i64, u64)`.
fn create_or_reset(path: &Path) -> Result<Database> {
    match open_db(path) {
        Err(err) if is_schema_mismatch(&err) => {
            std::fs::remove_file(path)?;
            Ok(open_db(path)?)
        }
        other => other,
    }
}

/// True when `err` means "the on-disk schema does not match this build’s
/// schema" (format-version bump or table value-type mismatch) rather than
/// "the store is genuinely broken".
fn is_schema_mismatch(err: &Error) -> bool {
    match err {
        Error::RedbDatabase(de) => matches!(**de, redb::DatabaseError::UpgradeRequired(_)),
        Error::RedbTable(te) => matches!(
            **te,
            TableError::TableTypeMismatch { .. } | TableError::TypeDefinitionChanged { .. }
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(n: i64) -> FileStamp {
        FileStamp {
            mtime_ns: n,
            len: n as u64,
        }
    }

    /// Both stores forget everything on `clear`, and keep working after it (#195).
    #[test]
    fn clear_forgets_every_path_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("track_v2.redb");
        let redb = RedbTrackStore::open(&path).unwrap();
        let memory = crate::open_in_memory();
        for store in [&redb as &dyn TrackStore, &memory] {
            store
                .upsert_many(&[("a".into(), stamp(1)), ("b".into(), stamp(2))])
                .unwrap();
            store.clear().unwrap();
            assert_eq!(store.count().unwrap(), 0);
            store.upsert("c", stamp(3)).unwrap();
            assert_eq!(store.count().unwrap(), 1);
        }
        drop(redb);
        // Cleared on disk, not only in memory.
        assert_eq!(RedbTrackStore::open(&path).unwrap().count().unwrap(), 1);
    }

    #[test]
    fn redb_round_trips_and_persists() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("track_v2.redb");

        {
            let store = RedbTrackStore::open(&path).unwrap();
            store
                .upsert(
                    "a",
                    FileStamp {
                        mtime_ns: 1,
                        len: 1,
                    },
                )
                .unwrap();
            store
                .upsert_many(&[
                    (
                        "b".into(),
                        FileStamp {
                            mtime_ns: 2,
                            len: 2,
                        },
                    ),
                    (
                        "c".into(),
                        FileStamp {
                            mtime_ns: 3,
                            len: 3,
                        },
                    ),
                ])
                .unwrap();
            store.remove("c").unwrap();
            assert_eq!(store.count().unwrap(), 2);
        }

        let store = RedbTrackStore::open(&path).unwrap();
        let m = store.mtimes().unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(
            m.get("a"),
            Some(&FileStamp {
                mtime_ns: 1,
                len: 1
            })
        );
    }

    #[test]
    fn open_recovers_from_a_schema_mismatch_by_recreating() {
        // A leftover file written with the pre-#118 value format cannot be
        // opened by the current schema; opening it must discard and recreate
        // (orphan-and-rebuild), not fail hard. Simulated by creating a file
        // whose `tracked` table was written with a bare `i64` value (the v1
        // format), then reopening it through `RedbTrackStore::open`.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("track_v1.redb");
        {
            let db = Database::create(&path).unwrap();
            let wtx = db.begin_write().unwrap();
            {
                let old: TableDefinition<&str, i64> = TableDefinition::new("tracked");
                let mut table = wtx.open_table(old).unwrap();
                table.insert("a", 1i64).unwrap();
            }
            wtx.commit().unwrap();
        }
        let store = RedbTrackStore::open(&path).unwrap();
        // Orphaned + recreated: empty, but openable and usable again.
        assert_eq!(store.count().unwrap(), 0);
        store
            .upsert(
                "a",
                FileStamp {
                    mtime_ns: 1,
                    len: 1,
                },
            )
            .unwrap();
        assert_eq!(store.count().unwrap(), 1);
    }
}
