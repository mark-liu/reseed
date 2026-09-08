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

/// Replay geometry. Both axes are read off the stream: rendering narrower than
/// the real terminal makes vt100 wrap the long border rows, which shifts every
/// row drawn relatively below them and destroys the box.
const MIN_COLS: u16 = 120;
const MAX_COLS: u16 = 1024;
const MAX_ROWS: u16 = 400;
const MIN_ROWS: u16 = 24;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoxState {
    /// The prompt box exists and holds nothing. The only state that may be typed into.
    Empty,
    /// The human has typed something and not submitted it. `fp` fingerprints
    /// the text so two refusals can be told apart in the log without it.
    Draft { chars: usize, fp: String },
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
            BoxState::Draft { chars, fp } => {
                format!("prompt box holds {chars} unsubmitted chars (fp {fp})")
            }
            BoxState::NotRecognised(why) => format!("prompt box not recognised: {why}"),
        }
    }
}

/// The real terminal's geometry, read off the stream's own cursor addressing.
///
/// Rows come from `CUP`, and rendering shorter scrolls the box away before we
/// can read it. Columns come from `CUP`, `CHA` and `CUF` together, because a
/// session wider than the render wraps its border rows and drags everything
/// below them out of place.
fn addressed_geometry(stream: &[u8]) -> (u16, u16) {
    let text = String::from_utf8_lossy(stream);
    let bytes = text.as_bytes();
    let (mut rows, mut cols) = (MIN_ROWS, MIN_COLS);
    let mut i = 0;
    while let Some(off) = text[i..].find("\u{1b}[") {
        let start = i + off + 2;
        let mut j = start;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > start && j < bytes.len() {
            let first: u16 = text[start..j].parse().unwrap_or(0);
            match bytes[j] {
                // CHA is absolute; CUF is relative, so it is only a lower bound
                // on the column reached, which is all this needs.
                b'G' | b'C' => cols = cols.max(first),
                b';' => {
                    let mut k = j + 1;
                    while k < bytes.len() && bytes[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k < bytes.len() && bytes[k] == b'H' {
                        rows = rows.max(first);
                        cols = cols.max(text[j + 1..k].parse().unwrap_or(0));
                    }
                }
                _ => {}
            }
        }
        i = start;
    }
    (rows.min(MAX_ROWS), cols.saturating_add(8).min(MAX_COLS))
}

/// Eight hex of sha256: enough to tell a stuck classifier reading the same
/// pixels every tick from a human retyping, and not reversible to the draft.
fn fingerprint(typed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(typed.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

pub fn classify(stream: &[u8]) -> BoxState {
    if stream.is_empty() {
        return BoxState::NotRecognised("empty screen stream".into());
    }
    let (rows, cols) = addressed_geometry(stream);
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(stream);
    let screen = parser.screen();
    let (cy, cx) = screen.cursor_position();

    let row_at = |y: u16| -> String {
        (0..cols)
            .map(|c| {
                screen
                    .cell(y, c)
                    .map(|cell| cell.contents())
                    .filter(|s| !s.is_empty())
                    .unwrap_or(" ")
            })
            .collect()
    };

    // Scan up for the box rather than reading the caret row alone, so a draft is
    // still counted when the caret sits mid-text. MARKER + NBSP is what makes the
    // scan safe: a picker option ("\u{276f} 1. charts/canton only") and a scrollback
    // echo both use a plain space.
    let mut found: Option<(u16, String)> = None;
    for y in (0..rows).rev() {
        let row = row_at(y);
        let mut chars = row.chars();
        if chars.next() == Some(MARKER) && chars.next() == Some(NBSP) {
            found = Some((y, row));
            break;
        }
    }
    let (by, row) = match found {
        Some(v) => v,
        None => {
            return BoxState::NotRecognised(format!("no prompt box on screen (cursor row {cy})"))
        }
    };

    let typed: String = row.chars().skip(BASE_COL as usize).collect();
    let typed = typed.trim_end();
    if !typed.is_empty() {
        return BoxState::Draft {
            chars: typed.chars().count(),
            fp: fingerprint(typed),
        };
    }
    // An empty box row alone does not prove an empty box: a multi-line draft whose
    // first line is blank paints exactly this, and so does a stale row left below
    // the live area by a terminal resize. The caret parked on it is the proof.
    if (cy, cx) != (by, BASE_COL) {
        return BoxState::NotRecognised(format!(
            "caret at row {cy} column {cx}, not parked on box row {by} column {BASE_COL}"
        ));
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
        assert!(matches!(state, BoxState::Draft { chars: 37, .. }));
        assert!(!state.reason().contains("memberships"));
        assert!(!state.may_type());
    }

    #[test]
    fn a_missing_marker_refuses_rather_than_guesses() {
        let stream = b"\x1b[50;1H\x1b[46;1H\x1b[KDo you want to proceed?\x1b[46;1H";
        assert!(matches!(classify(stream), BoxState::NotRecognised(_)));
    }

    /// The same render, but with the caret left somewhere other than the box row.
    /// Live sessions park it on the box at `BASE_COL`; this is the shape a
    /// multi-line draft with a blank first line produces.
    fn parked(rows: u16, typed: &str) -> Vec<u8> {
        let line = rows - 4;
        let status = rows - 1;
        format!("\u{1b}[{rows};1H\u{1b}[{line};1H\u{1b}[K\u{276f}\u{a0}{typed}\u{1b}[{status};3H")
            .into_bytes()
    }

    #[test]
    fn a_blank_box_row_without_the_caret_on_it_is_never_empty() {
        // A draft continued on line two paints the box row blank. Typing there
        // splices `/clear` into that draft and submits it.
        assert!(matches!(
            classify(&parked(64, "")),
            BoxState::NotRecognised(_)
        ));
    }

    #[test]
    fn the_scan_still_counts_a_draft_the_caret_has_left() {
        assert!(matches!(
            classify(&parked(64, "open for printing")),
            BoxState::Draft { chars: 17, .. }
        ));
    }

    #[test]
    fn a_box_row_wider_than_the_default_render_survives() {
        // A 460-column session wrapped its border rows at the old fixed 400 and
        // dragged the box out of place; the geometry is read off the stream now.
        let border: String = "\u{2500}".repeat(455);
        let stream = format!(
            "\u{1b}[130;1H\u{1b}[124;1H\u{1b}[K{border}\u{1b}[459G\u{1b}[127;1H\u{1b}[K\u{276f}\u{a0}\u{1b}[127;3H"
        );
        assert_eq!(classify(stream.as_bytes()), BoxState::Empty);
    }

    #[test]
    fn a_picker_option_is_never_mistaken_for_the_box() {
        // A picker draws its options with a plain space after the marker, never the
        // NBSP the live box uses, so a screen of options holds no box at all.
        let stream = "\u{1b}[64;1H\u{1b}[46;1H\u{1b}[K\u{276f} 1. charts/canton only\
\u{1b}[47;1H\u{1b}[K  2. all three canton dirs\u{1b}[63;1H"
            .as_bytes();
        assert!(matches!(classify(stream), BoxState::NotRecognised(_)));
    }

    #[test]
    fn a_scrollback_echo_loses_to_the_live_box() {
        let stream = "\u{1b}[64;1H\u{1b}[43;1H\u{1b}[K\u{276f} open for printing\
\u{1b}[60;1H\u{1b}[K\u{276f}\u{a0}live draft\u{1b}[63;3H"
            .as_bytes();
        assert!(matches!(
            classify(stream),
            BoxState::Draft { chars: 10, .. }
        ));
    }

    #[test]
    fn an_empty_stream_refuses() {
        assert!(matches!(classify(b""), BoxState::NotRecognised(_)));
    }

    #[test]
    fn geometry_comes_from_the_cells_the_stream_addresses() {
        assert_eq!(addressed_geometry(b"\x1b[64;1H\x1b[61;3H").0, 64);
        assert_eq!(
            addressed_geometry(b"no escapes here"),
            (MIN_ROWS, MIN_COLS + 8)
        );
        // CHA and CUF both widen the render; the fixed 400 lost these sessions.
        assert_eq!(addressed_geometry(b"\x1b[130;1H\x1b[461G").1, 469);
        assert_eq!(addressed_geometry(b"\x1b[130;1H\x1b[438C").1, 446);
        assert_eq!(addressed_geometry(b"\x1b[9;9999H").1, MAX_COLS);
    }

    #[test]
    fn a_tall_session_is_read_at_its_own_height() {
        assert_eq!(classify(&painted(64, "")), BoxState::Empty);
        assert!(matches!(
            classify(&painted(64, "hi")),
            BoxState::Draft { chars: 2, .. }
        ));
    }
}
