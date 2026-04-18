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
//! | JSON array element | line of the opening `{` | line of the closing `}` |
//!
//! # Example
//!
//! ```no_run
//! use sapphire_retrieve::chunker::{Chunker, JsonChunker, MarkdownChunker};
//!
//! let md = MarkdownChunker;
//! let chunks = md.chunk("My note", "First paragraph.\n\nSecond paragraph.");
//! assert_eq!(chunks[0].line_start, 0);
//! assert_eq!(chunks[1].line_start, 2);
//!
//! let json = JsonChunker;
//! let jsonl = "{\"role\":\"user\",\"content\":\"Hello\"}\n{\"role\":\"assistant\",\"content\":\"Hi\"}";
//! let chunks = json.chunk("chat.jsonl", jsonl);
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

// ── JsonChunker ───────────────────────────────────────────────────────────────

/// Message-level chunker for JSON and JSONL files.
///
/// Handles three layouts automatically:
///
/// 1. **JSONL** (one JSON object per line) — each line becomes one chunk.
///    `line_start == line_end == <line index>`.
/// 2. **JSON top-level array** — each element becomes one chunk.  The source
///    line range of each element is tracked by scanning the raw text.
/// 3. **JSON object with a messages/chat field** — extracts the nested array
///    (`"messages"`, `"chat"`, `"history"`, or `"log"` key) and treats each
///    element as a chunk.  Falls back to a single chunk for other objects.
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
        if let Some(chunks) = try_parse_jsonl(content) {
            return chunks;
        }

        match serde_json::from_str::<serde_json::Value>(content) {
            Ok(value) => chunks_from_json_value(content, &value),
            Err(_) => {
                vec![TextChunk {
                    line_start: 0,
                    line_end: count_newlines(content),
                    text: normalise(content),
                }]
            }
        }
    }
}

// ── JSONL parsing ─────────────────────────────────────────────────────────────

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
                line_start: *line_idx,
                line_end: *line_idx,
                text: extract_message_text(&v),
            }),
            Err(_) => return None,
        }
    }
    Some(chunks)
}

// ── JSON value → chunks ───────────────────────────────────────────────────────

fn chunks_from_json_value(raw: &str, value: &serde_json::Value) -> Vec<TextChunk> {
    match value {
        serde_json::Value::Array(arr) => {
            let positions = find_array_element_lines(raw);
            arr.iter()
                .enumerate()
                .map(|(i, v)| {
                    let (line_start, line_end) = positions.get(i).copied().unwrap_or((i, i));
                    TextChunk {
                        line_start,
                        line_end,
                        text: extract_message_text(v),
                    }
                })
                .collect()
        }
        serde_json::Value::Object(map) => {
            const ARRAY_KEYS: &[&str] = &["messages", "chat", "history", "log"];
            for key in ARRAY_KEYS {
                if let Some(serde_json::Value::Array(arr)) = map.get(*key)
                    && !arr.is_empty()
                {
                    let nested_positions = find_nested_array_lines(raw, key);
                    return arr
                        .iter()
                        .enumerate()
                        .map(|(i, v)| {
                            let (line_start, line_end) =
                                nested_positions.get(i).copied().unwrap_or((i, i));
                            TextChunk {
                                line_start,
                                line_end,
                                text: extract_message_text(v),
                            }
                        })
                        .collect();
                }
            }
            vec![TextChunk {
                line_start: 0,
                line_end: count_newlines(raw),
                text: extract_message_text(value),
            }]
        }
        other => vec![TextChunk {
            line_start: 0,
            line_end: count_newlines(raw),
            text: normalise(&other.to_string()),
        }],
    }
}

// ── source-position scanning ──────────────────────────────────────────────────

/// Scan `raw` JSON text for the line range of each top-level array element.
///
/// Returns `(line_start, line_end)` tuples (inclusive, 0-based).
fn find_array_element_lines(raw: &str) -> Vec<(usize, usize)> {
    let start = match raw.find('[') {
        Some(i) => i + 1,
        None => return vec![],
    };
    scan_array_element_lines(raw, start)
}

fn find_nested_array_lines(raw: &str, key: &str) -> Vec<(usize, usize)> {
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
    scan_array_element_lines(raw, abs_start)
}

/// Scan `raw[start..]` for the `(line_start, line_end)` of each direct child
/// element of a JSON array.  `start` is the byte offset just after the opening
/// `[`.  Both values are inclusive 0-based line numbers.
fn scan_array_element_lines(raw: &str, start: usize) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let bytes = raw.as_bytes();
    let len = bytes.len();

    let prefix = &raw[..start.min(len)];
    let mut line = count_newlines(prefix);

    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = start.min(len);

    // Track the currently-open element's start line so we can pair it with
    // its end when depth returns to 0.
    let mut current_start: Option<usize> = None;

    while i < len {
        let b = bytes[i];

        if escaped {
            escaped = false;
            if b == b'\n' {
                line += 1;
            }
            i += 1;
            continue;
        }

        if in_string {
            if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
                if depth == 0 && current_start.is_some() {
                    // Top-level string element ends at this closing quote.
                    let s = current_start.take().unwrap();
                    ranges.push((s, line));
                }
            }
            if b == b'\n' {
                line += 1;
            }
            i += 1;
            continue;
        }

        match b {
            b'"' => {
                in_string = true;
                if depth == 0 && current_start.is_none() {
                    current_start = Some(line);
                }
            }
            b'{' | b'[' => {
                if depth == 0 && current_start.is_none() {
                    current_start = Some(line);
                }
                depth += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                if depth < 0 {
                    break;
                }
                if depth == 0 && current_start.is_some() {
                    let s = current_start.take().unwrap();
                    ranges.push((s, line));
                }
            }
            b',' if depth == 0 => {
                // Close any pending scalar element at this comma.
                if let Some(s) = current_start.take() {
                    ranges.push((s, line));
                }
            }
            b'0'..=b'9' | b'-' | b't' | b'f' | b'n' if depth == 0 => {
                if current_start.is_none() {
                    current_start = Some(line);
                }
            }
            b'\n' => {
                line += 1;
            }
            _ => {}
        }
        i += 1;
    }

    // If a scalar is still open when we break (trailing element before `]`),
    // close it at the current line.
    if let Some(s) = current_start.take() {
        ranges.push((s, line));
    }

    ranges
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

    // ── JsonChunker ──────────────────────────────────────────────────────────

    #[test]
    fn jsonl_line_ranges() {
        let c = JsonChunker;
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
    fn json_openai_messages() {
        let c = JsonChunker;
        let json = "{\n  \"messages\": [\n    {\"role\":\"user\",\"content\":\"What is 2+2?\"},\n    {\"role\":\"assistant\",\"content\":\"4\"}\n  ]\n}";
        let chunks = c.chunk("session.json", json);
        assert_eq!(chunks.len(), 2);
        // Both message objects are single-line in this layout.
        assert_eq!(chunks[0].line_start, chunks[0].line_end);
        assert_eq!(chunks[1].line_start, chunks[1].line_end);
        assert!(chunks[0].line_start >= 2);
        assert!(chunks[1].line_start > chunks[0].line_start);
    }

    #[test]
    fn json_array() {
        let c = JsonChunker;
        let json = "[\n  {\"role\":\"user\",\"content\":\"Ping\"},\n  {\"role\":\"assistant\",\"content\":\"Pong\"}\n]";
        let chunks = c.chunk("msgs.json", json);
        assert_eq!(chunks.len(), 2);
        assert!(chunks[0].text.contains("Ping"));
        assert!(chunks[1].text.contains("Pong"));
        assert!(chunks[0].line_start >= 1);
        assert_eq!(chunks[0].line_start, chunks[0].line_end);
    }

    #[test]
    fn json_array_multiline_element() {
        let c = JsonChunker;
        // A single pretty-printed element spanning multiple lines.
        let json = "[\n  {\n    \"role\": \"user\",\n    \"content\": \"Hello\"\n  }\n]";
        let chunks = c.chunk("msgs.json", json);
        assert_eq!(chunks.len(), 1);
        assert!(chunks[0].line_start >= 1);
        assert!(
            chunks[0].line_end > chunks[0].line_start,
            "expected multi-line range, got {}..={}",
            chunks[0].line_start,
            chunks[0].line_end
        );
    }

    #[test]
    fn json_single_object() {
        let c = JsonChunker;
        let json = "{\"role\":\"user\",\"content\":\"Just one message\"}";
        let chunks = c.chunk("single.json", json);
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].line_start, 0);
        assert_eq!(chunks[0].line_end, 0);
        assert!(chunks[0].text.contains("Just one message"));
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
            assert!(!ch.text.contains("\n\n"));
        }
    }
}
