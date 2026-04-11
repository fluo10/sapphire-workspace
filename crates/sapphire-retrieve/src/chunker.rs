//! Text chunkers.
//!
//! This module provides the [`Chunker`] trait for splitting file content into
//! indexable text chunks, and built-in implementations for common file formats:
//!
//! - [`MarkdownChunker`] — paragraph-based chunker for Markdown/plain-text files.
//! - [`JsonChunker`] — message-level chunker for JSON and JSONL files (AI
//!   conversation logs from apps such as SillyTavern, Zerolaw, etc.).
//!
//! # Source positions
//!
//! Each [`TextChunk`] carries the exact source position ([`line`] and
//! [`column`]) of where that chunk begins in the original file.  The
//! [`line`](TextChunk::line) value is stored in the `line` column of the
//! database so [`ChunkSearchResult::line`][crate::vector_store::ChunkSearchResult::line]
//! is always a 0-based **source line number**, regardless of file format.
//!
//! This lets a GUI jump directly to `line:column` in the source file, and
//! render surrounding context (like `git diff`) without any extra lookup step.
//!
//! ## Examples
//!
//! | Format | `line` | `column` |
//! |--------|--------|---------|
//! | Markdown paragraph | line of the first non-blank character | 0 |
//! | JSONL line | line index of the JSON line | 0 |
//! | JSON array element | line of the opening `{` | col of `{` |
//!
//! # Example
//!
//! ```no_run
//! use sapphire_retrieve::chunker::{Chunker, JsonChunker, MarkdownChunker};
//!
//! let md = MarkdownChunker;
//! let chunks = md.chunk("My note", "First paragraph.\n\nSecond paragraph.");
//! assert_eq!(chunks[0].line, 0);
//! assert_eq!(chunks[1].line, 2);
//!
//! let json = JsonChunker;
//! let jsonl = "{\"role\":\"user\",\"content\":\"Hello\"}\n{\"role\":\"assistant\",\"content\":\"Hi\"}";
//! let chunks = json.chunk("chat.jsonl", jsonl);
//! assert_eq!(chunks[0].line, 0);
//! assert_eq!(chunks[1].line, 1);
//! ```

/// A single chunk of text extracted from a file, carrying its precise source
/// location.
///
/// The [`line`](Self::line) field is stored in the `line` column of the
/// database so `ChunkSearchResult::line` from a vector search is always a
/// source line number that a GUI can navigate to directly.
#[derive(Debug, Clone)]
pub struct TextChunk {
    /// Zero-based line number in the source file where this chunk begins.
    ///
    /// This value is stored in the `line` column of the database.
    pub line: usize,
    /// Zero-based byte offset within `line` where this chunk begins.
    ///
    /// For Markdown paragraphs and JSONL this is always `0`.  For elements
    /// inside a single-line or compactly serialised JSON array it may be
    /// non-zero.
    pub column: usize,
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
/// for conversation JSON, …).  The [`line`](TextChunk::line) field of each
/// chunk must be the 0-based line number of that chunk's start in the original
/// file.
///
/// # Contract
///
/// - Returns at least one chunk even for empty/unparseable content.
/// - Chunks are returned in source order.
/// - `TextChunk::text` must **not** contain `"\n\n"`; use `"\n"` instead, so
///   the storage layer can use `"\n\n"` as the inter-chunk boundary.
/// - `TextChunk::line` must be unique across all chunks returned for a given
///   file (stored as the `line` column in the database).
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
/// Splits on blank lines (`\n\n`), records the source line of each
/// paragraph's first non-blank character.
pub struct MarkdownChunker;

impl Chunker for MarkdownChunker {
    fn chunk(&self, title: &str, content: &str) -> Vec<TextChunk> {
        let mut chunks: Vec<TextChunk> = Vec::new();
        // Track absolute line number as we consume the content.
        let mut abs_line: usize = 0;
        let mut remaining = content;

        loop {
            match remaining.find("\n\n") {
                Some(sep_pos) => {
                    let part = &remaining[..sep_pos];
                    push_para_chunk(&mut chunks, part, abs_line);
                    abs_line += count_newlines(part) + 2; // +2 for the \n\n sep
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
                line: 0,
                column: 0,
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
    // Find how many leading newlines the trim() removed so we can compute
    // the exact line where the non-blank content starts.
    let leading = &part[..part.len() - part.trim_start().len()];
    let leading_newlines = count_newlines(leading);
    chunks.push(TextChunk {
        line: abs_line + leading_newlines,
        column: 0,
        text: trimmed.to_owned(),
    });
}

// ── JsonChunker ───────────────────────────────────────────────────────────────

/// Message-level chunker for JSON and JSONL files.
///
/// Handles three layouts automatically:
///
/// 1. **JSONL** (one JSON object per line) — each line becomes one chunk.
///    `line` == the line index in the file.  Suitable for SillyTavern chat
///    logs (`*.jsonl`) and similar formats.
/// 2. **JSON top-level array** — each element becomes one chunk.  The source
///    `line`/`column` of each element's opening `{` or `[` is tracked by
///    scanning the raw text.
/// 3. **JSON object with a messages/chat field** — extracts the nested array
///    (`"messages"`, `"chat"`, `"history"`, or `"log"` key) and treats each
///    element as a chunk.  `line` is the line of each element's opening `{`.
///    Falls back to a single chunk for other objects.
///
/// Within each JSON element the chunker extracts human-readable text by
/// probing common field names:
///
/// | Priority | Content field | Speaker/role field |
/// |----------|---------------|--------------------|
/// | 1st | `"mes"` (SillyTavern) | `"name"` |
/// | 2nd | `"content"` (OpenAI) | `"role"` |
/// | 3rd | `"message"` | `"speaker"` |
/// | 4th | `"text"` | `"author"` |
/// | fallback | string-valued fields joined | — |
///
/// The resulting `text` for each chunk has the form `"{speaker}: {content}"`
/// when a speaker field is present, otherwise just `"{content}"`.
pub struct JsonChunker;

impl Chunker for JsonChunker {
    fn chunk(&self, _title: &str, content: &str) -> Vec<TextChunk> {
        // Try JSONL first: multiple non-empty lines each parseable as JSON.
        if let Some(chunks) = try_parse_jsonl(content) {
            return chunks;
        }

        // Fall back to a single JSON value.
        match serde_json::from_str::<serde_json::Value>(content) {
            Ok(value) => chunks_from_json_value(content, &value),
            Err(_) => {
                vec![TextChunk {
                    line: 0,
                    column: 0,
                    text: normalise(content),
                }]
            }
        }
    }
}

// ── JSONL parsing ─────────────────────────────────────────────────────────────

/// Try to parse `content` as JSONL (newline-delimited JSON).
///
/// Returns `None` if the content is not JSONL (e.g. fewer than 2 non-empty
/// lines, or any non-empty line is not valid JSON).
fn try_parse_jsonl(content: &str) -> Option<Vec<TextChunk>> {
    let non_empty_lines: Vec<(usize, &str)> = content
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();

    if non_empty_lines.len() < 2 {
        return None;
    }

    let mut chunks = Vec::with_capacity(non_empty_lines.len());
    for (line_idx, line_text) in &non_empty_lines {
        match serde_json::from_str::<serde_json::Value>(line_text) {
            Ok(v) => chunks.push(TextChunk {
                line: *line_idx,
                column: 0,
                text: extract_message_text(&v),
            }),
            Err(_) => return None, // not valid JSONL
        }
    }
    Some(chunks)
}

// ── JSON value → chunks ───────────────────────────────────────────────────────

/// Build chunks from a parsed JSON value, tracking source positions in `raw`.
fn chunks_from_json_value(raw: &str, value: &serde_json::Value) -> Vec<TextChunk> {
    match value {
        serde_json::Value::Array(arr) => {
            let positions = find_array_element_positions(raw);
            arr.iter()
                .enumerate()
                .map(|(i, v)| {
                    let (line, column) = positions.get(i).copied().unwrap_or((i, 0));
                    TextChunk {
                        line,
                        column,
                        text: extract_message_text(v),
                    }
                })
                .collect()
        }
        serde_json::Value::Object(map) => {
            // Look for a nested messages/chat array.
            const ARRAY_KEYS: &[&str] = &["messages", "chat", "history", "log"];
            for key in ARRAY_KEYS {
                if let Some(serde_json::Value::Array(arr)) = map.get(*key)
                    && !arr.is_empty()
                {
                    // Find the position of this key's array in the raw text,
                    // then scan for element positions within it.
                    let nested_positions = find_nested_array_positions(raw, key);
                    return arr
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let (line, column) = nested_positions.get(i).copied().unwrap_or((i, 0));
                            TextChunk {
                                line,
                                column,
                                text: extract_message_text(v),
                            }
                        })
                        .collect();
                }
            }
            // Single object — one chunk at the start of the file.
            vec![TextChunk {
                line: 0,
                column: 0,
                text: extract_message_text(value),
            }]
        }
        other => vec![TextChunk {
            line: 0,
            column: 0,
            text: normalise(&other.to_string()),
        }],
    }
}

// ── source-position scanning ──────────────────────────────────────────────────

/// Scan `raw` JSON text for the positions of top-level array elements.
///
/// Returns `(line, column)` for the opening `{`, `[`, `"`, or digit of each
/// element in the top-level array.
fn find_array_element_positions(raw: &str) -> Vec<(usize, usize)> {
    // Find the opening '[' of the top-level array.
    let start = match raw.find('[') {
        Some(i) => i + 1,
        None => return vec![],
    };
    scan_array_element_positions(raw, start)
}

/// Same as [`find_array_element_positions`] but first seeks to the value of
/// `key` inside the top-level object.
fn find_nested_array_positions(raw: &str, key: &str) -> Vec<(usize, usize)> {
    // Look for `"key"` followed (after optional whitespace/colon) by `[`.
    let needle = format!("\"{key}\"");
    let key_pos = match raw.find(&needle) {
        Some(p) => p + needle.len(),
        None => return vec![],
    };
    let after_key = &raw[key_pos..];
    let bracket_offset = match after_key.find('[') {
        Some(i) => i + 1,
        None => return vec![],
    };
    let abs_start = key_pos + bracket_offset;
    scan_array_element_positions(raw, abs_start)
}

/// Scan `raw[start..]` for the (line, column) of each direct child element of
/// a JSON array.  `start` is the byte offset just after the opening `[`.
fn scan_array_element_positions(raw: &str, start: usize) -> Vec<(usize, usize)> {
    let mut positions: Vec<(usize, usize)> = Vec::new();
    let bytes = raw.as_bytes();
    let len = bytes.len();

    // Track line/column up to `start`.
    let prefix = &raw[..start.min(len)];
    let mut line = count_newlines(prefix);
    let last_nl = prefix.rfind('\n').map(|p| p + 1).unwrap_or(0);
    let mut col = start.min(len).saturating_sub(last_nl);

    let mut depth: i32 = 0; // depth relative to the array we're scanning
    let mut in_string = false;
    let mut escaped = false;
    let mut i = start.min(len);

    while i < len {
        let b = bytes[i];

        if escaped {
            escaped = false;
            if b == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }

        if in_string {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            if b == b'\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                if depth == 0 {
                    positions.push((line, col));
                }
                col += 1;
            }
            b'{' | b'[' => {
                if depth == 0 {
                    positions.push((line, col));
                }
                depth += 1;
                col += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                if depth < 0 {
                    // End of the parent array — stop.
                    break;
                }
                col += 1;
            }
            b'\n' => {
                line += 1;
                col = 0;
            }
            b'0'..=b'9' | b'-' | b't' | b'f' | b'n' if depth == 0 => {
                // Scalar value at top level of array.
                positions.push((line, col));
                // Skip until comma or ']'
                col += 1;
            }
            _ => {
                col += 1;
            }
        }
        i += 1;
    }

    positions
}

// ── message text extraction ───────────────────────────────────────────────────

/// Extract a human-readable string from a JSON value representing one message.
fn extract_message_text(value: &serde_json::Value) -> String {
    let obj = match value.as_object() {
        Some(o) => o,
        None => return normalise(&value.to_string()),
    };

    // Extract the main text content.
    const CONTENT_KEYS: &[&str] = &["mes", "content", "message", "text"];
    let content = CONTENT_KEYS
        .iter()
        .find_map(|k| obj.get(*k)?.as_str())
        .map(str::to_owned);

    // Extract the speaker / role.
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
            // Fall back: join all string-valued fields.
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
///
/// Splitting rules:
///
/// 1. The body is split on blank lines (`\n\n`).
/// 2. The title is prepended to every chunk.
/// 3. If the body is empty, a single title-only chunk is returned.
///
/// # Example
///
/// ```
/// # use sapphire_retrieve::chunker::chunk_document;
/// let chunks = chunk_document("Meeting notes", "First item.\n\nSecond item.");
/// assert_eq!(chunks.len(), 2);
/// assert_eq!(chunks[0], "Meeting notes\n\nFirst item.");
/// assert_eq!(chunks[1], "Meeting notes\n\nSecond item.");
/// ```
pub fn chunk_document(title: &str, body: &str) -> Vec<String> {
    let paragraphs: Vec<&str> = body
        .split("\n\n")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    if paragraphs.is_empty() {
        return vec![title.to_owned()];
    }

    paragraphs
        .iter()
        .map(|p| {
            if title.is_empty() {
                p.to_string()
            } else {
                format!("{title}\n\n{p}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── chunk_document (legacy) ──────────────────────────────────────────────

    #[test]
    fn two_paragraphs() {
        let chunks = chunk_document("Title", "First.\n\nSecond.");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], "Title\n\nFirst.");
        assert_eq!(chunks[1], "Title\n\nSecond.");
    }

    #[test]
    fn empty_body_returns_title() {
        let chunks = chunk_document("Title", "");
        assert_eq!(chunks, vec!["Title"]);
    }

    #[test]
    fn blank_only_body_returns_title() {
        let chunks = chunk_document("Title", "   \n\n   ");
        assert_eq!(chunks, vec!["Title"]);
    }

    #[test]
    fn filters_empty_paragraphs() {
        let chunks = chunk_document("T", "Para 1.\n\n\n\nPara 2.");
        assert_eq!(chunks.len(), 2);
    }

    #[test]
    fn empty_title() {
        let chunks = chunk_document("", "Only body.");
        assert_eq!(chunks, vec!["Only body."]);
    }

    // ── MarkdownChunker ──────────────────────────────────────────────────────

    #[test]
    fn markdown_line_numbers() {
        let c = MarkdownChunker;
        // Line 0: "First."
        // Line 1: ""   (blank)
        // Line 2: "Second."
        let chunks = c.chunk("Title", "First.\n\nSecond.");
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line, 0);
        assert_eq!(chunks[0].text, "First.");
        assert_eq!(chunks[1].line, 2);
        assert_eq!(chunks[1].text, "Second.");
    }

    #[test]
    fn markdown_leading_blank_lines() {
        let c = MarkdownChunker;
        // Content starts with two blank lines before the paragraph.
        let chunks = c.chunk("T", "\n\nActual paragraph.");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line, 2);
    }

    #[test]
    fn markdown_empty_body_returns_title_at_line_0() {
        let c = MarkdownChunker;
        let chunks = c.chunk("Title", "");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line, 0);
        assert_eq!(chunks[0].text, "Title");
    }

    #[test]
    fn markdown_column_always_zero() {
        let c = MarkdownChunker;
        let chunks = c.chunk("T", "A.\n\nB.\n\nC.");
        for ch in &chunks {
            assert_eq!(ch.column, 0, "expected column 0 for markdown chunk");
        }
    }

    // ── JsonChunker ──────────────────────────────────────────────────────────

    #[test]
    fn jsonl_line_numbers() {
        let c = JsonChunker;
        let jsonl = concat!(
            "{\"name\":\"User\",\"is_user\":true,\"mes\":\"Hello there\"}\n",
            "{\"name\":\"Aria\",\"is_user\":false,\"mes\":\"Hi! How can I help?\"}"
        );
        let chunks = c.chunk("chat.jsonl", jsonl);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0].line, 0, "first message should be on line 0");
        assert_eq!(chunks[1].line, 1, "second message should be on line 1");
        assert!(
            chunks[0].text.contains("Hello there"),
            "got: {}",
            chunks[0].text
        );
        assert!(
            chunks[1].text.contains("Hi! How can I help?"),
            "got: {}",
            chunks[1].text
        );
    }

    #[test]
    fn jsonl_column_always_zero() {
        let c = JsonChunker;
        let jsonl =
            "{\"role\":\"user\",\"content\":\"A\"}\n{\"role\":\"assistant\",\"content\":\"B\"}";
        let chunks = c.chunk("chat.jsonl", jsonl);
        for ch in &chunks {
            assert_eq!(ch.column, 0, "JSONL chunks should always have column 0");
        }
    }

    #[test]
    fn json_openai_messages() {
        let c = JsonChunker;
        let json = "{\n  \"messages\": [\n    {\"role\":\"user\",\"content\":\"What is 2+2?\"},\n    {\"role\":\"assistant\",\"content\":\"4\"}\n  ]\n}";
        let chunks = c.chunk("session.json", json);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("2+2"), "got: {}", chunks[0].text);
        assert!(chunks[1].text.contains('4'), "got: {}", chunks[1].text);
        // Messages start at lines 2 and 3 in the pretty-printed JSON above.
        assert!(
            chunks[0].line >= 2,
            "expected line >= 2, got {}",
            chunks[0].line
        );
    }

    #[test]
    fn json_array() {
        let c = JsonChunker;
        let json = "[\n  {\"role\":\"user\",\"content\":\"Ping\"},\n  {\"role\":\"assistant\",\"content\":\"Pong\"}\n]";
        let chunks = c.chunk("msgs.json", json);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("Ping"), "got: {}", chunks[0].text);
        assert!(chunks[1].text.contains("Pong"), "got: {}", chunks[1].text);
        assert!(chunks[0].line >= 1, "first element starts on line 1");
    }

    #[test]
    fn json_single_object() {
        let c = JsonChunker;
        let json = "{\"role\":\"user\",\"content\":\"Just one message\"}";
        let chunks = c.chunk("single.json", json);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line, 0);
        assert!(
            chunks[0].text.contains("Just one message"),
            "got: {}",
            chunks[0].text
        );
    }

    #[test]
    fn text_has_no_double_newline() {
        let c = JsonChunker;
        let jsonl = concat!(
            "{\"role\":\"user\",\"content\":\"Line one\\n\\nLine two\"}\n",
            "{\"role\":\"assistant\",\"content\":\"OK\"}"
        );
        let chunks = c.chunk("chat.jsonl", jsonl);
        for ch in &chunks {
            assert!(
                !ch.text.contains("\n\n"),
                "chunk at line {} contains \\n\\n: {:?}",
                ch.line,
                ch.text
            );
        }
    }
}
