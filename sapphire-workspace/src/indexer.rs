use std::{collections::HashSet, path::Path};

use sapphire_retrieve::{Chunker, Document, JsonChunker, RetrieveDb};

use crate::{error::Result, workspace::Workspace};

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
/// | Extension | Chunking | `line` in results |
/// |-----------|----------|-------------------|
/// | `md`, `markdown`, `txt`, `rst`, `org` | paragraph split | paragraph start line |
/// | `json` | message/element extraction | source line of element `{` |
/// | `jsonl` | one message per line | line index |
///
/// For JSON/JSONL files the body stored in the database is extracted message
/// text (no JSON syntax noise), and each chunk's `line` value in
/// [`ChunkSearchResult`](sapphire_retrieve::ChunkSearchResult) is the 0-based
/// source line number of that message in the original file.
pub fn sync_workspace(workspace: &Workspace, retrieve_db: &RetrieveDb) -> Result<(usize, usize)> {
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
            let body = text_chunks.iter().map(|c| c.text.as_str()).collect::<Vec<_>>().join("\n\n");
            // Build embed text: prepend title to each chunk (same as chunk_document).
            let chunks: Vec<(usize, usize, String)> = text_chunks
                .into_iter()
                .map(|c| {
                    let embed = if title.is_empty() {
                        c.text.clone()
                    } else {
                        format!("{title}\n\n{}", c.text)
                    };
                    (c.line, c.column, embed)
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
