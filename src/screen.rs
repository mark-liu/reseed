//! What is sitting in a session's prompt box, read from its rendered screen.
//!
//! The daemon types `/clear` in as a bracketed paste, and Claude Code's paste
//! handler splices at the cursor rather than replacing the buffer. So text the
//! human typed and never submitted would be glued to the injected command and
//! submitted as one ordinary prompt, tools and all. Nothing may be typed into a
//! session whose box is not provably empty.
//!
//! The evidence is the pty socket's screen replay, which is a byte ring, not a
//! snapshot: on a long session the box borders were drawn hours ago and have
//! scrolled out of it. The cursor is the durable anchor instead, because it is
//! parked at the insertion point on every render.
//!
//! This is an allow-list. Only the exact idle shape returns `Empty`; a modal,
//! an unfamiliar layout, a truncated replay and a multi-line draft all fall
//! through to a verdict that refuses the clear.

/// Where the prompt marker puts the first typed character: `❯`, then U+00A0.
const BASE_COL: u16 = 2;
const MARKER: char = '❯';
const NBSP: char = '\u{a0}';

/// Replay geometry. Columns are absolute in the stream, so a screen wider than
/// the real terminal reconstructs the same cells; a shorter one loses the box.
const REPLAY_COLS: u16 = 400;
const MAX_ROWS: u16 = 400;
const MIN_ROWS: u16 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxState {
    /// The prompt box exists and holds nothing. The only state that may be typed into.
    Empty,
    /// The human has typed something and not submitted it.
    Draft { chars: usize },
    /// Anything else, including a layout this version does not know.
    NotRecognised(String),
}

impl BoxState {
    pub fn may_type(&self) -> bool {
        matches!(self, BoxState::Empty)
    }

    /// Never carries draft text: it is Mark's private working state and would
    /// land in a world-readable log.
    pub fn reason(&self) -> String {
        match self {
            BoxState::Empty => "prompt box is empty".into(),
            BoxState::Draft { chars } => format!("prompt box holds {chars} unsubmitted chars"),
            BoxState::NotRecognised(why) => format!("prompt box not recognised: {why}"),
        }
    }
}

/// Highest absolute row the stream addresses, which is the real terminal's
/// height. Replaying shorter scrolls the box away before we can read it.
fn addressed_rows(stream: &[u8]) -> u16 {
    let text = String::from_utf8_lossy(stream);
    let mut max = MIN_ROWS;
    let bytes = text.as_bytes();
    let mut i = 0;
    while let Some(off) = text[i..].find("\u{1b}[") {
        let start = i + off + 2;
        let mut j = start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        // CUP is `ESC [ row ; col H`; a bare `ESC [ n H` addresses column n.
        if j > start && j < bytes.len() && bytes[j] == b';' {
            if let Ok(row) = text[start..j].parse::<u16>() {
                let mut k = j + 1;
                while k < bytes.len() && bytes[k].is_ascii_digit() {
                    k += 1;
                }
                if k < bytes.len() && bytes[k] == b'H' {
                    max = max.max(row);
                }
            }
        }
        i = start;
    }
    max.min(MAX_ROWS)
}

pub fn classify(stream: &[u8]) -> BoxState {
    if stream.is_empty() {
        return BoxState::NotRecognised("empty screen stream".into());
    }
    let rows = addressed_rows(stream);
    let mut parser = vt100::Parser::new(rows, REPLAY_COLS, 0);
    parser.process(stream);
    let screen = parser.screen();
    let (cy, cx) = screen.cursor_position();

    let row: String = (0..REPLAY_COLS)
        .map(|c| {
            screen
                .cell(cy, c)
                .map(|cell| cell.contents())
                .filter(|s| !s.is_empty())
                .unwrap_or(" ")
        })
        .collect();

    let mut chars = row.chars();
    if chars.next() != Some(MARKER) {
        return BoxState::NotRecognised(format!("no prompt marker on cursor row {cy}"));
    }
    if chars.next() != Some(NBSP) {
        return BoxState::NotRecognised("prompt marker is not followed by its spacer".into());
    }
    let typed: String = row.chars().skip(BASE_COL as usize).collect();
    let typed = typed.trim_end();
    if !typed.is_empty() {
        return BoxState::Draft {
            chars: typed.chars().count(),
        };
    }
    // An empty row with the cursor away from the insertion point means the box
    // is holding something this reader cannot see, e.g. a wrapped draft above.
    if cx != BASE_COL {
        return BoxState::NotRecognised(format!("cursor at column {cx}, expected {BASE_COL}"));
    }
    BoxState::Empty
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One render of the idle prompt box at the bottom of a `rows`-tall screen.
    fn painted(rows: u16, typed: &str) -> Vec<u8> {
        let line = rows - 4;
        format!(
            "\u{1b}[{rows};1H\u{1b}[{line};1H\u{1b}[K❯\u{a0}{typed}\u{1b}[{line};{}H",
            3 + typed.chars().count()
        )
        .into_bytes()
    }

    #[test]
    fn empty_box_is_the_only_clearable_state() {
        assert_eq!(classify(&painted(50, "")), BoxState::Empty);
    }

    #[test]
    fn a_draft_is_seen_and_counted_without_being_quoted() {
        let state = classify(&painted(50, "amend 3315 with the memberships entry"));
        assert_eq!(state, BoxState::Draft { chars: 37 });
        assert!(!state.reason().contains("memberships"));
        assert!(!state.may_type());
    }

    #[test]
    fn a_missing_marker_refuses_rather_than_guesses() {
        let stream = b"\x1b[50;1H\x1b[46;1H\x1b[KDo you want to proceed?\x1b[46;1H";
        assert!(matches!(classify(stream), BoxState::NotRecognised(_)));
    }

    #[test]
    fn an_empty_stream_refuses() {
        assert!(matches!(classify(b""), BoxState::NotRecognised(_)));
    }

    #[test]
    fn height_comes_from_the_rows_the_stream_addresses() {
        assert_eq!(addressed_rows(b"\x1b[64;1H\x1b[61;3H"), 64);
        assert_eq!(addressed_rows(b"no escapes here"), MIN_ROWS);
    }

    #[test]
    fn a_tall_session_is_read_at_its_own_height() {
        assert_eq!(classify(&painted(64, "")), BoxState::Empty);
        assert_eq!(classify(&painted(64, "hi")), BoxState::Draft { chars: 2 });
    }
}
