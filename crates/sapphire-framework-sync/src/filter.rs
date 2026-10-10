//! Which paths take part in sync.

use std::path::Path;

use ignore::gitignore::{Gitignore, GitignoreBuilder};

use crate::error::Result;
use crate::paths;

/// Name of the per-workspace ignore file.
pub const IGNORE_FILE: &str = ".sapphireignore";

/// Prefix of a conflict copy of the ignore file (`merge::conflict_path`).
const IGNORE_FILE_CONFLICT_PREFIX: &str = ".sapphireignore.conflict-";

/// The tag `merge::conflict_path` puts in a conflict copy's file name.
const CONFLICT_TAG: &str = ".conflict-";

/// The app directory's subdirectory of synced vectors (#187).
pub const EMBEDDED_DIR: &str = "embedded";

/// Declarative sync filter: the built-in rule for an app name plus `.sapphireignore`.
pub struct SyncFilter {
    app_dir: String,
    ignore: Option<Gitignore>,
}

impl SyncFilter {
    /// Build the filter for `root`, reading `.sapphireignore` if present.
    pub fn load(root: &Path, app_name: &str) -> Result<Self> {
        let file = root.join(IGNORE_FILE);
        let ignore = if file.is_file() {
            let mut builder = GitignoreBuilder::new(root);
            if let Some(err) = builder.add(&file) {
                return Err(err.into());
            }
            Some(builder.build()?)
        } else {
            None
        };
        Ok(Self {
            app_dir: format!(".{app_name}"),
            ignore,
        })
    }

    /// Whether `rel` takes part in sync.
    pub fn allows(&self, rel: &str, is_dir: bool) -> bool {
        if !paths::is_valid_rel(rel) {
            return false;
        }
        if rel == IGNORE_FILE {
            return true;
        }
        // A conflict copy of the ignore file is a hidden name, which the built-in rule
        // below would reject — so no replica could ever write it, and a concurrent edit
        // to the ignore file would be silently superseded everywhere. The file the
        // filter can never exclude cannot have its copy excluded either. Require a
        // single path segment so a directory that merely starts with this prefix does
        // not carve out its entire subtree.
        if rel.starts_with(IGNORE_FILE_CONFLICT_PREFIX) && !rel.contains('/') {
            return true;
        }
        if !rel
            .split('/')
            .all(|seg| !seg.starts_with('.') || seg == self.app_dir)
        {
            return false;
        }
        // Two vectors at one path were computed from the same input, so the newest wins and
        // a conflict copy would only be clutter that nothing reads. Built in rather than left
        // to `.sapphireignore`, which a user may edit.
        if self.is_embedded_conflict(rel) {
            return false;
        }
        match &self.ignore {
            Some(gi) => !gi
                .matched_path_or_any_parents(paths::to_native(Path::new(""), rel), is_dir)
                .is_ignore(),
            None => true,
        }
    }
}

impl SyncFilter {
    /// Whether `rel` is a conflict copy under `.<app>/embedded/`.
    fn is_embedded_conflict(&self, rel: &str) -> bool {
        let Some(inside) = rel
            .strip_prefix(self.app_dir.as_str())
            .and_then(|r| r.strip_prefix('/'))
            .and_then(|r| r.strip_prefix(EMBEDDED_DIR))
            .and_then(|r| r.strip_prefix('/'))
        else {
            return false;
        };
        inside
            .rsplit('/')
            .next()
            .is_some_and(|name| name.contains(CONFLICT_TAG))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filter_with(ignore: Option<&str>) -> (tempfile::TempDir, SyncFilter) {
        let dir = tempfile::tempdir().unwrap();
        if let Some(body) = ignore {
            std::fs::write(dir.path().join(IGNORE_FILE), body).unwrap();
        }
        let f = SyncFilter::load(dir.path(), "test-app").unwrap();
        (dir, f)
    }

    #[test]
    fn built_in_rule() {
        let (_d, f) = filter_with(None);
        assert!(f.allows("notes/a.md", false));
        assert!(f.allows(".test-app", true));
        assert!(f.allows(".test-app/sync-id", false));
        assert!(f.allows("sub/.test-app/x", false));
        assert!(f.allows(IGNORE_FILE, false));
        assert!(!f.allows(".git/config", false));
        assert!(!f.allows("a/.hidden", false));
        assert!(!f.allows(".other-app/x", false));
        assert!(!f.allows("../escape", false));
    }

    #[test]
    fn no_conflict_copies_of_synced_vectors() {
        let (_d, f) = filter_with(None);
        let copy = crate::merge::conflict_path(
            ".test-app/embedded/m-8-0011aabb/abc.vec",
            &crate::vv::Dot {
                replica: crate::id::ReplicaId(uuid::Uuid::from_u128(7)),
                counter: 3,
            },
        );
        assert!(copy.contains(".conflict-"), "{copy}");
        assert!(!f.allows(&copy, false), "{copy}");
        assert!(f.allows(".test-app/embedded/m-8-0011aabb/abc.vec", false));
        // Elsewhere, a conflict copy is the user's data and syncs.
        assert!(f.allows("notes/a.conflict-0000-1.md", false));
        assert!(f.allows(".test-app/state.conflict-0000-1.toml", false));
    }

    #[test]
    fn ignore_file_patterns() {
        let (_d, f) = filter_with(Some("*.tmp\nbuild/\n!keep.tmp\n"));
        assert!(!f.allows("a.tmp", false));
        assert!(f.allows("keep.tmp", false));
        assert!(!f.allows("build", true));
        assert!(!f.allows("build/out.bin", false));
        assert!(f.allows("src/main.rs", false));
    }

    #[test]
    fn the_ignore_file_itself_cannot_be_ignored() {
        let (_d, f) = filter_with(Some("*\n"));
        assert!(f.allows(IGNORE_FILE, false));
        assert!(!f.allows("anything", false));
    }

    #[test]
    fn a_conflict_copy_of_the_ignore_file_is_allowed() {
        for ignore in [None, Some("*\n"), Some("*.conflict-*\n")] {
            let (_d, f) = filter_with(ignore);
            assert!(
                f.allows(".sapphireignore.conflict-123abc-4", false),
                "{ignore:?}"
            );
        }
        // Only the copies of the ignore file itself; other hidden names stay excluded.
        let (_d, f) = filter_with(None);
        assert!(!f.allows(".sapphireignored", false));
        assert!(!f.allows(".other.conflict-123abc-4", false));
        assert!(!f.allows("dir/.sapphireignore.conflict-123abc-4", false));
        // A directory that merely starts with the conflict-copy prefix must not carve
        // out its whole subtree.
        assert!(!f.allows(".sapphireignore.conflict-1/inner.txt", false));
    }
}
