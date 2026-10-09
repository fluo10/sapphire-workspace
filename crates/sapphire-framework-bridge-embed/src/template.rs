//! The prompt template, token truncation and MRL. Pure functions, tested without a model.

/// Names the template [`wrap`] applies. #187's vector header records it.
pub const TEMPLATE_VERSION: u32 = 1;

/// `content` wrapped in Qwen3-VL-Embedding's official text template.
pub fn wrap(content: &str) -> String {
    format!(
        "<|im_start|>system\nRepresent the user's input.<|im_end|>\n<|im_start|>user\n{content}<|im_end|>\n<|im_start|>assistant\n"
    )
}

/// `text` cut at the byte offset where token `max - 1` ends.
///
/// `offsets[i] = (start, end)` are the byte offsets of token `i` in `text`, as the tokenizer
/// reports them. A text of at most `max` tokens is returned unchanged. When the cut falls
/// inside a multi-byte character, it moves back to that character's start, so the result is
/// always a valid `&str` of at most `max` tokens.
pub fn truncate_at<'a>(text: &'a str, offsets: &[(usize, usize)], max: usize) -> &'a str {
    if offsets.len() <= max {
        return text;
    }
    if max == 0 {
        return "";
    }
    let mut end = offsets[max - 1].1.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// The first `dim` values of `v`, L2-normalized again. A zero vector stays zero.
pub fn mrl(v: &[f32], dim: usize) -> Vec<f32> {
    let mut out = v[..dim.min(v.len())].to_vec();
    let norm = out.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        out.iter_mut().for_each(|x| *x /= norm);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn norm(v: &[f32]) -> f32 {
        v.iter().map(|x| x * x).sum::<f32>().sqrt()
    }

    #[test]
    fn wrap_applies_the_exact_template() {
        assert_eq!(
            wrap("x"),
            "<|im_start|>system\nRepresent the user's input.<|im_end|>\n<|im_start|>user\nx<|im_end|>\n<|im_start|>assistant\n"
        );
    }

    #[test]
    fn truncate_at_cuts_after_token_max_minus_one() {
        // "ab cd ef gh" as four tokens.
        let text = "ab cd ef gh";
        let offsets = [(0, 2), (2, 5), (5, 8), (8, 11)];
        assert_eq!(truncate_at(text, &offsets, 2), "ab cd");
        assert_eq!(truncate_at(text, &offsets, 3), "ab cd ef");
        assert_eq!(truncate_at(text, &offsets, 0), "");
    }

    #[test]
    fn truncate_at_leaves_short_text_alone() {
        let text = "ab cd";
        let offsets = [(0, 2), (2, 5)];
        assert_eq!(truncate_at(text, &offsets, 2), text);
        assert_eq!(truncate_at(text, &offsets, 10), text);
    }

    #[test]
    fn truncate_at_japanese_gives_a_char_boundary() {
        // Each kanji is 3 bytes. Byte-level BPE can end a token inside a character.
        let text = "今日は雨";
        let offsets = [(0, 3), (3, 7), (7, 9), (9, 12)];
        let cut = truncate_at(text, &offsets, 2);
        assert!(text.is_char_boundary(cut.len()));
        assert_eq!(cut, "今日");
        // Aligned offsets cut exactly.
        let aligned = [(0, 3), (3, 6), (6, 9), (9, 12)];
        assert_eq!(truncate_at(text, &aligned, 3), "今日は");
    }

    #[test]
    fn mrl_truncates_and_renormalizes() {
        let v: Vec<f32> = (1..=2048).map(|i| (i as f32).sin()).collect();
        let out = mrl(&v, 1024);
        assert_eq!(out.len(), 1024);
        assert!((norm(&out) - 1.0).abs() < 1e-5, "norm {}", norm(&out));
        // Direction is kept: proportional to the first 1024 inputs.
        let ratio = out[0] / v[0];
        assert!((out[10] - v[10] * ratio).abs() < 1e-5);
    }

    #[test]
    fn mrl_keeps_a_zero_vector_zero() {
        let out = mrl(&[0.0; 8], 4);
        assert_eq!(out, vec![0.0; 4]);
    }
}
