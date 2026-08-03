//! The terminal grid this frontend draws.
//!
//! The server is the only terminal emulator: it sends styled runs, a cursor
//! position and the resolved default colours, and the client draws them. This
//! module owns nothing but that received state.
//!
//! Merge semantics follow `cmux-tui/spec/render.md`:
//!
//! * a snapshot replaces every row and resets scroll state;
//! * a patch carrying a size must be a full reset;
//! * a full reset replaces every row;
//! * otherwise each row in the patch replaces the row with the same index.

use cmux::{ColorHex, RenderCursor, RenderPatch, RenderRow, RenderSnapshot, Size, TerminalId};

#[derive(Debug)]
pub struct Screen {
    pub terminal: Option<TerminalId>,
    pub size: Size,
    pub cursor: Option<RenderCursor>,
    pub default_fg: ColorHex,
    pub default_bg: ColorHex,
    pub scrollback_rows: u32,
    pub at_bottom: bool,
    /// One entry per viewport row, indexed by row number.
    pub rows: Vec<Option<RenderRow>>,
    /// Frontend-owned selection, as ((start_row, start_col), (end_row, end_col)).
    pub selection: Option<((u16, u16), (u16, u16))>,
}

impl Default for Screen {
    fn default() -> Self {
        Self {
            terminal: None,
            size: Size { cols: 80, rows: 24 },
            cursor: None,
            default_fg: ColorHex::parse("#d0d0d0").expect("valid literal"),
            default_bg: ColorHex::parse("#101010").expect("valid literal"),
            scrollback_rows: 0,
            at_bottom: true,
            rows: Vec::new(),
            selection: None,
        }
    }
}

impl Screen {
    pub fn is_initialized(&self) -> bool {
        self.terminal.is_some()
    }

    pub fn apply_snapshot(&mut self, terminal: TerminalId, render: RenderSnapshot) {
        self.terminal = Some(terminal);
        self.size = render.size;
        self.cursor = Some(render.cursor);
        self.default_fg = render.default_fg;
        self.default_bg = render.default_bg;
        self.scrollback_rows = render.scrollback_rows;
        self.at_bottom = true;
        self.rows = place_rows(render.size.rows, render.rows);
        // The old selection pointed into a grid that no longer exists.
        self.selection = None;
    }

    /// Returns an error string when the patch cannot be applied. A frontend
    /// that has lost sync must re-attach rather than draw a half-updated grid,
    /// so this never patches on a best-effort basis.
    pub fn apply_patch(&mut self, terminal: &TerminalId, render: RenderPatch) -> Result<(), String> {
        match &self.terminal {
            Some(current) if current == terminal => {}
            Some(_) => return Err("render patch is for a different terminal".to_string()),
            None => return Err("render patch arrived before the initial snapshot".to_string()),
        }

        if render.size.is_some() && !render.full_reset {
            return Err("render patch carrying a size must be a full reset".to_string());
        }

        if let Some(fg) = render.default_fg {
            self.default_fg = fg;
        }
        if let Some(bg) = render.default_bg {
            self.default_bg = bg;
        }
        if let Some(scrollback) = render.scrollback_rows {
            self.scrollback_rows = scrollback;
        }
        self.cursor = Some(render.cursor);

        if render.full_reset {
            let size = render.size.unwrap_or(self.size);
            self.size = size;
            self.rows = place_rows(size.rows, render.rows);
            self.selection = None;
            return Ok(());
        }

        for row in render.rows {
            let index = usize::from(row.row);
            if index >= self.rows.len() {
                return Err(format!(
                    "render patch row {index} is outside the {} row viewport",
                    self.rows.len()
                ));
            }
            self.rows[index] = Some(row);
        }
        Ok(())
    }

    pub fn apply_scroll(&mut self, at_bottom: bool) {
        self.at_bottom = at_bottom;
    }

    /// Selection is frontend-owned state: `spec/native-frontend.md` puts
    /// selection, hover and scroll position on the client, not the mux.
    pub fn set_selection(&mut self, anchor: (u16, u16), head: (u16, u16)) {
        self.selection = Some(normalize(anchor, head));
    }

    pub fn clear_selection(&mut self) {
        self.selection = None;
    }

    pub fn is_selected(&self, row: u16, column: u16) -> bool {
        let Some((start, end)) = self.selection else { return false };
        // Linear selection, the way a terminal selects: full rows between the
        // endpoints, partial rows at each end.
        if row < start.0 || row > end.0 {
            return false;
        }
        if start.0 == end.0 {
            return column >= start.1 && column < end.1;
        }
        if row == start.0 {
            return column >= start.1;
        }
        if row == end.0 {
            return column < end.1;
        }
        true
    }

    /// The selected text, with trailing blanks on each row trimmed the way a
    /// terminal copy does.
    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection?;
        let mut lines = Vec::new();
        for row_index in start.0..=end.0 {
            let Some(Some(row)) = self.rows.get(usize::from(row_index)) else {
                continue;
            };
            let mut line = String::new();
            let mut column = 0u16;
            for run in &row.runs {
                let cells = run
                    .width_hint
                    .unwrap_or_else(|| run.text.chars().count() as u16);
                for (offset, character) in run.text.chars().enumerate() {
                    let cell = column.saturating_add(offset as u16);
                    if self.is_selected(row_index, cell) {
                        line.push(character);
                    }
                }
                column = column.saturating_add(cells);
            }
            lines.push(line.trim_end().to_string());
        }
        if lines.is_empty() {
            return None;
        }
        Some(lines.join("\n"))
    }
}

/// Orders two cell coordinates into (top-left-most, bottom-right-most). The
/// head column is exclusive so a click without a drag selects nothing.
fn normalize(anchor: (u16, u16), head: (u16, u16)) -> ((u16, u16), (u16, u16)) {
    if (head.0, head.1) >= (anchor.0, anchor.1) {
        (anchor, head)
    } else {
        (head, anchor)
    }
}

fn place_rows(row_count: u16, rows: Vec<RenderRow>) -> Vec<Option<RenderRow>> {
    let mut placed = vec![None; usize::from(row_count)];
    for row in rows {
        let index = usize::from(row.row);
        // Rows outside the advertised viewport are dropped rather than
        // resizing the grid; the next snapshot is authoritative on size.
        if index < placed.len() {
            placed[index] = Some(row);
        }
    }
    placed
}
