//! Copy-on-select: a text selection in the session pane, multi-click
//! detection, and the OSC 52 sequence that hands the selected text to the
//! outer terminal's clipboard.
//!
//! Everything here is pure. The app glue lives in `interaction.rs`
//! (`App::select_mouse`), text extraction in [`crate::term::screen::Screen::text`],
//! and the OSC 52 write in the effect runner (`tui/mod.rs`).
//!
//! A selection is in pane cells. It is a stream (like a terminal's), not a
//! block: from `start` to the end of its row, whole rows in between, and the
//! start of the last row up to `end`, both ends inclusive.

use std::time::{Duration, Instant};

use crate::proto::SessionId;

/// A cell of the pane, `row` first so that the derived order is reading order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub row: u16,
    pub col: u16,
}

impl Pos {
    pub fn new(row: u16, col: u16) -> Pos {
        Pos { row, col }
    }
}

/// What one end of a selection snaps to: a cell (drag), a word
/// (double-click), or a whole row (triple-click).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Cell,
    Word,
    Line,
}

impl Unit {
    /// The unit of the `n`th click in a multi-click (1, 2, 3).
    pub fn from_clicks(n: u8) -> Unit {
        match n {
            0 | 1 => Unit::Cell,
            2 => Unit::Word,
            _ => Unit::Line,
        }
    }

    /// The cells `at` covers in this unit, inclusive, on a row whose text
    /// is `line` (one char per column).
    pub fn span(self, at: Pos, line: &[char]) -> (Pos, Pos) {
        match self {
            Unit::Cell => (at, at),
            Unit::Word => {
                let (s, e) = word_span(line, at.col as usize);
                (Pos::new(at.row, s as u16), Pos::new(at.row, e as u16))
            }
            Unit::Line => {
                let last = line.len().saturating_sub(1) as u16;
                (Pos::new(at.row, 0), Pos::new(at.row, last))
            }
        }
    }
}

/// Characters that end a word on a double-click, besides whitespace. `:`,
/// `/`, `.`, `-`, `_`, `~`, `@` and `=` are word characters, so a double-click
/// takes a whole path or URL.
const WORD_SEPARATORS: &str = "\"'`()[]{}<>|│,;";

#[derive(PartialEq)]
enum Class {
    Space,
    Word,
    Other,
}

fn class(c: char) -> Class {
    if c.is_whitespace() || c == '\0' {
        Class::Space
    } else if WORD_SEPARATORS.contains(c) {
        Class::Other
    } else {
        Class::Word
    }
}

/// The inclusive column range of the word at `col` in `line`: the run of
/// word characters (or of blanks) around it, or the separator alone.
pub fn word_span(line: &[char], col: usize) -> (usize, usize) {
    let Some(&c) = line.get(col) else {
        return (col, col);
    };
    let k = class(c);
    if k == Class::Other {
        return (col, col);
    }
    let mut start = col;
    while start > 0 && class(line[start - 1]) == k {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < line.len() && class(line[end + 1]) == k {
        end += 1;
    }
    (start, end)
}

/// A selection in one session's pane.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// The session whose screen it is on.
    pub session: SessionId,
    unit: Unit,
    /// The span under the press, and the one under the mouse now.
    anchor: (Pos, Pos),
    head: (Pos, Pos),
    /// The button is still held: the head follows the mouse.
    dragging: bool,
}

impl Selection {
    /// A press: the selection is `span` (from [`Unit::span`]) for now.
    pub fn new(session: SessionId, unit: Unit, span: (Pos, Pos)) -> Selection {
        Selection {
            session,
            unit,
            anchor: span,
            head: span,
            dragging: true,
        }
    }

    pub fn unit(&self) -> Unit {
        self.unit
    }

    pub fn dragging(&self) -> bool {
        self.dragging
    }

    /// The mouse moved to `span` (in this selection's unit) with the button
    /// held.
    pub fn extend(&mut self, span: (Pos, Pos)) {
        self.head = span;
    }

    /// The button was released.
    pub fn finish(&mut self) {
        self.dragging = false;
    }

    /// The first and last selected cell, in reading order, however the
    /// mouse was dragged.
    pub fn bounds(&self) -> (Pos, Pos) {
        (
            self.anchor.0.min(self.head.0),
            self.anchor.1.max(self.head.1),
        )
    }

    /// Whether the cell at `row`, `col` is selected.
    pub fn contains(&self, row: u16, col: u16) -> bool {
        let (start, end) = self.bounds();
        (start..=end).contains(&Pos::new(row, col))
    }

    /// A plain click that never moved selects nothing; a double or triple
    /// click selects its word or row at once.
    pub fn is_empty(&self) -> bool {
        self.unit == Unit::Cell && self.anchor == self.head
    }
}

/// How close in time two presses on the same cell must be to count as a
/// double (or triple) click.
pub const MULTI_CLICK: Duration = Duration::from_millis(400);

/// Counts consecutive presses on one cell: 1, 2, 3, then 1 again.
#[derive(Debug, Default)]
pub struct Clicks {
    last: Option<(Instant, Pos, u8)>,
}

impl Clicks {
    /// A press at `at`: which click of a multi-click it is.
    pub fn press(&mut self, at: Pos, now: Instant) -> u8 {
        let n = match self.last {
            Some((t, p, n)) if p == at && now.saturating_duration_since(t) <= MULTI_CLICK => {
                n % 3 + 1
            }
            _ => 1,
        };
        self.last = Some((now, at, n));
        n
    }
}

// ── Clipboard ────────────────────────────────────────────────────────────────

/// The most text copied at once, in bytes. Terminals cap OSC 52 payloads
/// (xterm's default is about 100 KB of base64); a larger selection is cut.
pub const CLIPBOARD_MAX: usize = 75_000;

/// `text` cut to at most [`CLIPBOARD_MAX`] bytes, on a char boundary.
pub fn clip(mut text: String) -> String {
    if text.len() > CLIPBOARD_MAX {
        let mut end = CLIPBOARD_MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// The OSC 52 sequence that sets the system clipboard (`c`) to `text`.
pub fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

/// Standard base64 with padding.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(s: &str) -> Vec<char> {
        s.chars().collect()
    }

    fn id() -> SessionId {
        uuid::Uuid::from_u128(7)
    }

    #[test]
    fn a_backward_drag_is_normalized() {
        let mut s = Selection::new(id(), Unit::Cell, (Pos::new(3, 10), Pos::new(3, 10)));
        s.extend((Pos::new(1, 4), Pos::new(1, 4)));
        assert_eq!(s.bounds(), (Pos::new(1, 4), Pos::new(3, 10)));
        // Same row, leftwards.
        let mut s = Selection::new(id(), Unit::Cell, (Pos::new(2, 9), Pos::new(2, 9)));
        s.extend((Pos::new(2, 3), Pos::new(2, 3)));
        assert_eq!(s.bounds(), (Pos::new(2, 3), Pos::new(2, 9)));
    }

    #[test]
    fn contains_is_a_stream_from_start_to_end() {
        let mut s = Selection::new(id(), Unit::Cell, (Pos::new(1, 5), Pos::new(1, 5)));
        s.extend((Pos::new(3, 2), Pos::new(3, 2)));
        assert!(!s.contains(1, 4));
        assert!(
            s.contains(1, 5) && s.contains(1, 79),
            "the first row to its end"
        );
        assert!(
            s.contains(2, 0) && s.contains(2, 79),
            "rows in between whole"
        );
        assert!(
            s.contains(3, 0) && s.contains(3, 2),
            "the last row up to the end"
        );
        assert!(!s.contains(3, 3) && !s.contains(0, 7) && !s.contains(4, 0));
    }

    #[test]
    fn a_click_without_a_drag_is_empty() {
        let mut s = Selection::new(id(), Unit::Cell, (Pos::new(1, 1), Pos::new(1, 1)));
        assert!(s.is_empty());
        s.extend((Pos::new(1, 1), Pos::new(1, 1)));
        assert!(s.is_empty());
        s.extend((Pos::new(1, 2), Pos::new(1, 2)));
        assert!(!s.is_empty());
        let w = Selection::new(id(), Unit::Word, (Pos::new(1, 1), Pos::new(1, 1)));
        assert!(
            !w.is_empty(),
            "a double-click on a one-letter word selects it"
        );
    }

    #[test]
    fn word_and_line_spans() {
        let line = chars("ls /tmp/a-b.txt (x) foo,bar  end");
        let at = |col| Unit::Word.span(Pos::new(4, col), &line);
        assert_eq!(
            at(4),
            (Pos::new(4, 3), Pos::new(4, 14)),
            "a path is one word"
        );
        assert_eq!(at(0), (Pos::new(4, 0), Pos::new(4, 1)));
        assert_eq!(
            at(16),
            (Pos::new(4, 16), Pos::new(4, 16)),
            "a bracket alone"
        );
        assert_eq!(at(17), (Pos::new(4, 17), Pos::new(4, 17)));
        assert_eq!(
            at(21),
            (Pos::new(4, 20), Pos::new(4, 22)),
            "a comma ends a word"
        );
        assert_eq!(
            at(27),
            (Pos::new(4, 27), Pos::new(4, 28)),
            "blanks select blanks"
        );
        assert_eq!(at(99), (Pos::new(4, 99), Pos::new(4, 99)), "past the row");
        assert_eq!(
            Unit::Line.span(Pos::new(4, 7), &line),
            (Pos::new(4, 0), Pos::new(4, line.len() as u16 - 1))
        );
        assert_eq!(
            Unit::Cell.span(Pos::new(4, 7), &line),
            (Pos::new(4, 7), Pos::new(4, 7))
        );
    }

    #[test]
    fn a_word_drag_extends_by_words_both_ways() {
        let line = chars("alpha beta gamma");
        let span = |col| Unit::Word.span(Pos::new(0, col), &line);
        let mut s = Selection::new(id(), Unit::Word, span(7));
        s.extend(span(1));
        assert_eq!(
            s.bounds(),
            (Pos::new(0, 0), Pos::new(0, 9)),
            "backwards keeps 'beta'"
        );
        s.extend(span(13));
        assert_eq!(s.bounds(), (Pos::new(0, 6), Pos::new(0, 15)));
    }

    #[test]
    fn clicks_count_up_to_three_on_one_cell_in_time() {
        let t = Instant::now();
        let ms = Duration::from_millis;
        let mut c = Clicks::default();
        let p = Pos::new(2, 2);
        assert_eq!(c.press(p, t), 1);
        assert_eq!(c.press(p, t + ms(100)), 2);
        assert_eq!(c.press(p, t + ms(200)), 3);
        assert_eq!(c.press(p, t + ms(300)), 1, "a fourth starts over");
        assert_eq!(c.press(Pos::new(2, 3), t + ms(350)), 1, "another cell");
        assert_eq!(
            c.press(Pos::new(2, 3), t + ms(350) + MULTI_CLICK + ms(1)),
            1,
            "too slow"
        );
        assert_eq!(Unit::from_clicks(1), Unit::Cell);
        assert_eq!(Unit::from_clicks(2), Unit::Word);
        assert_eq!(Unit::from_clicks(3), Unit::Line);
    }

    #[test]
    fn base64_matches_rfc4648() {
        for (plain, encoded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), encoded, "{plain:?}");
        }
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
    }

    #[test]
    fn osc52_sets_the_clipboard() {
        assert_eq!(osc52("hi ✓"), "\x1b]52;c;aGkg4pyT\x07");
    }

    #[test]
    fn clip_cuts_on_a_char_boundary() {
        assert_eq!(clip("short".into()), "short");
        let long = "é".repeat(CLIPBOARD_MAX); // 2 bytes each
        let cut = clip(long);
        assert!(cut.len() <= CLIPBOARD_MAX && cut.len() >= CLIPBOARD_MAX - 1);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
