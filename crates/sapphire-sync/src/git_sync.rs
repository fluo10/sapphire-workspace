use std::path::{Path, PathBuf};

use git2::{MergeOptions, Repository, Signature};

use crate::{Error, Result, SyncBackend};

/// Git-based sync backend: stages file changes via `libgit2`.
///
/// Each operation re-opens the repository to avoid holding a non-`Sync` handle.
pub struct GitSync {
    /// Starting path for repository discovery (repo root or any subdirectory).
    search_path: PathBuf,
    /// Remote name (default: "origin").
    remote: String,
    /// Commit message used when staging changes (default: `"auto: sync"`).
    commit_message: String,
}

impl GitSync {
    /// Discover a git repository from `path` (walks up the directory tree).
    ///
    /// Returns an error if no repository is found.
    pub fn open(path: &Path) -> Result<Self> {
        Repository::discover(path).map_err(|_| Error::NoRepository {
            path: path.to_owned(),
        })?;
        Ok(Self {
            search_path: path.to_owned(),
            remote: "origin".to_owned(),
            commit_message: "auto: sync".to_owned(),
        })
    }

    /// Create a `GitSync` that pushes/pulls against the specified remote.
    pub fn with_remote(path: &Path, remote: &str) -> Result<Self> {
        Repository::discover(path).map_err(|_| Error::NoRepository {
            path: path.to_owned(),
        })?;
        Ok(Self {
            search_path: path.to_owned(),
            remote: remote.to_owned(),
            commit_message: "auto: sync".to_owned(),
        })
    }

    /// Override the commit message used when staging changes.
    pub fn with_commit_message(mut self, message: impl Into<String>) -> Self {
        self.commit_message = message.into();
        self
    }

    fn with_repo<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Repository, &Path) -> Result<T>,
    {
        let repo = Repository::discover(&self.search_path).map_err(|_| Error::NoRepository {
            path: self.search_path.clone(),
        })?;
        let workdir = repo.workdir().ok_or(Error::BareRepository)?;
        f(&repo, workdir)
    }

    /// Returns `true` when the index contains staged (but not yet committed) changes.
    fn has_staged_changes(repo: &Repository) -> Result<bool> {
        let head = match repo.head() {
            Ok(h) => Some(h.peel_to_commit()?),
            // Empty repo (no commits yet) — any staged content counts.
            Err(_) => None,
        };

        let diff = if let Some(commit) = &head {
            let tree = commit.tree()?;
            repo.diff_tree_to_index(Some(&tree), None, None)?
        } else {
            repo.diff_tree_to_index(None, None, None)?
        };

        Ok(diff.deltas().len() > 0)
    }

    /// Commit all staged changes.  No-op when nothing is staged.
    fn commit_staged(repo: &Repository, message: &str) -> Result<()> {
        if !Self::has_staged_changes(repo)? {
            return Ok(());
        }

        let mut index = repo.index()?;
        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;

        let sig = repo
            .signature()
            .unwrap_or_else(|_| Signature::now("sapphire-workspace", "sync@sapphire").unwrap());

        let parent_commit = repo.head().ok().and_then(|h| h.peel_to_commit().ok());
        let parents: Vec<&git2::Commit> = parent_commit.iter().collect();

        repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parents)?;
        Ok(())
    }

    /// Fetch from remote, merge the remote tracking branch into HEAD, then push.
    ///
    /// Conflict resolution: when two sides modify the same file, the version
    /// from the commit with the newer (higher) author timestamp is kept.
    fn sync_git(repo: &Repository, remote_name: &str) -> Result<()> {
        // ── 1. Fetch ──────────────────────────────────────────────────────────
        let mut remote = repo
            .find_remote(remote_name)
            .map_err(|_| Error::RemoteNotFound {
                name: remote_name.to_owned(),
            })?;

        remote.fetch(&[] as &[&str], None, None)?;

        // ── 2. Find the remote tracking ref ──────────────────────────────────
        let fetch_head = repo.find_reference("FETCH_HEAD")?;
        let remote_commit = fetch_head.peel_to_commit()?;

        // Check if local HEAD is already up-to-date.
        let head_commit = match repo.head().and_then(|h| h.peel_to_commit()) {
            Ok(c) => c,
            // No local commits yet — nothing to merge, nothing to push.
            Err(_) => return Ok(()),
        };

        if head_commit.id() == remote_commit.id() {
            return Ok(()); // already in sync
        }

        // ── 3. Merge ──────────────────────────────────────────────────────────
        let remote_annotated = repo.find_annotated_commit(remote_commit.id())?;
        let analysis = repo.merge_analysis(&[&remote_annotated])?;

        if analysis.0.is_up_to_date() {
            // Nothing to pull; fall through to push.
        } else if analysis.0.is_fast_forward() {
            // Simple fast-forward.
            let mut head_ref = repo.head()?;
            head_ref.set_target(remote_commit.id(), "fast-forward")?;
            repo.set_head(head_ref.name().unwrap_or("HEAD"))?;
            repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))?;
        } else {
            // Three-way merge.
            let mut merge_opts = MergeOptions::new();
            merge_opts.fail_on_conflict(false);
            repo.merge(
                &[&remote_annotated],
                Some(&mut merge_opts),
                Some(git2::build::CheckoutBuilder::default().allow_conflicts(true)),
            )?;

            // Resolve any conflicts by timestamp: keep the version from the
            // commit with the newer author time.
            let our_time = head_commit.author().when().seconds();
            let their_time = remote_commit.author().when().seconds();
            let use_ours = our_time >= their_time;

            let index = repo.index()?;
            let conflicts: Vec<_> = index.conflicts()?.collect::<std::result::Result<_, _>>()?;

            if !conflicts.is_empty() {
                let mut idx = repo.index()?;
                for conflict in &conflicts {
                    let path = conflict
                        .our
                        .as_ref()
                        .or(conflict.their.as_ref())
                        .map(|e| PathBuf::from(std::str::from_utf8(&e.path).unwrap_or("")))
                        .unwrap_or_default();

                    if use_ours {
                        repo.checkout_head(Some(
                            git2::build::CheckoutBuilder::default()
                                .allow_conflicts(true)
                                .use_ours(true)
                                .path(&path),
                        ))?;
                    } else {
                        repo.checkout_head(Some(
                            git2::build::CheckoutBuilder::default()
                                .allow_conflicts(true)
                                .use_theirs(true)
                                .path(&path),
                        ))?;
                    }
                    idx.add_path(&path)?;
                }
                idx.write()?;
            }

            // Create the merge commit.
            let sig = repo
                .signature()
                .unwrap_or_else(|_| Signature::now("sapphire-workspace", "sync@sapphire").unwrap());
            let mut idx = repo.index()?;
            let tree_oid = idx.write_tree()?;
            let tree = repo.find_tree(tree_oid)?;
            repo.commit(
                Some("HEAD"),
                &sig,
                &sig,
                "merge: remote sync",
                &tree,
                &[&head_commit, &remote_commit],
            )?;
            repo.cleanup_state()?;
        }

        // ── 4. Push ───────────────────────────────────────────────────────────
        let head_name = repo.head()?.shorthand().unwrap_or("main").to_owned();
        let refspec = format!("refs/heads/{head_name}:refs/heads/{head_name}");
        let mut remote = repo.find_remote(remote_name)?;
        remote.push(&[refspec.as_str()], None)?;

        Ok(())
    }
}

impl SyncBackend for GitSync {
    fn add_file(&self, path: &Path) -> Result<()> {
        self.with_repo(|repo, workdir| {
            let relative = path
                .strip_prefix(workdir)
                .map_err(|_| Error::PathOutsideWorkdir {
                    path: path.to_owned(),
                    workdir: workdir.to_owned(),
                })?;
            let mut index = repo.index()?;
            index.add_path(relative)?;
            index.write()?;
            Ok(())
        })
    }

    fn remove_file(&self, path: &Path) -> Result<()> {
        self.with_repo(|repo, workdir| {
            let relative = path
                .strip_prefix(workdir)
                .map_err(|_| Error::PathOutsideWorkdir {
                    path: path.to_owned(),
                    workdir: workdir.to_owned(),
                })?;
            let mut index = repo.index()?;
            index.remove_path(relative)?;
            index.write()?;
            Ok(())
        })
    }

    /// Full git sync cycle: commit staged changes → fetch+merge remote → push.
    fn sync(&self) -> Result<()> {
        let remote = self.remote.clone();
        let message = self.commit_message.clone();
        self.with_repo(|repo, _workdir| {
            Self::commit_staged(repo, &message)?;
            Self::sync_git(repo, &remote)?;
            Ok(())
        })
    }
}
