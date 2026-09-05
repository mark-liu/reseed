//! Park-ledger keyword matching, ported from `park-ledger-match.py`. Finds
//! ledger lines naming a reseed bundle's subject; ported to Rust since the
//! Python contract text and constants stay the single source of truth.

use regex::Regex;
use std::collections::HashSet;
use std::sync::OnceLock;

const MAX_LINES: usize = 8;
const CUT: usize = 3000;
/// Floor for a shortened line: enough to carry its date and subject phrase.
const MIN_CUT: usize = 240;
const MIN_HITS: usize = 4;
const DF_MAX: f64 = 0.20;
const DF_MIN_LINES: usize = 20;

const STOP: &[&str] = &[
    "about",
    "above",
    "after",
    "again",
    "against",
    "almost",
    "already",
    "always",
    "among",
    "another",
    "anything",
    "around",
    "because",
    "before",
    "being",
    "below",
    "between",
    "could",
    "doing",
    "during",
    "either",
    "every",
    "first",
    "found",
    "going",
    "having",
    "however",
    "inside",
    "instead",
    "itself",
    "later",
    "least",
    "might",
    "never",
    "nothing",
    "often",
    "other",
    "others",
    "ought",
    "rather",
    "really",
    "right",
    "should",
    "since",
    "still",
    "their",
    "theirs",
    "there",
    "these",
    "things",
    "those",
    "three",
    "through",
    "under",
    "until",
    "using",
    "where",
    "whether",
    "which",
    "while",
    "whole",
    "whose",
    "within",
    "without",
    "would",
    "write",
    "written",
    "wrote",
    "yours",
    "assistant",
    "command",
    "command",
    "command-name",
    "command-message",
    "command-args",
    "clear",
    "continue",
    "resume",
    "reseed",
    "bundle",
    "narrative",
    "session",
    "sessions",
    "claude",
    "context",
    "thread",
    "tool",
    "tools",
    "reading",
    "reads",
    "local-command-stdout",
    "local-command-caveat",
    "system-reminder",
    "message",
    "messages",
    "please",
    "thanks",
];

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[a-z0-9][a-z0-9_-]{4,}").unwrap())
}

fn datelike_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[\d_-]+$").unwrap())
}

fn block_split_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?m)^\*\*(user|assistant):\*\*\s*$").unwrap())
}

fn tag_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"<[^>]{1,80}>").unwrap())
}

/// (role, body) for each `**user:**` / `**assistant:**` block.
fn blocks(text: &str) -> Vec<(String, String)> {
    let re = block_split_re();
    let mut roles = Vec::new();
    let mut bodies = Vec::new();
    let mut last_end = 0;
    for m in re.find_iter(text) {
        if !roles.is_empty() {
            bodies.push(text[last_end..m.start()].to_string());
        }
        let caps = re.captures(&text[m.start()..m.end()]).unwrap();
        roles.push(caps[1].to_string());
        last_end = m.end();
    }
    if !roles.is_empty() {
        bodies.push(text[last_end..].to_string());
    }
    roles.into_iter().zip(bodies).collect()
}

fn is_slug(tok: &str) -> bool {
    let seps = tok.matches('-').count() + tok.matches('_').count();
    (seps >= 1 && tok.chars().any(|c| c.is_ascii_digit())) || seps >= 2
}

/// (slugs, words) extracted from the bundle's user turns and last assistant turn.
pub fn keywords(narrative_text: &str) -> (HashSet<String>, HashSet<String>) {
    let mut corpus: Vec<String> = Vec::new();
    let mut last_assistant = String::new();
    for (role, body) in blocks(narrative_text) {
        if role == "user" {
            let b = tag_re().replace_all(&body, " ").trim().to_string();
            let low = b.to_lowercase();
            if b.is_empty() || matches!(low.as_str(), "go" | "continue" | "carry on" | "yes" | "y")
            {
                continue;
            }
            corpus.push(b.chars().take(600).collect());
        } else {
            last_assistant = body;
        }
    }
    corpus.push(last_assistant.chars().take(800).collect());

    let mut toks: HashSet<String> = HashSet::new();
    for chunk in &corpus {
        let low = chunk.to_lowercase();
        for m in token_re().find_iter(&low) {
            toks.insert(m.as_str().to_string());
        }
    }
    let toks: HashSet<String> = toks
        .into_iter()
        .map(|t| t.trim_matches(|c| c == '-' || c == '_').to_string())
        .filter(|t| !STOP.contains(&t.as_str()))
        .filter(|t| t.len() >= 5 && !datelike_re().is_match(t))
        .collect();
    let slugs: HashSet<String> = toks.iter().filter(|t| is_slug(t)).cloned().collect();
    let words: HashSet<String> = toks.difference(&slugs).cloned().collect();
    (slugs, words)
}

/// True when `word` occurs in `line` with no `[a-z0-9]` on either side. Hand
/// scanner (P4): the Python original uses lookaround, unavailable in `regex`.
fn word_hit(word: &str, line: &str) -> bool {
    let bytes = line.as_bytes();
    let wbytes = word.as_bytes();
    if wbytes.is_empty() || wbytes.len() > bytes.len() {
        return false;
    }
    let is_boundary_char = |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit();
    for start in 0..=(bytes.len() - wbytes.len()) {
        if &bytes[start..start + wbytes.len()] != wbytes {
            continue;
        }
        let before_ok = start == 0 || !is_boundary_char(bytes[start - 1]);
        let after = start + wbytes.len();
        let after_ok = after == bytes.len() || !is_boundary_char(bytes[after]);
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn drop_frequent(tokens: HashSet<String>, lows: &[String]) -> HashSet<String> {
    if lows.len() < DF_MIN_LINES {
        return tokens;
    }
    let cap = DF_MAX * lows.len() as f64;
    tokens
        .into_iter()
        .filter(|t| lows.iter().filter(|l| word_hit(t, l)).count() as f64 <= cap)
        .collect()
}

/// Ledger lines (1-based lineno, line) named by a slug, else hit by
/// `MIN_HITS`+ distinct keywords. Newest last, capped at `MAX_LINES`.
pub fn matches(narrative_text: &str, ledger_lines: &[String]) -> Vec<(usize, String)> {
    let (slugs, words) = keywords(narrative_text);
    let lows: Vec<String> = ledger_lines.iter().map(|l| l.to_lowercase()).collect();
    let slugs = drop_frequent(slugs, &lows);
    let words = drop_frequent(words, &lows);

    let by_slug: Vec<(usize, String)> = ledger_lines
        .iter()
        .zip(lows.iter())
        .enumerate()
        .filter(|(_, (_line, low))| slugs.iter().any(|s| low.contains(s.as_str())))
        .map(|(i, (line, _))| (i + 1, line.clone()))
        .collect();
    if !by_slug.is_empty() {
        return tail(by_slug, MAX_LINES);
    }

    let by_words: Vec<(usize, String)> = ledger_lines
        .iter()
        .zip(lows.iter())
        .enumerate()
        .filter(|(_, (_line, low))| words.iter().filter(|w| word_hit(w, low)).count() >= MIN_HITS)
        .map(|(i, (line, _))| (i + 1, line.clone()))
        .collect();
    tail(by_words, MAX_LINES)
}

fn tail<T>(mut v: Vec<T>, n: usize) -> Vec<T> {
    if v.len() > n {
        v.drain(0..v.len() - n);
    }
    v
}

/// Render matches as `<lineno>: <line cut to CUT chars>`, one per line.
pub fn render(matches: &[(usize, String)]) -> String {
    render_cut(matches, CUT)
}

fn render_cut(matches: &[(usize, String)], cut: usize) -> String {
    matches
        .iter()
        .map(|(n, line)| {
            let text: String = line.chars().take(cut).collect();
            format!("{n}: {text}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A park block rendered to fit a byte budget.
pub struct Fitted {
    pub text: String,
    /// Ledger line numbers left out of the block entirely.
    pub dropped: Vec<usize>,
    /// Surviving lines were cut shorter than the normal width.
    pub shortened: bool,
}

/// Render within a byte budget. Shortens every line before dropping any: a
/// short line is still checkable against the subject, a missing one is not.
/// Returns the full-width render untouched whenever it already fits.
pub fn render_within(matches: &[(usize, String)], budget: usize) -> Fitted {
    let full = render(matches);
    if full.len() <= budget {
        return Fitted {
            text: full,
            dropped: Vec::new(),
            shortened: false,
        };
    }
    for cut in [2000, 1200, 800, 500, 360, MIN_CUT] {
        let shorter = render_cut(matches, cut);
        if shorter.len() <= budget {
            return Fitted {
                text: shorter,
                dropped: Vec::new(),
                shortened: true,
            };
        }
    }
    let mut kept = matches.to_vec();
    let mut dropped = Vec::new();
    while kept.len() > 1 && render_cut(&kept, MIN_CUT).len() > budget {
        if let Some((n, _)) = kept.pop() {
            dropped.push(n);
        }
    }
    dropped.reverse();
    Fitted {
        text: render_cut(&kept, MIN_CUT),
        dropped,
        shortened: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NARRATIVE: &str = "# Reseed narrative\n\n**user:**\n\n<command-name>/clear</command-name>\n\ngo\n\n**assistant:**\n\nResuming the reseed bundle: checking whether it is a chain link first.\n\n**user:**\n\napply weekly-review-20260904 harness cards from decisions.json\n\n**assistant:**\n\nRenamed the thread to apply weekly-review-20260904 harness; dashboard rebuilt.\n";

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keywords_keep_slug_and_drop_noise() {
        let (slugs, words) = keywords(NARRATIVE);
        assert!(slugs.contains("weekly-review-20260904"));
        assert!(words.contains("harness"));
        assert!(words.contains("decisions"));
        for n in ["go", "clear", "command-name", "reseed", "bundle"] {
            assert!(!slugs.contains(n) && !words.contains(n));
        }
    }

    #[test]
    fn slug_hit_matches_and_unrelated_line_does_not() {
        let l = lines(&[
            "2026-08-29 | OWED, avax failover 21shares2 | resume: x",
            "2026-09-04 | OWED, apply weekly-review-20260904 planned | resume: y",
        ]);
        let got: Vec<usize> = matches(NARRATIVE, &l).into_iter().map(|(n, _)| n).collect();
        assert_eq!(got, vec![2]);
    }

    #[test]
    fn slug_hit_suppresses_keyword_only_lines() {
        let l = lines(&[
            "2026-09-04 | harness cards rebuilt from decisions, dashboard open | resume: a",
            "2026-09-04 | OWED, apply weekly-review-20260904 planned | resume: b",
        ]);
        let got: Vec<usize> = matches(NARRATIVE, &l).into_iter().map(|(n, _)| n).collect();
        assert_eq!(got, vec![2]);
    }

    #[test]
    fn underscore_names_are_slugs_and_dates_are_not() {
        let text = "**user:**\n\nbuild mason_mixed18 and mark drill_commit9 on 2026-09-04\n\n**assistant:**\n\nWorking on the mason_mixed18 generator.\n";
        let (slugs, words) = keywords(text);
        assert!(slugs.contains("mason_mixed18") && slugs.contains("drill_commit9"));
        for t in slugs.iter().chain(words.iter()) {
            assert!(!datelike_re().is_match(t));
        }
        let l = lines(&[
            "2026-09-04 | Ethan drill 9 printable | resume: x",
            "2026-09-04 | mason_mixed18 half built | resume: y",
        ]);
        let narrative2 = "**user:**\n\nbuild mason_mixed18\n\n**assistant:**\n\nok\n";
        let got: Vec<usize> = matches(narrative2, &l)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got, vec![2]);
    }

    #[test]
    fn four_keyword_hits_match_without_a_slug() {
        let l = lines(&[
            "2026-09-03 | harness cards rebuilt from decisions, dashboard open | resume: z",
        ]);
        let got: Vec<usize> = matches(NARRATIVE, &l).into_iter().map(|(n, _)| n).collect();
        assert_eq!(got, vec![1]);
    }

    #[test]
    fn three_hits_are_not_enough() {
        let l = lines(&["2026-09-03 | harness dashboard cards | resume: z"]);
        assert!(matches(NARRATIVE, &l).is_empty());
    }

    #[test]
    fn frequent_words_are_dropped_before_counting() {
        let mut filler: Vec<String> = (1..25)
            .map(|i| format!("2026-08-{i:02} | harness sweep {i}, unrelated thread | resume: q"))
            .collect();
        let only_with_harness =
            "2026-09-03 | harness cards decisions dashboard | resume: z".to_string();
        let without_harness =
            "2026-09-03 | cards decisions dashboard rebuilt | resume: y".to_string();
        filler.push(only_with_harness.clone());
        filler.push(without_harness);
        assert!(filler.len() >= DF_MIN_LINES);
        let got: Vec<usize> = matches(NARRATIVE, &filler)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got, vec![filler.len()]);
        let single = vec![only_with_harness];
        let got2: Vec<usize> = matches(NARRATIVE, &single)
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        assert_eq!(got2, vec![1]);
    }

    #[test]
    fn render_caps_and_cuts() {
        let ledger: Vec<String> = (0..12)
            .map(|i| format!("{i} | weekly-review-20260904 {}", "x".repeat(4000)))
            .collect();
        let m = matches(NARRATIVE, &ledger);
        let rendered = render(&m);
        let out: Vec<&str> = rendered.lines().collect();
        assert_eq!(out.len(), MAX_LINES);
        assert!(out.last().unwrap().starts_with("12: "));
        assert!(out[0].len() <= CUT + 5);
    }

    /// Eight full-width candidates are about 24 KB, three times the reload's
    /// inline limit, so `render_within` is what keeps a bundle deliverable.
    fn fat_matches() -> Vec<(usize, String)> {
        let ledger: Vec<String> = (1..=12)
            .map(|i| format!("{i} | weekly-review-20260904 {}", "x".repeat(4000)))
            .collect();
        matches(NARRATIVE, &ledger)
    }

    #[test]
    fn render_within_leaves_a_fitting_block_alone() {
        let m = fat_matches();
        let fitted = render_within(&m, 60_000);
        assert_eq!(fitted.text, render(&m));
        assert!(!fitted.shortened);
        assert!(fitted.dropped.is_empty());
    }

    #[test]
    fn render_within_shortens_before_dropping() {
        let m = fat_matches();
        let fitted = render_within(&m, 5_600);
        assert!(fitted.text.len() <= 5_600);
        assert!(fitted.shortened);
        assert!(fitted.dropped.is_empty());
        assert_eq!(fitted.text.lines().count(), m.len());
        for (n, _) in &m {
            assert!(fitted.text.contains(&format!("{n}: ")));
        }
    }

    #[test]
    fn render_within_drops_only_once_shortening_is_not_enough() {
        let m = fat_matches();
        let fitted = render_within(&m, 900);
        assert!(fitted.text.len() <= 900);
        assert!(!fitted.dropped.is_empty());
        assert_eq!(fitted.text.lines().count() + fitted.dropped.len(), m.len());
        for n in &fitted.dropped {
            assert!(!fitted.text.contains(&format!("{n}: ")));
        }
    }

    #[test]
    fn render_within_keeps_one_line_even_under_an_impossible_budget() {
        let m = fat_matches();
        let fitted = render_within(&m, 1);
        assert_eq!(fitted.text.lines().count(), 1);
        assert_eq!(fitted.dropped.len(), m.len() - 1);
    }
}
