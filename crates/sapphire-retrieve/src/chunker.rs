//! Text chunkers.
//!
//! This module provides the [`Chunker`] trait for splitting file content into
//! indexable text chunks, and built-in implementations for common file formats:
//!
//! - [`MarkdownChunker`] — paragraph-based chunker for Markdown/plain-text files.
//! - [`JsonlChunker`] — line-based chunker for JSONL files (one JSON object
//!   per line; AI conversation logs from apps such as SillyTavern, etc.).
//!
//! # Source positions
//!
//! Each [`TextChunk`] carries the line range
//! ([`line_start`](TextChunk::line_start), [`line_end`](TextChunk::line_end))
//! that the chunk occupies in the original file (both values are 0-based and
//! inclusive).  A GUI / AI client can use this range to jump to the chunk or
//! fetch the surrounding context without scanning the whole file.
//!
//! ## Examples
//!
//! | Format | `line_start` | `line_end` |
//! |--------|--------------|-----------|
//! | Markdown paragraph | line of the first non-blank char | line of the last non-blank char |
//! | JSONL line | line index of the JSON line | same |
//!
//! # Example
//!
//! ```no_run
//! use sapphire_retrieve::chunker::{Chunker, JsonlChunker, MarkdownChunker};
//!
//! let md = MarkdownChunker;
//! let chunks = md.chunk("My note", "First paragraph.\n\nSecond paragraph.");
//! assert_eq!(chunks[0].line_start, 0);
//! assert_eq!(chunks[1].line_start, 2);
//!
//! let jsonl = JsonlChunker;
//! let log = "{\"role\":\"user\",\"content\":\"Hello\"}\n{\"role\":\"assistant\",\"content\":\"Hi\"}";
//! let chunks = jsonl.chunk("chat.jsonl", log);
//! assert_eq!(chunks[0].line_start, 0);
//! assert_eq!(chunks[1].line_start, 1);
//! ```

/// A single chunk of text extracted from a file, with the source line range it
/// occupies (both 0-based, inclusive).
#[derive(Debug, Clone)]
pub struct TextChunk {
    /// First source line of the chunk (inclusive, 0-based).
    pub line_start: usize,
    /// Last source line of the chunk (inclusive, 0-based).
    ///
    /// For single-line chunks (JSONL entries, one-line paragraphs), equal to
    /// `line_start`.
    pub line_end: usize,
    /// Extracted, human-readable text content (no JSON syntax noise).
    ///
    /// Internal `\n\n` sequences are normalised to `\n` so that the storage
    /// layer can safely use `\n\n` as the inter-chunk separator.
    pub text: String,
}

// ── Chunker trait ─────────────────────────────────────────────────────────────

/// Splits the content of a single file into indexable [`TextChunk`]s.
///
/// Implementations decide the granularity (paragraphs for Markdown, messages
/// for conversation JSON, …).
///
/// # Contract
///
/// - Returns at least one chunk even for empty/unparseable content.
/// - Chunks are returned in source order.
/// - `TextChunk::text` must **not** contain `"\n\n"`; use `"\n"` instead, so
///   the storage layer can use `"\n\n"` as the inter-chunk boundary.
/// - `TextChunk::line_start` must be unique across all chunks returned for a
///   given file (stored as the unique key in the database).
/// - `line_end >= line_start`.
pub trait Chunker: Send + Sync {
    /// Split `content` (raw file bytes as UTF-8) into chunks.
    ///
    /// `title` is the document title (usually the file name).
    fn chunk(&self, title: &str, content: &str) -> Vec<TextChunk>;
}

// ── helpers shared by both chunkers ──────────────────────────────────────────

/// Replace `"\n\n"` with `"\n"` and trim the result.
///
/// This is required by the [`Chunker`] contract: the storage layer uses
/// `"\n\n"` as the inter-chunk separator, so individual chunks must not
/// contain that sequence.
fn normalise(s: &str) -> String {
    s.replace("\n\n", "\n").trim().to_owned()
}

/// Count `'\n'` characters in `s`.
#[inline]
fn count_newlines(s: &str) -> usize {
    s.bytes().filter(|&b| b == b'\n').count()
}

// ── MarkdownChunker ───────────────────────────────────────────────────────────

/// Paragraph-based chunker for Markdown and plain-text files.
///
/// Splits on blank lines (`\n\n`), records the source line range of each
/// paragraph.
pub struct MarkdownChunker;

impl Chunker for MarkdownChunker {
    fn chunk(&self, title: &str, content: &str) -> Vec<TextChunk> {
        let mut chunks: Vec<TextChunk> = Vec::new();
        let mut abs_line: usize = 0;
        let mut remaining = content;

        loop {
            match remaining.find("\n\n") {
                Some(sep_pos) => {
                    let part = &remaining[..sep_pos];
                    push_para_chunk(&mut chunks, part, abs_line);
                    abs_line += count_newlines(part) + 2;
                    remaining = &remaining[sep_pos + 2..];
                }
                None => {
                    push_para_chunk(&mut chunks, remaining, abs_line);
                    break;
                }
            }
        }

        if chunks.is_empty() {
            vec![TextChunk {
                line_start: 0,
                line_end: 0,
                text: title.to_owned(),
            }]
        } else {
            chunks
        }
    }
}

/// Extract one paragraph chunk from `part`, adding it to `chunks` if non-empty.
///
/// `abs_line` is the line number of the first character of `part` in the
/// original document.
fn push_para_chunk(chunks: &mut Vec<TextChunk>, part: &str, abs_line: usize) {
    let trimmed = part.trim();
    if trimmed.is_empty() {
        return;
    }
    let leading = &part[..part.len() - part.trim_start().len()];
    let leading_newlines = count_newlines(leading);
    let line_start = abs_line + leading_newlines;
    let line_end = line_start + count_newlines(trimmed);
    chunks.push(TextChunk {
        line_start,
        line_end,
        text: trimmed.to_owned(),
    });
}

// ── JsonlChunker ──────────────────────────────────────────────────────────────

/// Line-based chunker for JSONL files (one JSON object per line).
///
/// Each non-empty line becomes one chunk with
/// `line_start == line_end == <line index>`.  This makes JSONL log appends
/// maximally cache-friendly: existing lines retain their identity (keyed by
/// `line_start`), so only newly-appended lines need to be re-embedded.
///
/// Within each line the chunker extracts human-readable text by probing common
/// field names (matches typical chat-log shapes from SillyTavern, OpenAI, etc.):
///
/// | Priority | Content field | Speaker/role field |
/// |----------|---------------|--------------------|
/// | 1st | `"mes"` (SillyTavern) | `"name"` |
/// | 2nd | `"content"` (OpenAI) | `"role"` |
/// | 3rd | `"message"` | `"speaker"` |
/// | 4th | `"text"` | `"author"` |
/// | fallback | string-valued fields joined | — |
///
/// The resulting `text` has the form `"{speaker}: {content}"` when a speaker
/// field is present, otherwise just `"{content}"`.  Lines that fail to parse
/// as JSON are kept as raw text so partial writes don't break indexing.
pub struct JsonlChunker;

impl Chunker for JsonlChunker {
    fn chunk(&self, title: &str, content: &str) -> Vec<TextChunk> {
        let mut chunks: Vec<TextChunk> = Vec::new();
        for (line_idx, line_text) in content.lines().enumerate() {
            if line_text.trim().is_empty() {
                continue;
            }
            let text = match serde_json::from_str::<serde_json::Value>(line_text) {
                Ok(v) => extract_message_text(&v),
                Err(_) => normalise(line_text),
            };
            if text.is_empty() {
                continue;
            }
            chunks.push(TextChunk {
                line_start: line_idx,
                line_end: line_idx,
                text,
            });
        }

        if chunks.is_empty() {
            vec![TextChunk {
                line_start: 0,
                line_end: 0,
                text: title.to_owned(),
            }]
        } else {
            chunks
        }
    }
}

// ── message text extraction ───────────────────────────────────────────────────

fn extract_message_text(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return normalise(&value.to_string()),
    };

    const CONTENT_KEYS: &[&str] = &["mes", "content", "message", "text"];
    let content = CONTENT_KEYS
        .iter()
        .find_map(|k| obj.get(*k)?.as_str())
        .map(str::to_owned);

    const ROLE_KEYS: &[&str] = &["name", "role", "speaker", "author"];
    let role = ROLE_KEYS
        .iter()
        .find_map(|k| obj.get(*k)?.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    match (role, content) {
        (Some(r), Some(c)) => normalise(&format!("{r}: {c}")),
        (None, Some(c)) => normalise(&c),
        _ => {
            let fallback = obj
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| format!("{k}: {s}")))
                .collect::<Vec<_>>()
                .join("\n");
            if fallback.is_empty() {
                normalise(&serde_json::to_string(value).unwrap_or_default())
            } else {
                normalise(&fallback)
            }
        }
    }
}

// ── legacy free function (kept for storage backends) ─────────────────────────

/// Split a document into embeddable text chunks.
///
/// This function is used internally by the SQLite and LanceDB storage backends
/// as the default chunking strategy when no pre-computed chunks are provided.
/// New code should prefer the [`Chunker`] trait.
pub fn chunk_document(body: &str) -> Vec<String> {
    let paragraphs: Vec<&str> = body
        .split("\n\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if paragraphs.is_empty() {
        return Vec::new();
    }

    paragraphs.iter().map(|p| p.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_paragraphs() {
        let chunks = chunk_document("First.\n\nSecond.");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "First.");
        assert_eq!(chunks[1], "Second.");
    }

    #[test]
    fn empty_body_returns_empty() {
        let chunks = chunk_document("");
        assert!(chunks.is_empty());
    }

    #[test]
    fn blank_only_body_returns_empty() {
        let chunks = chunk_document("   \n\n   ");
        assert!(chunks.is_empty());
    }

    #[test]
    fn single_paragraph() {
        let chunks = chunk_document("Only body.");
        assert_eq!(chunks, vec!["Only body."]);
    }

    // ── MarkdownChunker ──────────────────────────────────────────────────────

    #[test]
    fn markdown_line_range_single_line() {
        let c = MarkdownChunker;
        let chunks = c.chunk("Title", "First.\n\nSecond.");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 0);
        assert_eq!(chunks[0].text, "First.");
        assert_eq!(chunks[1].line_start, 2);
        assert_eq!(chunks[1].line_end, 2);
        assert_eq!(chunks[1].text, "Second.");
    }

    #[test]
    fn markdown_multiline_paragraph() {
        let c = MarkdownChunker;
        // "A\nB\nC" is a single paragraph spanning lines 0-2.
        let chunks = c.chunk("T", "A\nB\nC\n\nD");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 2);
        assert_eq!(chunks[0].text, "A\nB\nC");
        assert_eq!(chunks[1].line_start, 4);
        assert_eq!(chunks[1].line_end, 4);
    }

    #[test]
    fn markdown_leading_blank_lines() {
        let c = MarkdownChunker;
        let chunks = c.chunk("T", "\n\nActual paragraph.");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 2);
        assert_eq!(chunks[0].line_end, 2);
    }

    #[test]
    fn markdown_empty_body_returns_title() {
        let c = MarkdownChunker;
        let chunks = c.chunk("Title", "");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 0);
        assert_eq!(chunks[0].text, "Title");
    }

    // ── JsonlChunker ─────────────────────────────────────────────────────────

    #[test]
    fn jsonl_line_ranges() {
        let c = JsonlChunker;
        let jsonl = concat!(
            "{\"name\":\"User\",\"is_user\":true,\"mes\":\"Hello there\"}\n",
            "{\"name\":\"Aria\",\"is_user\":false,\"mes\":\"Hi! How can I help?\"}"
        );
        let chunks = c.chunk("chat.jsonl", jsonl);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 0);
        assert_eq!(chunks[1].line_start, 1);
        assert_eq!(chunks[1].line_end, 1);
        assert!(chunks[0].text.contains("Hello there"));
        assert!(chunks[1].text.contains("Hi! How can I help?"));
    }

    #[test]
    fn jsonl_single_line() {
        let c = JsonlChunker;
        let jsonl = "{\"role\":\"user\",\"content\":\"Just one message\"}";
        let chunks = c.chunk("single.jsonl", jsonl);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 0);
        assert!(chunks[0].text.contains("Just one message"));
    }

    #[test]
    fn jsonl_skips_blank_lines_preserves_indices() {
        // Blank lines must not produce chunks, but surviving chunks retain
        // their absolute line index so cache identity is preserved across
        // unrelated edits.
        let c = JsonlChunker;
        let jsonl = "{\"content\":\"first\"}\n\n{\"content\":\"third\"}";
        let chunks = c.chunk("log.jsonl", jsonl);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[1].line_start, 2);
    }

    #[test]
    fn jsonl_unparseable_line_falls_back_to_raw() {
        let c = JsonlChunker;
        // Second line is mid-write garbage — keep it as raw text instead of
        // dropping it entirely.
        let jsonl = "{\"content\":\"ok\"}\n{not json";
        let chunks = c.chunk("log.jsonl", jsonl);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[1].line_start, 1);
        assert!(chunks[1].text.contains("not json"));
    }

    #[test]
    fn jsonl_append_preserves_existing_line_starts() {
        // Append-stability is the whole reason this chunker exists: appending
        // a new line must not change the (line_start, text) tuple of any
        // existing chunk, so the storage layer's chunk-level diff treats the
        // old chunks as unchanged and skips re-embedding.
        let c = JsonlChunker;
        let before = "{\"content\":\"a\"}\n{\"content\":\"b\"}";
        let after = "{\"content\":\"a\"}\n{\"content\":\"b\"}\n{\"content\":\"c\"}";
        let before_chunks = c.chunk("log.jsonl", before);
        let after_chunks = c.chunk("log.jsonl", after);
        assert_eq!(before_chunks.len(), 2);
        assert_eq!(after_chunks.len(), 3);
        for (b, a) in before_chunks.iter().zip(after_chunks.iter()) {
            assert_eq!(b.line_start, a.line_start);
            assert_eq!(b.text, a.text);
        }
    }

    #[test]
    fn jsonl_empty_body_returns_title() {
        let c = JsonlChunker;
        let chunks = c.chunk("empty.jsonl", "");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text, "empty.jsonl");
    }

    #[test]
    fn jsonl_text_has_no_double_newline() {
        let c = JsonlChunker;
        let jsonl = concat!(
            "{\"role\":\"user\",\"content\":\"Line one\\n\\nLine two\"}\n",
            "{\"role\":\"assistant\",\"content\":\"OK\"}"
        );
        let chunks = c.chunk("chat.jsonl", jsonl);
        for ch in &chunks {
            assert!(!ch.text.contains("\n\n"));
        }
    }
}
