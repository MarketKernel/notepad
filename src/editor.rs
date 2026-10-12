//! A virtualised plain-text editor widget.
//!
//! Only the lines that are visible on screen are ever read, laid out and
//! painted, so the cost per frame does not depend on the file size.
//! Positions are byte offsets into the [`Document`].

use std::ops::Range;
use std::sync::Arc;

use eframe::egui::{
    self, Align2, Color32, CursorIcon, Event, EventFilter, FontId, Galley, Id, ImeEvent, Key,
    Modifiers, Pos2, Rect, Sense, Stroke, Ui, Vec2,
    text::{CCursor, LayoutJob, LayoutSection, TextFormat},
};

use crate::buffer::{Change, Document, fold_case};
use crate::highlight::{self, Spans};

/// Lines longer than this are shown truncated (editing still works on the
/// full line; the rest just isn't rendered).
const MAX_LINE_DISPLAY: usize = 64 * 1024;
/// Lines longer than this are not syntax-highlighted.
const MAX_HIGHLIGHT_LEN: usize = 4096;
const HIGHLIGHT_WARMUP_LINES: u64 = 30;
const UNDO_LIMIT: usize = 1000;
/// Refuse to put more than this into the clipboard.
pub const MAX_CLIPBOARD: u64 = 256 * 1024 * 1024;
pub const DEFAULT_FONT_SIZE: f32 = 16.0;
const TEXT_PAD: f32 = 6.0;
const SCROLLBAR_W: f32 = 12.0;

// ---------------------------------------------------------------------------
// Line text & layout

/// The decoded text of one line.
struct LineText {
    start: u64,
    /// End of the line text (excluding `\r\n` / `\n`).
    end: u64,
    /// End of what is displayed (`< end` if the line is truncated).
    shown_end: u64,
    /// Start of the next line (`None` for the last line).
    next: Option<u64>,
    text: String,
    /// Byte offset (relative to `start`) of every char, plus one final entry.
    char_bytes: Vec<u32>,
}

impl LineText {
    fn truncated(&self) -> bool {
        self.shown_end < self.end
    }

    fn char_count(&self) -> usize {
        self.char_bytes.len() - 1
    }

    fn byte_to_char(&self, pos: u64) -> usize {
        let rel = (pos.clamp(self.start, self.shown_end) - self.start) as u32;
        match self.char_bytes.binary_search(&rel) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    fn char_to_byte(&self, c: usize) -> u64 {
        self.start + self.char_bytes[c.min(self.char_count())] as u64
    }
}

struct LineLayout {
    line: u64,
    t: LineText,
    galley: Arc<Galley>,
}

impl LineLayout {
    fn rows(&self) -> usize {
        self.galley.rows.len().max(1)
    }

    fn row_of(&self, c: usize) -> usize {
        self.galley
            .layout_from_cursor(ccursor(c))
            .row
            .min(self.rows() - 1)
    }

    /// Char index of the first char of each row, plus the total.
    fn row_starts(&self) -> Vec<usize> {
        let mut v = Vec::with_capacity(self.galley.rows.len() + 1);
        let mut c = 0;
        for r in &self.galley.rows {
            v.push(c);
            c += r.char_count_including_newline().0;
        }
        v.push(c);
        v
    }

    fn row_top(&self, row: usize) -> f32 {
        self.galley.rows.get(row).map_or(0.0, |r| r.min_y())
    }

    fn height(&self, row_h: f32) -> f32 {
        self.galley.size().y.max(row_h)
    }

    /// Char index at `x` (galley coordinates) on row `row`.
    fn char_at_row_x(&self, row: usize, x: f32) -> usize {
        let starts = self.row_starts();
        let Some(r) = self.galley.rows.get(row) else {
            return 0;
        };
        let mut col = r.row.char_at(x - r.pos.x).0;
        let count = r.row.char_count_excluding_newline().0;
        // On a wrapped (non-last) row, "end of row" is the start of the next one.
        if row + 1 < self.galley.rows.len() && col >= count && count > 0 {
            col = count - 1;
        }
        starts[row] + col
    }
}

fn ccursor(c: usize) -> CCursor {
    CCursor {
        index: c.into(),
        prefer_next_row: true,
    }
}

fn display_char(c: char) -> char {
    match c {
        '\t' => '\t',
        '\0'..='\u{1f}' => char::from_u32(0x2400 + c as u32).unwrap_or('?'),
        '\u{7f}' => '\u{2421}',
        c => c,
    }
}

/// Lossy UTF-8 decoding that remembers where every char came from.
fn decode(bytes: &[u8]) -> (String, Vec<u32>) {
    let mut s = String::with_capacity(bytes.len());
    let mut map = Vec::with_capacity(bytes.len() + 1);
    fn push(v: &str, base: usize, s: &mut String, map: &mut Vec<u32>) {
        for (off, ch) in v.char_indices() {
            map.push((base + off) as u32);
            s.push(display_char(ch));
        }
    }
    let mut i = 0;
    while i < bytes.len() {
        match std::str::from_utf8(&bytes[i..]) {
            Ok(v) => {
                push(v, i, &mut s, &mut map);
                break;
            }
            Err(e) => {
                let valid = e.valid_up_to();
                push(
                    std::str::from_utf8(&bytes[i..i + valid]).unwrap_or_default(),
                    i,
                    &mut s,
                    &mut map,
                );
                i += valid;
                let bad = e.error_len().unwrap_or(bytes.len() - i);
                for _ in 0..bad {
                    map.push(i as u32);
                    s.push('\u{FFFD}');
                    i += 1;
                }
            }
        }
    }
    map.push(bytes.len() as u32);
    (s, map)
}

/// The byte pattern to search for (case-folded unless `match_case`).
fn search_key(needle: &str, match_case: bool) -> Vec<u8> {
    let mut v = needle.as_bytes().to_vec();
    if !match_case {
        fold_case(&mut v);
    }
    v
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

// ---------------------------------------------------------------------------
// Editor state

#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    Typing,
    Deleting,
    Other,
}

struct UndoEntry {
    /// Piece-list changes of this step, oldest first.
    changes: Vec<Change>,
    cursor: u64,
    anchor: u64,
    state: u64,
}

struct HighlightCache {
    key: (u64, u64, usize, String, bool),
    spans: Vec<Spans>,
}

pub struct Editor {
    pub doc: Document,
    pub cursor: u64,
    pub anchor: u64,
    pub crlf: bool,
    pub wrap: bool,
    pub line_numbers: bool,
    pub font_size: f32,
    pub language: Option<String>,
    /// Status message shown by the app (e.g. "clipboard too large").
    pub notice: Option<String>,

    top_line: u64,
    top_row: usize,
    scroll_frac: f32,
    /// Furthest (top_line, top_row) that still keeps the view full.
    max_top: (u64, usize),
    h_offset: f32,
    max_line_w: f32,
    desired_x: Option<f32>,
    scroll_to_cursor: bool,
    view_rows: usize,
    wrap_width: f32,
    font_id: FontId,
    row_h: f32,

    undo: Vec<UndoEntry>,
    redo: Vec<UndoEntry>,
    last_kind: Option<EditKind>,
    last_edit_time: f64,
    last_edit_cursor: u64,
    state: u64,
    next_state: u64,
    saved_state: u64,

    preedit: String,
    hl: Option<HighlightCache>,
    status_cache: Option<((u64, u64), (u64, u64))>,
    v_drag_offset: Option<f32>,
}

impl Editor {
    pub fn new(doc: Document) -> Self {
        // A new document gets the platform's line endings.
        let crlf = doc.original().map_or(cfg!(windows), |o| o.crlf);
        Self {
            doc,
            cursor: 0,
            anchor: 0,
            crlf,
            wrap: true,
            line_numbers: true,
            font_size: DEFAULT_FONT_SIZE,
            language: None,
            notice: None,
            top_line: 0,
            top_row: 0,
            scroll_frac: 0.0,
            max_top: (0, 0),
            h_offset: 0.0,
            max_line_w: 0.0,
            desired_x: None,
            scroll_to_cursor: false,
            view_rows: 40,
            wrap_width: 600.0,
            font_id: FontId::monospace(DEFAULT_FONT_SIZE),
            row_h: 16.0,
            undo: Vec::new(),
            redo: Vec::new(),
            last_kind: None,
            last_edit_time: 0.0,
            last_edit_cursor: 0,
            state: 0,
            next_state: 1,
            saved_state: 0,
            preedit: String::new(),
            hl: None,
            status_cache: None,
            v_drag_offset: None,
        }
    }

    pub fn id() -> Id {
        Id::new("notepad-editor")
    }

    pub fn is_modified(&self) -> bool {
        self.state != self.saved_state
    }

    pub fn mark_saved(&mut self) {
        self.saved_state = self.state;
    }

    pub fn selection(&self) -> Range<u64> {
        self.cursor.min(self.anchor)..self.cursor.max(self.anchor)
    }

    pub fn has_selection(&self) -> bool {
        self.cursor != self.anchor
    }

    /// (line, column), both 1-based, for the status bar.
    pub fn cursor_line_col(&mut self) -> (u64, u64) {
        let key = (self.cursor, self.doc.version);
        if let Some((k, v)) = self.status_cache
            && k == key
        {
            return v;
        }
        let line = self.doc.line_of(self.cursor);
        let start = self.doc.line_start(line);
        let a = start.max(self.cursor.saturating_sub(1 << 20));
        let bytes = self.doc.read(a, self.cursor);
        let col = (self.cursor - a) - bytes.iter().filter(|&&b| b & 0xC0 == 0x80).count() as u64;
        let v = (line + 1, col + 1);
        self.status_cache = Some((key, v));
        v
    }

    // ---- reading lines --------------------------------------------------------

    /// Reads the line starting at `start` (which must be a line start).
    fn line_text_at(&self, line: u64, start: u64) -> LineText {
        let len = self.doc.len();
        let mut want = 256usize;
        loop {
            let b = (start + want as u64).min(len);
            let bytes = self.doc.read(start, b);
            if let Some(i) = memchr::memchr(b'\n', &bytes) {
                let mut end = i;
                if end > 0 && bytes[end - 1] == b'\r' {
                    end -= 1;
                }
                let shown = end.min(MAX_LINE_DISPLAY);
                let (text, char_bytes) = decode(&bytes[..shown]);
                return LineText {
                    start,
                    end: start + end as u64,
                    shown_end: start + shown as u64,
                    next: Some(start + i as u64 + 1),
                    text,
                    char_bytes,
                };
            }
            if b == len {
                let shown = bytes.len().min(MAX_LINE_DISPLAY);
                let (text, char_bytes) = decode(&bytes[..shown]);
                return LineText {
                    start,
                    end: len,
                    shown_end: start + shown as u64,
                    next: None,
                    text,
                    char_bytes,
                };
            }
            if want > MAX_LINE_DISPLAY {
                // A huge line: find its end through the index instead of scanning.
                let next = self.doc.line_start(line + 1);
                let mut end = next - 1;
                if end > start && self.doc.byte_at(end - 1) == Some(b'\r') {
                    end -= 1;
                }
                let shown = MAX_LINE_DISPLAY.min(bytes.len());
                let (text, char_bytes) = decode(&bytes[..shown]);
                return LineText {
                    start,
                    end,
                    shown_end: start + shown as u64,
                    next: Some(next),
                    text,
                    char_bytes,
                };
            }
            want *= 4;
        }
    }

    fn line_text(&self, line: u64) -> LineText {
        self.line_text_at(line, self.doc.line_start(line))
    }

    fn make_job(&self, text: &str, spans: Option<&Spans>, color: Color32) -> LayoutJob {
        let mut job = LayoutJob {
            text: text.to_owned(),
            keep_trailing_whitespace: true,
            ..Default::default()
        };
        job.wrap.max_width = if self.wrap { self.wrap_width } else { f32::INFINITY };
        let fmt = |c| TextFormat::simple(self.font_id.clone(), c);
        let mut pos = 0;
        if let Some(spans) = spans {
            for (r, c) in spans {
                if r.start != pos || r.end > text.len() || !text.is_char_boundary(r.end) {
                    break;
                }
                job.sections.push(LayoutSection {
                    leading_space: 0.0,
                    byte_range: r.start.into()..r.end.into(),
                    format: fmt(*c),
                });
                pos = r.end;
            }
        }
        if pos < text.len() || job.sections.is_empty() {
            job.sections.push(LayoutSection {
                leading_space: 0.0,
                byte_range: pos.into()..text.len().into(),
                format: fmt(color),
            });
        }
        job
    }

    fn layout_text(&self, ui: &Ui, line: u64, t: LineText, spans: Option<&Spans>) -> LineLayout {
        let job = self.make_job(&t.text, spans, ui.visuals().strong_text_color());
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        LineLayout { line, t, galley }
    }

    fn layout_line(&self, ui: &Ui, line: u64) -> LineLayout {
        let t = self.line_text(line);
        self.layout_text(ui, line, t, None)
    }

    fn rows_of(&self, ui: &Ui, line: u64) -> usize {
        if self.wrap {
            self.layout_line(ui, line).rows()
        } else {
            1
        }
    }

    // ---- scrolling ------------------------------------------------------------

    fn step_down(&mut self, ui: &Ui) -> bool {
        if self.top_row + 1 < self.rows_of(ui, self.top_line) {
            self.top_row += 1;
            true
        } else if self.top_line + 1 < self.doc.line_count() {
            self.top_line += 1;
            self.top_row = 0;
            true
        } else {
            false
        }
    }

    fn step_up(&mut self, ui: &Ui) -> bool {
        if self.top_row > 0 {
            self.top_row -= 1;
            true
        } else if self.top_line > 0 {
            self.top_line -= 1;
            self.top_row = self.rows_of(ui, self.top_line) - 1;
            true
        } else {
            false
        }
    }

    fn scroll_px(&mut self, ui: &Ui, dy: f32) {
        self.scroll_frac += dy;
        while self.scroll_frac >= self.row_h {
            if !self.step_down(ui) {
                self.scroll_frac = 0.0;
                break;
            }
            self.scroll_frac -= self.row_h;
        }
        while self.scroll_frac < 0.0 {
            if !self.step_up(ui) {
                self.scroll_frac = 0.0;
                break;
            }
            self.scroll_frac += self.row_h;
        }
    }

    fn scroll_rows(&mut self, ui: &Ui, n: i64) {
        for _ in 0..n.unsigned_abs() {
            let moved = if n > 0 { self.step_down(ui) } else { self.step_up(ui) };
            if !moved {
                break;
            }
        }
    }

    /// Top position at which the last row of the document is the last visible row.
    fn compute_max_top(&self, ui: &Ui) -> (u64, usize) {
        let mut line = self.doc.line_count() - 1;
        let mut row = self.rows_of(ui, line) - 1;
        for _ in 1..self.view_rows {
            if row > 0 {
                row -= 1;
            } else if line > 0 {
                line -= 1;
                row = self.rows_of(ui, line) - 1;
            } else {
                break;
            }
        }
        (line, row)
    }

    /// Don't scroll past the end: the view stays put instead of overshooting
    /// and snapping back (which made trackpad scrolling and thumb dragging jitter).
    fn clamp_top(&mut self) {
        if (self.top_line, self.top_row) >= self.max_top {
            (self.top_line, self.top_row) = self.max_top;
            self.scroll_frac = 0.0;
        }
    }

    /// Scroll position and its maximum, in lines (wrapped rows count as fractions).
    fn v_scroll(&self, ui: &Ui) -> (f64, f64) {
        let rows = |line| self.rows_of(ui, line) as f64;
        let pos = self.top_line as f64
            + (self.top_row as f64 + (self.scroll_frac / self.row_h) as f64) / rows(self.top_line);
        let max = self.max_top.0 as f64 + self.max_top.1 as f64 / rows(self.max_top.0);
        (pos, max)
    }

    fn set_top(&mut self, line: u64) {
        self.top_line = line.min(self.doc.line_count() - 1);
        self.top_row = 0;
        self.scroll_frac = 0.0;
    }

    fn ensure_cursor_visible(&mut self, ui: &Ui, text_w: f32) {
        let cl = self.doc.line_of(self.cursor);
        let lay = self.layout_line(ui, cl);
        let ci = lay.t.byte_to_char(self.cursor);
        let crow = lay.row_of(ci);

        if (cl, crow) < (self.top_line, self.top_row)
            || ((cl, crow) == (self.top_line, self.top_row) && self.scroll_frac > 0.0)
        {
            self.top_line = cl;
            self.top_row = crow;
            self.scroll_frac = 0.0;
        } else {
            // Count rows from the top of the view down to the cursor row.
            let limit = self.view_rows.saturating_sub(1);
            let mut rows = 0usize;
            let mut too_far = cl - self.top_line > limit as u64 + 1;
            if !too_far {
                let mut l = self.top_line;
                while l < cl {
                    rows += self.rows_of(ui, l);
                    if rows > limit + self.top_row {
                        too_far = true;
                        break;
                    }
                    l += 1;
                }
                rows = (rows + crow).saturating_sub(self.top_row);
                let frac_row = usize::from(self.scroll_frac > 0.0);
                too_far |= rows + frac_row > limit;
            }
            if too_far {
                // Put the cursor row on the last visible row.
                self.top_line = cl;
                self.top_row = crow;
                self.scroll_frac = 0.0;
                self.scroll_rows(ui, -(limit as i64));
            }
        }

        if !self.wrap {
            let x = lay.galley.pos_from_cursor(ccursor(ci)).min.x;
            let margin = 4.0 * self.row_h;
            if x < self.h_offset {
                self.h_offset = (x - margin).max(0.0);
            } else if x > self.h_offset + text_w - 2.0 * TEXT_PAD {
                self.h_offset = x - text_w + margin;
            }
        } else {
            self.h_offset = 0.0;
        }
    }

    /// Scroll so that the cursor line is roughly a third from the top.
    pub fn center_cursor(&mut self) {
        let line = self.doc.line_of(self.cursor);
        self.set_top(line.saturating_sub(self.view_rows as u64 / 3));
        self.scroll_to_cursor = true;
    }

    // ---- cursor movement ------------------------------------------------------

    fn prev_boundary(&self, pos: u64) -> u64 {
        if pos == 0 {
            return 0;
        }
        let a = pos.saturating_sub(4);
        let b = self.doc.read(a, pos);
        if b.ends_with(b"\r\n") {
            return pos - 2;
        }
        let n = b.len();
        for k in 1..=n {
            let s = n - k;
            if b[s] & 0xC0 != 0x80 {
                if std::str::from_utf8(&b[s..]).is_ok() {
                    return pos - k as u64;
                }
                break;
            }
        }
        pos - 1
    }

    fn next_boundary(&self, pos: u64) -> u64 {
        let len = self.doc.len();
        if pos >= len {
            return len;
        }
        let b = self.doc.read(pos, pos + 4);
        if b.starts_with(b"\r\n") {
            return pos + 2;
        }
        let l = match b[0] {
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => 1,
        };
        if l > 1 && b.len() >= l && std::str::from_utf8(&b[..l]).is_ok() {
            pos + l as u64
        } else {
            pos + 1
        }
    }

    fn word_left(&self, pos: u64) -> u64 {
        let t = self.line_text(self.doc.line_of(pos));
        if pos <= t.start || pos > t.shown_end {
            return self.prev_boundary(pos);
        }
        let chars: Vec<char> = t.text.chars().collect();
        let mut i = t.byte_to_char(pos);
        while i > 0 && !is_word(chars[i - 1]) {
            i -= 1;
        }
        while i > 0 && is_word(chars[i - 1]) {
            i -= 1;
        }
        t.char_to_byte(i)
    }

    fn word_right(&self, pos: u64) -> u64 {
        let t = self.line_text(self.doc.line_of(pos));
        if pos >= t.end || pos >= t.shown_end {
            return self.next_boundary(pos);
        }
        let chars: Vec<char> = t.text.chars().collect();
        let mut i = t.byte_to_char(pos);
        while i < chars.len() && !is_word(chars[i]) {
            i += 1;
        }
        while i < chars.len() && is_word(chars[i]) {
            i += 1;
        }
        t.char_to_byte(i)
    }

    fn word_at(&self, pos: u64) -> Range<u64> {
        let t = self.line_text(self.doc.line_of(pos));
        let chars: Vec<char> = t.text.chars().collect();
        if chars.is_empty() {
            return pos..pos;
        }
        let i = t.byte_to_char(pos).min(chars.len() - 1);
        if !is_word(chars[i]) {
            return t.char_to_byte(i)..t.char_to_byte(i + 1);
        }
        let (mut a, mut b) = (i, i);
        while a > 0 && is_word(chars[a - 1]) {
            a -= 1;
        }
        while b < chars.len() && is_word(chars[b]) {
            b += 1;
        }
        t.char_to_byte(a)..t.char_to_byte(b)
    }

    fn line_home(&self, pos: u64) -> u64 {
        self.doc.line_start(self.doc.line_of(pos))
    }

    fn line_end(&self, pos: u64) -> u64 {
        self.doc.line_range(self.doc.line_of(pos)).1
    }

    /// Moves `rows` visual rows up (negative) or down from `from`.
    fn vertical(&mut self, ui: &Ui, from: u64, rows: i64) -> u64 {
        let mut line = self.doc.line_of(from);
        let mut lay = self.layout_line(ui, line);
        let ci = lay.t.byte_to_char(from);
        let x = *self
            .desired_x
            .get_or_insert_with(|| lay.galley.pos_from_cursor(ccursor(ci)).min.x);
        let mut row = lay.row_of(ci);
        let mut left = rows;
        while left != 0 {
            if left < 0 {
                if row > 0 {
                    row -= 1;
                } else if line > 0 {
                    line -= 1;
                    lay = self.layout_line(ui, line);
                    row = lay.rows() - 1;
                } else {
                    return 0;
                }
                left += 1;
            } else {
                if row + 1 < lay.rows() {
                    row += 1;
                } else if line + 1 < self.doc.line_count() {
                    line += 1;
                    lay = self.layout_line(ui, line);
                    row = 0;
                } else {
                    return self.doc.len();
                }
                left -= 1;
            }
        }
        lay.t.char_to_byte(lay.char_at_row_x(row, x))
    }

    fn move_to(&mut self, pos: u64, select: bool, keep_x: bool) {
        self.cursor = pos.min(self.doc.len());
        if !select {
            self.anchor = self.cursor;
        }
        if !keep_x {
            self.desired_x = None;
        }
        self.scroll_to_cursor = true;
        self.last_kind = None;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.doc.len();
        self.last_kind = None;
    }

    // ---- editing ----------------------------------------------------------------

    fn begin_edit(&mut self, kind: EditKind, now: f64) {
        let merge = kind != EditKind::Other
            && self.last_kind == Some(kind)
            && now - self.last_edit_time < 2.0
            && self.cursor == self.last_edit_cursor
            && !self.has_selection();
        if !merge {
            self.undo.push(UndoEntry {
                changes: Vec::new(),
                cursor: self.cursor,
                anchor: self.anchor,
                state: self.state,
            });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last_kind = Some(kind);
        self.last_edit_time = now;
    }

    fn end_edit(&mut self) {
        let changes = self.doc.take_changes();
        if let Some(e) = self.undo.last_mut() {
            e.changes.extend(changes);
        }
        self.state = self.next_state;
        self.next_state += 1;
        self.anchor = self.cursor;
        self.last_edit_cursor = self.cursor;
        self.desired_x = None;
        self.scroll_to_cursor = true;
    }

    fn replace_selection(&mut self, bytes: &[u8], kind: EditKind, now: f64) {
        let sel = self.selection();
        if sel.is_empty() && bytes.is_empty() {
            return;
        }
        self.begin_edit(kind, now);
        self.doc.delete(sel.start, sel.end);
        self.doc.insert(sel.start, bytes);
        self.cursor = sel.start + bytes.len() as u64;
        self.end_edit();
    }

    fn delete_range(&mut self, r: Range<u64>, now: f64) {
        if r.is_empty() {
            return;
        }
        self.begin_edit(EditKind::Deleting, now);
        self.doc.delete(r.start, r.end);
        self.cursor = r.start;
        self.end_edit();
    }

    /// Converts newlines in pasted / typed text to the document's style.
    fn normalize(&self, text: &str) -> Vec<u8> {
        let t = text.replace("\r\n", "\n").replace('\r', "\n");
        if self.crlf { t.replace('\n', "\r\n") } else { t }.into_bytes()
    }

    pub fn insert_text(&mut self, text: &str, now: f64) {
        let bytes = self.normalize(text);
        self.replace_selection(&bytes, EditKind::Other, now);
    }

    pub fn undo(&mut self) {
        if let Some(e) = self.undo.pop() {
            let inverse = self.apply_entry(e);
            self.redo.push(inverse);
        }
    }

    pub fn redo(&mut self) {
        if let Some(e) = self.redo.pop() {
            let inverse = self.apply_entry(e);
            self.undo.push(inverse);
        }
    }

    /// Reverts the changes of `e`; returns the entry that reverts this again.
    fn apply_entry(&mut self, e: UndoEntry) -> UndoEntry {
        let inverse = UndoEntry {
            changes: self.doc.revert(e.changes),
            cursor: self.cursor,
            anchor: self.anchor,
            state: self.state,
        };
        self.cursor = e.cursor.min(self.doc.len());
        self.anchor = e.anchor.min(self.doc.len());
        self.state = e.state;
        self.last_kind = None;
        self.desired_x = None;
        self.scroll_to_cursor = true;
        inverse
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Selected text for the clipboard, or `None` if nothing / too large.
    pub fn selected_text(&mut self) -> Option<String> {
        let sel = self.selection();
        if sel.is_empty() {
            return None;
        }
        if sel.end - sel.start > MAX_CLIPBOARD {
            self.notice = Some(format!(
                "Selection is too large for the clipboard (max {} MB)",
                MAX_CLIPBOARD >> 20
            ));
            return None;
        }
        Some(String::from_utf8_lossy(&self.doc.read(sel.start, sel.end)).into_owned())
    }

    pub fn copy(&mut self, ctx: &egui::Context) {
        if let Some(t) = self.selected_text() {
            ctx.copy_text(t);
        }
    }

    pub fn cut(&mut self, ctx: &egui::Context, now: f64) {
        if let Some(t) = self.selected_text() {
            ctx.copy_text(t);
            let sel = self.selection();
            self.delete_range(sel, now);
        }
    }

    // ---- search -------------------------------------------------------------------

    /// Finds the next (or previous) occurrence and selects it. Wraps around.
    pub fn find(&mut self, needle: &str, match_case: bool, backwards: bool) -> bool {
        if needle.is_empty() {
            return false;
        }
        let needle = search_key(needle, match_case);
        let sel = self.selection();
        let len = self.doc.len();
        let found = if backwards {
            self.search_back(&needle, match_case, 0, sel.start)
                .or_else(|| self.search_back(&needle, match_case, sel.start, len))
        } else {
            self.search_fwd(&needle, match_case, sel.end, len)
                .or_else(|| self.search_fwd(&needle, match_case, 0, sel.end))
        };
        if let Some(p) = found {
            self.anchor = p;
            self.cursor = p + needle.len() as u64;
            self.desired_x = None;
            self.last_kind = None;
            self.scroll_to_cursor = true;
            true
        } else {
            false
        }
    }

    /// Replaces the selection if it is a match, then selects the next match.
    pub fn replace(&mut self, needle: &str, replacement: &str, match_case: bool, now: f64) -> bool {
        if needle.is_empty() {
            return false;
        }
        let sel = self.selection();
        if sel.end - sel.start == needle.len() as u64 {
            let mut selected = self.doc.read(sel.start, sel.end);
            if !match_case {
                fold_case(&mut selected);
            }
            if selected == search_key(needle, match_case) {
                let rep = self.normalize(replacement);
                self.replace_selection(&rep, EditKind::Other, now);
            }
        }
        self.find(needle, match_case, false)
    }

    /// Replaces all occurrences (one undo step). Returns how many were replaced.
    pub fn replace_all(&mut self, needle: &str, replacement: &str, match_case: bool) -> u64 {
        if needle.is_empty() {
            return 0;
        }
        let rep = self.normalize(replacement);
        let count = self.doc.replace_all(&search_key(needle, match_case), &rep, !match_case);
        if count > 0 {
            self.undo.push(UndoEntry {
                changes: self.doc.take_changes(),
                cursor: self.cursor,
                anchor: self.anchor,
                state: self.state,
            });
            self.redo.clear();
            self.last_kind = None;
            self.cursor = self.cursor.min(self.doc.len());
            self.end_edit();
        }
        count
    }

    fn search_fwd(&self, needle: &[u8], match_case: bool, a: u64, b: u64) -> Option<u64> {
        const CHUNK: u64 = 4 << 20;
        let finder = memchr::memmem::Finder::new(needle);
        let overlap = needle.len() as u64 - 1;
        let mut pos = a;
        while pos + (needle.len() as u64) <= b {
            let end = (pos + CHUNK + overlap).min(b);
            let mut buf = self.doc.read(pos, end);
            if !match_case {
                fold_case(&mut buf);
            }
            if let Some(i) = finder.find(&buf) {
                return Some(pos + i as u64);
            }
            if end == b {
                break;
            }
            pos = end - overlap;
        }
        None
    }

    fn search_back(&self, needle: &[u8], match_case: bool, a: u64, b: u64) -> Option<u64> {
        const CHUNK: u64 = 4 << 20;
        let finder = memchr::memmem::FinderRev::new(needle);
        let overlap = needle.len() as u64 - 1;
        let mut end = b;
        while end >= a + needle.len() as u64 {
            let start = end.saturating_sub(CHUNK + overlap).max(a);
            let mut buf = self.doc.read(start, end);
            if !match_case {
                fold_case(&mut buf);
            }
            if let Some(i) = finder.rfind(&buf) {
                return Some(start + i as u64);
            }
            if start == a {
                break;
            }
            end = start + overlap;
        }
        None
    }

    pub fn goto_line(&mut self, line: u64) {
        let line = line.clamp(1, self.doc.line_count()) - 1;
        self.move_to(self.doc.line_start(line), false, false);
        self.center_cursor();
    }

    // ---- input ----------------------------------------------------------------------

    fn handle_key(&mut self, ui: &Ui, key: Key, m: Modifiers, now: f64) {
        let mac = cfg!(target_os = "macos");
        let word = if mac { m.alt } else { m.ctrl };
        let line_mod = mac && m.mac_cmd;
        let doc_mod = if mac { m.mac_cmd } else { m.ctrl };
        let sel = m.shift;
        let page = self.view_rows.saturating_sub(1).max(1) as i64;
        match key {
            Key::ArrowLeft => {
                if !sel && self.has_selection() && !word && !line_mod {
                    let p = self.selection().start;
                    self.move_to(p, false, false);
                } else {
                    let p = if line_mod {
                        self.line_home(self.cursor)
                    } else if word {
                        self.word_left(self.cursor)
                    } else {
                        self.prev_boundary(self.cursor)
                    };
                    self.move_to(p, sel, false);
                }
            }
            Key::ArrowRight => {
                if !sel && self.has_selection() && !word && !line_mod {
                    let p = self.selection().end;
                    self.move_to(p, false, false);
                } else {
                    let p = if line_mod {
                        self.line_end(self.cursor)
                    } else if word {
                        self.word_right(self.cursor)
                    } else {
                        self.next_boundary(self.cursor)
                    };
                    self.move_to(p, sel, false);
                }
            }
            Key::ArrowUp => {
                if doc_mod {
                    self.move_to(0, sel, false);
                } else {
                    let p = self.vertical(ui, self.cursor, -1);
                    self.move_to(p, sel, true);
                }
            }
            Key::ArrowDown => {
                if doc_mod {
                    self.move_to(self.doc.len(), sel, false);
                } else {
                    let p = self.vertical(ui, self.cursor, 1);
                    self.move_to(p, sel, true);
                }
            }
            Key::Home => {
                let p = if m.command { 0 } else { self.line_home(self.cursor) };
                self.move_to(p, sel, false);
            }
            Key::End => {
                let p = if m.command { self.doc.len() } else { self.line_end(self.cursor) };
                self.move_to(p, sel, false);
            }
            Key::PageUp => {
                let p = self.vertical(ui, self.cursor, -page);
                self.scroll_rows(ui, -page);
                self.move_to(p, sel, true);
            }
            Key::PageDown => {
                let p = self.vertical(ui, self.cursor, page);
                self.scroll_rows(ui, page);
                self.move_to(p, sel, true);
            }
            Key::Backspace => {
                let r = if self.has_selection() {
                    self.selection()
                } else if line_mod {
                    self.line_home(self.cursor)..self.cursor
                } else if word {
                    self.word_left(self.cursor)..self.cursor
                } else {
                    self.prev_boundary(self.cursor)..self.cursor
                };
                self.delete_range(r, now);
            }
            Key::Delete => {
                let r = if self.has_selection() {
                    self.selection()
                } else if word {
                    self.cursor..self.word_right(self.cursor)
                } else {
                    self.cursor..self.next_boundary(self.cursor)
                };
                self.delete_range(r, now);
            }
            Key::Enter => {
                let nl: &[u8] = if self.crlf { b"\r\n" } else { b"\n" };
                self.replace_selection(nl, EditKind::Other, now);
            }
            Key::Tab if !m.shift && !m.command => {
                self.replace_selection(b"\t", EditKind::Typing, now);
            }
            Key::A if m.command => self.select_all(),
            Key::Z if m.command && m.shift => self.redo(),
            Key::Z if m.command => self.undo(),
            Key::Y if m.command && !mac => self.redo(),
            _ => {}
        }
    }

    fn handle_events(&mut self, ui: &Ui) {
        let filter = EventFilter {
            tab: true,
            horizontal_arrows: true,
            vertical_arrows: true,
            escape: false,
        };
        ui.memory_mut(|m| m.set_focus_lock_filter(Self::id(), filter));
        let events = ui.input(|i| i.filtered_events(&filter));
        let now = ui.input(|i| i.time);
        for ev in events {
            match ev {
                Event::Text(t) => {
                    if !t.is_empty() && !t.chars().all(char::is_control) {
                        let kind = if t == " " { EditKind::Other } else { EditKind::Typing };
                        let bytes = self.normalize(&t);
                        self.replace_selection(&bytes, kind, now);
                    }
                }
                Event::Paste(t) => self.insert_text(&t, now),
                Event::Copy => self.copy(ui.ctx()),
                Event::Cut => self.cut(ui.ctx(), now),
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } => self.handle_key(ui, key, modifiers, now),
                Event::Ime(ImeEvent::Preedit { text, .. }) => self.preedit = text,
                Event::Ime(ImeEvent::Commit(t)) => {
                    self.preedit.clear();
                    if !t.is_empty() {
                        let bytes = self.normalize(&t);
                        self.replace_selection(&bytes, EditKind::Typing, now);
                    }
                }
                _ => {}
            }
        }
    }

    // ---- frame ------------------------------------------------------------------------

    /// Highlight spans for `count` lines starting at `first`.
    fn highlight_spans(&mut self, ui: &Ui, first: u64, texts: &[&str]) -> Option<Vec<Spans>> {
        let lang = self.language.clone()?;
        let dark = ui.visuals().dark_mode;
        let key = (self.doc.version, first, texts.len(), lang.clone(), dark);
        if let Some(c) = &self.hl
            && c.key == key
        {
            return Some(c.spans.clone());
        }
        let warm = first.min(HIGHLIGHT_WARMUP_LINES);
        let mut warm_texts = Vec::new();
        let mut pos = self.doc.line_start(first - warm);
        for l in first - warm..first {
            let t = self.line_text_at(l, pos);
            pos = t.next.unwrap_or(self.doc.len());
            warm_texts.push(t.text);
        }
        let mut all: Vec<&str> = warm_texts.iter().map(String::as_str).collect();
        all.extend_from_slice(texts);
        let mut spans = highlight::highlight(&lang, dark, &all, MAX_HIGHLIGHT_LEN);
        let spans = spans.split_off(warm as usize);
        self.hl = Some(HighlightCache {
            key,
            spans: spans.clone(),
        });
        Some(spans)
    }

    /// Lays out the lines visible from the current top position.
    /// Returns layouts with the y offset (relative to the view top) of each galley.
    fn visible_lines(&mut self, ui: &Ui, height: f32) -> (Vec<(LineLayout, f32)>, bool) {
        let count = self.view_rows as u64 + 2;
        let mut texts = Vec::new();
        let mut pos = self.doc.line_start(self.top_line);
        let mut line = self.top_line;
        while line < self.doc.line_count() && (texts.len() as u64) < count {
            let t = self.line_text_at(line, pos);
            let next = t.next;
            texts.push(t);
            match next {
                Some(n) => pos = n,
                None => break,
            }
            line += 1;
        }
        let strs: Vec<&str> = texts.iter().map(|t| t.text.as_str()).collect();
        let spans = self.highlight_spans(ui, self.top_line, &strs);

        let mut out = Vec::new();
        let mut y = 0.0;
        let mut reached_end = false;
        for (i, t) in texts.into_iter().enumerate() {
            let line = self.top_line + i as u64;
            let lay = self.layout_text(ui, line, t, spans.as_ref().map(|s| &s[i]));
            let top = if i == 0 {
                self.top_row = self.top_row.min(lay.rows() - 1);
                -lay.row_top(self.top_row) - self.scroll_frac
            } else {
                y
            };
            y = top + lay.height(self.row_h);
            let last = lay.t.next.is_none();
            out.push((lay, top));
            if last {
                reached_end = y <= height;
                break;
            }
            if y > height {
                break;
            }
        }
        if out.is_empty() {
            reached_end = true;
        }
        let empty_rows = if reached_end {
            ((height - y) / self.row_h).floor().max(0.0) as i64
        } else {
            0
        };
        (out, reached_end && empty_rows > 0 && (self.top_line, self.top_row) != (0, 0))
    }

    fn hit_test(&self, vis: &[(LineLayout, f32)], origin: Pos2, p: Pos2) -> u64 {
        let Some(first) = vis.first() else { return 0 };
        if p.y < origin.y + first.1 {
            return first.0.t.start;
        }
        for (lay, y) in vis {
            if p.y < origin.y + y + lay.height(self.row_h) {
                let local = p - (origin + Vec2::new(0.0, *y));
                let c = lay.galley.cursor_from_pos(local).index.0;
                return lay.t.char_to_byte(c);
            }
        }
        let last = &vis.last().unwrap().0;
        if last.t.next.is_none() {
            self.doc.len()
        } else {
            let local = p - (origin + Vec2::new(0.0, vis.last().unwrap().1));
            let c = last.galley.cursor_from_pos(local).index.0;
            last.t.char_to_byte(c)
        }
    }

    pub fn request_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| m.request_focus(Self::id()));
    }

    pub fn show(&mut self, ui: &mut Ui) -> egui::Response {
        let full = ui.available_rect_before_wrap();
        let response = ui.interact(full, Self::id(), Sense::click_and_drag());
        ui.advance_cursor_after_rect(full);
        let has_focus = response.has_focus();

        self.font_id = FontId::monospace(self.font_size);
        self.row_h = ui.fonts_mut(|f| f.row_height(&self.font_id));
        let char_w = ui.fonts_mut(|f| f.glyph_width(&self.font_id, '0'));

        // Geometry.
        let digits = self.doc.line_count().to_string().len().max(3) as f32;
        let gutter_w = if self.line_numbers { digits * char_w + 16.0 } else { 0.0 };
        let show_hbar = !self.wrap;
        let bottom = if show_hbar { SCROLLBAR_W } else { 0.0 };
        let text_rect = Rect::from_min_max(
            Pos2::new(full.left() + gutter_w, full.top()),
            Pos2::new(full.right() - SCROLLBAR_W, full.bottom() - bottom),
        );
        self.view_rows = ((text_rect.height() / self.row_h).floor() as usize).max(1);
        self.wrap_width = (text_rect.width() - 2.0 * TEXT_PAD).max(char_w * 4.0);

        if has_focus {
            self.handle_events(ui);
        }
        self.top_line = self.top_line.min(self.doc.line_count() - 1);
        self.cursor = self.cursor.min(self.doc.len());
        self.anchor = self.anchor.min(self.doc.len());
        self.max_top = self.compute_max_top(ui);

        // Mouse wheel / trackpad.
        if response.hovered() || response.contains_pointer() {
            let d = ui.input(|i| i.smooth_scroll_delta);
            if d.y != 0.0 {
                self.scroll_px(ui, -d.y);
            }
            if d.x != 0.0 && !self.wrap {
                self.h_offset = (self.h_offset - d.x).clamp(0.0, self.max_line_w);
            }
        }

        if std::mem::take(&mut self.scroll_to_cursor) {
            self.ensure_cursor_visible(ui, text_rect.width());
        }
        self.clamp_top();

        let (mut vis, underfilled) = self.visible_lines(ui, text_rect.height());
        if underfilled {
            // Never leave empty space below the end of the document.
            let (_, y) = vis.last().map_or((0, 0.0), |(l, y)| (0, y + l.height(self.row_h)));
            let empty = ((text_rect.height() - y) / self.row_h).floor() as i64;
            self.scroll_frac = 0.0;
            self.scroll_rows(ui, -empty);
            vis = self.visible_lines(ui, text_rect.height()).0;
        }

        let origin = Pos2::new(text_rect.left() + TEXT_PAD - self.h_offset, text_rect.top());

        // Mouse selection.
        let pointer = ui.input(|i| i.pointer.interact_pos());
        if let Some(p) = pointer
            && text_rect.contains(p)
        {
            ui.ctx().set_cursor_icon(CursorIcon::Text);
        }
        if (response.clicked() || response.drag_started() || response.is_pointer_button_down_on())
            && !has_focus
        {
            response.request_focus();
        }
        if let Some(p) = pointer {
            let shift = ui.input(|i| i.modifiers.shift);
            let pressed = ui.input(|i| i.pointer.primary_pressed());
            if pressed && text_rect.contains(p) && response.contains_pointer() {
                let pos = self.hit_test(&vis, origin, p);
                self.cursor = pos;
                if !shift {
                    self.anchor = pos;
                }
                self.desired_x = None;
                self.last_kind = None;
            } else if response.dragged() && self.v_drag_offset.is_none() {
                if p.y < text_rect.top() {
                    self.scroll_rows(ui, -1);
                    ui.ctx().request_repaint();
                } else if p.y > text_rect.bottom() {
                    self.scroll_rows(ui, 1);
                    ui.ctx().request_repaint();
                }
                self.cursor = self.hit_test(&vis, origin, p);
                self.desired_x = None;
            }
            if response.triple_clicked() {
                let line = self.doc.line_of(self.cursor);
                self.anchor = self.doc.line_start(line);
                self.cursor = self.doc.line_start(line + 1);
            } else if response.double_clicked() {
                let r = self.word_at(self.cursor);
                self.anchor = r.start;
                self.cursor = r.end;
            }
        }

        self.paint(ui, full, text_rect, gutter_w, origin, &vis, has_focus);
        self.scrollbars(ui, full, text_rect);
        response
    }

    #[allow(clippy::too_many_arguments)]
    fn paint(
        &mut self,
        ui: &Ui,
        full: Rect,
        text_rect: Rect,
        gutter_w: f32,
        origin: Pos2,
        vis: &[(LineLayout, f32)],
        has_focus: bool,
    ) {
        let visuals = ui.visuals().clone();
        let painter = ui.painter_at(full);
        painter.rect_filled(full, 0.0, visuals.extreme_bg_color);
        let tp = ui.painter_at(text_rect);
        let sel = self.selection();
        let sel_color = visuals.selection.bg_fill;
        let space_w = ui.fonts_mut(|f| f.glyph_width(&self.font_id, ' '));
        let cursor_line = self.doc.line_of(self.cursor);
        let mut max_w: f32 = 0.0;
        let mut cursor_rect = None;

        if gutter_w > 0.0 {
            let g = Rect::from_min_max(full.min, Pos2::new(full.left() + gutter_w, text_rect.bottom()));
            painter.rect_filled(g, 0.0, visuals.faint_bg_color);
        }

        for (lay, y) in vis {
            let o = origin + Vec2::new(0.0, *y);
            max_w = max_w.max(lay.galley.size().x);

            // Selection.
            let t = &lay.t;
            let line_end_incl = t.next.unwrap_or(t.end);
            if !sel.is_empty() && sel.start < line_end_incl.max(t.start + 1) && sel.end > t.start {
                let ca = t.byte_to_char(sel.start);
                let cb = t.byte_to_char(sel.end);
                let covers_newline = sel.end > t.end && t.next.is_some();
                let starts = lay.row_starts();
                for (ri, r) in lay.galley.rows.iter().enumerate() {
                    let (r0, r1) = (starts[ri], starts[ri + 1]);
                    let a = ca.max(r0);
                    let b = cb.min(r1);
                    let last_row = ri + 1 == lay.galley.rows.len();
                    if a > b || (a == b && !(last_row && covers_newline && a >= r0)) {
                        continue;
                    }
                    let mut x0 = r.pos.x + r.row.x_offset((a - r0).into());
                    let mut x1 = r.pos.x + r.row.x_offset((b - r0).into());
                    if last_row && covers_newline {
                        x1 += space_w;
                    }
                    if x1 < x0 {
                        std::mem::swap(&mut x0, &mut x1);
                    }
                    let rr = Rect::from_min_max(
                        o + Vec2::new(x0, r.min_y()),
                        o + Vec2::new(x1, r.max_y()),
                    );
                    tp.rect_filled(rr, 0.0, sel_color);
                }
            }

            tp.galley(o, lay.galley.clone(), visuals.strong_text_color());

            if t.truncated() {
                let x = o.x + lay.galley.size().x + space_w;
                let last_row_y = lay.galley.rows.last().map_or(0.0, |r| r.min_y());
                tp.text(
                    Pos2::new(x, o.y + last_row_y),
                    Align2::LEFT_TOP,
                    format!("… [+{} bytes not shown]", t.end - t.shown_end),
                    self.font_id.clone(),
                    visuals.weak_text_color(),
                );
            }

            // Line number.
            if gutter_w > 0.0 && o.y + self.row_h > text_rect.top() {
                let color = if lay.line == cursor_line {
                    visuals.text_color()
                } else {
                    visuals.weak_text_color()
                };
                painter.with_clip_rect(Rect::from_min_max(full.min, Pos2::new(full.left() + gutter_w, text_rect.bottom())))
                    .text(
                        Pos2::new(full.left() + gutter_w - 8.0, o.y),
                        Align2::RIGHT_TOP,
                        (lay.line + 1).to_string(),
                        self.font_id.clone(),
                        color,
                    );
            }

            // Cursor.
            if lay.line == cursor_line {
                let c = t.byte_to_char(self.cursor);
                let r = lay.galley.pos_from_cursor(ccursor(c)).translate(o.to_vec2());
                let r = Rect::from_min_max(r.min, Pos2::new(r.min.x + 2.0, r.max.y.max(r.min.y + self.row_h)));
                cursor_rect = Some(r);
            }
        }
        self.max_line_w = (max_w - text_rect.width() + 4.0 * TEXT_PAD).max(0.0);

        if let Some(r) = cursor_rect {
            if has_focus {
                tp.rect_filled(r, 0.0, visuals.text_cursor.stroke.color);
                if !self.preedit.is_empty() {
                    let g = ui.fonts_mut(|f| {
                        f.layout_no_wrap(self.preedit.clone(), self.font_id.clone(), visuals.text_color())
                    });
                    let pr = Rect::from_min_size(r.min, g.size());
                    tp.rect_filled(pr, 0.0, visuals.extreme_bg_color);
                    tp.galley(r.min, g, visuals.text_color());
                    tp.line_segment(
                        [pr.left_bottom(), pr.right_bottom()],
                        Stroke::new(1.0, visuals.text_color()),
                    );
                }
                let to_global = ui
                    .ctx()
                    .layer_transform_to_global(ui.layer_id())
                    .unwrap_or_default();
                ui.output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        rect: to_global * text_rect,
                        cursor_rect: to_global * r,
                        should_interrupt_composition: false,
                    });
                });
            } else {
                tp.rect_filled(r, 0.0, visuals.weak_text_color());
            }
        }
    }

    fn scrollbars(&mut self, ui: &Ui, full: Rect, text_rect: Rect) {
        let visuals = ui.visuals().clone();
        let painter = ui.painter_at(full);
        let thumb_color = visuals.widgets.inactive.bg_fill;
        let hover_color = visuals.widgets.hovered.bg_fill;

        // Vertical: proportional to the line number of the top line.
        let track = Rect::from_min_max(
            Pos2::new(full.right() - SCROLLBAR_W, full.top()),
            Pos2::new(full.right(), text_rect.bottom()),
        );
        let lines = self.doc.line_count() as f64;
        let (pos, max) = self.v_scroll(ui);
        if max > 0.0 || pos > 0.0 {
            let max = max.max(pos);
            let thumb_h = (track.height() * ((lines - max) / lines) as f32).clamp(24.0, track.height());
            let range = (track.height() - thumb_h).max(1.0);
            let frac = if max > 0.0 { pos / max } else { 0.0 };
            let y = track.top() + (frac * range as f64) as f32;
            let thumb = Rect::from_min_size(Pos2::new(track.left() + 2.0, y), Vec2::new(SCROLLBAR_W - 4.0, thumb_h));
            let resp = ui.interact(track, Self::id().with("vscroll"), Sense::click_and_drag());
            if (resp.drag_started() || resp.clicked())
                && let Some(p) = resp.interact_pointer_pos()
            {
                self.v_drag_offset = Some(if thumb.contains(p) { p.y - thumb.top() } else { thumb_h / 2.0 });
            }
            if (resp.dragged() || resp.clicked())
                && let (Some(p), Some(off)) = (resp.interact_pointer_pos(), self.v_drag_offset)
            {
                let f = ((p.y - off - track.top()) / range).clamp(0.0, 1.0) as f64;
                let target = f * max;
                self.set_top(target as u64);
                let rows = self.rows_of(ui, self.top_line);
                self.top_row = (((target - self.top_line as f64) * rows as f64) as usize).min(rows - 1);
                self.clamp_top();
            }
            if !resp.dragged() && !resp.drag_started() {
                self.v_drag_offset = None;
            }
            let c = if resp.hovered() || resp.dragged() { hover_color } else { thumb_color };
            painter.rect_filled(thumb, 4.0, c);
        }

        // Horizontal (no-wrap mode only).
        if !self.wrap {
            let track = Rect::from_min_max(
                Pos2::new(text_rect.left(), text_rect.bottom()),
                Pos2::new(text_rect.right(), full.bottom()),
            );
            let content = self.max_line_w + text_rect.width();
            if self.max_line_w > 0.0 || self.h_offset > 0.0 {
                let content = content.max(self.h_offset + text_rect.width());
                let thumb_w = (track.width() * text_rect.width() / content).clamp(24.0, track.width());
                let range = (track.width() - thumb_w).max(1.0);
                let max_off = (content - text_rect.width()).max(1.0);
                let x = track.left() + range * (self.h_offset / max_off).clamp(0.0, 1.0);
                let thumb = Rect::from_min_size(Pos2::new(x, track.top() + 2.0), Vec2::new(thumb_w, SCROLLBAR_W - 4.0));
                let resp = ui.interact(track, Self::id().with("hscroll"), Sense::click_and_drag());
                if (resp.dragged() || resp.clicked())
                    && let Some(p) = resp.interact_pointer_pos()
                {
                    let f = ((p.x - thumb_w / 2.0 - track.left()) / range).clamp(0.0, 1.0);
                    self.h_offset = f * max_off;
                }
                let c = if resp.hovered() || resp.dragged() { hover_color } else { thumb_color };
                painter.rect_filled(thumb, 4.0, c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer::Original;
    use eframe::egui::{MouseWheelUnit, RawInput, TouchPhase};
    use std::sync::atomic::{AtomicBool, AtomicU64};

    /// What winit reports for the Cmd key on macOS (Ctrl elsewhere).
    const CMD: Modifiers = if cfg!(target_os = "macos") {
        Modifiers::MAC_CMD.plus(Modifiers::COMMAND)
    } else {
        Modifiers::CTRL.plus(Modifiers::COMMAND)
    };

    /// Runs real egui frames against the editor without a window.
    struct Harness {
        ctx: egui::Context,
        ed: Editor,
        t: f64,
    }

    impl Harness {
        fn new(doc: Document) -> Self {
            let mut h = Self {
                ctx: egui::Context::default(),
                ed: Editor::new(doc),
                t: 1.0,
            };
            // Expected texts use "\n" on every platform.
            h.ed.crlf = h.ed.doc.original().is_some_and(|o| o.crlf);
            h.ed.request_focus(&h.ctx);
            h.frame(vec![]);
            h.frame(vec![]);
            h
        }

        fn frame(&mut self, events: Vec<Event>) {
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0))),
                events,
                time: Some(self.t),
                focused: true,
                ..Default::default()
            };
            self.t += 0.5;
            let ed = &mut self.ed;
            let _ = self.ctx.run_ui(raw, |ui| {
                egui::CentralPanel::no_frame().show(ui, |ui| {
                    ed.show(ui);
                });
            });
        }

        fn key(&mut self, key: Key, modifiers: Modifiers) {
            self.frame(vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers,
            }]);
        }

        fn text(&mut self, s: &str) {
            self.frame(vec![Event::Text(s.into())]);
        }

        fn content(&self) -> String {
            String::from_utf8(self.ed.doc.read(0, self.ed.doc.len())).unwrap()
        }
    }

    fn big_doc(lines: usize) -> (Document, std::path::PathBuf) {
        let mut content = String::new();
        for i in 0..lines {
            content.push_str(&format!("line {i} lorem ipsum\n"));
        }
        let p = std::env::temp_dir().join(format!("notepad-ed-{}-{lines}", std::process::id()));
        std::fs::write(&p, content).unwrap();
        let o = Original::open(&p, &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        (Document::from_original(o), p)
    }

    /// `NOTEPAD_BIG=/path/to/big.txt cargo test --release perf_big -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn perf_big() {
        use std::time::Instant;
        let rss = |label: &str| {
            let pid = std::process::id().to_string();
            let kb: u64 = if cfg!(windows) {
                // Working set: the last CSV field, e.g. "123,456 K" (separator depends on locale).
                let out = std::process::Command::new("tasklist")
                    .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
                    .output()
                    .unwrap();
                let text = String::from_utf8_lossy(&out.stdout);
                let mem = text.trim().rsplit("\",\"").next().unwrap_or("");
                mem.chars().filter(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)
            } else {
                let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &pid]).output().unwrap();
                String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
            };
            println!("  [rss {label}: {} MB]", kb / 1024);
        };
        let path = std::env::var("NOTEPAD_BIG").expect("NOTEPAD_BIG");
        let t = Instant::now();
        let o = Original::open(path.as_ref(), &AtomicU64::new(0), &AtomicBool::new(false)).unwrap();
        println!("index: {:?}", t.elapsed());
        let mut h = Harness::new(Document::from_original(o));
        println!("lines: {}", h.ed.doc.line_count());
        rss("lines:");
        let t = Instant::now();
        h.ed.goto_line(h.ed.doc.line_count() / 2);
        h.frame(vec![]);
        println!("goto middle + frame: {:?}", t.elapsed());
        let t = Instant::now();
        for _ in 0..20 {
            h.key(Key::PageDown, Modifiers::NONE);
        }
        println!("20x PageDown frames: {:?}", t.elapsed());
        let t = Instant::now();
        for c in "hello".chars() {
            h.text(&c.to_string());
        }
        println!("5 typed chars: {:?}", t.elapsed());
        let t = Instant::now();
        h.key(Key::ArrowDown, CMD);
        h.frame(vec![]);
        println!("go to end: {:?}", t.elapsed());
        let t = Instant::now();
        let found = h.ed.find("0000000001 The", true, false);
        println!("find (wraps around): {found} {:?}", t.elapsed());
        let t = Instant::now();
        let n = h.ed.replace_all("orderservice", "Svc", false);
        h.frame(vec![]);
        println!("replace all ({n} matches): {:?}", t.elapsed());
        rss("replace");
        let t = Instant::now();
        for _ in 0..30 {
            h.key(Key::ArrowLeft, Modifiers::NONE);
            h.text("x");
        }
        println!("30 separate edits after replace all: {:?}", t.elapsed());
        rss("30");
        let t = Instant::now();
        while h.ed.can_undo() {
            h.ed.undo();
        }
        h.frame(vec![]);
        println!("undo everything: {:?}", t.elapsed());
        rss("undo");
        let out = std::env::temp_dir().join("notepad-perf-out.txt");
        let t = Instant::now();
        h.ed.doc.save(&out).unwrap();
        println!("save {} bytes: {:?}", h.ed.doc.len(), t.elapsed());
        std::fs::remove_file(out).unwrap();
    }

    #[test]
    fn find_and_replace() {
        let mut h = Harness::new(Document::new());
        h.frame(vec![Event::Paste("Привет мир. ПРИВЕТ! привет?".into())]);
        h.key(Key::ArrowUp, CMD);
        // Case-insensitive search works for Cyrillic too.
        assert!(h.ed.find("привет", false, false));
        assert_eq!(h.ed.selection(), 0.."Привет".len() as u64);
        assert!(!h.ed.find("ПРИВЕТ МИР!", true, false));
        // Replace: the selected match is replaced, the next one gets selected.
        assert!(h.ed.replace("привет", "Hi", false, 10.0));
        assert_eq!(h.content(), "Hi мир. ПРИВЕТ! привет?");
        assert_eq!(h.ed.doc.read(h.ed.selection().start, h.ed.selection().end), "ПРИВЕТ".as_bytes());
        assert_eq!(h.ed.replace_all("привет", "Hello", false), 2);
        assert_eq!(h.content(), "Hi мир. Hello! Hello?");
        h.ed.undo();
        assert_eq!(h.content(), "Hi мир. ПРИВЕТ! привет?");
        h.ed.undo();
        assert_eq!(h.content(), "Привет мир. ПРИВЕТ! привет?");
        assert_eq!(h.ed.replace_all("нет такого", "x", false), 0);
    }

    #[test]
    fn typing_editing_undo() {
        let mut h = Harness::new(Document::new());
        h.text("hello");
        h.key(Key::Enter, Modifiers::NONE);
        h.text("wörld");
        assert_eq!(h.content(), "hello\nwörld");
        h.key(Key::Backspace, Modifiers::NONE);
        assert_eq!(h.content(), "hello\nwörl");
        h.key(Key::ArrowLeft, Modifiers::NONE);
        h.key(Key::ArrowLeft, Modifiers::NONE);
        h.key(Key::ArrowLeft, Modifiers::NONE);
        h.text("X");
        assert_eq!(h.content(), "hello\nwXörl");
        h.key(Key::ArrowUp, Modifiers::NONE);
        h.text("Y");
        assert_eq!(h.content(), "heYllo\nwXörl");
        h.key(Key::Z, CMD);
        assert_eq!(h.content(), "hello\nwXörl");
        h.key(Key::Z, CMD.plus(Modifiers::SHIFT));
        assert_eq!(h.content(), "heYllo\nwXörl");
        h.key(Key::A, CMD);
        h.frame(vec![Event::Paste("plain\r\ntext".into())]);
        assert_eq!(h.content(), "plain\ntext");
        assert!(h.ed.is_modified());
        while h.ed.can_undo() {
            h.ed.undo();
        }
        assert_eq!(h.content(), "");
        assert!(!h.ed.is_modified());
    }

    #[test]
    fn navigation_in_big_file() {
        let (doc, path) = big_doc(100_000);
        let mut h = Harness::new(doc);
        assert_eq!(h.ed.top_line, 0);
        h.key(Key::ArrowDown, CMD);
        assert_eq!(h.ed.cursor, h.ed.doc.len());
        h.frame(vec![]);
        // The last line is visible at the bottom.
        let last = h.ed.doc.line_count() - 1;
        assert!(h.ed.top_line > last - h.ed.view_rows as u64 && h.ed.top_line <= last);
        h.key(Key::PageUp, Modifiers::NONE);
        assert!(h.ed.doc.line_of(h.ed.cursor) < last);

        h.ed.goto_line(50_001);
        h.frame(vec![]);
        assert_eq!(h.ed.doc.line_of(h.ed.cursor), 50_000);
        assert!(h.ed.top_line <= 50_000 && h.ed.top_line + h.ed.view_rows as u64 > 50_000);
        h.text("Z");
        assert_eq!(h.ed.doc.read(h.ed.cursor - 1, h.ed.cursor + 12), b"Zline 50000 l");

        // Mouse wheel scrolls the view.
        let top = h.ed.top_line;
        h.frame(vec![Event::PointerMoved(Pos2::new(400.0, 300.0))]);
        for _ in 0..10 {
            h.frame(vec![Event::MouseWheel {
                unit: MouseWheelUnit::Point,
                delta: Vec2::new(0.0, -200.0),
                phase: TouchPhase::Move,
                modifiers: Modifiers::NONE,
            }]);
        }
        assert!(h.ed.top_line > top, "wheel did not scroll");

        // Find wraps around.
        assert!(h.ed.find("line 99999 ", true, false));
        assert!(h.ed.find("LINE 7 ", false, false));
        assert_eq!(h.ed.doc.line_of(h.ed.cursor), 7);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wheel_stops_at_end_of_short_doc() {
        let (doc, path) = big_doc(60);
        let mut h = Harness::new(doc);
        h.frame(vec![Event::PointerMoved(Pos2::new(400.0, 300.0))]);
        let wheel = |dy| Event::MouseWheel {
            unit: MouseWheelUnit::Point,
            delta: Vec2::new(0.0, dy),
            phase: TouchPhase::Move,
            modifiers: Modifiers::NONE,
        };
        for _ in 0..20 {
            h.frame(vec![wheel(-200.0)]);
        }
        let end = (h.ed.top_line, h.ed.top_row);
        assert_eq!(end, h.ed.max_top);
        assert!(end.0 > 0);
        // Small trackpad deltas at the end don't move (or jiggle) the view.
        for _ in 0..20 {
            h.frame(vec![wheel(-3.0)]);
            assert_eq!((h.ed.top_line, h.ed.top_row, h.ed.scroll_frac), (end.0, end.1, 0.0));
        }
        // The scroll bar thumb is at the very bottom.
        let mut at_end = false;
        let ed = &h.ed;
        let _ = h.ctx.run_ui(RawInput::default(), |ui| {
            let (pos, max) = ed.v_scroll(ui);
            at_end = pos == max;
        });
        assert!(at_end);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wrapped_long_line_and_click() {
        let mut h = Harness::new(Document::new());
        let long = "word ".repeat(400);
        h.frame(vec![Event::Paste(format!("first\n{long}\nlast"))]);
        h.key(Key::ArrowUp, CMD);
        assert_eq!(h.ed.cursor, 0);
        // Down moves through the wrapped rows of line 2, not straight to line 3.
        h.key(Key::ArrowDown, Modifiers::NONE);
        h.key(Key::ArrowDown, Modifiers::NONE);
        assert_eq!(h.ed.doc.line_of(h.ed.cursor), 1);
        assert!(h.ed.cursor > 6 + 20);
        // Clicking on the first row puts the cursor in line 1.
        h.frame(vec![
            Event::PointerMoved(Pos2::new(200.0, 5.0)),
            Event::PointerButton {
                pos: Pos2::new(200.0, 5.0),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Modifiers::NONE,
            },
        ]);
        assert_eq!(h.ed.doc.line_of(h.ed.cursor), 0);
        assert_eq!(h.ed.cursor, 5);
    }

    #[test]
    fn decode_maps_bytes() {
        let (s, m) = decode("aж\u{1F600}".as_bytes());
        assert_eq!(s, "aж\u{1F600}");
        assert_eq!(m, vec![0, 1, 3, 7]);
        let (s, m) = decode(b"a\xffb\x01");
        assert_eq!(s, "a\u{FFFD}b\u{2401}");
        assert_eq!(m, vec![0, 1, 2, 3, 4]);
    }
}
