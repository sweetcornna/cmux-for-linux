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
