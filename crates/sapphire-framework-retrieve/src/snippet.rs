//! Short excerpts of a file for search results.
//!
//! A result names one whole file, so it carries a snippet instead of matched chunks:
//! the FTS fragment around the match when there is one, else the file's leading text.

/// Maximum length of a snippet, in characters.
pub const SNIPPET_CHARS: usize = 150;

/// Collapse whitespace runs (newlines included) to single spaces, trim, and keep at
/// most [`SNIPPET_CHARS`] characters. Cuts on `char` boundaries, so any UTF-8 text is safe.
pub fn collapse_and_cut(text: &str) -> String {
    let mut out = String::new();
    let mut count = 0;
    let mut pending_space = false;
    for c in text.chars() {
        if c.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        if pending_space {
            if count + 1 >= SNIPPET_CHARS {
                break;
            }
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= SNIPPET_CHARS {
            break;
        }
        out.push(c);
        count += 1;
    }
    out
}

/// The leading text of a file, as a snippet.
pub fn leading(text: &str) -> String {
    collapse_and_cut(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_collapses_and_trims() {
        assert_eq!(collapse_and_cut("  a\n\n b\t c  "), "a b c");
    }

    #[test]
    fn short_text_is_kept_whole() {
        assert_eq!(leading("hello world"), "hello world");
    }

    #[test]
    fn long_text_is_cut_to_the_cap_in_chars() {
        let s = "あ".repeat(SNIPPET_CHARS + 50);
        let out = leading(&s);
        assert_eq!(out.chars().count(), SNIPPET_CHARS);
        assert!(out.chars().all(|c| c == 'あ'));
    }

    #[test]
    fn empty_text_gives_an_empty_snippet() {
        assert_eq!(leading(""), "");
        assert_eq!(leading(" \n\t "), "");
    }
}
