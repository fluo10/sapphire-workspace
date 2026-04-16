use std::{collections::HashSet, path::Path, sync::Arc};

use sapphire_retrieve::{Chunker, Document, JsonChunker, RetrieveStore};

use crate::{error::Result, workspace::Workspace};

/// Return the mtime of `path` as seconds since UNIX epoch, or 0 on error.
fn file_mtime_secs(path: &Path) -> i64 {
    path.metadata()
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs() as i64
        })
        .unwrap_or(0)
}

const MARKDOWN_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "rst", "org"];
const JSON_EXTENSIONS: &[&str] = &["json", "jsonl"];

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
///
/// # Supported file types
///
/// | Extension | Chunking | line range in results |
/// |-----------|----------|-----------------------|
/// | `md`, `markdown`, `txt`, `rst`, `org` | paragraph split | start/end line of paragraph |
/// | `json` | message/element extraction | source line range of element |
/// | `jsonl` | one message per line | `line_start == line_end` |
pub fn sync_workspace(
    workspace: &Workspace,
    retrieve_db: Arc<dyn RetrieveStore + Send + Sync>,
) -> Result<(usize, usize)> {
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

        let is_markdown = MARKDOWN_EXTENSIONS.contains(&ext.as_str());
        let is_json = JSON_EXTENSIONS.contains(&ext.as_str());

        if !is_markdown && !is_json {
            continue;
        }

        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let doc_id = path_to_doc_id(path);

        let doc = if is_json {
            // For JSON/JSONL: extract message chunks with source positions.
            // The body is the extracted text (no JSON syntax), and each chunk
            // carries the source line of its origin in the file.
            let text_chunks = JsonChunker.chunk(&title, &raw);
            let body = text_chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            // Build embed text: prepend title to each chunk (same as chunk_document).
            let chunks: Vec<(usize, usize, String)> = text_chunks
                .into_iter()
                .map(|c| {
                    let embed = if title.is_empty() {
                        c.text.clone()
                    } else {
                        format!("{title}\n\n{}", c.text)
                    };
                    (c.line_start, c.line_end, embed)
                })
                .collect();
            Document {
                id: doc_id,
                title,
                body,
                path: path.to_string_lossy().into_owned(),
                chunks: Some(chunks),
            }
        } else {
            // For Markdown/text: use raw content as body, let backends auto-chunk.
            Document {
                id: doc_id,
                title,
                body: raw,
                path: path.to_string_lossy().into_owned(),
                chunks: None,
            }
        };

        retrieve_db.upsert_document(&doc)?;
        current_ids.insert(doc_id);
        upserted += 1;
    }

    retrieve_db.rebuild_fts()?;

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

/// Walk the workspace and update only files whose mtime has changed since the
/// last sync.  Also removes documents for files that no longer exist.
///
/// Returns `(upserted, removed)`.
///
/// Unlike [`sync_workspace`], this function compares filesystem mtimes against
/// the values stored in the retrieve DB and skips unchanged files, making it
/// much cheaper for periodic background refreshes.
pub fn sync_workspace_incremental(
    workspace: &Workspace,
    retrieve_db: Arc<dyn RetrieveStore + Send + Sync>,
) -> Result<(usize, usize)> {
    let known_mtimes = retrieve_db.file_mtimes().unwrap_or_default();
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

        let is_markdown = MARKDOWN_EXTENSIONS.contains(&ext.as_str());
        let is_json = JSON_EXTENSIONS.contains(&ext.as_str());

        if !is_markdown && !is_json {
            continue;
        }

        let doc_id = path_to_doc_id(path);
        current_ids.insert(doc_id);

        // Check mtime — skip if unchanged.
        let path_str = path.to_string_lossy();
        let disk_mtime = file_mtime_secs(path);
        if let Some(&cached_mtime) = known_mtimes.get(path_str.as_ref())
            && cached_mtime == disk_mtime
        {
            continue;
        }

        let raw = match std::fs::read_to_string(path) {
            Ok(s) => s,
            Err(_) => continue,
        };

        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let doc = if is_json {
            let text_chunks = JsonChunker.chunk(&title, &raw);
            let body = text_chunks
                .iter()
                .map(|c| c.text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n");
            let chunks: Vec<(usize, usize, String)> = text_chunks
                .into_iter()
                .map(|c| {
                    let embed = if title.is_empty() {
                        c.text.clone()
                    } else {
                        format!("{title}\n\n{}", c.text)
                    };
                    (c.line_start, c.line_end, embed)
                })
                .collect();
            Document {
                id: doc_id,
                title,
                body,
                path: path_str.into_owned(),
                chunks: Some(chunks),
            }
        } else {
            Document {
                id: doc_id,
                title,
                body: raw,
                path: path_str.into_owned(),
                chunks: None,
            }
        };

        retrieve_db.upsert_file(&doc.path, disk_mtime)?;
        retrieve_db.upsert_document(&doc)?;
        upserted += 1;
    }

    if upserted > 0 {
        retrieve_db.rebuild_fts()?;
    }

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
