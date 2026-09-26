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
//!
//! The render needs the terminal's real geometry, which `pty::read_screen` asks
//! the pty for. Without it the stream's own addressing is the guess, and that is
//! only a floor on the width: a titled session's top rule is short of the width
//! by its title, so rendered at the floor it wraps and the box lands a row under
//! the caret. Live 2026-09-26, that refused `go` for seventy minutes.

/// Where the prompt marker puts the first typed character: `❯`, then U+00A0.
const BASE_COL: u16 = 2;
const MARKER: char = '❯';
const NBSP: char = '\u{a0}';
const BORDER: char = '\u{2500}';

/// Bounds on the guessed geometry. Rendering narrower than the real terminal
/// makes vt100 wrap the long border rows, which shifts every row drawn
/// relatively below them and destroys the box.
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

/// The fallback geometry, read off the stream's own cursor addressing when the
/// pty could not say.
///
/// Rows come from `CUP`, and rendering shorter scrolls the box away before we
/// can read it. Columns come from `CUP`, `CHA` and `CUF` together, because a
/// session wider than the render wraps its border rows and drags everything
/// below them out of place. All of them only bound the width from below.
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
    // A fresh session may address no column past 3; its box borders span the
    // full width, so the longest run of them is the width floor.
    let border = text
        .split(|c| c != BORDER)
        .map(|run| run.chars().count())
        .max()
        .unwrap_or(0);
    let cols = cols.max(u16::try_from(border).unwrap_or(u16::MAX));
    (rows.min(MAX_ROWS), cols.saturating_add(8).min(MAX_COLS))
}

/// Eight hex of sha256: enough to tell a stuck classifier reading the same
/// pixels every tick from a human retyping, and not reversible to the draft.
fn fingerprint(typed: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(typed.as_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether dim cells on the box row count as typed. CC paints two dim things
/// there: a prompt suggestion, which is not in the box's value, and a voice
/// dictation interim, which is. Only a read followed by `echo` may ignore them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Typed,
    Ignored,
}

/// What typing a command into a box read as empty actually produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Echo {
    /// The box holds the command and nothing else, caret at its end. Return is safe.
    Exact,
    /// The command sits right before the caret with other text on the row or
    /// under it: a draft or a dim dictation interim. Deleting it restores the box.
    Glued,
    /// Anything else. Nothing more may be typed.
    Other(String),
}

/// The row right under the box row: a draft's second line, or the bottom border.
/// Blank when the ring dropped the border; rows further down are the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Below {
    Border,
    Blank,
    Text,
}

/// The geometry a screen was rendered at and where it came from; every
/// refusal's reason ends with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Geometry {
    rows: u16,
    cols: u16,
    from_pty: bool,
}

impl std::fmt::Display for Geometry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let source = if self.from_pty {
            "from the pty"
        } else {
            "guessed from the stream"
        };
        write!(f, "at {}x{} {source}", self.rows, self.cols)
    }
}

/// One rendered cell as (contents, dim); a blank cell reads as a space.
fn cell(screen: &vt100::Screen, y: u16, c: u16) -> (String, bool) {
    screen
        .cell(y, c)
        .filter(|cell| !cell.contents().is_empty())
        .map_or((" ".into(), false), |cell| {
            (cell.contents().to_string(), cell.dim())
        })
}

/// Row `y` as text, every cell from column 0 to `cols`.
fn row(screen: &vt100::Screen, y: u16, cols: u16) -> String {
    (0..cols).map(|c| cell(screen, y, c).0).collect()
}

/// The live box row: each cell from `BASE_COL` as (contents, dim), and the caret's
/// offset into those cells when it sits on the row.
struct BoxRow {
    cells: Vec<(String, bool)>,
    caret: Option<usize>,
    row: u16,
    cursor: (u16, u16),
    below: Below,
    at: Geometry,
}

/// `size` is the pty's own (rows, cols) when the daemon's hello named a REPL
/// whose tty answered; `None` renders at the guess.
fn box_row(stream: &[u8], size: Option<(u16, u16)>) -> Result<BoxRow, String> {
    if stream.is_empty() {
        return Err("empty screen stream".into());
    }
    if let Some((rows, cols)) = size {
        // vt100 allocates rows x cols cells before parsing a byte: a pty claiming
        // more than any terminal has would ask for gigabytes.
        if rows == 0 || cols == 0 || rows > MAX_ROWS || cols > MAX_COLS {
            return Err(format!(
                "pty size {rows}x{cols} is outside the render bounds, up to {MAX_ROWS}x{MAX_COLS}"
            ));
        }
    }
    let at = match size {
        Some((rows, cols)) => Geometry {
            rows,
            cols,
            from_pty: true,
        },
        None => {
            let (rows, cols) = addressed_geometry(stream);
            Geometry {
                rows,
                cols,
                from_pty: false,
            }
        }
    };
    let Geometry { rows, cols, .. } = at;
    let mut parser = vt100::Parser::new(rows, cols, 0);
    parser.process(stream);
    let screen = parser.screen();
    let (cy, cx) = screen.cursor_position();
    let cell_at = |y: u16, c: u16| cell(screen, y, c);

    // Scan up for the box rather than reading the caret row alone, so a draft is
    // still counted when the caret sits mid-text. MARKER + NBSP is what makes the
    // scan safe: a picker option ("\u{276f} 1. charts/canton only") and a scrollback
    // echo both use a plain space.
    let found = (0..rows)
        .rev()
        .find(|&y| cell_at(y, 0).0.starts_with(MARKER) && cell_at(y, 1).0.starts_with(NBSP));
    let Some(by) = found else {
        return Err(format!("no prompt box on screen (cursor row {cy}) {at}"));
    };
    let row_at = |y: u16| row(screen, y, cols);
    let is_border = |row: &str| {
        let run = row.trim_end();
        !run.is_empty()
            && run.chars().all(|c| c == BORDER)
            && run.chars().count() * 2 >= usize::from(cols)
    };
    let under = row_at(by + 1);
    let below = if under.trim().is_empty() {
        Below::Blank
    } else if is_border(&under) && !is_border(&row_at(by + 2)) {
        // The real border spans the box and the status block follows it, never a
        // second border; a draft line of border chars fails one of the two.
        Below::Border
    } else {
        Below::Text
    };
    Ok(BoxRow {
        cells: (BASE_COL..cols).map(|c| cell_at(by, c)).collect(),
        caret: (cy == by && cx >= BASE_COL).then(|| usize::from(cx - BASE_COL)),
        row: by,
        cursor: (cy, cx),
        below,
        at,
    })
}

/// The box row's undimmed text, to tell a restored draft from a changed one.
pub fn row_text(stream: &[u8], size: Option<(u16, u16)>) -> Option<String> {
    box_row(stream, size)
        .ok()
        .map(|row| text_of(&row.cells, Dim::Ignored))
}

fn text_of(cells: &[(String, bool)], dim: Dim) -> String {
    let text: String = cells
        .iter()
        .map(|(s, is_dim)| match (dim, is_dim) {
            (Dim::Ignored, true) => " ",
            _ => s.as_str(),
        })
        .collect();
    text.trim_end().to_string()
}

pub fn classify(stream: &[u8], size: Option<(u16, u16)>, dim: Dim) -> BoxState {
    let row = match box_row(stream, size) {
        Ok(row) => row,
        Err(why) => return BoxState::NotRecognised(why),
    };
    let typed = text_of(&row.cells, dim);
    if !typed.is_empty() {
        return BoxState::Draft {
            chars: typed.chars().count(),
            fp: fingerprint(&typed),
        };
    }
    // An empty box row alone does not prove an empty box: a multi-line draft whose
    // first line is blank paints exactly this, and so does a stale row left below
    // the live area by a terminal resize. The caret parked on it is the proof.
    if row.caret != Some(0) {
        let (cy, cx) = row.cursor;
        return BoxState::NotRecognised(format!(
            "caret at row {cy} column {cx}, not parked on box row {} column {BASE_COL} {}",
            row.row, row.at
        ));
    }
    // Nor does the caret: arrowing up to a blank first line parks it here while
    // the draft's text sits below, and `/clear` then runs with that text as args.
    if row.below == Below::Text {
        return BoxState::NotRecognised(format!(
            "text on the row under the box row: a draft's second line or an unknown layout {}",
            row.at
        ));
    }
    BoxState::Empty
}

/// Read the box after typing `typed` (ASCII) into it. Typing replaces a prompt
/// suggestion but lands beside a dictation interim, so this read is what tells
/// the two dim shapes apart. Reasons never quote the row.
pub fn echo(stream: &[u8], size: Option<(u16, u16)>, typed: &str) -> Echo {
    let row = match box_row(stream, size) {
        Ok(row) => row,
        Err(why) => return Echo::Other(why),
    };
    let Some(caret) = row.caret.filter(|&c| c <= row.cells.len()) else {
        let (cy, cx) = row.cursor;
        return Echo::Other(format!(
            "caret at row {cy} column {cx}, off box row {} {}",
            row.row, row.at
        ));
    };
    let n = typed.len();
    let ours = caret
        .checked_sub(n)
        .map(|start| &row.cells[start..caret])
        .filter(|cells| cells.iter().all(|(_, dim)| !dim))
        .is_some_and(|cells| text_of(cells, Dim::Typed) == typed);
    if !ours {
        return Echo::Other(format!(
            "the {n} chars before the caret are not what was typed"
        ));
    }
    let rest_blank = row.cells[..caret - n]
        .iter()
        .chain(&row.cells[caret..])
        .all(|(s, _)| s == " ");
    if rest_blank && row.below != Below::Text {
        Echo::Exact
    } else {
        Echo::Glued
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every fixture below was drawn for the guess, so these read without a size.
    fn classify(stream: &[u8], dim: Dim) -> BoxState {
        super::classify(stream, None, dim)
    }

    fn echo(stream: &[u8], typed: &str) -> Echo {
        super::echo(stream, None, typed)
    }

    /// Classify a render with no dim text, which both modes must read alike.
    fn read(stream: &[u8]) -> BoxState {
        let strict = classify(stream, Dim::Typed);
        assert_eq!(strict, classify(stream, Dim::Ignored));
        strict
    }

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
        assert_eq!(read(&painted(50, "")), BoxState::Empty);
    }

    #[test]
    fn a_draft_is_seen_and_counted_without_being_quoted() {
        let state = read(&painted(50, "amend 3315 with the memberships entry"));
        assert!(matches!(state, BoxState::Draft { chars: 37, .. }));
        assert!(!state.reason().contains("memberships"));
        assert!(!state.may_type());
    }

    /// The idle box as CC 2.1.271 paints a prompt suggestion, bytes as captured
    /// live: the marker, the dim placeholder, then the caret parked back at column 3.
    fn suggested(suggestion: &str, caret_col: u16) -> Vec<u8> {
        format!(
            "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K❯\u{a0}\u{1b}[H\r\u{1b}[2C\u{1b}[60B\u{1b}[2m{suggestion}\u{1b}[22m\u{1b}[64;1H\u{1b}[61;{caret_col}H"
        )
        .into_bytes()
    }

    #[test]
    fn a_prompt_suggestion_is_an_empty_box_only_when_dim_is_ignored() {
        let stream = suggested("go, use dev14", 3);
        assert_eq!(classify(&stream, Dim::Ignored), BoxState::Empty);
        assert!(matches!(
            classify(&stream, Dim::Typed),
            BoxState::Draft { chars: 13, .. }
        ));
    }

    #[test]
    fn a_dictation_interim_paints_exactly_like_a_suggestion() {
        // Codex's case: an all-interim voice draft is dim with the caret at column 3,
        // so only the strict read refuses it. A paste must never use the other one.
        let stream = suggested("and then ship the fix", 3);
        assert_eq!(classify(&stream, Dim::Ignored), BoxState::Empty);
        assert!(!classify(&stream, Dim::Typed).may_type());
    }

    #[test]
    fn dim_text_the_caret_has_left_is_never_empty() {
        let stream = suggested("go, use dev14", 16);
        assert!(matches!(
            classify(&stream, Dim::Ignored),
            BoxState::NotRecognised(_)
        ));
        assert!(!classify(&stream, Dim::Typed).may_type());
    }

    #[test]
    fn typed_text_before_a_dim_completion_is_still_a_draft() {
        let stream = "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K❯\u{a0}/cl\u{1b}[2mear\u{1b}[22m\u{1b}[61;6H";
        assert!(matches!(
            classify(stream.as_bytes(), Dim::Ignored),
            BoxState::Draft { chars: 3, .. }
        ));
        assert!(matches!(
            classify(stream.as_bytes(), Dim::Typed),
            BoxState::Draft { chars: 6, .. }
        ));
    }

    /// The box after typing: `typed` normal, then `dim` text, caret parked after `typed`.
    fn typed_into(typed: &str, dim: &str) -> Vec<u8> {
        format!(
            "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K❯\u{a0}{typed}\u{1b}[2m{dim}\u{1b}[22m\u{1b}[61;{}H",
            3 + typed.chars().count()
        )
        .into_bytes()
    }

    #[test]
    fn a_command_alone_in_the_box_is_exact() {
        assert_eq!(echo(&typed_into("/clear", ""), "/clear"), Echo::Exact);
        assert_eq!(echo(&typed_into("go", ""), "go"), Echo::Exact);
    }

    #[test]
    fn a_command_glued_to_a_draft_is_glued() {
        assert_eq!(
            echo(&typed_into("half typed draft/clear", ""), "/clear"),
            Echo::Glued
        );
    }

    #[test]
    fn a_command_beside_a_dictation_interim_never_reaches_return() {
        assert_eq!(
            echo(&typed_into("/clear", "and then ship the fix"), "/clear"),
            Echo::Glued
        );
    }

    #[test]
    fn a_keystroke_after_the_command_is_not_our_echo() {
        assert!(matches!(
            echo(&typed_into("/clearx", ""), "/clear"),
            Echo::Other(_)
        ));
    }

    #[test]
    fn dim_cells_under_the_caret_are_not_our_echo() {
        let stream = "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K❯\u{a0}\u{1b}[2m/clear\u{1b}[22m\u{1b}[61;9H";
        assert!(matches!(echo(stream.as_bytes(), "/clear"), Echo::Other(_)));
    }

    #[test]
    fn an_echo_with_the_caret_off_the_box_row_refuses() {
        assert!(matches!(
            echo(&parked(64, "/clear"), "/clear"),
            Echo::Other(_)
        ));
        assert!(matches!(echo(b"", "/clear"), Echo::Other(_)));
    }

    #[test]
    fn an_echo_refusal_never_quotes_the_row() {
        let Echo::Other(why) = echo(&typed_into("the secret plan", ""), "/clear") else {
            panic!("expected a refusal");
        };
        assert!(!why.contains("secret"));
    }

    #[test]
    fn a_missing_marker_refuses_rather_than_guesses() {
        let stream = b"\x1b[50;1H\x1b[46;1H\x1b[KDo you want to proceed?\x1b[46;1H";
        assert!(matches!(read(stream), BoxState::NotRecognised(_)));
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
        assert!(matches!(read(&parked(64, "")), BoxState::NotRecognised(_)));
    }

    /// CC 2.1.272's box with `first` on the marker row, `second` on the row under
    /// it, then the bottom border, caret parked at the end of `first`.
    fn two_lines(first: &str, second: &str) -> Vec<u8> {
        let border = "\u{2500}".repeat(200);
        format!(
            "\u{1b}[50;1H\u{1b}[46;1H\u{1b}[K\u{276f}\u{a0}{first}\u{1b}[47;1H  {second}\u{1b}[48;1H{border}\u{1b}[46;{}H",
            3 + first.chars().count()
        )
        .into_bytes()
    }

    #[test]
    fn the_border_under_the_box_row_proves_a_one_line_box() {
        assert_eq!(read(&two_lines("", "")), BoxState::Empty);
        assert_eq!(echo(&two_lines("/clear", ""), "/clear"), Echo::Exact);
    }

    /// `two_lines`, but with line two at column 0 as a live draft can paint it.
    fn flush_second(first: &str, second: &str) -> Vec<u8> {
        String::from_utf8(two_lines(first, ""))
            .unwrap()
            .replace("\u{1b}[47;1H  ", &format!("\u{1b}[47;1H{second}"))
            .into_bytes()
    }

    #[test]
    fn a_second_line_made_of_border_chars_is_still_a_draft() {
        let full = "\u{2500}".repeat(200);
        for second in ["\u{2500}\u{2500} notes", "\u{2500}\u{2500}", full.as_str()] {
            let n = second.chars().count();
            assert!(
                matches!(read(&flush_second("", second)), BoxState::NotRecognised(_)),
                "{n}-char second line read as empty"
            );
            assert_eq!(
                echo(&flush_second("/clear", second), "/clear"),
                Echo::Glued,
                "{n}-char second line"
            );
        }
        // Same short line with the ring's border gone: only the width floor sees it.
        let dropped = |first: &str| {
            format!(
                "\u{1b}[50;1H\u{1b}[46;1H\u{1b}[K\u{276f}\u{a0}{first}\u{1b}[47;1H\u{2500}\u{2500}\u{1b}[46;{}H",
                3 + first.len()
            )
            .into_bytes()
        };
        assert!(matches!(read(&dropped("")), BoxState::NotRecognised(_)));
        assert_eq!(echo(&dropped("/clear"), "/clear"), Echo::Glued);
    }

    #[test]
    fn a_blank_first_line_with_the_caret_parked_on_it_is_never_empty() {
        // Live: arrowing up to it and typing ran `/clear` with line two as its args.
        assert!(matches!(
            read(&two_lines("", "second line draft")),
            BoxState::NotRecognised(_)
        ));
        assert_eq!(
            echo(&two_lines("/clear", "second line draft"), "/clear"),
            Echo::Glued
        );
    }

    #[test]
    fn a_long_session_whose_ring_dropped_the_borders_is_still_empty() {
        // Masked shape of three idle live jobs: blank under the box row, then only
        // the status line's changed fragments two rows down.
        let stream = "\u{1b}[64;1H\u{1b}[61;1H\u{1b}[K\u{276f}\u{a0}\u{1b}[63;33H12\u{1b}[63;54H+40/-2 \u{b7} model:opus-5\u{1b}[61;3H";
        assert_eq!(read(stream.as_bytes()), BoxState::Empty);
    }

    #[test]
    fn the_scan_still_counts_a_draft_the_caret_has_left() {
        assert!(matches!(
            read(&parked(64, "open for printing")),
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
        assert_eq!(read(stream.as_bytes()), BoxState::Empty);
    }

    #[test]
    fn a_fresh_session_addressing_no_wide_column_still_renders_at_its_width() {
        // CC 2.1.272 on a new 200-column job: borders drawn relative, caret at 47;3,
        // no CHA or CUF past column 3. At 128 columns the top border wrapped.
        let border = "\u{2500}".repeat(200);
        let stream =
            format!("\u{1b}[50;1H\u{1b}[45;1H{border}\r\n\u{276f}\u{a0}\r\n{border}\u{1b}[46;3H");
        assert_eq!(read(stream.as_bytes()), BoxState::Empty);
    }

    #[test]
    fn a_picker_option_is_never_mistaken_for_the_box() {
        // A picker draws its options with a plain space after the marker, never the
        // NBSP the live box uses, so a screen of options holds no box at all.
        let stream = "\u{1b}[64;1H\u{1b}[46;1H\u{1b}[K\u{276f} 1. charts/canton only\
\u{1b}[47;1H\u{1b}[K  2. all three canton dirs\u{1b}[63;1H"
            .as_bytes();
        assert!(matches!(read(stream), BoxState::NotRecognised(_)));
    }

    #[test]
    fn a_scrollback_echo_loses_to_the_live_box() {
        let stream = "\u{1b}[64;1H\u{1b}[43;1H\u{1b}[K\u{276f} open for printing\
\u{1b}[60;1H\u{1b}[K\u{276f}\u{a0}live draft\u{1b}[63;3H"
            .as_bytes();
        assert!(matches!(read(stream), BoxState::Draft { chars: 10, .. }));
    }

    #[test]
    fn an_empty_stream_refuses() {
        assert!(matches!(read(b""), BoxState::NotRecognised(_)));
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
        assert_eq!(read(&painted(64, "")), BoxState::Empty);
        assert!(matches!(
            read(&painted(64, "hi")),
            BoxState::Draft { chars: 2, .. }
        ));
    }

    /// CC 2.1.283 right after `/clear` on a titled 63x238 job, as its Ink frame
    /// paints it: rows drawn relative to home, the top rule short of the width by
    /// the title, no full-width row and no wide addressing, the caret parked last.
    fn titled_after_clear(title: &str, cols: usize) -> Vec<u8> {
        let dashes = "\u{2500}".repeat(cols - title.chars().count() - 3);
        let rule = format!("{dashes} {title} \u{2500}");
        assert_eq!(rule.chars().count(), cols);
        format!(
            "\u{1b}[H\r\u{1b}[58B{rule}\r\u{1b}[1B\u{276f}\u{a0}\r\u{1b}[1B\u{1b}[K\r\u{1b}[1B  \
             Fable 5.1 \u{b7} style:concise-mark\u{1b}[63;1H\u{1b}[60;3H"
        )
        .into_bytes()
    }

    #[test]
    fn a_titled_top_rule_wraps_at_the_guess_and_fits_at_the_ptys_width() {
        // Live 2026-09-26: this exact reason held `go` for seventy minutes.
        let stream = titled_after_clear("japan trip options english booking availability", 238);
        let BoxState::NotRecognised(why) = classify(&stream, Dim::Typed) else {
            panic!("the guess must refuse this frame");
        };
        assert!(
            why.starts_with("caret at row 59 column 2, not parked on box row 60 column 2"),
            "{why}"
        );
        assert!(why.ends_with("at 63x196 guessed from the stream"), "{why}");
        let size = Some((63, 238));
        assert_eq!(super::classify(&stream, size, Dim::Typed), BoxState::Empty);
        assert_eq!(
            super::classify(&stream, size, Dim::Ignored),
            BoxState::Empty
        );
    }

    #[test]
    fn a_short_bottom_border_is_a_border_at_the_ptys_width() {
        // Live 2026-09-21 (stall-32aaf9bf): a 69-column job whose ring still held
        // a `[134C` from a wider frame, so the guess ran 142 wide and the 52-char
        // bottom border failed its half-width test for two hours.
        let stream = format!(
            "\u{1b}[134C\u{1b}[130;1H\u{1b}[127;1H\u{1b}[K\u{276f}\u{a0}\u{1b}[128;1H\u{1b}[K{}\u{1b}[127;3H",
            "\u{2500}".repeat(52)
        );
        let BoxState::NotRecognised(why) = classify(stream.as_bytes(), Dim::Typed) else {
            panic!("the guess must refuse this frame");
        };
        assert!(
            why.starts_with("text on the row under the box row"),
            "{why}"
        );
        assert!(why.ends_with("at 130x142 guessed from the stream"), "{why}");
        let size = Some((130, 69));
        assert_eq!(
            super::classify(stream.as_bytes(), size, Dim::Typed),
            BoxState::Empty
        );
        assert_eq!(super::echo(stream.as_bytes(), size, ""), Echo::Exact);
    }

    #[test]
    fn a_pty_size_outside_the_render_bounds_is_refused_before_rendering() {
        let stream = titled_after_clear("t", 238);
        for size in [(u16::MAX, u16::MAX), (0, 238), (63, 0), (MAX_ROWS + 1, 238)] {
            let BoxState::NotRecognised(why) = super::classify(&stream, Some(size), Dim::Typed)
            else {
                panic!("{size:?} must be refused");
            };
            assert!(why.starts_with("pty size"), "{why}");
            assert!(why.contains("outside the render bounds"), "{why}");
        }
        assert_eq!(
            super::classify(&stream, Some((63, 238)), Dim::Typed),
            BoxState::Empty
        );
    }

    /// Not a unit test: renders `RESEED_SCREEN` (a saved replay) at the guessed
    /// geometry and at `RESEED_RENDER_GEOMETRY=rows,cols`, printing the bottom rows.
    /// `RESEED_SCREEN=<file> cargo test probe_render_a_saved_screen -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn probe_render_a_saved_screen() {
        let path = std::env::var("RESEED_SCREEN").expect("set RESEED_SCREEN");
        let stream = std::fs::read(&path).expect("reading the screen file");
        let guessed = addressed_geometry(&stream);
        let forced = std::env::var("RESEED_RENDER_GEOMETRY").ok().and_then(|g| {
            let (r, c) = g.split_once(',')?;
            Some((r.parse().ok()?, c.parse().ok()?))
        });
        println!("classify(Typed) = {:?}", classify(&stream, Dim::Typed));
        for (label, (rows, cols)) in
            std::iter::once(("guessed", guessed)).chain(forced.map(|g| ("forced", g)))
        {
            let mut parser = vt100::Parser::new(rows, cols, 0);
            parser.process(&stream);
            let screen = parser.screen();
            let (cy, cx) = screen.cursor_position();
            let row_at = |y: u16| row(screen, y, cols).trim_end().to_string();
            let markers: Vec<u16> = (0..rows)
                .filter(|&y| row_at(y).starts_with(MARKER))
                .collect();
            println!(
                "== {label} rows={rows} cols={cols} cursor=({cy},{cx}) marker rows={markers:?}"
            );
            for y in rows.saturating_sub(9)..rows {
                let row = row_at(y);
                let shown: String = row.chars().take(120).collect();
                println!(
                    "{y:3}|{shown}{}",
                    if row.chars().count() > 120 { "..." } else { "" }
                );
            }
        }
    }
}
