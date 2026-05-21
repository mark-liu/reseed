//! Rough token estimation.
//!
//! Anthropic does not ship a public local tokenizer for current models, and
//! OpenAI's `tiktoken` is systematically wrong for Claude. The accurate
//! path is the `POST /v1/messages/count_tokens` endpoint, which needs an
//! API key and a network round-trip per call.
//!
//! For a transcript-wide point-in-time *ratio* — full vs distilled — that
//! accuracy is not needed: a consistent heuristic gives the same relative
//! answer. We use the well-known ~4-characters-per-token approximation.
//! Absolute numbers are approximate; the full-vs-distilled percentage is
//! what matters and is stable under the heuristic.

/// Estimate token count as `chars / 4` (minimum 1 for non-empty input).
pub fn estimate(s: &str) -> usize {
    let chars = s.chars().count();
    if chars == 0 {
        0
    } else {
        (chars / 4).max(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(estimate(""), 0);
    }

    #[test]
    fn short_nonempty_is_at_least_one() {
        assert_eq!(estimate("hi"), 1);
    }

    #[test]
    fn four_chars_per_token() {
        assert_eq!(estimate("abcdefgh"), 2);
    }
}
