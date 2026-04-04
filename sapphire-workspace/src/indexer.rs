use std::{collections::HashSet, path::Path};

use sapphire_retrieve::{Document, RetrieveDb};

use crate::{error::Result, workspace::Workspace};

const SUPPORTED_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "rst", "org"];

/// Generate a stable `i64` document ID from a file path (FNV-1a).
pub fn path_to_doc_id(path: &Path) -> i64 {
    const OFFSET: u64 = 14695981039346656037;
    const PRIME: u64 = 1099511628211;
    let mut h = OFFSET;
    for b in path.as_os_str().as_encoded_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    h as i64
}

/// Recursively walk `workspace` and upsert all text files into `retrieve_db`.
///
/// Returns `(upserted, removed)`.
pub fn sync_workspace(workspace: &Workspace, retrieve_db: &RetrieveDb) -> Result<(usize, usize)> {
    // Collect existing doc IDs before we start so we can detect deletions.
    let existing_ids: HashSet<i64> = retrieve_db
        .document_ids()
        .unwrap_or_default()
        .into_iter()
        .collect();

    let mut current_ids: HashSet<i64> = HashSet::new();
    let mut upserted = 0;

    for entry in walkdir::WalkDir::new(&workspace.root)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| {
            // Skip hidden directories (.git, .obsidian, etc.)
            if e.file_type().is_dir() {
                !e.file_name().to_string_lossy().starts_with('.')
            } else {
                true
            }
        })
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();

        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if !SUPPORTED_EXTENSIONS.contains(&ext.as_str()) {
            continue;
        }

        let body = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue, // skip unreadable files (binary, permission denied, etc.)
        };

        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let doc_id = path_to_doc_id(path);

        retrieve_db.upsert_document(&Document {
            id: doc_id,
            title,
            body,
            path: path.to_string_lossy().into_owned(),
        })?;
        current_ids.insert(doc_id);
        upserted += 1;
    }

    retrieve_db.rebuild_fts()?;

    // Remove documents whose files no longer exist.
    let mut removed = 0;
    for id in &existing_ids {
        if !current_ids.contains(id) {
            retrieve_db.remove_document(*id)?;
            removed += 1;
        }
    }
    if removed > 0 {
        retrieve_db.rebuild_fts()?;
    }

    Ok((upserted, removed))
}
