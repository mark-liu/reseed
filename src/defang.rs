//! Defang tainted text so it can't re-inject prompts when read back.
//!
//! A Claude Code transcript records every tool result verbatim, including
//! web pages, file contents, and MCP responses that may carry prompt-
//! injection payloads ("ignore previous instructions", role-override
//! markers, and the like). When those bytes are read back into a fresh
//! context window they can be re-interpreted as instructions — the
//! "transcript-as-backdoor" channel.
//!
//! [`defang`] interleaves U+00B7 MIDDLE DOT inside alphabetic runs of four
//! or more characters, so "ignore" becomes "i·g·n·o·r·e". That defeats
//! literal-substring and most regex pattern matching while staying
//! readable for a human auditor. Short connecting words, digits,
//! punctuation, and whitespace pass through unchanged.

use regex::Regex;
use std::sync::OnceLock;

/// U+00B7 MIDDLE DOT, interleaved between characters of a matched run.
const SEP: char = '\u{00B7}';

fn word_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z]{4,}").expect("static defang regex compiles"))
}

/// Interleave U+00B7 inside alphabetic runs of 4+ characters.
///
/// Idempotency note: the middle dot is not in `[A-Za-z]`, so re-running
/// `defang` on already-defanged text leaves runs of length 1 between the
/// dots — they no longer match the 4+ rule and pass through unchanged.
pub fn defang(s: &str) -> String {
    word_re()
        .replace_all(s, |caps: &regex::Captures| {
            caps[0]
                .chars()
                .map(|c| c.to_string())
                .collect::<Vec<_>>()
                .join(&SEP.to_string())
        })
        .into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaves_long_words() {
        assert_eq!(defang("ignore"), "i·g·n·o·r·e");
    }

    #[test]
    fn passes_short_tokens_and_punctuation() {
        // Words of 3 or fewer chars, digits, and symbols are left intact.
        assert_eq!(defang("the cat ate 42 figs!"), "the cat ate 42 f·i·g·s!");
        assert_eq!(defang("the cat -> 42!"), "the cat -> 42!");
    }

    #[test]
    fn breaks_injection_marker() {
        let out = defang("ignore previous instructions");
        assert!(!out.contains("ignore previous instructions"));
        assert!(out.contains("i·g·n·o·r·e"));
    }

    #[test]
    fn already_defanged_text_is_stable() {
        let once = defang("instructions");
        let twice = defang(&once);
        assert_eq!(once, twice);
    }
}
