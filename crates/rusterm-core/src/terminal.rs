use std::collections::{HashSet, VecDeque};

use bitflags::bitflags;
use unicode_width::UnicodeWidthChar;
use vte::ansi::{self, Attr, CharsetIndex, ClearMode, Color, Handler, LineClearMode, PrivateMode, StandardCharset};

// ── Cell & Row ──────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct Cell {
    pub character: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    pub wide: bool,     // true for the first cell of a wide char
    pub wide_next: bool, // true for the continuation cell of a wide char
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            character: ' ',
            fg: CellColor::Default,
            bg: CellColor::Default,
            flags: CellFlags::empty(),
            wide: false,
            wide_next: false,
        }
    }
}

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
    pub struct CellFlags: u8 {
        const BOLD          = 0x01;
        const DIM           = 0x02;
        const ITALIC        = 0x04;
        const UNDERLINE     = 0x08;
        const DOUBLE_UNDERLINE = 0x10;
        const STRIKETHROUGH = 0x20;
        const INVERSE       = 0x40;
        const HIDDEN        = 0x80;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellColor {
    Default,
    Named(ansi::NamedColor),
    Indexed(u8),
    Spec(ansi::Rgb),
}

impl Default for CellColor {
    fn default() -> Self {
        Self::Default
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub cells: Vec<Cell>,
    pub wrapped: bool,
}

impl Row {
    fn new(cols: usize) -> Self {
        Self {
            cells: vec![Cell::default(); cols],
            wrapped: false,
        }
    }
}

// ── Render Output ───────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderCell {
    pub character: char,
    pub fg: CellColor,
    pub bg: CellColor,
    pub flags: CellFlags,
    pub wide: bool,
    pub wide_next: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderRow {
    pub cells: Vec<RenderCell>,
    pub wrapped: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderOutput {
    pub rows: Vec<RenderRow>,
    pub cursor_row: usize,
    pub cursor_col: usize,
    pub cursor_visible: bool,
    pub title: Option<String>,
    pub tmux_session: Option<String>,
    pub scrollback_offset: usize,
    pub scrollback_total: usize,
    pub mode_cursor_keys: bool,
    pub mode_bracketed_paste: bool,
    pub line_number_start: usize,
    /// Max scrollback lines the terminal retains. Stable across output (unlike
    /// `scrollback_total`), so the UI can size the line-number gutter once
    /// instead of widening it (and shifting all content) as scrollback fills.
    pub scrollback_capacity: usize,
}

// ── Cursor State (for save/restore) ─────────────────────────────────

#[derive(Clone)]
struct CursorState {
    row: usize,
    col: usize,
    fg: CellColor,
    bg: CellColor,
    flags: CellFlags,
    charsets: [StandardCharset; 4],
    active_charset: CharsetIndex,
}

// ── Screen Switch State (for ESC[?1049h/l) ─────────────────────────

#[derive(Clone)]
struct ScreenSwitchState {
    cursor: CursorState,
    mode_show_cursor: bool,
    mode_cursor_keys: bool,
    mode_bracketed_paste: bool,
    mode_line_wrap: bool,
    mode_origin: bool,
    scroll_top: usize,
    scroll_bottom: usize,
    saved_cursor: Option<CursorState>,
}

// ── Terminal ────────────────────────────────────────────────────────

pub struct Terminal {
    grid: Vec<Row>,
    size: TerminalSize,

    cursor_row: usize,
    cursor_col: usize,

    attrs_fg: CellColor,
    attrs_bg: CellColor,
    attrs_flags: CellFlags,

    saved_cursor: Option<CursorState>,
    screen_switch_cursor: Option<ScreenSwitchState>, // Saved by ESC[?1049h, restored by ESC[?1049l

    scroll_top: usize,
    scroll_bottom: usize,

    alt_grid: Option<Vec<Row>>,
    using_alt_screen: bool,

    scrollback: VecDeque<Row>,
    scrollback_capacity: usize,

    tab_stops: Vec<bool>,

    mode_line_wrap: bool,
    mode_show_cursor: bool,
    mode_bracketed_paste: bool,
    mode_cursor_keys: bool,
    mode_origin: bool,
    mode_insert: bool,

    charsets: [StandardCharset; 4],
    active_charset: CharsetIndex,

    dirty: bool,
    dirty_rows: HashSet<usize>,

    title: Option<String>,

    // For sending responses back (DA, DSR, etc.)
    input_tx: Option<tokio::sync::mpsc::UnboundedSender<Vec<u8>>>,

    // tmux state
    tmux_detected: bool,
    tmux_session: Option<String>,

    // Shell integration (OSC 133): exit code of the last command, reported by
    // the shell via ESC ] 133 ; D ; <code> ST. None until the shell reports one.
    last_exit_code: Option<i32>,
    // Set true when a new OSC 133;D arrives; cleared by take_exit_code().
    exit_code_dirty: bool,
}

impl Terminal {
    pub fn new(size: TerminalSize) -> Self {
        let rows = size.rows as usize;
        let cols = size.cols as usize;
        let mut tab_stops = vec![false; cols];
        for i in (0..cols).step_by(8) {
            tab_stops[i] = true;
        }

        Self {
            grid: (0..rows).map(|_| Row::new(cols)).collect(),
            size,
            cursor_row: 0,
            cursor_col: 0,
            attrs_fg: CellColor::Default,
            attrs_bg: CellColor::Default,
            attrs_flags: CellFlags::empty(),
            saved_cursor: None,
            screen_switch_cursor: None,
            scroll_top: 0,
            scroll_bottom: rows.saturating_sub(1),
            alt_grid: None,
            using_alt_screen: false,
            scrollback: VecDeque::new(),
            scrollback_capacity: 10_000,
            tab_stops,
            mode_line_wrap: true,
            mode_show_cursor: true,
            mode_bracketed_paste: false,
            mode_cursor_keys: false,
            mode_origin: false,
            mode_insert: false,
            charsets: [StandardCharset::Ascii; 4],
            active_charset: CharsetIndex::G0,
            dirty: false,
            dirty_rows: HashSet::new(),
            title: None,
            input_tx: None,
            tmux_detected: false,
            tmux_session: None,
            last_exit_code: None,
            exit_code_dirty: false,
        }
    }

    pub fn set_input_sender(&mut self, tx: tokio::sync::mpsc::UnboundedSender<Vec<u8>>) {
        self.input_tx = Some(tx);
    }

    pub fn size(&self) -> TerminalSize {
        self.size
    }

    /// Returns the exit code of the last command, reported by the shell via
    /// OSC 133;D (shell integration). None until the shell reports one.
    pub fn last_exit_code(&self) -> Option<i32> {
        self.last_exit_code
    }

    /// If a new exit code arrived since the last call (via OSC 133;D), return
    /// it and clear the dirty flag. Used to decide whether to keep or drop the
    /// most recently recorded command (drop on non-zero exit).
    pub fn take_exit_code(&mut self) -> Option<i32> {
        if self.exit_code_dirty {
            self.exit_code_dirty = false;
            self.last_exit_code
        } else {
            None
        }
    }

    pub fn process(&mut self, data: &[u8], parser: &mut ansi::Processor) {
        // Extract OSC 133;D;<exit_code> (shell integration) before feeding to
        // VTE — vte's ansi::Handler doesn't expose OSC dispatch, so unknown OSCs
        // (like 133) are silently dropped by the Processor's Perform impl.
        self.scan_exit_codes(data);
        parser.advance(self, data);
    }

    /// Scan raw output for FinalTerm shell-integration markers
    /// `ESC ] 1 3 3 ; D ; <exit_code> (BEL|ST)` and record the last exit code.
    ///
    /// Single pass over the buffer keyed off the rare ESC (0x1b) byte. This
    /// replaces the previous `windows().position()` search, which performed a
    /// naive O(n·m) substring comparison on every `process()` call. Because ESC
    /// almost never appears in ordinary terminal output, the inner literal
    /// comparison only fires on actual escape sequences, making this effectively
    /// O(n) with a tiny constant. Behaviour (including not matching markers that
    /// straddle a process() boundary) is unchanged.
    fn scan_exit_codes(&mut self, data: &[u8]) {
        // `ESC ] 1 3 3 ; D`
        const SUFFIX: &[u8] = b"]133;D";
        let data_len = data.len();
        let mut i = 0;
        while i < data_len {
            if data[i] == 0x1b {
                // Need at least ESC + suffix bytes after this position.
                if i + 1 + SUFFIX.len() <= data_len && data[i + 1..i + 1 + SUFFIX.len()] == *SUFFIX {
                    let mut j = i + 1 + SUFFIX.len();
                    // Optional ';' separator before the numeric exit code.
                    if j < data_len && data[j] == b';' {
                        j += 1;
                    }
                    let digit_start = j;
                    while j < data_len && data[j].is_ascii_digit() {
                        j += 1;
                    }
                    let rc = if j > digit_start {
                        std::str::from_utf8(&data[digit_start..j])
                            .ok()
                            .and_then(|s| s.parse::<i32>().ok())
                    } else {
                        // 133;D with no code → treat as success (0).
                        Some(0)
                    };
                    if let Some(rc) = rc {
                        self.last_exit_code = Some(rc);
                        self.exit_code_dirty = true;
                    }
                    // Skip past the consumed marker; continue scanning in case
                    // multiple exit codes arrive in one batch (last one wins).
                    i = j;
                    continue;
                }
            }
            i += 1;
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let old_cols = self.size.cols as usize;
        let old_rows = self.size.rows as usize;
        let new_cols = cols as usize;
        let new_rows = rows as usize;

        if new_cols == old_cols && new_rows == old_rows {
            return;
        }

        // Reflow: collect logical lines, re-wrap into new width
        let logical_lines = self.reflow_grid(old_cols, old_rows);

        self.grid = Vec::with_capacity(new_rows);
        let mut line_iter = logical_lines.into_iter().peekable();
        let mut row_idx = 0;

        while row_idx < new_rows {
            if let Some(line) = line_iter.next() {
                // Wrap this logical line into rows of new_cols width
                let chars: Vec<Cell> = line;
                for chunk in chars.chunks(new_cols) {
                    if row_idx >= new_rows {
                        // Push excess into scrollback
                        self.scrollback.push_back(Row { cells: chunk.to_vec(), wrapped: true });
                        row_idx += 1;
                        continue;
                    }
                    let mut row = Row::new(new_cols);
                    for (i, cell) in chunk.iter().enumerate() {
                        if i < new_cols {
                            row.cells[i] = cell.clone();
                        }
                    }
                    row.wrapped = chunk.len() > new_cols || (chunk.len() == new_cols && line_iter.peek().is_some());
                    self.grid.push(row);
                    row_idx += 1;
                }
            } else {
                self.grid.push(Row::new(new_cols));
                row_idx += 1;
            }
        }

        // Pad if needed
        while self.grid.len() < new_rows {
            self.grid.push(Row::new(new_cols));
        }

        self.size = TerminalSize { cols, rows, pixel_width: 0, pixel_height: 0 };
        self.scroll_top = 0;
        self.scroll_bottom = new_rows.saturating_sub(1);

        // Adjust cursor
        if self.cursor_row >= new_rows {
            self.cursor_row = new_rows.saturating_sub(1);
        }
        if self.cursor_col >= new_cols {
            self.cursor_col = new_cols.saturating_sub(1);
        }

        // Rebuild tab stops
        self.tab_stops = vec![false; new_cols];
        for i in (0..new_cols).step_by(8) {
            self.tab_stops[i] = true;
        }

        self.mark_all_dirty();
    }

    fn reflow_grid(&self, _old_cols: usize, _old_rows: usize) -> Vec<Vec<Cell>> {
        // Simple reflow: treat each row as a logical line for now
        // TODO: handle wrapped lines properly
        self.grid.iter().map(|row| row.cells.clone()).collect()
    }

    pub fn render(&self) -> RenderOutput {
        self.render_with_scroll(0)
    }

    pub fn render_with_scroll(&self, scroll_offset: usize) -> RenderOutput {
        let visible_rows = self.size.rows as usize;
        let scrollback_len = self.scrollback.len();

        let render_row = |row: &Row| RenderRow {
            cells: row.cells.iter().map(|c| RenderCell {
                character: c.character,
                fg: c.fg,
                bg: c.bg,
                flags: c.flags,
                wide: c.wide,
                wide_next: c.wide_next,
            }).collect(),
            wrapped: row.wrapped,
        };

        // Build the output in a single pre-sized Vec. The previous code allocated
        // THREE Vec<RenderRow> containers in the scrolled branch (scrollback rows,
        // grid rows, then a third to hold the chain). Pre-allocating to the exact
        // visible-row count and extending in place cuts that to one allocation.
        let rows = if scroll_offset > 0 && scrollback_len > 0 {
            let offset = scroll_offset.min(scrollback_len);
            let scrollback_avail = offset.min(visible_rows);
            let from_scrollback_start = scrollback_len.saturating_sub(offset);

            let mut out: Vec<RenderRow> = Vec::with_capacity(visible_rows);
            out.extend(self.scrollback.range(from_scrollback_start..from_scrollback_start + scrollback_avail).map(render_row));
            let remaining = visible_rows - out.len();
            out.extend(self.grid.iter().take(remaining).map(render_row));
            out
        } else {
            self.grid.iter().map(render_row).collect()
        };

        // Cursor is only visible when not scrolled back
        let (cursor_row, cursor_visible) = if scroll_offset == 0 {
            (self.cursor_row, self.mode_show_cursor)
        } else {
            (0, false)
        };

        let line_number_start = if scroll_offset > 0 && scrollback_len > 0 {
            let offset = scroll_offset.min(scrollback_len);
            scrollback_len - offset + 1
        } else {
            scrollback_len + 1
        };

        RenderOutput {
            rows,
            cursor_row,
            cursor_col: self.cursor_col,
            cursor_visible,
            title: self.title.clone(),
            tmux_session: self.tmux_session.clone(),
            scrollback_offset: scroll_offset,
            scrollback_total: scrollback_len,
            mode_cursor_keys: self.mode_cursor_keys,
            mode_bracketed_paste: self.mode_bracketed_paste,
            line_number_start,
            scrollback_capacity: self.scrollback_capacity,
        }
    }

    pub fn scrollback_len(&self) -> usize {
        self.scrollback.len()
    }

    /// Maximum number of lines retained in scrollback. Raising it does not
    /// retroactively trim existing scrollback; lowering it trims immediately.
    pub fn set_scrollback_capacity(&mut self, capacity: usize) {
        self.scrollback_capacity = capacity;
        let len = self.scrollback.len();
        if len > capacity {
            // Drain the surplus from the front (oldest lines), keeping the most
            // recent `capacity` lines. VecDeque::drain on a front range is O(k)
            // where k is the number of drained elements — independent of the
            // total length, so this stays cheap even for very large scrollbacks.
            let drop = len - capacity;
            self.scrollback.drain(0..drop);
        }
    }

    pub fn scrollback_capacity(&self) -> usize {
        self.scrollback_capacity
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
        self.dirty_rows.clear();
    }

    pub fn extract_current_line(&self) -> String {
        let row = &self.grid[self.cursor_row];
        row.cells[..self.cursor_col]
            .iter()
            .filter(|c| !c.wide_next)
            .map(|c| c.character)
            .collect()
    }

    pub fn is_tmux_detected(&self) -> bool {
        self.tmux_detected
    }

    pub fn tmux_session(&self) -> Option<&str> {
        self.tmux_session.as_deref()
    }

    /// Returns true when the alternate screen is active (e.g. vim, less, man,
    /// tmux copy-mode panes showing a full-screen app). The suggestion popup
    /// should be suppressed in this state because the cursor line is not a
    /// shell prompt.
    pub fn is_alt_screen(&self) -> bool {
        self.using_alt_screen
    }

    fn detect_tmux_from_title(&mut self, title: &str) {
        // tmux typically sets titles like "[session] 0:window*"
        // or "session:window" patterns
        if title.contains('[') && title.contains(']') {
            if let Some(start) = title.find('[') {
                if let Some(end) = title.find(']') {
                    if start < end {
                        self.tmux_detected = true;
                        self.tmux_session = Some(title[start + 1..end].to_string());
                        return;
                    }
                }
            }
        }
        // Also detect from common tmux title patterns
        if title.contains("tmux") {
            self.tmux_detected = true;
        }
    }

    // ── Internal helpers ────────────────────────────────────────────

    fn rows(&self) -> usize {
        self.size.rows as usize
    }

    fn cols(&self) -> usize {
        self.size.cols as usize
    }

    fn mark_dirty(&mut self, row: usize) {
        self.dirty = true;
        self.dirty_rows.insert(row);
    }

    /// Create a cell for erased/cleared areas using the given background color (ECMA-48).
    /// Erased cells preserve bg but reset fg/flags to defaults.
    /// `CellColor` is `Copy`, so taking it by value avoids a clone on every call
    /// — this runs on every scrolled/erased cell, so it is hot.
    fn make_erased_cell(bg: CellColor) -> Cell {
        Cell {
            character: ' ',
            fg: CellColor::Default,
            bg,
            flags: CellFlags::empty(),
            wide: false,
            wide_next: false,
        }
    }

    /// Create a new row filled with erased cells using the given background color.
    fn make_row_with_bg(cols: usize, bg: CellColor) -> Row {
        let cell = Self::make_erased_cell(bg);
        Row {
            cells: vec![cell; cols],
            wrapped: false,
        }
    }

    fn mark_all_dirty(&mut self) {
        self.dirty = true;
        for i in 0..self.grid.len() {
            self.dirty_rows.insert(i);
        }
    }

    fn scroll_up(&mut self, count: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let cols = self.cols();
        let bg = self.attrs_bg;

        for _ in 0..count {
            // Blank the top row. On the primary screen the old top row is pushed
            // to scrollback first; on the alternate screen it is discarded.
            let old_top = std::mem::replace(&mut self.grid[top], Self::make_row_with_bg(cols, bg));
            if !self.using_alt_screen {
                self.scrollback.push_back(old_top);
                // VecDeque::pop_front is O(1) — the whole reason the scrollback
                // is a deque instead of a Vec. (Vec::remove(0) was O(n) and made
                // the terminal progressively slower as scrollback filled.)
                if self.scrollback.len() > self.scrollback_capacity {
                    self.scrollback.pop_front();
                }
            }

            // Bubble the freshly-blanked top row down to the bottom via swaps.
            // After the loop grid[bottom] holds that blank row. The old code did
            // `(bottom - top)` per-cell mem::replace + a trailing fresh blank
            // allocation per iteration — each allocated a full Vec<Cell> of width
            // `cols` only to immediately drop it. Swapping whole rows avoids all
            // of those intermediate allocations.
            for i in top..bottom {
                self.grid.swap(i, i + 1);
            }
        }

        for i in top..=bottom {
            self.mark_dirty(i);
        }
    }

    fn scroll_down(&mut self, count: usize) {
        let top = self.scroll_top;
        let bottom = self.scroll_bottom;
        let cols = self.cols();
        let bg = self.attrs_bg;

        for _ in 0..count {
            // Blank the bottom row (its content is discarded — scroll_down does
            // not feed scrollback, matching the original behaviour), then bubble
            // that blank up to the top via reverse swaps. After the loop each
            // grid[i] holds old grid[i-1] and grid[top] is blank. The old code
            // allocated a full-width Vec<Cell> on every mem::replace iteration;
            // swapping whole rows is allocation-free apart from the single blank.
            self.grid[bottom] = Self::make_row_with_bg(cols, bg);
            for i in (top + 1..=bottom).rev() {
                self.grid.swap(i, i - 1);
            }
        }

        for i in top..=bottom {
            self.mark_dirty(i);
        }
    }

    fn write_char(&mut self, c: char) {
        let cols = self.cols();
        if self.cursor_col >= cols {
            if self.mode_line_wrap {
                self.cursor_col = 0;
                self.linefeed();
            } else {
                self.cursor_col = cols - 1;
            }
        }

        let width = UnicodeWidthChar::width(c).unwrap_or(1);

        // Handle wide chars (CJK)
        if width == 2 && self.cursor_col + 1 >= cols {
            let row = &mut self.grid[self.cursor_row];
            row.cells[self.cursor_col] = Self::make_erased_cell(self.attrs_bg);
            if self.mode_line_wrap {
                self.cursor_col = 0;
                self.linefeed();
            }
        }

        // Insert mode: shift cells right before writing
        if self.mode_insert && self.cursor_col < cols {
            let row = &mut self.grid[self.cursor_row];
            let shift = width;
            for i in (self.cursor_col + shift..cols).rev() {
                if i >= shift {
                    row.cells[i] = std::mem::take(&mut row.cells[i - shift]);
                }
            }
            // Clear the cells we're about to write to
            for i in 0..shift {
                if self.cursor_col + i < cols {
                    row.cells[self.cursor_col + i] = Cell::default();
                }
            }
        }

        let row = &mut self.grid[self.cursor_row];
        if self.cursor_col < cols {
            row.cells[self.cursor_col] = Cell {
                character: c,
                fg: self.attrs_fg,
                bg: self.attrs_bg,
                flags: self.attrs_flags,
                wide: width > 1,
                wide_next: false,
            };

            if width == 2 && self.cursor_col + 1 < cols {
                row.cells[self.cursor_col + 1] = Cell {
                    character: ' ',
                    fg: self.attrs_fg,
                    bg: self.attrs_bg,
                    flags: self.attrs_flags,
                    wide: false,
                    wide_next: true,
                };
            }

            self.mark_dirty(self.cursor_row);
            self.cursor_col += width;
            if self.cursor_col >= cols {
                // Stay at cols (will wrap on next input if line_wrap is on)
            }
        }
    }

    fn send_response(&self, data: &[u8]) {
        if let Some(tx) = &self.input_tx {
            let _ = tx.send(data.to_vec());
        }
    }
}

// ── vte::ansi::Handler implementation ───────────────────────────────

impl Handler for Terminal {
    fn input(&mut self, c: char) {
        let mapped = self.charsets[self.active_charset as usize].map(c);
        self.write_char(mapped);
    }

    fn goto(&mut self, line: i32, col: usize) {
        let row = if line < 0 { 0 } else { line as usize };
        let row = row.min(self.rows().saturating_sub(1));
        let col = col.min(self.cols().saturating_sub(1));
        // If cursor lands on the continuation cell of a wide char, move to the start
        let col = if row < self.grid.len() && col < self.grid[row].cells.len()
            && self.grid[row].cells[col].wide_next
        {
            col.saturating_sub(1)
        } else {
            col
        };
        self.cursor_row = row;
        self.cursor_col = col;
        self.dirty = true;
    }

    fn goto_line(&mut self, line: i32) {
        let row = if line < 0 { 0 } else { line as usize };
        self.cursor_row = row.min(self.rows().saturating_sub(1));
        self.dirty = true;
    }

    fn goto_col(&mut self, col: usize) {
        self.cursor_col = col.min(self.cols().saturating_sub(1));
        self.dirty = true;
    }

    fn move_up(&mut self, rows: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(rows);
        self.dirty = true;
    }

    fn move_down(&mut self, rows: usize) {
        self.cursor_row = (self.cursor_row + rows).min(self.rows().saturating_sub(1));
        self.dirty = true;
    }

    fn move_forward(&mut self, cols: usize) {
        self.cursor_col = (self.cursor_col + cols).min(self.cols().saturating_sub(1));
        self.dirty = true;
    }

    fn move_backward(&mut self, cols: usize) {
        self.cursor_col = self.cursor_col.saturating_sub(cols);
        self.dirty = true;
    }

    fn move_down_and_cr(&mut self, rows: usize) {
        self.cursor_row = (self.cursor_row + rows).min(self.rows().saturating_sub(1));
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn move_up_and_cr(&mut self, rows: usize) {
        self.cursor_row = self.cursor_row.saturating_sub(rows);
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn put_tab(&mut self, count: u16) {
        for _ in 0..count {
            let next_tab = (self.cursor_col + 1..self.cols())
                .find(|&c| self.tab_stops.get(c).copied().unwrap_or(false));
            self.cursor_col = next_tab.unwrap_or(self.cols().saturating_sub(1));
        }
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        }
        self.dirty = true;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn linefeed(&mut self) {
        if self.cursor_row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.cursor_row < self.rows().saturating_sub(1) {
            self.cursor_row += 1;
        }
        self.dirty = true;
    }

    fn bell(&mut self) {}

    fn substitute(&mut self) {}

    fn newline(&mut self) {
        self.carriage_return();
        self.linefeed();
    }

    fn set_horizontal_tabstop(&mut self) {
        if self.cursor_col < self.tab_stops.len() {
            self.tab_stops[self.cursor_col] = true;
        }
    }

    fn scroll_up(&mut self, count: usize) {
        self.scroll_up(count);
    }

    fn scroll_down(&mut self, count: usize) {
        self.scroll_down(count);
    }

    fn insert_blank(&mut self, count: usize) {
        let cols = self.cols();
        let erased = Self::make_erased_cell(self.attrs_bg);
        let row = &mut self.grid[self.cursor_row];
        let col = self.cursor_col;
        // Shift cells right from the end
        for i in (col..cols).rev() {
            if i >= count {
                row.cells[i] = std::mem::take(&mut row.cells[i - count]);
            } else {
                row.cells[i] = erased.clone();
            }
        }
        // Fill the inserted blank cells with current bg
        for i in col..col.saturating_add(count).min(cols) {
            row.cells[i] = erased.clone();
        }
        self.mark_dirty(self.cursor_row);
    }

    fn insert_blank_lines(&mut self, count: usize) {
        if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
            let bg = self.attrs_bg;
            let cols = self.cols();
            for _ in 0..count {
                if self.scroll_bottom > 0 {
                    self.grid.remove(self.scroll_bottom);
                    self.grid.insert(self.cursor_row, Self::make_row_with_bg(cols, bg));
                }
            }
            for i in self.cursor_row..=self.scroll_bottom {
                self.mark_dirty(i);
            }
        }
    }

    fn delete_lines(&mut self, count: usize) {
        if self.cursor_row >= self.scroll_top && self.cursor_row <= self.scroll_bottom {
            let bg = self.attrs_bg;
            let cols = self.cols();
            for _ in 0..count {
                if self.cursor_row < self.grid.len() && self.scroll_bottom < self.grid.len() {
                    self.grid.remove(self.cursor_row);
                    self.grid.insert(self.scroll_bottom, Self::make_row_with_bg(cols, bg));
                }
            }
            for i in self.cursor_row..=self.scroll_bottom {
                self.mark_dirty(i);
            }
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let erased = Self::make_erased_cell(self.attrs_bg);
        let row = &mut self.grid[self.cursor_row];
        for i in 0..count {
            let col = self.cursor_col + i;
            if col < row.cells.len() {
                row.cells[col] = erased.clone();
            }
        }
        self.mark_dirty(self.cursor_row);
    }

    fn delete_chars(&mut self, count: usize) {
        let cols = self.cols();
        let erased = Self::make_erased_cell(self.attrs_bg);
        let row = &mut self.grid[self.cursor_row];
        let col = self.cursor_col;
        // Shift cells left
        for i in col..cols {
            if i + count < cols {
                row.cells[i] = std::mem::replace(&mut row.cells[i + count], erased.clone());
            } else {
                row.cells[i] = erased.clone();
            }
        }
        self.mark_dirty(self.cursor_row);
    }

    fn move_backward_tabs(&mut self, count: u16) {
        for _ in 0..count {
            let prev_tab = (0..self.cursor_col).rev()
                .find(|&c| self.tab_stops.get(c).copied().unwrap_or(false));
            self.cursor_col = prev_tab.unwrap_or(0);
        }
        self.dirty = true;
    }

    fn move_forward_tabs(&mut self, count: u16) {
        self.put_tab(count);
    }

    fn save_cursor_position(&mut self) {
        self.saved_cursor = Some(CursorState {
            row: self.cursor_row,
            col: self.cursor_col,
            fg: self.attrs_fg,
            bg: self.attrs_bg,
            flags: self.attrs_flags,
            charsets: self.charsets,
            active_charset: self.active_charset,
        });
    }

    fn restore_cursor_position(&mut self) {
        if let Some(saved) = &self.saved_cursor {
            self.cursor_row = saved.row;
            self.cursor_col = saved.col;
            self.attrs_fg = saved.fg;
            self.attrs_bg = saved.bg;
            self.attrs_flags = saved.flags;
            self.charsets = saved.charsets;
            self.active_charset = saved.active_charset;
            self.dirty = true;
        }
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let erased = Self::make_erased_cell(self.attrs_bg);
        let row = &mut self.grid[self.cursor_row];
        match mode {
            LineClearMode::Right => {
                for i in self.cursor_col..row.cells.len() {
                    row.cells[i] = erased.clone();
                }
            }
            LineClearMode::Left => {
                for i in 0..=self.cursor_col.min(row.cells.len().saturating_sub(1)) {
                    row.cells[i] = erased.clone();
                }
            }
            LineClearMode::All => {
                for cell in &mut row.cells {
                    *cell = erased.clone();
                }
            }
        }
        self.mark_dirty(self.cursor_row);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let erased = Self::make_erased_cell(self.attrs_bg);
        let bg = self.attrs_bg;
        let cols = self.cols();
        let rows = self.rows();
        match mode {
            ClearMode::Below => {
                let row = &mut self.grid[self.cursor_row];
                for i in self.cursor_col..row.cells.len() {
                    row.cells[i] = erased.clone();
                }
                self.mark_dirty(self.cursor_row);
                for i in (self.cursor_row + 1)..rows {
                    self.grid[i] = Self::make_row_with_bg(cols, bg);
                    self.mark_dirty(i);
                }
            }
            ClearMode::Above => {
                for i in 0..self.cursor_row {
                    self.grid[i] = Self::make_row_with_bg(cols, bg);
                    self.mark_dirty(i);
                }
                let row = &mut self.grid[self.cursor_row];
                for i in 0..=self.cursor_col.min(row.cells.len().saturating_sub(1)) {
                    row.cells[i] = erased.clone();
                }
                self.mark_dirty(self.cursor_row);
            }
            ClearMode::All => {
                for i in 0..rows {
                    self.grid[i] = Self::make_row_with_bg(cols, bg);
                    self.mark_dirty(i);
                }
            }
            ClearMode::Saved => {
                self.scrollback.clear();
            }
        }
    }

    fn clear_tabs(&mut self, mode: vte::ansi::TabulationClearMode) {
        match mode {
            vte::ansi::TabulationClearMode::Current => {
                if self.cursor_col < self.tab_stops.len() {
                    self.tab_stops[self.cursor_col] = false;
                }
            }
            vte::ansi::TabulationClearMode::All => {
                for stop in &mut self.tab_stops {
                    *stop = false;
                }
            }
        }
    }

    fn set_tabs(&mut self, interval: u16) {
        self.tab_stops = vec![false; self.cols()];
        for i in (0..self.cols()).step_by(interval as usize) {
            self.tab_stops[i] = true;
        }
    }

    fn reset_state(&mut self) {
        let rows = self.rows();
        let cols = self.cols();
        self.grid = (0..rows).map(|_| Row::new(cols)).collect();
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.attrs_fg = CellColor::Default;
        self.attrs_bg = CellColor::Default;
        self.attrs_flags = CellFlags::empty();
        self.saved_cursor = None;
        self.scroll_top = 0;
        self.scroll_bottom = rows.saturating_sub(1);
        self.mode_line_wrap = true;
        self.mode_show_cursor = true;
        self.mode_bracketed_paste = false;
        self.mode_cursor_keys = false;
        self.mode_origin = false;
        self.mode_insert = false;
        self.charsets = [StandardCharset::Ascii; 4];
        self.active_charset = CharsetIndex::G0;
        self.title = None;
        self.mark_all_dirty();
    }

    fn reverse_index(&mut self) {
        if self.cursor_row == self.scroll_top {
            self.scroll_down(1);
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
        }
        self.dirty = true;
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        match attr {
            Attr::Reset => {
                self.attrs_fg = CellColor::Default;
                self.attrs_bg = CellColor::Default;
                self.attrs_flags = CellFlags::empty();
            }
            Attr::Bold => self.attrs_flags.insert(CellFlags::BOLD),
            Attr::Dim => self.attrs_flags.insert(CellFlags::DIM),
            Attr::Italic => self.attrs_flags.insert(CellFlags::ITALIC),
            Attr::Underline => {
                self.attrs_flags.remove(CellFlags::DOUBLE_UNDERLINE);
                self.attrs_flags.insert(CellFlags::UNDERLINE);
            }
            Attr::DoubleUnderline => {
                self.attrs_flags.remove(CellFlags::UNDERLINE);
                self.attrs_flags.insert(CellFlags::DOUBLE_UNDERLINE);
            }
            Attr::Undercurl | Attr::DottedUnderline | Attr::DashedUnderline => {
                self.attrs_flags.insert(CellFlags::UNDERLINE);
            }
            Attr::BlinkSlow | Attr::BlinkFast => {}
            Attr::Reverse => self.attrs_flags.insert(CellFlags::INVERSE),
            Attr::Hidden => self.attrs_flags.insert(CellFlags::HIDDEN),
            Attr::Strike => self.attrs_flags.insert(CellFlags::STRIKETHROUGH),
            Attr::CancelBold => self.attrs_flags.remove(CellFlags::BOLD),
            Attr::CancelBoldDim => {
                self.attrs_flags.remove(CellFlags::BOLD | CellFlags::DIM);
            }
            Attr::CancelItalic => self.attrs_flags.remove(CellFlags::ITALIC),
            Attr::CancelUnderline => {
                self.attrs_flags.remove(CellFlags::UNDERLINE | CellFlags::DOUBLE_UNDERLINE);
            }
            Attr::CancelBlink => {}
            Attr::CancelReverse => self.attrs_flags.remove(CellFlags::INVERSE),
            Attr::CancelHidden => self.attrs_flags.remove(CellFlags::HIDDEN),
            Attr::CancelStrike => self.attrs_flags.remove(CellFlags::STRIKETHROUGH),
            Attr::Foreground(color) => {
                self.attrs_fg = match color {
                    Color::Named(nc) => CellColor::Named(nc),
                    Color::Indexed(idx) => CellColor::Indexed(idx),
                    Color::Spec(rgb) => CellColor::Spec(rgb),
                };
            }
            Attr::Background(color) => {
                self.attrs_bg = match color {
                    Color::Named(nc) => CellColor::Named(nc),
                    Color::Indexed(idx) => CellColor::Indexed(idx),
                    Color::Spec(rgb) => CellColor::Spec(rgb),
                };
            }
            Attr::UnderlineColor(_) => {}
        }
    }

    fn set_mode(&mut self, mode: ansi::Mode) {
        match mode {
            ansi::Mode::Named(ansi::NamedMode::Insert) => self.mode_insert = true,
            _ => {}
        }
    }

    fn unset_mode(&mut self, mode: ansi::Mode) {
        match mode {
            ansi::Mode::Named(ansi::NamedMode::Insert) => self.mode_insert = false,
            _ => {}
        }
    }

    fn set_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(ansi::NamedPrivateMode::CursorKeys) => self.mode_cursor_keys = true,
            PrivateMode::Named(ansi::NamedPrivateMode::LineWrap) => self.mode_line_wrap = true,
            PrivateMode::Named(ansi::NamedPrivateMode::Origin) => self.mode_origin = true,
            PrivateMode::Named(ansi::NamedPrivateMode::ShowCursor) => self.mode_show_cursor = true,
            PrivateMode::Named(ansi::NamedPrivateMode::BracketedPaste) => self.mode_bracketed_paste = true,
            PrivateMode::Named(ansi::NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                if !self.using_alt_screen {
                    tracing::debug!("[TERM] ESC[?1049h — switching to alternate screen, cursor=({}, {})", self.cursor_row, self.cursor_col);
                    self.screen_switch_cursor = Some(ScreenSwitchState {
                        cursor: CursorState {
                            row: self.cursor_row,
                            col: self.cursor_col,
                            fg: self.attrs_fg,
                            bg: self.attrs_bg,
                            flags: self.attrs_flags,
                            charsets: self.charsets,
                            active_charset: self.active_charset,
                        },
                        mode_show_cursor: self.mode_show_cursor,
                        mode_cursor_keys: self.mode_cursor_keys,
                        mode_bracketed_paste: self.mode_bracketed_paste,
                        mode_line_wrap: self.mode_line_wrap,
                        mode_origin: self.mode_origin,
                        scroll_top: self.scroll_top,
                        scroll_bottom: self.scroll_bottom,
                        saved_cursor: self.saved_cursor.clone(),
                    });
                    let rows = self.rows();
                    let cols = self.cols();
                    let new_grid = (0..rows).map(|_| Row::new(cols)).collect();
                    self.alt_grid = Some(std::mem::replace(&mut self.grid, new_grid));
                    self.using_alt_screen = true;
                    self.cursor_row = 0;
                    self.cursor_col = 0;
                    // Reset scroll region and DECSC slot for the alternate screen
                    self.scroll_top = 0;
                    self.scroll_bottom = rows.saturating_sub(1);
                    self.saved_cursor = None;
                    self.mark_all_dirty();
                }
            }
            _ => {}
        }
    }

    fn unset_private_mode(&mut self, mode: PrivateMode) {
        match mode {
            PrivateMode::Named(ansi::NamedPrivateMode::CursorKeys) => self.mode_cursor_keys = false,
            PrivateMode::Named(ansi::NamedPrivateMode::LineWrap) => self.mode_line_wrap = false,
            PrivateMode::Named(ansi::NamedPrivateMode::Origin) => self.mode_origin = false,
            PrivateMode::Named(ansi::NamedPrivateMode::ShowCursor) => self.mode_show_cursor = false,
            PrivateMode::Named(ansi::NamedPrivateMode::BracketedPaste) => self.mode_bracketed_paste = false,
            PrivateMode::Named(ansi::NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                if self.using_alt_screen {
                    tracing::debug!("[TERM] ESC[?1049l — switching back to primary screen");
                    if let Some(alt) = self.alt_grid.take() {
                        self.grid = alt;
                    }
                    self.using_alt_screen = false;
                    if let Some(saved) = self.screen_switch_cursor.take() {
                        tracing::debug!("[TERM] restoring cursor to ({}, {}), show_cursor={}", saved.cursor.row, saved.cursor.col, saved.mode_show_cursor);
                        self.cursor_row = saved.cursor.row;
                        self.cursor_col = saved.cursor.col;
                        self.attrs_fg = saved.cursor.fg;
                        self.attrs_bg = saved.cursor.bg;
                        self.attrs_flags = saved.cursor.flags;
                        self.charsets = saved.cursor.charsets;
                        self.active_charset = saved.cursor.active_charset;
                        self.mode_show_cursor = saved.mode_show_cursor;
                        self.mode_cursor_keys = saved.mode_cursor_keys;
                        self.mode_bracketed_paste = saved.mode_bracketed_paste;
                        self.mode_line_wrap = saved.mode_line_wrap;
                        self.mode_origin = saved.mode_origin;
                        self.scroll_top = saved.scroll_top;
                        self.scroll_bottom = saved.scroll_bottom;
                        self.saved_cursor = saved.saved_cursor;
                        self.dirty = true;
                    }
                    self.mark_all_dirty();
                }
            }
            _ => {}
        }
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.scroll_top = top.saturating_sub(1); // 1-based to 0-based
        self.scroll_bottom = match bottom {
            Some(b) => (b.saturating_sub(1)).min(self.rows().saturating_sub(1)),
            None => self.rows().saturating_sub(1),
        };
        // Move cursor to home position
        self.cursor_row = if self.mode_origin { self.scroll_top } else { 0 };
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn set_active_charset(&mut self, index: CharsetIndex) {
        self.active_charset = index;
    }

    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        self.charsets[index as usize] = charset;
    }

    fn set_title(&mut self, title: Option<String>) {
        if let Some(ref t) = title {
            self.detect_tmux_from_title(t);
        }
        self.title = title;
    }

    fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            // DA1: Identify as VT220 with 132-column support
            None => self.send_response(b"\x1b[?62c"),
            // DA2: Identify version
            Some('>') => self.send_response(b"\x1b[>1;95;0c"),
            _ => {}
        }
    }

    fn device_status(&mut self, arg: usize) {
        match arg {
            5 => self.send_response(b"\x1b[0n"),  // OK
            6 => {
                // Cursor position report
                let row = self.cursor_row + 1;
                let col = self.cursor_col + 1;
                let resp = format!("\x1b[{};{}R", row, col);
                self.send_response(resp.as_bytes());
            }
            _ => {}
        }
    }

    fn decaln(&mut self) {
        for row in &mut self.grid {
            for cell in &mut row.cells {
                cell.character = 'E';
            }
        }
        self.mark_all_dirty();
    }

    fn push_title(&mut self) {}
    fn pop_title(&mut self) {}
    fn set_keypad_application_mode(&mut self) {}
    fn unset_keypad_application_mode(&mut self) {}
    fn set_color(&mut self, _: usize, _: ansi::Rgb) {}
    fn dynamic_color_sequence(&mut self, _: String, _: usize, _: &str) {}
    fn reset_color(&mut self, _: usize) {}
    fn clipboard_store(&mut self, _: u8, _: &[u8]) {}
    fn clipboard_load(&mut self, _: u8, _: &str) {}
    fn report_mode(&mut self, mode: ansi::Mode) {
        let is_set = match mode {
            ansi::Mode::Named(ansi::NamedMode::Insert) => self.mode_insert,
            _ => false,
        };
        // DECRPM: ESC [ ? <mode> ; <status> $ y
        // status: 1 = set, 2 = reset, 3 = permanently set, 4 = permanently reset
        let mode_num = mode.raw();
        let status = if is_set { 1 } else { 2 };
        let resp = format!("\x1b[{};{}$y", mode_num, status);
        self.send_response(resp.as_bytes());
    }

    fn report_private_mode(&mut self, mode: PrivateMode) {
        let is_set = match mode {
            PrivateMode::Named(ansi::NamedPrivateMode::CursorKeys) => self.mode_cursor_keys,
            PrivateMode::Named(ansi::NamedPrivateMode::LineWrap) => self.mode_line_wrap,
            PrivateMode::Named(ansi::NamedPrivateMode::Origin) => self.mode_origin,
            PrivateMode::Named(ansi::NamedPrivateMode::ShowCursor) => self.mode_show_cursor,
            PrivateMode::Named(ansi::NamedPrivateMode::BracketedPaste) => self.mode_bracketed_paste,
            PrivateMode::Named(ansi::NamedPrivateMode::SwapScreenAndSetRestoreCursor) => {
                self.using_alt_screen
            }
            _ => false,
        };
        let mode_num = mode.raw();
        let status = if is_set { 1 } else { 2 };
        let resp = format!("\x1b[?{};{}$y", mode_num, status);
        self.send_response(resp.as_bytes());
    }
}

// ── TerminalSize ────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TerminalSize {
    pub cols: u16,
    pub rows: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

impl Default for TerminalSize {
    fn default() -> Self {
        Self {
            cols: 80,
            rows: 24,
            pixel_width: 0,
            pixel_height: 0,
        }
    }
}

use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc133_exit_code_parsed() {
        let mut term = Terminal::new(TerminalSize::default());
        let mut parser = vte::ansi::Processor::new();
        // OSC 133;D;0 (BEL-terminated) — successful command.
        term.process(b"\x1b]133;D;0\x07", &mut parser);
        assert_eq!(term.take_exit_code(), Some(0));
        // Dirty flag cleared — second take returns None.
        assert_eq!(term.take_exit_code(), None);

        // OSC 133;D;127 (ST-terminated) — failed command.
        term.process(b"\x1b]133;D;127\x1b\\", &mut parser);
        assert_eq!(term.take_exit_code(), Some(127));
    }

    #[test]
    fn osc133_exit_code_among_output() {
        let mut term = Terminal::new(TerminalSize::default());
        let mut parser = vte::ansi::Processor::new();
        // Marker embedded in normal output.
        term.process(b"command output\r\n\x1b]133;D;1\x07$ ", &mut parser);
        assert_eq!(term.take_exit_code(), Some(1));
    }

    #[test]
    fn osc133_no_code_defaults_zero() {
        let mut term = Terminal::new(TerminalSize::default());
        let mut parser = vte::ansi::Processor::new();
        // 133;D with no exit code → treat as 0 (success).
        term.process(b"\x1b]133;D\x07", &mut parser);
        assert_eq!(term.take_exit_code(), Some(0));
    }

    /// Regression guard for the VecDeque scrollback: feeding more lines than the
    /// capacity must evict the oldest lines (FIFO) and never grow unbounded,
    /// and the surviving scrollback must remain renderable.
    #[test]
    fn scrollback_evicts_oldest_when_full() {
        let mut term = Terminal::new(TerminalSize { cols: 10, rows: 2, ..Default::default() });
        term.scrollback_capacity = 3;
        let mut parser = vte::ansi::Processor::new();

        // Each "aaa\r\n" writes one row then scrolls it off the 2-row grid.
        // After 5 lines we should have evicted the oldest 2, keeping exactly 3.
        term.process(b"aaa\r\n", &mut parser);
        term.process(b"bbb\r\n", &mut parser);
        term.process(b"ccc\r\n", &mut parser);
        term.process(b"ddd\r\n", &mut parser);
        term.process(b"eee\r\n", &mut parser);

        assert_eq!(term.scrollback_len(), 3, "scrollback must be capped at capacity");

        // Render the entire scrollback by scrolling past the grid. The surviving
        // lines are bbb, ccc, ddd — aaa must have been evicted (FIFO), and the
        // newest line eee lives in the grid, not the scrollback.
        let out = term.render_with_scroll(3);
        assert_eq!(out.rows.len(), 2);
        let all_text: String = out.rows.iter()
            .flat_map(|r| r.cells.iter().map(|c| c.character))
            .collect();
        assert!(!all_text.contains('a'), "evicted line 'aaa' must not appear, got {all_text:?}");
        assert!(all_text.contains('b') && all_text.contains('c'), "oldest survivors bbb/ccc expected, got {all_text:?}");

        // Eviction must keep memory bounded — capacity is the hard ceiling.
        term.process(b"fff\r\n", &mut parser);
        term.process(b"ggg\r\n", &mut parser);
        assert_eq!(term.scrollback_len(), 3, "scrollback stays capped under sustained output");
        // After two more lines, even bbb should now be evicted.
        let out2 = term.render_with_scroll(3);
        let text2: String = out2.rows.iter()
            .flat_map(|r| r.cells.iter().map(|c| c.character))
            .collect();
        assert!(!text2.contains('b'), "bbb should be evicted after fff/ggg, got {text2:?}");
    }

    /// Multi-line scroll (count > 1) must blank the correct number of bottom
    /// rows. Guards the scroll_up swap rewrite which previously risked leaking
    /// the scrolled-away content into the bottom row on the alt screen.
    #[test]
    fn scroll_up_blanks_bottom_rows() {
        let mut term = Terminal::new(TerminalSize { cols: 4, rows: 4, ..Default::default() });
        let mut parser = vte::ansi::Processor::new();
        // Fill 4 distinct rows: AAAA, BBBB, CCCC, DDDD
        term.process(b"AAAA\r\nBBBB\r\nCCCC\r\nDDDD", &mut parser);
        // Cursor sits on row 3 (DDDD). Scrolling up 2 from the bottom should
        // push AAAA, BBBB into scrollback and leave two blank rows at bottom.
        term.process(b"\r\n\r\n", &mut parser);

        // The two bottom rows must be blank (all spaces), not leftover content.
        let out = term.render();
        for cell in &out.rows[2].cells {
            assert_eq!(cell.character, ' ', "bottom-1 row must be blank after scroll");
        }
        assert!(term.scrollback_len() >= 2);
    }
}
