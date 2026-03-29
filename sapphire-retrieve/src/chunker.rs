//! Text chunker.
//!
//! Splits a document's body into paragraph-level chunks for vector embedding.
//! Paragraph-level granularity is appropriate for Markdown-based notes because:
//!
//! - Paragraphs are natural semantic units in prose / bullet-list notes.
//! - Most embedding models have token-length limits (≈ 8 k tokens); a single
//!   document may exceed this, but individual paragraphs rarely will.
//! - Chunking improves recall: a query about a specific concept can match the
//!   exact paragraph that discusses it rather than competing with the rest of
//!   the document for "share of attention" in the embedding.
//!
//! The title is prepended to every chunk so that each chunk can be
//! interpreted in isolation (a standard RAG practice).

/// Split a document into embeddable text chunks.
///
/// Splitting rules:
///
/// 1. The body is split on blank lines (`\n\n`).  Each non-empty paragraph
///    becomes one chunk.
/// 2. The title is prepended to every chunk so that it carries context when
///    retrieved in isolation.
/// 3. If the body is empty (title-only document), a single chunk containing
///    just the title is returned.
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
}
