//! Lenient frontend configuration.
//!
//! The shared TUI document is read as untyped JSON because this frontend only
//! owns a few presentation fields and must not reject newer TUI settings.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

const DEFAULT_FONT: &str = "monospace 11";
pub const DEFAULT_DARK_BACKGROUND: Rgb = Rgb(0x1e, 0x1e, 0x1e);
pub const DEFAULT_LIGHT_BACKGROUND: Rgb = Rgb(0xfe, 0xff, 0xff);
pub const DEFAULT_CURSOR: Rgb = Rgb(0x98, 0x98, 0x9d);
pub const DEFAULT_DARK_SELECTION_BACKGROUND: Rgb = Rgb(0x3f, 0x63, 0x8b);
pub const DEFAULT_LIGHT_SELECTION_BACKGROUND: Rgb = Rgb(0xab, 0xd8, 0xff);
pub const DEFAULT_NOTIFICATION_INFO: Rgb = Rgb(0x87, 0xaf, 0xd7);
pub const DEFAULT_NOTIFICATION_WARNING: Rgb = Rgb(0xd7, 0xaf, 0x5f);
pub const DEFAULT_NOTIFICATION_ERROR: Rgb = Rgb(0xd7, 0x5f, 0x5f);

/// Light-chrome values sampled from the upstream application's own window, so
/// `theme.chrome = "light"` reproduces it rather than approximating it. The row
/// selection there is a filled accent with white text, not a grey wash, and the
/// secondary text is warm rather than neutral grey.
pub const UPSTREAM_LIGHT_SELECTED_FOREGROUND: Rgb = Rgb(0xff, 0xff, 0xff);
pub const UPSTREAM_LIGHT_DIM_FOREGROUND: Rgb = Rgb(0x77, 0x72, 0x68);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    pub fn cairo(self) -> (f64, f64, f64) {
        (
            f64::from(self.0) / 255.0,
            f64::from(self.1) / 255.0,
            f64::from(self.2) / 255.0,
        )
    }

    fn css(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }

    pub fn parse(value: &str) -> Option<Self> {
        parse_color_string(value)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChromeMode {
    #[default]
    Auto,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThemeOverrides {
    pub selection_background: Option<Rgb>,
    pub selection_foreground: Option<Option<Rgb>>,
    pub sidebar_rail: Option<Rgb>,
    pub sidebar_selected_background: Option<Rgb>,
    pub tab_bar_background: Option<Rgb>,
    pub tab_active_background: Option<Rgb>,
    pub border_active: Option<Rgb>,
    pub border_inactive: Option<Rgb>,
    pub scrollbar_thumb: Option<Rgb>,
    pub scrollbar_thumb_active: Option<Rgb>,
    pub prompt_background: Option<Rgb>,
    pub prompt_foreground: Option<Rgb>,
    pub prompt_border: Option<Rgb>,
    pub prompt_input_background: Option<Rgb>,
    pub prompt_input_foreground: Option<Rgb>,
    pub notification_info: Option<Rgb>,
    pub notification_warning: Option<Rgb>,
    pub notification_error: Option<Rgb>,
}

/// Cursor shape requested by configuration, overriding the protocol's own
/// per-terminal style when set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

/// Client-side overrides for the terminal surface itself.
///
/// The server resolves `default_fg`, `default_bg` and the 16 ANSI colors for
/// every render frame, so these exist to let a user restyle the grid without
/// changing the shared `cmux-tui.json` palette every frontend reads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct TerminalAppearance {
    pub foreground: Option<Rgb>,
    pub background: Option<Rgb>,
    pub cursor: Option<Rgb>,
    pub cursor_shape: Option<CursorShape>,
    pub cursor_blink: Option<bool>,
    /// ANSI 0-15 overrides, indexed by palette slot.
    pub palette: [Option<Rgb>; 16],
    /// Multiplier on the font's natural line height. Clamped on read.
    pub line_height: Option<f64>,
    /// Extra pixels between cell origins. Clamped on read.
    pub letter_spacing: Option<f64>,
    /// Padding in pixels between a pane's edge and its first cell.
    pub padding: Option<f64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub font: String,
    pub chrome: ChromeMode,
    pub theme: ThemeOverrides,
    pub terminal: TerminalAppearance,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            font: DEFAULT_FONT.to_string(),
            chrome: ChromeMode::Auto,
            theme: ThemeOverrides::default(),
            terminal: TerminalAppearance::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub color: Rgb,
    pub alpha: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChromeColors {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub separator: Rgba,
    pub pane_separator: Rgba,
    pub sidebar_background: Rgb,
    pub selection_background: Rgb,
    pub chrome_selection_background: Rgb,
    pub selection_foreground: Option<Rgb>,
    pub accent: Rgb,
    pub tab_bar_background: Rgb,
    pub tab_bar_is_opaque: bool,
    pub tab_foreground: Rgb,
    pub tab_active_background: Rgb,
    pub tab_active_foreground: Rgb,
    pub tab_active_unfocused_background: Rgb,
    pub tab_active_unfocused_foreground: Rgb,
    pub sidebar_selected_background: Rgb,
    pub sidebar_selected_foreground: Rgb,
    pub sidebar_dim_foreground: Rgb,
    pub sidebar_border: Rgb,
    pub border_foreground: Rgb,
    pub border_active_foreground: Rgb,
    pub draw_active_border: bool,
    pub menu_background: Rgb,
    pub menu_foreground: Rgb,
    pub toast_background: Rgb,
    pub toast_foreground: Rgb,
    pub scrollbar_thumb_foreground: Rgb,
    pub scrollbar_thumb_active_foreground: Rgb,
    pub prompt_background: Rgb,
    pub prompt_foreground: Rgb,
    pub prompt_border: Rgb,
    pub prompt_input_background: Rgb,
    pub prompt_input_foreground: Rgb,
    pub notification_info: Rgb,
    pub notification_warning: Rgb,
    pub notification_error: Rgb,
    pub workspace_rail: Option<Rgb>,
    pub is_light: bool,
}

impl ChromeColors {
    pub fn derive(background: Rgb, mode: ChromeMode, overrides: ThemeOverrides) -> Self {
        let terminal_is_light = is_light_background(background);
        let (is_light, chrome_background) = match mode {
            ChromeMode::Auto => (terminal_is_light, background),
            ChromeMode::Light => (
                true,
                if terminal_is_light {
                    background
                } else {
                    DEFAULT_LIGHT_BACKGROUND
                },
            ),
            ChromeMode::Dark => (
                false,
                if terminal_is_light {
                    DEFAULT_DARK_BACKGROUND
                } else {
                    background
                },
            ),
        };
        let separator = separator_color(chrome_background);
        let pane_separator = overrides
            .border_inactive
            .map(|color| Rgba { color, alpha: 1.0 })
            .unwrap_or(separator);

        let mut colors = if is_light {
            Self {
                background: chrome_background,
                foreground: Rgb(0x00, 0x00, 0x00),
                cursor: DEFAULT_CURSOR,
                separator,
                pane_separator,
                sidebar_background: sidebar_background(chrome_background),
                selection_background: DEFAULT_LIGHT_SELECTION_BACKGROUND,
                chrome_selection_background: Rgb(0xcc, 0xdd, 0xf5),
                selection_foreground: Some(Rgb(0x00, 0x00, 0x00)),
                accent: Rgb(0x00, 0x88, 0xff),
                tab_bar_background: Rgb(0xe4, 0xe4, 0xe4),
                tab_bar_is_opaque: false,
                tab_foreground: Rgb(0x58, 0x58, 0x58),
                tab_active_background: Rgb(0xd0, 0xd0, 0xd0),
                tab_active_foreground: Rgb(0x1c, 0x1c, 0x1c),
                tab_active_unfocused_background: Rgb(0xda, 0xda, 0xda),
                tab_active_unfocused_foreground: Rgb(0x30, 0x30, 0x30),
                sidebar_selected_background: Rgb(0x00, 0x88, 0xff),
                sidebar_selected_foreground: UPSTREAM_LIGHT_SELECTED_FOREGROUND,
                sidebar_dim_foreground: UPSTREAM_LIGHT_DIM_FOREGROUND,
                sidebar_border: Rgb(0x94, 0x94, 0x94),
                border_foreground: Rgb(0x94, 0x94, 0x94),
                border_active_foreground: overrides.border_active.unwrap_or(Rgb(0x00, 0x87, 0xaf)),
                draw_active_border: overrides.border_active.is_some(),
                menu_background: Rgb(0xe4, 0xe4, 0xe4),
                menu_foreground: Rgb(0x30, 0x30, 0x30),
                toast_background: Rgb(0xd0, 0xd0, 0xd0),
                toast_foreground: Rgb(0x1c, 0x1c, 0x1c),
                scrollbar_thumb_foreground: overrides
                    .scrollbar_thumb
                    .unwrap_or(Rgb(0x94, 0x94, 0x94)),
                scrollbar_thumb_active_foreground: overrides
                    .scrollbar_thumb_active
                    .unwrap_or(Rgb(0x58, 0x58, 0x58)),
                prompt_background: overrides.prompt_background.unwrap_or(Rgb(0xe4, 0xe4, 0xe4)),
                prompt_foreground: overrides.prompt_foreground.unwrap_or(Rgb(0x30, 0x30, 0x30)),
                prompt_border: overrides.prompt_border.unwrap_or(Rgb(0x94, 0x94, 0x94)),
                prompt_input_background: overrides
                    .prompt_input_background
                    .unwrap_or(Rgb(0xee, 0xee, 0xee)),
                prompt_input_foreground: overrides
                    .prompt_input_foreground
                    .unwrap_or(Rgb(0x1c, 0x1c, 0x1c)),
                notification_info: overrides
                    .notification_info
                    .unwrap_or(DEFAULT_NOTIFICATION_INFO),
                notification_warning: overrides
                    .notification_warning
                    .unwrap_or(DEFAULT_NOTIFICATION_WARNING),
                notification_error: overrides
                    .notification_error
                    .unwrap_or(DEFAULT_NOTIFICATION_ERROR),
                workspace_rail: overrides.sidebar_rail,
                is_light,
            }
        } else {
            Self {
                background: chrome_background,
                foreground: Rgb(0xff, 0xff, 0xff),
                cursor: DEFAULT_CURSOR,
                separator,
                pane_separator,
                sidebar_background: sidebar_background(chrome_background),
                selection_background: DEFAULT_DARK_SELECTION_BACKGROUND,
                chrome_selection_background: Rgb(0x3a, 0x3a, 0x3a),
                selection_foreground: Some(Rgb(0xff, 0xff, 0xff)),
                accent: Rgb(0x00, 0x91, 0xff),
                tab_bar_background: Rgb(0x30, 0x30, 0x30),
                tab_bar_is_opaque: false,
                tab_foreground: Rgb(0xa8, 0xa8, 0xa8),
                tab_active_background: Rgb(0x58, 0x58, 0x58),
                tab_active_foreground: Rgb(0xee, 0xee, 0xee),
                tab_active_unfocused_background: Rgb(0x44, 0x44, 0x44),
                tab_active_unfocused_foreground: Rgb(0xd0, 0xd0, 0xd0),
                sidebar_selected_background: Rgb(0x30, 0x30, 0x30),
                sidebar_selected_foreground: Rgb(0xee, 0xee, 0xee),
                sidebar_dim_foreground: Rgb(0x6c, 0x6c, 0x6c),
                sidebar_border: Rgb(0x3a, 0x3a, 0x3a),
                border_foreground: Rgb(0x44, 0x44, 0x44),
                border_active_foreground: overrides.border_active.unwrap_or(Rgb(0x87, 0xaf, 0xd7)),
                draw_active_border: overrides.border_active.is_some(),
                menu_background: Rgb(0x3a, 0x3a, 0x3a),
                menu_foreground: Rgb(0xd0, 0xd0, 0xd0),
                toast_background: Rgb(0x58, 0x58, 0x58),
                toast_foreground: Rgb(0xee, 0xee, 0xee),
                scrollbar_thumb_foreground: overrides
                    .scrollbar_thumb
                    .unwrap_or(Rgb(0x94, 0x94, 0x94)),
                scrollbar_thumb_active_foreground: overrides
                    .scrollbar_thumb_active
                    .unwrap_or(Rgb(0xd0, 0xd0, 0xd0)),
                prompt_background: overrides.prompt_background.unwrap_or(Rgb(0x30, 0x30, 0x30)),
                prompt_foreground: overrides.prompt_foreground.unwrap_or(Rgb(0xd0, 0xd0, 0xd0)),
                prompt_border: overrides.prompt_border.unwrap_or(Rgb(0x80, 0x80, 0x80)),
                prompt_input_background: overrides
                    .prompt_input_background
                    .unwrap_or(Rgb(0x12, 0x12, 0x12)),
                prompt_input_foreground: overrides
                    .prompt_input_foreground
                    .unwrap_or(Rgb(0xee, 0xee, 0xee)),
                notification_info: overrides
                    .notification_info
                    .unwrap_or(DEFAULT_NOTIFICATION_INFO),
                notification_warning: overrides
                    .notification_warning
                    .unwrap_or(DEFAULT_NOTIFICATION_WARNING),
                notification_error: overrides
                    .notification_error
                    .unwrap_or(DEFAULT_NOTIFICATION_ERROR),
                workspace_rail: overrides.sidebar_rail,
                is_light,
            }
        };

        if let Some(color) = overrides.selection_background {
            colors.selection_background = color;
            colors.chrome_selection_background = color;
        }
        if let Some(color) = overrides.selection_foreground {
            colors.selection_foreground = color;
        }
        if let Some(color) = overrides.sidebar_selected_background {
            colors.sidebar_selected_background = color;
        }
        if let Some(color) = overrides.tab_bar_background {
            colors.tab_bar_background = color;
            colors.tab_bar_is_opaque = true;
        }
        if let Some(color) = overrides.tab_active_background {
            colors.tab_active_background = color;
            colors.tab_active_unfocused_background = color;
        }
        colors
    }

    pub fn css(&self) -> String {
        let hover = format!(
            "rgba({}, {}, {}, 0.08)",
            self.foreground.0, self.foreground.1, self.foreground.2
        );
        let accent_25 = format!(
            "rgba({}, {}, {}, 0.25)",
            self.accent.0, self.accent.1, self.accent.2
        );
        format!(
            r#"
@define-color chrome_bg {chrome_bg};
@define-color chrome_fg {chrome_fg};
@define-color terminal_cursor {terminal_cursor};
@define-color chrome_separator {chrome_separator};
@define-color pane_separator {pane_separator};
@define-color sidebar_bg {sidebar_bg};
@define-color selection_bg {selection_bg};
@define-color sidebar_selected_bg {sidebar_selected_bg};
@define-color sidebar_selected_fg {sidebar_selected_fg};
@define-color sidebar_dim_fg {sidebar_dim_fg};
@define-color sidebar_border {sidebar_border};
@define-color accent {accent};
@define-color accent_25 {accent_25};
@define-color tab_bar_bg {tab_bar_bg};
@define-color tab_fg {tab_fg};
@define-color tab_active_bg {tab_active_bg};
@define-color tab_active_fg {tab_active_fg};
@define-color tab_active_unfocused_bg {tab_active_unfocused_bg};
@define-color tab_active_unfocused_fg {tab_active_unfocused_fg};
@define-color border_fg {border_fg};
@define-color border_active_fg {border_active_fg};
@define-color menu_bg {menu_bg};
@define-color menu_fg {menu_fg};
@define-color toast_bg {toast_bg};
@define-color toast_fg {toast_fg};
	@define-color scrollbar_thumb_fg {scrollbar_thumb_fg};
	@define-color scrollbar_thumb_active_fg {scrollbar_thumb_active_fg};
	@define-color prompt_bg {prompt_bg};
	@define-color prompt_fg {prompt_fg};
	@define-color prompt_border {prompt_border};
	@define-color prompt_input_bg {prompt_input_bg};
	@define-color prompt_input_fg {prompt_input_fg};
@define-color sidebar_hover_bg {sidebar_hover_bg};

window.cmux-window,
.cmux-titlebar {{
  background-color: @chrome_bg;
  color: @chrome_fg;
}}
window.cmux-window * {{ transition: none; }}
.cmux-titlebar {{
  min-height: 28px;
  padding: 0;
}}
.cmux-title {{
  font-size: 13px;
  font-weight: 700;
  color: @chrome_fg;
}}
.cmux-titlebar windowcontrols button {{
  min-width: 20px;
  min-height: 20px;
  padding: 0;
  margin: 4px 2px;
  border-radius: 6px;
}}
.cmux-titlebar windowcontrols button image {{
  min-width: 12px;
  min-height: 12px;
}}
.titlebar-actions {{ margin-right: 2px; }}
.titlebar-action,
.workspace-close {{
  min-width: 20px;
  min-height: 20px;
  padding: 0;
  border-radius: 6px;
  background-color: transparent;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @chrome_fg;
}}
.titlebar-action {{ margin: 4px 2px; }}
.titlebar-action:hover,
.workspace-close:hover {{ background-color: @sidebar_hover_bg; }}
.titlebar-action:active,
.workspace-close:active {{ opacity: 0.5; }}
.titlebar-action image,
.workspace-close image {{
  min-width: 12px;
  min-height: 12px;
}}
.sidebar-surface,
.workspace-list {{
  background-color: @sidebar_bg;
  background-image: none;
  border: none;
  box-shadow: none;
}}
.workspace-list {{ padding-top: 2px; }}
.workspace-row {{
  margin: 0 6px 2px 6px;
  padding: 0;
  border-radius: 6px;
  background-color: transparent;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @chrome_fg;
}}
.workspace-row:hover {{ background-color: transparent; }}
.workspace-row:selected,
.workspace-row:selected:hover {{
  background-color: @sidebar_selected_bg;
  color: @sidebar_selected_fg;
}}
.workspace-content {{
  padding: 8px 10px;
  border-radius: 6px;
}}
.workspace-title {{
  font-size: 12.5px;
  font-weight: 600;
}}
.workspace-description {{ font-size: 10.5px; font-weight: 400; }}
.workspace-subtitle {{ font-size: 10px; }}
.workspace-details {{ border-spacing: 4px; }}
.workspace-remote-target {{ font-family: monospace; font-size: 10px; }}
.workspace-remote-status {{ font-size: 9px; font-weight: 500; }}
.workspace-badge {{
  font-size: 9px;
  font-weight: 600;
  color: @sidebar_dim_fg;
}}
.workspace-pin,
.workspace-media {{ font-size: 9px; }}
.task-status-slot {{ min-width: 11px; min-height: 11px; }}
.workspace-row:active .workspace-content {{ opacity: 0.5; }}
.workspace-row.dragging .workspace-content {{ opacity: 0.6; }}
.workspace-row.multi-selected {{ background-color: @accent_25; }}
.workspace-rail {{
  min-width: 3px;
  border-radius: 1.5px;
  margin-top: 5px;
  margin-bottom: 5px;
  opacity: 0.95;
}}
window.cmux-window .workspace-close,
window.cmux-window .group-add {{
  opacity: 0;
  transition: opacity 120ms ease-out;
}}
.workspace-row:hover .workspace-close,
.group-header:hover .group-add {{ opacity: 1; }}
.workspace-close {{
  margin-right: 8px;
  color: @sidebar_dim_fg;
}}
.group-header {{ border-radius: 4px; }}
.group-header:hover {{ background-color: @sidebar_hover_bg; }}
.group-header.multi-selected {{ border-radius: 6px; }}
.group-name {{ font-size: 11px; }}
.group-chevron {{ font-size: 9px; }}
.group-folder,
.group-add {{ font-size: 11px; }}
.group-unread {{ font-size: 10px; }}
.group-member {{ margin-left: 12px; }}
.unread-badge {{
  min-width: 16px;
  min-height: 16px;
  border-radius: 8px;
  padding: 0;
  font-size: 9px;
  font-weight: 600;
  background-color: @accent;
}}
.drop-indicator {{
  min-height: 2px;
  margin-left: 8px;
  margin-right: 8px;
  background-color: @accent;
}}
paned.cmux-split > separator {{
  min-width: 10px;
  background-image: linear-gradient(to right,
    transparent 0%, transparent 60%,
    @chrome_separator 60%, @chrome_separator 70%,
    transparent 70%, transparent 100%);
}}
paned.cmux-split > separator:hover,
paned.cmux-split > separator:active {{
  background-image: linear-gradient(to right,
    transparent 0%, transparent 60%,
    @chrome_separator 60%, @chrome_separator 70%,
    transparent 70%, transparent 100%);
}}
popover.cmux-menu > contents {{
  background-color: @menu_bg;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @menu_fg;
}}
popover.cmux-menu modelbutton {{
  background-image: none;
  border: none;
  box-shadow: none;
  color: @menu_fg;
}}
popover.cmux-menu modelbutton:hover {{
  background-color: @selection_bg;
}}
popover.session-menu > contents {{
  min-width: 210px;
  padding: 4px;
  background-color: @menu_bg;
  color: @menu_fg;
}}
.session-list {{ border-spacing: 1px; }}
.session-item,
.session-new {{
  min-height: 28px;
  padding: 2px 8px;
  border-radius: 4px;
  background-color: transparent;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @menu_fg;
}}
.session-item:hover,
.session-new:hover {{ background-color: @selection_bg; }}
.session-item.current {{ background-color: @selection_bg; }}
.session-check {{ min-width: 14px; min-height: 14px; }}
.session-empty {{
  min-height: 28px;
  padding: 2px 8px;
  color: @menu_fg;
  opacity: 0.7;
}}
popover.session-menu separator {{
  margin: 4px 2px;
  background-color: @chrome_separator;
  background-image: none;
  border: none;
  box-shadow: none;
}}
popover.rename-prompt > contents {{
  padding: 6px;
  background-color: @menu_bg;
  color: @menu_fg;
}}
.rename-entry {{
  min-height: 24px;
  padding: 2px 6px;
  border-radius: 4px;
  background-color: @selection_bg;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @menu_fg;
}}
.toast {{
  margin: 12px;
  padding: 6px 10px;
  border-radius: 6px;
  background-color: @toast_bg;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @toast_fg;
}}
.search-bar {{
  min-height: 28px;
  padding: 3px 6px;
  border-spacing: 6px;
  background-color: @prompt_bg;
  background-image: none;
  border: none;
  border-bottom: 1px solid @prompt_border;
  box-shadow: none;
  color: @prompt_fg;
}}
.search-entry {{
  min-height: 22px;
  padding: 1px 6px;
  border-radius: 4px;
  background-color: @prompt_input_bg;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @prompt_input_fg;
}}
.search-count {{
  min-width: 48px;
  color: @prompt_fg;
  font-size: 11px;
}}
.search-close {{
  min-width: 22px;
  min-height: 22px;
  padding: 0;
  border-radius: 4px;
  background-color: transparent;
  background-image: none;
  border: none;
  box-shadow: none;
  color: @prompt_fg;
}}
.search-close:hover {{ background-color: @sidebar_hover_bg; }}
.sidebar-surface scrollbar slider {{
  min-width: 4px;
  min-height: 24px;
  background-color: @scrollbar_thumb_fg;
  background-image: none;
  border: none;
  box-shadow: none;
}}
"#,
            chrome_bg = self.background.css(),
            chrome_fg = self.foreground.css(),
            terminal_cursor = self.cursor.css(),
            chrome_separator = self.separator.css(),
            pane_separator = self.pane_separator.css(),
            sidebar_bg = self.sidebar_background.css(),
            selection_bg = self.chrome_selection_background.css(),
            sidebar_selected_bg = self.sidebar_selected_background.css(),
            sidebar_selected_fg = self.sidebar_selected_foreground.css(),
            sidebar_dim_fg = self.sidebar_dim_foreground.css(),
            sidebar_border = self.sidebar_border.css(),
            accent = self.accent.css(),
            tab_bar_bg = self.tab_bar_background.css(),
            tab_fg = self.tab_foreground.css(),
            tab_active_bg = self.tab_active_background.css(),
            tab_active_fg = self.tab_active_foreground.css(),
            tab_active_unfocused_bg = self.tab_active_unfocused_background.css(),
            tab_active_unfocused_fg = self.tab_active_unfocused_foreground.css(),
            border_fg = self.border_foreground.css(),
            border_active_fg = self.border_active_foreground.css(),
            menu_bg = self.menu_background.css(),
            menu_fg = self.menu_foreground.css(),
            toast_bg = self.toast_background.css(),
            toast_fg = self.toast_foreground.css(),
            scrollbar_thumb_fg = self.scrollbar_thumb_foreground.css(),
            scrollbar_thumb_active_fg = self.scrollbar_thumb_active_foreground.css(),
            prompt_bg = self.prompt_background.css(),
            prompt_fg = self.prompt_foreground.css(),
            prompt_border = self.prompt_border.css(),
            prompt_input_bg = self.prompt_input_background.css(),
            prompt_input_fg = self.prompt_input_foreground.css(),
            sidebar_hover_bg = hover,
        )
    }
}

impl Rgba {
    fn css(self) -> String {
        format!(
            "rgba({}, {}, {}, {:.2})",
            self.color.0, self.color.1, self.color.2, self.alpha
        )
    }
}

pub fn is_light_background(background: Rgb) -> bool {
    0.2126 * f64::from(background.0)
        + 0.7152 * f64::from(background.1)
        + 0.0722 * f64::from(background.2)
        > 128.0
}

pub fn separator_color(background: Rgb) -> Rgba {
    let luminance = 0.299 * f64::from(background.0) / 255.0
        + 0.587 * f64::from(background.1) / 255.0
        + 0.114 * f64::from(background.2) / 255.0;
    let (amount, alpha) = if luminance > 0.5 {
        (-0.12, 0.26)
    } else {
        (0.16, 0.36)
    };
    let offset =
        |channel: u8| ((f64::from(channel) / 255.0 + amount).clamp(0.0, 1.0) * 255.0).ceil() as u8;
    Rgba {
        color: Rgb(
            offset(background.0),
            offset(background.1),
            offset(background.2),
        ),
        alpha,
    }
}

/// Approximates the macOS sidebar material over a given window base.
///
/// Darkening reads as depth on a dark base, but the same multiplier on a light
/// one turns a warm background muddy grey; upstream's light window shows the
/// sidebar sharing the terminal's colour exactly. So a light base is carried
/// through unchanged and only a dark base is deepened.
pub fn sidebar_background(background: Rgb) -> Rgb {
    if is_light_background(background) {
        return background;
    }
    let channel = |value: u8| (f64::from(value) * 0.82).floor() as u8;
    Rgb(
        channel(background.0),
        channel(background.1),
        channel(background.2),
    )
}

pub const WORKSPACE_COLOR_PALETTE: [(&str, Rgb); 16] = [
    ("Red", Rgb(0xc0, 0x39, 0x2b)),
    ("Crimson", Rgb(0x92, 0x2b, 0x21)),
    ("Orange", Rgb(0xa0, 0x40, 0x00)),
    ("Amber", Rgb(0x7d, 0x66, 0x08)),
    ("Olive", Rgb(0x4a, 0x5c, 0x18)),
    ("Green", Rgb(0x19, 0x6f, 0x3d)),
    ("Teal", Rgb(0x00, 0x6b, 0x6b)),
    ("Aqua", Rgb(0x0e, 0x6b, 0x8c)),
    ("Blue", Rgb(0x15, 0x65, 0xc0)),
    ("Navy", Rgb(0x1a, 0x52, 0x76)),
    ("Indigo", Rgb(0x28, 0x35, 0x93)),
    ("Purple", Rgb(0x6a, 0x1b, 0x9a)),
    ("Magenta", Rgb(0xad, 0x14, 0x57)),
    ("Rose", Rgb(0x88, 0x0e, 0x4f)),
    ("Brown", Rgb(0x7b, 0x3f, 0x00)),
    ("Charcoal", Rgb(0x3e, 0x4b, 0x5e)),
];

pub fn workspace_palette_color(name: &str) -> Option<Rgb> {
    WORKSPACE_COLOR_PALETTE
        .iter()
        .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
        .map(|(_, color)| *color)
}

pub fn workspace_display_color(color: Rgb, is_light: bool) -> Rgb {
    if is_light {
        return color;
    }
    let (hue, saturation, brightness) = rgb_to_hsv(color);
    if saturation <= 0.08 {
        return color;
    }
    let boosted = (brightness.max(0.62) + (1.0 - brightness) * 0.28).min(1.0);
    hsv_to_rgb(hue, saturation, boosted)
}

fn rgb_to_hsv(color: Rgb) -> (f64, f64, f64) {
    let (red, green, blue) = color.cairo();
    let maximum = red.max(green).max(blue);
    let minimum = red.min(green).min(blue);
    let delta = maximum - minimum;
    let saturation = if maximum == 0.0 { 0.0 } else { delta / maximum };
    let hue = if delta == 0.0 {
        0.0
    } else if maximum == red {
        ((green - blue) / delta).rem_euclid(6.0) / 6.0
    } else if maximum == green {
        ((blue - red) / delta + 2.0) / 6.0
    } else {
        ((red - green) / delta + 4.0) / 6.0
    };
    (hue, saturation, maximum)
}

fn hsv_to_rgb(hue: f64, saturation: f64, brightness: f64) -> Rgb {
    let scaled = hue * 6.0;
    let chroma = brightness * saturation;
    let intermediate = chroma * (1.0 - (scaled.rem_euclid(2.0) - 1.0).abs());
    let (red, green, blue) = match scaled.floor() as u8 % 6 {
        0 => (chroma, intermediate, 0.0),
        1 => (intermediate, chroma, 0.0),
        2 => (0.0, chroma, intermediate),
        3 => (0.0, intermediate, chroma),
        4 => (intermediate, 0.0, chroma),
        _ => (chroma, 0.0, intermediate),
    };
    let match_value = brightness - chroma;
    let channel = |value: f64| ((value + match_value) * 255.0).round() as u8;
    Rgb(channel(red), channel(green), channel(blue))
}

#[derive(Default)]
struct Environment {
    tui_config: Option<PathBuf>,
    mux_config: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    home: Option<PathBuf>,
    font: Option<String>,
}

impl Environment {
    fn current() -> Self {
        Self {
            tui_config: env_path("CMUX_TUI_CONFIG"),
            mux_config: env_path("CMUX_MUX_CONFIG"),
            xdg_config_home: env_path("XDG_CONFIG_HOME"),
            home: env_path("HOME"),
            font: std::env::var("CMUX_GTK_FONT")
                .ok()
                .filter(|font| !font.trim().is_empty()),
        }
    }

    fn config_dir(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .as_ref()
            .map(|path| path.join("cmux"))
            .or_else(|| self.home.as_ref().map(|path| path.join(".config/cmux")))
    }
}

pub fn load() -> Settings {
    let environment = Environment::current();
    let mut settings = Settings::default();

    if let Some(path) = tui_config_path(&environment, Path::exists) {
        if let Some(document) = read_json(&path) {
            apply_tui_theme(&mut settings, &document);
        }
    }
    if let Some(path) = gtk_config_path(&environment) {
        if let Some(document) = read_json(&path) {
            apply_gtk_config(&mut settings, &document);
        }
    }
    if let Some(font) = environment.font {
        settings.font = font;
    }
    settings
}

fn read_json(path: &Path) -> Option<Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn apply_tui_theme(settings: &mut Settings, document: &Value) {
    let Some(theme) = document.get("theme").and_then(Value::as_object) else {
        return;
    };
    if let Some(mode) = theme.get("chrome").and_then(Value::as_str) {
        settings.chrome = match mode {
            "light" => ChromeMode::Light,
            "dark" => ChromeMode::Dark,
            "auto" => ChromeMode::Auto,
            _ => settings.chrome,
        };
    }
    apply_optional_color(
        theme.get("border_active"),
        &mut settings.theme.border_active,
    );
    apply_optional_color(
        theme.get("border_inactive"),
        &mut settings.theme.border_inactive,
    );
    apply_optional_color(
        theme.get("selection_background"),
        &mut settings.theme.selection_background,
    );
    if let Some(value) = theme.get("selection_foreground") {
        if value.is_null() {
            settings.theme.selection_foreground = Some(None);
        } else if let Some(color) = parse_color(value) {
            settings.theme.selection_foreground = Some(Some(color));
        }
    }
    apply_optional_color(theme.get("sidebar_rail"), &mut settings.theme.sidebar_rail);
    apply_optional_color(
        theme.get("sidebar_active_bg"),
        &mut settings.theme.sidebar_selected_background,
    );
    apply_optional_color(theme.get("tab_bg"), &mut settings.theme.tab_bar_background);
    apply_optional_color(
        theme.get("tab_active_bg"),
        &mut settings.theme.tab_active_background,
    );
    apply_optional_color(
        theme.get("scrollbar_thumb_fg"),
        &mut settings.theme.scrollbar_thumb,
    );
    apply_optional_color(
        theme.get("scrollbar_thumb_active_fg"),
        &mut settings.theme.scrollbar_thumb_active,
    );
    apply_optional_color(
        theme.get("prompt_bg"),
        &mut settings.theme.prompt_background,
    );
    apply_optional_color(
        theme.get("prompt_fg"),
        &mut settings.theme.prompt_foreground,
    );
    apply_optional_color(
        theme.get("prompt_border"),
        &mut settings.theme.prompt_border,
    );
    apply_optional_color(
        theme.get("prompt_input_bg"),
        &mut settings.theme.prompt_input_background,
    );
    apply_optional_color(
        theme.get("prompt_input_fg"),
        &mut settings.theme.prompt_input_foreground,
    );
    apply_optional_color(
        theme.get("notification_info"),
        &mut settings.theme.notification_info,
    );
    apply_optional_color(
        theme.get("notification_warning"),
        &mut settings.theme.notification_warning,
    );
    apply_optional_color(
        theme.get("notification_error"),
        &mut settings.theme.notification_error,
    );
}

pub const MIN_LINE_HEIGHT: f64 = 0.8;
pub const MAX_LINE_HEIGHT: f64 = 3.0;
pub const MAX_LETTER_SPACING: f64 = 8.0;
pub const MAX_PADDING: f64 = 64.0;

fn apply_gtk_config(settings: &mut Settings, document: &Value) {
    if let Some(font) = document
        .get("font")
        .and_then(Value::as_str)
        .filter(|font| !font.trim().is_empty())
    {
        settings.font = font.to_string();
    }
    if let Some(terminal) = document.get("terminal").and_then(Value::as_object) {
        apply_terminal_appearance(&mut settings.terminal, terminal);
    }
}

fn apply_terminal_appearance(
    appearance: &mut TerminalAppearance,
    terminal: &serde_json::Map<String, Value>,
) {
    apply_optional_color(terminal.get("foreground"), &mut appearance.foreground);
    apply_optional_color(terminal.get("background"), &mut appearance.background);
    apply_optional_color(terminal.get("cursor"), &mut appearance.cursor);
    if let Some(shape) = terminal.get("cursor_shape").and_then(Value::as_str) {
        appearance.cursor_shape = match shape {
            "block" => Some(CursorShape::Block),
            "bar" | "beam" => Some(CursorShape::Bar),
            "underline" => Some(CursorShape::Underline),
            _ => appearance.cursor_shape,
        };
    }
    if let Some(blink) = terminal.get("cursor_blink").and_then(Value::as_bool) {
        appearance.cursor_blink = Some(blink);
    }
    // A 16-entry array is the compact form; named keys stay available for
    // setting one slot without restating the rest.
    if let Some(entries) = terminal.get("palette").and_then(Value::as_array) {
        for (slot, value) in entries.iter().take(16).enumerate() {
            if let Some(color) = parse_color(value) {
                appearance.palette[slot] = Some(color);
            }
        }
    }
    if let Some(named) = terminal.get("palette").and_then(Value::as_object) {
        for (name, value) in named {
            if let (Some(slot), Some(color)) = (palette_slot(name), parse_color(value)) {
                appearance.palette[slot] = Some(color);
            }
        }
    }
    appearance.line_height = clamped_number(
        terminal.get("line_height"),
        MIN_LINE_HEIGHT,
        MAX_LINE_HEIGHT,
    )
    .or(appearance.line_height);
    appearance.letter_spacing =
        clamped_number(terminal.get("letter_spacing"), 0.0, MAX_LETTER_SPACING)
            .or(appearance.letter_spacing);
    appearance.padding =
        clamped_number(terminal.get("padding"), 0.0, MAX_PADDING).or(appearance.padding);
}

/// Maps an ANSI colour name to its palette slot. Both the plain and `bright_`
/// forms are accepted, matching how terminal configurations usually spell them.
fn palette_slot(name: &str) -> Option<usize> {
    const BASE: [&str; 8] = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if let Some(rest) = name.strip_prefix("bright_") {
        return BASE.iter().position(|base| *base == rest).map(|i| i + 8);
    }
    if let Some(index) = name.strip_prefix("color").and_then(|i| i.parse().ok()) {
        return (index < 16usize).then_some(index);
    }
    BASE.iter().position(|base| *base == name)
}

/// Reads a finite number and clamps it, so a malformed or absurd value cannot
/// produce an unusable grid.
fn clamped_number(value: Option<&Value>, min: f64, max: f64) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite())
        .map(|number| number.clamp(min, max))
}

fn apply_optional_color(value: Option<&Value>, target: &mut Option<Rgb>) {
    if let Some(color) = value.and_then(parse_color) {
        *target = Some(color);
    }
}

fn parse_color(value: &Value) -> Option<Rgb> {
    match value {
        Value::Number(index) => u8::try_from(index.as_u64()?).ok().map(xterm_color),
        Value::String(value) => parse_color_string(value),
        _ => None,
    }
}

fn parse_color_string(value: &str) -> Option<Rgb> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if !hex.is_ascii() {
            return None;
        }
        return match hex.len() {
            3 => {
                let mut digits = hex.chars().map(|digit| digit.to_digit(16).map(|n| n as u8));
                let red = digits.next()??;
                let green = digits.next()??;
                let blue = digits.next()??;
                Some(Rgb(red * 17, green * 17, blue * 17))
            }
            6 => Some(Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            )),
            _ => None,
        };
    }
    value.parse::<u8>().ok().map(xterm_color)
}

fn xterm_color(index: u8) -> Rgb {
    const ANSI: [Rgb; 16] = [
        Rgb(0, 0, 0),
        Rgb(128, 0, 0),
        Rgb(0, 128, 0),
        Rgb(128, 128, 0),
        Rgb(0, 0, 128),
        Rgb(128, 0, 128),
        Rgb(0, 128, 128),
        Rgb(192, 192, 192),
        Rgb(128, 128, 128),
        Rgb(255, 0, 0),
        Rgb(0, 255, 0),
        Rgb(255, 255, 0),
        Rgb(0, 0, 255),
        Rgb(255, 0, 255),
        Rgb(0, 255, 255),
        Rgb(255, 255, 255),
    ];
    const CUBE: [u8; 6] = [0, 95, 135, 175, 215, 255];

    match index {
        0..=15 => ANSI[usize::from(index)],
        16..=231 => {
            let offset = index - 16;
            Rgb(
                CUBE[usize::from(offset / 36)],
                CUBE[usize::from((offset % 36) / 6)],
                CUBE[usize::from(offset % 6)],
            )
        }
        232..=255 => {
            let level = 8 + (index - 232) * 10;
            Rgb(level, level, level)
        }
    }
}

fn tui_config_path(environment: &Environment, exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    if let Some(path) = environment
        .tui_config
        .as_ref()
        .or(environment.mux_config.as_ref())
    {
        return Some(path.clone());
    }
    let directory = environment.config_dir()?;
    let preferred = directory.join("cmux-tui.json");
    if exists(&preferred) {
        return Some(preferred);
    }
    let legacy = directory.join("mux.json");
    Some(if exists(&legacy) { legacy } else { preferred })
}

fn gtk_config_path(environment: &Environment) -> Option<PathBuf> {
    environment
        .config_dir()
        .map(|directory| directory.join("cmux-gtk.json"))
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).filter(nonempty).map(PathBuf::from)
}

fn nonempty(value: &OsString) -> bool {
    !value.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_xterm_ansi_cube_and_grayscale_colors() {
        assert_eq!(xterm_color(1), Rgb(128, 0, 0));
        assert_eq!(xterm_color(16), Rgb(0, 0, 0));
        assert_eq!(xterm_color(110), Rgb(135, 175, 215));
        assert_eq!(xterm_color(231), Rgb(255, 255, 255));
        assert_eq!(xterm_color(232), Rgb(8, 8, 8));
        assert_eq!(xterm_color(255), Rgb(238, 238, 238));
        assert_eq!(
            parse_color(&Value::String("#3af".into())),
            Some(Rgb(51, 170, 255))
        );
        assert_eq!(
            parse_color(&Value::String("#123456".into())),
            Some(Rgb(18, 52, 86))
        );
        assert_eq!(
            parse_color(&Value::String("110".into())),
            Some(Rgb(135, 175, 215))
        );
    }

    #[test]
    fn resolves_tui_config_in_documented_order() {
        let environment = Environment {
            tui_config: Some("/explicit/tui.json".into()),
            mux_config: Some("/explicit/mux.json".into()),
            xdg_config_home: Some("/xdg".into()),
            home: Some("/home/user".into()),
            font: None,
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/explicit/tui.json".into())
        );

        let environment = Environment {
            tui_config: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/explicit/mux.json".into())
        );

        let environment = Environment {
            mux_config: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |path| path.ends_with("mux.json")),
            Some("/xdg/cmux/mux.json".into())
        );
        assert_eq!(
            gtk_config_path(&environment),
            Some("/xdg/cmux/cmux-gtk.json".into())
        );

        let environment = Environment {
            xdg_config_home: None,
            ..environment
        };
        assert_eq!(
            tui_config_path(&environment, |_| false),
            Some("/home/user/.config/cmux/cmux-tui.json".into())
        );
    }

    #[test]
    fn ignores_unknown_and_invalid_theme_values() {
        let mut settings = Settings::default();
        let document = serde_json::json!({
            "theme": {
                "border_active": "#abc",
                "border_inactive": 999,
                "selection_background": false,
                "selection_foreground": "196",
                "notification_info": 110,
                "notification_warning": "179",
                "notification_error": false,
                "future_tui_key": {"anything": true}
            },
            "unknown_section": [1, 2, 3]
        });
        apply_tui_theme(&mut settings, &document);
        assert_eq!(settings.theme.border_active, Some(Rgb(170, 187, 204)));
        assert_eq!(settings.theme.border_inactive, None);
        assert_eq!(settings.theme.selection_background, None);
        assert_eq!(
            settings.theme.selection_foreground,
            Some(Some(Rgb(255, 0, 0)))
        );
        assert_eq!(
            settings.theme.notification_info,
            Some(DEFAULT_NOTIFICATION_INFO)
        );
        assert_eq!(
            settings.theme.notification_warning,
            Some(DEFAULT_NOTIFICATION_WARNING)
        );
        assert_eq!(settings.theme.notification_error, None);
    }

    #[test]
    fn derives_separator_sidebar_and_light_mode_from_terminal_background() {
        let dark_separator = separator_color(DEFAULT_DARK_BACKGROUND);
        assert_eq!(dark_separator.color, Rgb(0x47, 0x47, 0x47));
        assert_eq!(dark_separator.alpha, 0.36);
        assert_eq!(
            sidebar_background(DEFAULT_DARK_BACKGROUND),
            Rgb(0x18, 0x18, 0x18)
        );
        assert!(!is_light_background(DEFAULT_DARK_BACKGROUND));

        let light_separator = separator_color(DEFAULT_LIGHT_BACKGROUND);
        assert_eq!(light_separator.color, Rgb(0xe0, 0xe1, 0xe1));
        assert_eq!(light_separator.alpha, 0.26);
        // A light base keeps its colour: upstream's light window shows the
        // sidebar sharing the terminal background exactly, and darkening a warm
        // light colour reads as muddy grey rather than as depth.
        assert_eq!(
            sidebar_background(DEFAULT_LIGHT_BACKGROUND),
            DEFAULT_LIGHT_BACKGROUND
        );
        assert_eq!(
            sidebar_background(DEFAULT_DARK_BACKGROUND),
            Rgb(0x18, 0x18, 0x18)
        );
        assert!(is_light_background(DEFAULT_LIGHT_BACKGROUND));
    }

    #[test]
    fn chrome_colors_follow_mode_and_preserve_explicit_tui_overrides() {
        let overrides = ThemeOverrides {
            border_active: Some(Rgb(1, 2, 3)),
            border_inactive: Some(Rgb(4, 5, 6)),
            tab_active_background: Some(Rgb(7, 8, 9)),
            ..ThemeOverrides::default()
        };
        let colors = ChromeColors::derive(DEFAULT_DARK_BACKGROUND, ChromeMode::Light, overrides);
        assert!(colors.is_light);
        assert_eq!(colors.background, DEFAULT_LIGHT_BACKGROUND);
        assert_eq!(colors.foreground, Rgb(0x00, 0x00, 0x00));
        assert_eq!(colors.border_active_foreground, Rgb(1, 2, 3));
        assert!(colors.draw_active_border);
        assert_eq!(colors.pane_separator.color, Rgb(4, 5, 6));
        assert_eq!(colors.pane_separator.alpha, 1.0);
        assert_eq!(colors.tab_active_background, Rgb(7, 8, 9));
        assert_eq!(colors.tab_active_unfocused_background, Rgb(7, 8, 9));
        assert_eq!(colors.notification_info, DEFAULT_NOTIFICATION_INFO);
        assert_eq!(colors.notification_warning, DEFAULT_NOTIFICATION_WARNING);
        assert_eq!(colors.notification_error, DEFAULT_NOTIFICATION_ERROR);
    }

    #[test]
    fn explicit_chrome_mode_rebases_only_when_terminal_luminance_disagrees() {
        let light_on_dark = ChromeColors::derive(
            Rgb(0x05, 0x06, 0x07),
            ChromeMode::Light,
            ThemeOverrides::default(),
        );
        assert_eq!(light_on_dark.background, DEFAULT_LIGHT_BACKGROUND);
        assert_eq!(
            light_on_dark.sidebar_background,
            sidebar_background(DEFAULT_LIGHT_BACKGROUND)
        );
        assert_eq!(
            light_on_dark.separator,
            separator_color(DEFAULT_LIGHT_BACKGROUND)
        );
        assert_eq!(light_on_dark.pane_separator, light_on_dark.separator);

        let dark_on_light = ChromeColors::derive(
            Rgb(0xfa, 0xfb, 0xfc),
            ChromeMode::Dark,
            ThemeOverrides::default(),
        );
        assert_eq!(dark_on_light.background, DEFAULT_DARK_BACKGROUND);
        assert_eq!(dark_on_light.foreground, Rgb(0xff, 0xff, 0xff));
        assert_eq!(
            dark_on_light.sidebar_background,
            sidebar_background(DEFAULT_DARK_BACKGROUND)
        );

        let matching_light = Rgb(0xf0, 0xe8, 0xe0);
        let light =
            ChromeColors::derive(matching_light, ChromeMode::Light, ThemeOverrides::default());
        assert_eq!(light.background, matching_light);

        let auto_background = Rgb(0x05, 0x06, 0x07);
        let auto =
            ChromeColors::derive(auto_background, ChromeMode::Auto, ThemeOverrides::default());
        assert_eq!(auto.background, auto_background);
        assert_eq!(auto.sidebar_background, sidebar_background(auto_background));
    }

    #[test]
    fn flat_widget_css_clears_theme_paint_layers() {
        let css = ChromeColors::derive(
            DEFAULT_DARK_BACKGROUND,
            ChromeMode::Auto,
            ThemeOverrides::default(),
        )
        .css();
        let assert_flat = |selector: &str| {
            let rule = css
                .split_once(selector)
                .unwrap_or_else(|| panic!("missing CSS selector {selector}"))
                .1
                .split_once('}')
                .unwrap()
                .0;
            assert!(rule.contains("background-image: none;"), "{selector}");
            assert!(rule.contains("border: none;"), "{selector}");
            assert!(rule.contains("box-shadow: none;"), "{selector}");
        };

        for selector in [
            ".workspace-list {",
            ".workspace-row {",
            "popover.cmux-menu > contents {",
            "popover.cmux-menu modelbutton {",
            ".session-item,\n.session-new {",
            "popover.session-menu separator {",
            ".rename-entry {",
            ".toast {",
            ".search-bar {",
            ".search-entry {",
            ".search-close {",
            ".sidebar-surface scrollbar slider {",
        ] {
            assert_flat(selector);
        }
    }

    #[test]
    fn workspace_palette_and_dark_brightness_boost_match_the_spec() {
        assert_eq!(WORKSPACE_COLOR_PALETTE.len(), 16);
        assert_eq!(workspace_palette_color("blue"), Some(Rgb(0x15, 0x65, 0xc0)));
        assert_eq!(
            workspace_display_color(Rgb(0x80, 0x80, 0x80), false),
            Rgb(0x80, 0x80, 0x80)
        );

        let base = Rgb(0xc0, 0x39, 0x2b);
        let boosted = workspace_display_color(base, false);
        let (_, saturation, brightness) = rgb_to_hsv(base);
        let (_, boosted_saturation, boosted_brightness) = rgb_to_hsv(boosted);
        let expected = (brightness.max(0.62) + (1.0 - brightness) * 0.28).min(1.0);
        assert!((boosted_brightness - expected).abs() < 0.005);
        assert!((boosted_saturation - saturation).abs() < 0.005);
        assert_eq!(workspace_display_color(base, true), base);
    }

    #[test]
    fn terminal_appearance_reads_colors_palette_and_metrics() {
        let document = serde_json::json!({
            "font": "monospace 12",
            "terminal": {
                "foreground": "#161107",
                "background": "#f6f1e5",
                "cursor": 110,
                "cursor_shape": "bar",
                "cursor_blink": false,
                "palette": {"red": "#cc0000", "bright_blue": "#7aa6da", "color7": "#c5c8c6"},
                "line_height": 1.4,
                "letter_spacing": 0.5,
                "padding": 8
            }
        });
        let mut settings = Settings::default();
        apply_gtk_config(&mut settings, &document);

        assert_eq!(settings.font, "monospace 12");
        let terminal = &settings.terminal;
        assert_eq!(terminal.foreground, Some(Rgb(0x16, 0x11, 0x07)));
        assert_eq!(terminal.background, Some(Rgb(0xf6, 0xf1, 0xe5)));
        assert_eq!(terminal.cursor, Some(xterm_color(110)));
        assert_eq!(terminal.cursor_shape, Some(CursorShape::Bar));
        assert_eq!(terminal.cursor_blink, Some(false));
        assert_eq!(terminal.palette[1], Some(Rgb(0xcc, 0x00, 0x00)));
        assert_eq!(terminal.palette[12], Some(Rgb(0x7a, 0xa6, 0xda)));
        assert_eq!(terminal.palette[7], Some(Rgb(0xc5, 0xc8, 0xc6)));
        assert_eq!(terminal.palette[0], None);
        assert_eq!(terminal.line_height, Some(1.4));
        assert_eq!(terminal.letter_spacing, Some(0.5));
        assert_eq!(terminal.padding, Some(8.0));
    }

    #[test]
    fn terminal_palette_also_accepts_an_ordered_array() {
        let document = serde_json::json!({
            "terminal": {"palette": ["#000000", "#111111", "#222222"]}
        });
        let mut settings = Settings::default();
        apply_gtk_config(&mut settings, &document);

        assert_eq!(settings.terminal.palette[0], Some(Rgb(0, 0, 0)));
        assert_eq!(settings.terminal.palette[2], Some(Rgb(0x22, 0x22, 0x22)));
        assert_eq!(settings.terminal.palette[3], None);
    }

    #[test]
    fn terminal_metrics_are_clamped_and_bad_values_ignored() {
        let document = serde_json::json!({
            "terminal": {
                "line_height": 99.0,
                "letter_spacing": -5.0,
                "padding": 10_000,
                "cursor_shape": "spiral",
                "foreground": "not a colour"
            }
        });
        let mut settings = Settings::default();
        apply_gtk_config(&mut settings, &document);

        assert_eq!(settings.terminal.line_height, Some(MAX_LINE_HEIGHT));
        assert_eq!(settings.terminal.letter_spacing, Some(0.0));
        assert_eq!(settings.terminal.padding, Some(MAX_PADDING));
        // An unknown shape and an unparseable colour leave the defaults intact
        // rather than failing the whole configuration.
        assert_eq!(settings.terminal.cursor_shape, None);
        assert_eq!(settings.terminal.foreground, None);
    }

    #[test]
    fn light_chrome_matches_the_sampled_upstream_selection() {
        let colors = ChromeColors::derive(
            Rgb(0xf6, 0xf1, 0xe5),
            ChromeMode::Light,
            ThemeOverrides::default(),
        );
        assert_eq!(colors.sidebar_selected_background, Rgb(0x00, 0x88, 0xff));
        assert_eq!(
            colors.sidebar_selected_foreground,
            UPSTREAM_LIGHT_SELECTED_FOREGROUND
        );
        assert_eq!(colors.sidebar_dim_foreground, UPSTREAM_LIGHT_DIM_FOREGROUND);
    }
}
