//! Cell-grid rendering, pane layout, and keyboard input.
//!
//! Drawing is deliberately literal: the server already resolved palettes, OSC
//! overrides and inverse-video source colours. This module only places the
//! server's layout rectangles and stamps its styled runs inside them.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use cmux::{
    AgentState, ColorHex, LayoutDirection, LayoutNode, LayoutViewport, NotificationLevel, PaneId,
    RenderCursorStyle, RenderGraphicImage, RenderGraphicPlacement, RenderRow, RenderRun,
    RenderUnderline, ScreenId, Size, SplitId, TabId, TerminalId,
};
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{gdk, gdk_pixbuf, DrawingArea};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use crate::attention::{self, AttentionIndicator, AttentionState};
use crate::config::{
    ChromeColors, ChromeMode, CursorShape, Rgb, Settings, TerminalAppearance, ThemeOverrides,
    DEFAULT_DARK_BACKGROUND, DEFAULT_LIGHT_BACKGROUND, MAX_LETTER_SPACING, MAX_LINE_HEIGHT,
    MIN_LINE_HEIGHT,
};
use crate::screen::{PaneView, Screen, ScreenSet, TabContent, WorkspaceView};
use crate::search::{self, SearchUiState};
use crate::session::TerminalStatus;

const TAB_HEIGHT: f64 = 28.0;
const TAB_FADE_WIDTH: f64 = 100.0;
const SCREEN_ACTION_WIDTH: f64 = 28.0;
const SCREEN_CLOSE_SIZE: f64 = 20.0;
const ATTENTION_SLOT_SIZE: f64 = 11.0;
const NOTIFICATION_MARKER_SIZE: f64 = 11.0;
const SIDEBAR_SCRIM_HEIGHT: f64 = 50.0;
const UNFOCUSED_PANE_OPACITY: f64 = 0.70;
const DIVIDER_HIT_SIZE: f64 = 6.0;
const DIVIDER_THROTTLE_INTERVAL: Duration = Duration::from_millis(45);
const SCROLLBAR_WIDTH: f64 = 4.0;
const SCROLLBAR_ACTIVE_WIDTH: f64 = 7.0;
const SCROLLBAR_EDGE_INSET: f64 = 3.0;
const SCROLLBAR_TRACK_INSET: f64 = 3.0;
const SCROLLBAR_MIN_THUMB: f64 = 24.0;
const SCROLLBAR_HIT_WIDTH: f64 = 12.0;
const DEFAULT_FONT_SIZE: f64 = 11.0;
pub const MIN_FONT_SIZE: f64 = 6.0;
pub const MAX_FONT_SIZE: f64 = 32.0;
const FONT_ZOOM_STEP: f64 = 1.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlinkState {
    window_active: bool,
    phase_visible: bool,
    /// Monotonic tick count driving the activity spinner. Blink is a two-state
    /// toggle, so a spinner needs its own advancing phase.
    spin_phase: usize,
}

impl BlinkState {
    pub fn new(window_active: bool) -> Self {
        Self {
            window_active,
            phase_visible: true,
            spin_phase: 0,
        }
    }

    pub fn set_window_active(&mut self, active: bool) -> bool {
        let changed = self.window_active != active || !self.phase_visible;
        self.window_active = active;
        self.phase_visible = true;
        changed
    }

    pub fn tick(&mut self, has_blinking_content: bool) -> bool {
        // Spinners keep turning independently of blinking content, but only
        // while the window is focused: an unfocused window animating in the
        // background is the kind of motion the parity contract rules out.
        let spun = if self.window_active {
            self.spin_phase = self.spin_phase.wrapping_add(1);
            true
        } else {
            false
        };
        if self.window_active && has_blinking_content {
            self.phase_visible = !self.phase_visible;
            true
        } else if !self.phase_visible {
            self.phase_visible = true;
            true
        } else {
            spun
        }
    }

    pub fn spin_phase(self) -> usize {
        self.spin_phase
    }

    fn window_active(self) -> bool {
        self.window_active
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CursorPresentation {
    Hidden,
    Solid,
    Hollow,
}

fn cursor_presentation(
    style: RenderCursorStyle,
    cursor_blinks: bool,
    blink: BlinkState,
) -> CursorPresentation {
    if !blink.window_active {
        return if style == RenderCursorStyle::Block {
            CursorPresentation::Hollow
        } else {
            CursorPresentation::Solid
        };
    }
    if cursor_blinks && !blink.phase_visible {
        CursorPresentation::Hidden
    } else {
        CursorPresentation::Solid
    }
}

fn blinking_content_visible(blinks: bool, blink: BlinkState) -> bool {
    !blinks || !blink.window_active || blink.phase_visible
}

pub fn needs_blink(screens: &ScreenSet) -> bool {
    screens.grids.values().any(|screen| {
        screen
            .cursor
            .as_ref()
            .is_some_and(|cursor| cursor.visible && cursor.blink)
            || screen.rows.iter().flatten().any(|row| {
                row.runs.iter().any(|run| {
                    run.has_attr(RenderRun::ATTR_BLINK) && !run.has_attr(RenderRun::ATTR_INVISIBLE)
                })
            })
    })
}

#[derive(Debug, Default)]
pub struct MouseMoveThrottle {
    last: Option<(TerminalId, u16, u16)>,
}

#[derive(Debug, Default)]
pub struct TabStripState {
    hovered_screen: RefCell<Option<ScreenId>>,
    hovered_tab: RefCell<Option<(PaneId, TabId)>>,
}

#[derive(Debug, Default)]
pub struct ScrollbarState {
    hovered: RefCell<Option<TerminalId>>,
    dragging: RefCell<Option<TerminalId>>,
}

impl ScrollbarState {
    pub fn set_hovered(&self, terminal: Option<TerminalId>) -> bool {
        if *self.hovered.borrow() == terminal {
            return false;
        }
        *self.hovered.borrow_mut() = terminal;
        true
    }

    pub fn set_dragging(&self, terminal: Option<TerminalId>) -> bool {
        if *self.dragging.borrow() == terminal {
            return false;
        }
        *self.dragging.borrow_mut() = terminal;
        true
    }

    fn is_active(&self, terminal: &TerminalId) -> bool {
        self.hovered.borrow().as_ref() == Some(terminal)
            || self.dragging.borrow().as_ref() == Some(terminal)
    }
}

impl TabStripState {
    pub fn hovered_screen(&self) -> Option<ScreenId> {
        self.hovered_screen.borrow().clone()
    }

    pub fn set_hovered_screen(&self, screen: Option<ScreenId>) -> bool {
        if *self.hovered_screen.borrow() == screen {
            return false;
        }
        *self.hovered_screen.borrow_mut() = screen;
        true
    }

    pub fn hovered_tab(&self) -> Option<(PaneId, TabId)> {
        self.hovered_tab.borrow().clone()
    }

    pub fn set_hovered_tab(&self, tab: Option<(PaneId, TabId)>) -> bool {
        if *self.hovered_tab.borrow() == tab {
            return false;
        }
        *self.hovered_tab.borrow_mut() = tab;
        true
    }
}

impl MouseMoveThrottle {
    pub fn should_report(&mut self, terminal: &TerminalId, row: u16, column: u16) -> bool {
        let next = (terminal.clone(), row, column);
        if self.last.as_ref() == Some(&next) {
            return false;
        }
        self.last = Some(next);
        true
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[derive(Clone)]
pub struct ViewHandles {
    pub screens: Rc<RefCell<ScreenSet>>,
    pub attention: Rc<RefCell<AttentionState>>,
    pub terminal_statuses: Rc<RefCell<HashMap<TerminalId, TerminalStatus>>>,
    pub screen_terminals: Rc<RefCell<HashMap<ScreenId, Vec<TerminalId>>>>,
    pub theme: Rc<Theme>,
    pub blink: Rc<Cell<BlinkState>>,
    pub tab_strip: Rc<TabStripState>,
    pub scrollbar: Rc<ScrollbarState>,
    pub search_state: Rc<RefCell<SearchUiState>>,
    pub preedit: Rc<RefCell<Option<PreeditState>>>,
    pub im_cursor_location: Rc<Cell<Option<gdk::Rectangle>>>,
}

#[derive(Debug)]
pub struct PreeditState {
    text: String,
    _attributes: pango::AttrList,
    _cursor_offset: i32,
}

impl PreeditState {
    pub fn new(text: String, attributes: pango::AttrList, cursor_offset: i32) -> Self {
        Self {
            text,
            _attributes: attributes,
            _cursor_offset: cursor_offset,
        }
    }
}

pub struct Theme {
    font: RefCell<pango::FontDescription>,
    default_font: pango::FontDescription,
    default_font_size: f64,
    chrome_mode: ChromeMode,
    overrides: ThemeOverrides,
    terminal: TerminalAppearance,
    chrome: RefCell<ChromeColors>,
}

impl Theme {
    pub fn new(settings: Settings) -> Self {
        let fallback = match settings.chrome {
            ChromeMode::Light => DEFAULT_LIGHT_BACKGROUND,
            ChromeMode::Auto | ChromeMode::Dark => DEFAULT_DARK_BACKGROUND,
        };
        let mut font = pango::FontDescription::from_string(&settings.font);
        let default_font_size = clamp_font_size(font_description_size(&font));
        set_description_size(&mut font, default_font_size);
        // A configured terminal background is also the chrome's derivation base,
        // so the window agrees with the grid instead of deriving from a colour
        // the user just overrode.
        let base = settings.terminal.background.unwrap_or(fallback);
        Self {
            font: RefCell::new(font.clone()),
            default_font: font,
            default_font_size,
            chrome_mode: settings.chrome,
            overrides: settings.theme,
            terminal: settings.terminal,
            chrome: RefCell::new(ChromeColors::derive(base, settings.chrome, settings.theme)),
        }
    }

    pub fn terminal(&self) -> &TerminalAppearance {
        &self.terminal
    }

    /// Recomputes chrome for the terminal background that just arrived.
    ///
    /// A configured terminal background wins: the grid is already being drawn
    /// in it, so deriving chrome from the server's value instead would leave the
    /// window disagreeing with its own terminal.
    pub fn refresh_chrome(&self, background: Rgb) -> bool {
        let background = self.terminal.background.unwrap_or(background);
        let colors = ChromeColors::derive(background, self.chrome_mode, self.overrides);
        if *self.chrome.borrow() == colors {
            return false;
        }
        *self.chrome.borrow_mut() = colors;
        true
    }

    pub fn chrome(&self) -> ChromeColors {
        self.chrome.borrow().clone()
    }

    pub fn css(&self) -> String {
        self.chrome.borrow().css()
    }

    pub fn font(&self) -> pango::FontDescription {
        self.font.borrow().clone()
    }

    pub fn font_size(&self) -> f64 {
        font_description_size(&self.font.borrow())
    }

    pub fn font_percent(&self) -> u32 {
        (self.font_size() / self.default_font_size * 100.0).round() as u32
    }

    pub fn zoom_font(&self, direction: i32) -> bool {
        let next = adjusted_font_size(self.font_size(), f64::from(direction) * FONT_ZOOM_STEP);
        self.set_font_size(next)
    }

    pub fn reset_font(&self) -> bool {
        let current = self.font_size();
        if (current - self.default_font_size).abs() < f64::EPSILON {
            return false;
        }
        *self.font.borrow_mut() = self.default_font.clone();
        true
    }

    fn set_font_size(&self, size: f64) -> bool {
        let size = clamp_font_size(size);
        if (self.font_size() - size).abs() < f64::EPSILON {
            return false;
        }
        set_description_size(&mut self.font.borrow_mut(), size);
        true
    }
}

fn font_description_size(description: &pango::FontDescription) -> f64 {
    let size = f64::from(description.size()) / f64::from(pango::SCALE);
    if size.is_finite() && size > 0.0 {
        size
    } else {
        DEFAULT_FONT_SIZE
    }
}

fn set_description_size(description: &mut pango::FontDescription, size: f64) {
    if description.is_size_absolute() {
        description.set_absolute_size(size * f64::from(pango::SCALE));
    } else {
        description.set_size((size * f64::from(pango::SCALE)).round() as i32);
    }
}

pub fn clamp_font_size(size: f64) -> f64 {
    if !size.is_finite() {
        return DEFAULT_FONT_SIZE;
    }
    size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE)
}

pub fn adjusted_font_size(current: f64, delta: f64) -> f64 {
    clamp_font_size(current + delta)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CellMetrics {
    pub width: f64,
    pub height: f64,
    /// Distance from the top of a cell to the text baseline. A configured line
    /// height shifts this so glyphs stay centred in a taller cell.
    pub baseline: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollbarGeometry {
    pub track: Rect,
    pub thumb: Rect,
}

#[derive(Clone, Debug)]
pub struct ScrollbarHit {
    pub terminal: TerminalId,
    pub geometry: ScrollbarGeometry,
    pub scrollback_rows: u32,
}

impl Rect {
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.width && y >= self.y && y < self.y + self.height
    }

    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.width
            && self.x + self.width > other.x
            && self.y < other.y + other.height
            && self.y + self.height > other.y
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GraphicGeometry {
    pub destination: Rect,
    pub source: Rect,
}

type ImageCache = HashMap<(TerminalId, u32, u64), Option<gdk_pixbuf::Pixbuf>>;

#[derive(Clone, Debug)]
pub struct TabHit {
    pub id: TabId,
    pub rect: Rect,
    pub close_rect: Rect,
    pub terminal: Option<TerminalId>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenTabHit {
    pub id: ScreenId,
    pub rect: Rect,
    pub close_rect: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScreenBarGeometry {
    pub rect: Rect,
    pub tabs: Vec<ScreenTabHit>,
    pub new_button: Rect,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ScreenBarHit {
    Tab(ScreenId),
    Close(ScreenId),
    New,
}

#[derive(Clone, Debug)]
pub struct PaneGeometry {
    pub pane: PaneId,
    pub rect: Rect,
    pub content: Rect,
    pub terminal: Option<TerminalId>,
    pub tabs: Vec<TabHit>,
    pub new_tab_button: Option<Rect>,
    pub stack_header: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PaneBarHit {
    Tab(TabId),
    Close(TabId),
    New,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SplitDivider {
    pub split_id: SplitId,
    pub pane: PaneId,
    pub direction: LayoutDirection,
    pub split_rect: Rect,
    pub position: f64,
}

impl SplitDivider {
    fn hit_rect(&self) -> Rect {
        match self.direction {
            LayoutDirection::Horizontal => Rect {
                x: self.position - DIVIDER_HIT_SIZE / 2.0,
                y: self.split_rect.y,
                width: DIVIDER_HIT_SIZE,
                height: self.split_rect.height,
            },
            LayoutDirection::Vertical => Rect {
                x: self.split_rect.x,
                y: self.position - DIVIDER_HIT_SIZE / 2.0,
                width: self.split_rect.width,
                height: DIVIDER_HIT_SIZE,
            },
        }
    }

    fn distance_from_line(&self, x: f64, y: f64) -> f64 {
        match self.direction {
            LayoutDirection::Horizontal => (x - self.position).abs(),
            LayoutDirection::Vertical => (y - self.position).abs(),
        }
    }
}

#[derive(Debug, Default)]
pub struct DividerDragThrottle {
    last_sent: Option<(Duration, f64)>,
}

impl DividerDragThrottle {
    pub fn should_send(&mut self, now: Duration, ratio: f64, final_update: bool) -> bool {
        if final_update {
            self.last_sent = Some((now, ratio));
            return true;
        }
        if self.last_sent.is_some_and(|(_, sent)| sent == ratio) {
            return false;
        }
        if self
            .last_sent
            .is_some_and(|(sent_at, _)| now.saturating_sub(sent_at) < DIVIDER_THROTTLE_INTERVAL)
        {
            return false;
        }
        self.last_sent = Some((now, ratio));
        true
    }
}

/// Measures one cell from the font itself rather than assuming a size, so the
/// grid lines up with whatever monospace face the desktop resolves.
pub fn cell_metrics(widget: &impl IsA<gtk4::Widget>, theme: &Theme) -> CellMetrics {
    let context = widget.as_ref().pango_context();
    let font = theme.font();
    context.set_font_description(Some(&font));
    let metrics = context.metrics(Some(&font), None);
    let layout = widget.as_ref().create_pango_layout(Some("0"));
    layout.set_font_description(Some(&font));
    let width = f64::from(layout.extents().1.width()) / f64::from(pango::SCALE);
    let width = if width.is_finite() && width > 0.0 {
        width
    } else {
        let width = f64::from(metrics.approximate_char_width()) / f64::from(pango::SCALE);
        if width.is_finite() && width > 0.0 {
            width
        } else {
            f64::from(metrics.approximate_digit_width()) / f64::from(pango::SCALE)
        }
    };
    let ascent = f64::from(metrics.ascent()) / f64::from(pango::SCALE);
    let descent = f64::from(metrics.descent()) / f64::from(pango::SCALE);
    apply_cell_overrides(
        CellMetrics {
            width,
            height: ascent + descent,
            baseline: ascent,
        },
        theme.terminal(),
        widget.as_ref().scale_factor(),
    )
}

fn snap_to_pixel_grid(value: f64, scale_factor: i32) -> f64 {
    let scale = f64::from(scale_factor.max(1));
    (value * scale).round() / scale
}

/// Scales a cell to the configured line height and letter spacing. Extra height
/// is split above and below the glyphs so text stays optically centred rather
/// than riding the top of a taller cell.
fn apply_cell_overrides(
    metrics: CellMetrics,
    appearance: &TerminalAppearance,
    scale_factor: i32,
) -> CellMetrics {
    let scale = appearance
        .line_height
        .unwrap_or(1.0)
        .clamp(MIN_LINE_HEIGHT, MAX_LINE_HEIGHT);
    let spacing = appearance
        .letter_spacing
        .unwrap_or(0.0)
        .clamp(0.0, MAX_LETTER_SPACING);
    let device_scale = f64::from(scale_factor.max(1));
    let height = snap_to_pixel_grid(metrics.height * scale, scale_factor).max(1.0 / device_scale);
    CellMetrics {
        width: metrics.width + spacing,
        height,
        baseline: metrics.baseline + (height - metrics.height) / 2.0,
    }
}

pub fn pane_geometries(
    screens: &ScreenSet,
    metrics: CellMetrics,
    width: i32,
    height: i32,
) -> Vec<PaneGeometry> {
    let Some(workspace) = screens.workspace.as_ref() else {
        return Vec::new();
    };
    let area = workspace_content_rect(workspace.screen_tabs.len(), width, height);
    let mut panes = Vec::new();
    if let Some(zoomed) = workspace.layout.zoomed_pane_id.as_ref() {
        push_pane(workspace, zoomed, area, false, metrics, &mut panes);
    } else {
        walk_layout(&workspace.layout.root, area, workspace, metrics, &mut panes);
    }
    panes
}

fn workspace_content_rect(screen_count: usize, width: i32, height: i32) -> Rect {
    let screen_bar_height = if screen_bar_visible(screen_count) {
        TAB_HEIGHT.min(f64::from(height.max(0)))
    } else {
        0.0
    };
    Rect {
        x: 0.0,
        y: screen_bar_height,
        width: f64::from(width.max(0)),
        height: (f64::from(height.max(0)) - screen_bar_height).max(0.0),
    }
}

pub fn screen_bar_visible(screen_count: usize) -> bool {
    screen_count > 1
}

pub fn split_dividers(screens: &ScreenSet, width: i32, height: i32) -> Vec<SplitDivider> {
    let Some(workspace) = screens.workspace.as_ref() else {
        return Vec::new();
    };
    if workspace.layout.zoomed_pane_id.is_some() {
        return Vec::new();
    }
    let area = workspace_content_rect(workspace.screen_tabs.len(), width, height);
    let mut dividers = Vec::new();
    walk_dividers(
        &workspace.layout.root,
        area,
        &workspace.layout.active_pane_id,
        &mut dividers,
    );
    dividers
}

pub fn screen_bar_geometry(
    screens: &ScreenSet,
    width: i32,
    height: i32,
) -> Option<ScreenBarGeometry> {
    let workspace = screens.workspace.as_ref()?;
    if !screen_bar_visible(workspace.screen_tabs.len()) {
        return None;
    }
    let bar_height = TAB_HEIGHT.min(f64::from(height.max(0)));
    let width = f64::from(width.max(0));
    let action_width = SCREEN_ACTION_WIDTH.min(width);
    let tabs_width = (width - action_width).max(0.0);
    let tab_width = if workspace.screen_tabs.is_empty() {
        0.0
    } else {
        tabs_width / workspace.screen_tabs.len() as f64
    };
    let tabs = workspace
        .screen_tabs
        .iter()
        .enumerate()
        .map(|(index, screen)| {
            let rect = Rect {
                x: index as f64 * tab_width,
                y: 0.0,
                width: tab_width,
                height: bar_height,
            };
            let close_width = SCREEN_CLOSE_SIZE.min(rect.width);
            ScreenTabHit {
                id: screen.id.clone(),
                rect,
                close_rect: Rect {
                    x: rect.x + rect.width - close_width,
                    y: rect.y + (rect.height - SCREEN_CLOSE_SIZE).max(0.0) / 2.0,
                    width: close_width,
                    height: SCREEN_CLOSE_SIZE.min(rect.height),
                },
            }
        })
        .collect();
    Some(ScreenBarGeometry {
        rect: Rect {
            x: 0.0,
            y: 0.0,
            width,
            height: bar_height,
        },
        tabs,
        new_button: Rect {
            x: tabs_width,
            y: 0.0,
            width: action_width,
            height: bar_height,
        },
    })
}

pub fn screen_bar_hit(geometry: &ScreenBarGeometry, x: f64, y: f64) -> Option<ScreenBarHit> {
    if geometry.new_button.contains(x, y) {
        return Some(ScreenBarHit::New);
    }
    geometry.tabs.iter().find_map(|tab| {
        if tab.close_rect.contains(x, y) {
            Some(ScreenBarHit::Close(tab.id.clone()))
        } else if tab.rect.contains(x, y) {
            Some(ScreenBarHit::Tab(tab.id.clone()))
        } else {
            None
        }
    })
}

pub fn hovered_screen(geometry: &ScreenBarGeometry, x: f64, y: f64) -> Option<ScreenId> {
    geometry
        .tabs
        .iter()
        .find(|tab| tab.rect.contains(x, y))
        .map(|tab| tab.id.clone())
}

pub fn pane_bar_hit(geometry: &PaneGeometry, x: f64, y: f64) -> Option<PaneBarHit> {
    if geometry
        .new_tab_button
        .is_some_and(|button| button.contains(x, y))
    {
        return Some(PaneBarHit::New);
    }
    geometry.tabs.iter().find_map(|tab| {
        if tab.close_rect.contains(x, y) {
            Some(PaneBarHit::Close(tab.id.clone()))
        } else if tab.rect.contains(x, y) {
            Some(PaneBarHit::Tab(tab.id.clone()))
        } else {
            None
        }
    })
}

pub fn hovered_pane_tab(geometries: &[PaneGeometry], x: f64, y: f64) -> Option<(PaneId, TabId)> {
    geometries.iter().find_map(|pane| {
        pane.tabs
            .iter()
            .find(|tab| tab.rect.contains(x, y))
            .map(|tab| (pane.pane.clone(), tab.id.clone()))
    })
}

pub fn split_divider_at(dividers: &[SplitDivider], x: f64, y: f64) -> Option<&SplitDivider> {
    dividers
        .iter()
        .filter(|divider| divider.hit_rect().contains(x, y))
        .min_by(|left, right| {
            left.distance_from_line(x, y)
                .total_cmp(&right.distance_from_line(x, y))
        })
}

pub fn split_ratio_at(divider: &SplitDivider, x: f64, y: f64) -> Option<f64> {
    let (origin, extent, position) = match divider.direction {
        LayoutDirection::Horizontal => (divider.split_rect.x, divider.split_rect.width, x),
        LayoutDirection::Vertical => (divider.split_rect.y, divider.split_rect.height, y),
    };
    if !origin.is_finite() || !extent.is_finite() || !position.is_finite() || extent < 2.0 {
        return None;
    }
    let offset = (position - origin).round().clamp(1.0, extent - 1.0);
    Some(offset / extent)
}

/// Overlay scrollbar geometry from the server-owned viewport position.
pub fn scrollbar_geometry(
    content: Rect,
    scrollback_rows: u32,
    viewport_rows: u16,
    viewport_offset: u64,
    thumb_width: f64,
) -> Option<ScrollbarGeometry> {
    if scrollback_rows == 0 || viewport_rows == 0 || content.height <= 0.0 {
        return None;
    }
    let track = Rect {
        x: content.x,
        y: content.y + SCROLLBAR_TRACK_INSET,
        width: content.width,
        height: (content.height - 2.0 * SCROLLBAR_TRACK_INSET).max(0.0),
    };
    if track.height <= 0.0 || !track.height.is_finite() || !thumb_width.is_finite() {
        return None;
    }
    let total_rows = f64::from(scrollback_rows) + f64::from(viewport_rows);
    let thumb_height = (track.height * f64::from(viewport_rows) / total_rows)
        .max(SCROLLBAR_MIN_THUMB.min(track.height))
        .min(track.height);
    let travel = (track.height - thumb_height).max(0.0);
    let progress =
        viewport_offset.min(u64::from(scrollback_rows)) as f64 / f64::from(scrollback_rows);
    Some(ScrollbarGeometry {
        track,
        thumb: Rect {
            x: content.x + content.width - thumb_width - SCROLLBAR_EDGE_INSET,
            y: track.y + progress * travel,
            width: thumb_width,
            height: thumb_height,
        },
    })
}

/// Convert a thumb drag from pixels to the corresponding signed row delta.
pub fn scrollbar_drag_rows(
    delta_pixels: f64,
    scrollback_rows: u32,
    track_height: f64,
    thumb_height: f64,
) -> i32 {
    let travel = track_height - thumb_height;
    if !delta_pixels.is_finite() || !travel.is_finite() || travel <= 0.0 {
        return 0;
    }
    let rows = (delta_pixels / travel * f64::from(scrollback_rows)).round();
    rows.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
}

pub fn scrollbar_at(
    screens: &ScreenSet,
    metrics: CellMetrics,
    width: i32,
    height: i32,
    x: f64,
    y: f64,
) -> Option<ScrollbarHit> {
    pane_geometries(screens, metrics, width, height)
        .into_iter()
        .filter_map(|pane| {
            let terminal = pane.terminal?;
            let screen = screens.grids.get(&terminal)?;
            if screen.at_bottom {
                return None;
            }
            let geometry = scrollbar_geometry(
                pane.content,
                screen.scrollback_rows,
                screen.size.rows,
                screen.viewport_offset,
                SCROLLBAR_WIDTH,
            )?;
            let hit = Rect {
                x: pane.content.x + pane.content.width - SCROLLBAR_HIT_WIDTH,
                y: geometry.thumb.y,
                width: SCROLLBAR_HIT_WIDTH,
                height: geometry.thumb.height,
            };
            hit.contains(x, y).then_some(ScrollbarHit {
                terminal,
                geometry,
                scrollback_rows: screen.scrollback_rows,
            })
        })
        .next()
}

pub fn visible_terminal_sizes(
    screens: &ScreenSet,
    metrics: CellMetrics,
    width: i32,
    height: i32,
) -> Vec<(TerminalId, Size)> {
    let viewport = Rect {
        x: 0.0,
        y: 0.0,
        width: f64::from(width.max(0)),
        height: f64::from(height.max(0)),
    };
    let mut seen = HashSet::new();
    pane_geometries(screens, metrics, width, height)
        .into_iter()
        .filter(|pane| {
            !pane.stack_header
                && pane.content.intersects(viewport)
                && pane.content.width > 0.0
                && pane.content.height > 0.0
        })
        .filter_map(|pane| {
            let terminal = pane.terminal?;
            if !seen.insert(terminal.clone()) {
                return None;
            }
            let size = viewport_size(metrics, pane.content.width, pane.content.height);
            Some((terminal, size))
        })
        .collect()
}

fn walk_layout(
    node: &LayoutNode,
    rect: Rect,
    workspace: &WorkspaceView,
    metrics: CellMetrics,
    out: &mut Vec<PaneGeometry>,
) {
    match node {
        LayoutNode::Leaf(leaf) => {
            push_pane(workspace, &leaf.pane_id, rect, false, metrics, out);
        }
        LayoutNode::Split(split) => {
            let (first, second) = split_rect(rect, split.direction, split.ratio);
            walk_layout(&split.first, first, workspace, metrics, out);
            walk_layout(&split.second, second, workspace, metrics, out);
        }
        LayoutNode::Stack(stack) => {
            walk_stack(
                &stack.pane_ids,
                &stack.expanded_pane_id,
                rect,
                workspace,
                metrics,
                out,
            );
        }
        LayoutNode::Viewport(viewport) => {
            for (child, child_rect) in
                viewport_children(viewport, rect, &workspace.layout.active_pane_id)
            {
                walk_layout(child, child_rect, workspace, metrics, out);
            }
        }
    }
}

fn walk_dividers(node: &LayoutNode, rect: Rect, active_pane: &PaneId, out: &mut Vec<SplitDivider>) {
    match node {
        LayoutNode::Leaf(_) | LayoutNode::Stack(_) => {}
        LayoutNode::Split(split) => {
            let (first, second) = split_rect(rect, split.direction, split.ratio);
            let extent = match split.direction {
                LayoutDirection::Horizontal => rect.width,
                LayoutDirection::Vertical => rect.height,
            };
            if extent >= 2.0 {
                if let Some(pane) = first_pane_id(&split.first) {
                    let position = match split.direction {
                        LayoutDirection::Horizontal => second.x,
                        LayoutDirection::Vertical => second.y,
                    };
                    out.push(SplitDivider {
                        split_id: split.split_id.clone(),
                        pane: pane.clone(),
                        direction: split.direction,
                        split_rect: rect,
                        position,
                    });
                }
            }
            walk_dividers(&split.first, first, active_pane, out);
            walk_dividers(&split.second, second, active_pane, out);
        }
        LayoutNode::Viewport(viewport) => {
            for (child, child_rect) in viewport_children(viewport, rect, active_pane) {
                walk_dividers(child, child_rect, active_pane, out);
            }
        }
    }
}

fn viewport_children<'a>(
    viewport: &'a LayoutViewport,
    rect: Rect,
    active_pane: &PaneId,
) -> Vec<(&'a LayoutNode, Rect)> {
    let widths: Vec<f64> = viewport
        .columns
        .iter()
        .map(|column| (rect.width * column.width).max(1.0))
        .collect();
    let active_index = viewport
        .columns
        .iter()
        .position(|column| node_contains(&column.root, active_pane));
    let active_left = active_index
        .map(|index| widths.iter().take(index).sum::<f64>())
        .unwrap_or(0.0);
    let active_right = active_index
        .map(|index| active_left + widths[index])
        .unwrap_or(rect.width);
    let offset = (active_right - rect.width).max(0.0).min(active_left);
    let mut x = rect.x - offset;
    viewport
        .columns
        .iter()
        .zip(widths)
        .map(|(column, width)| {
            let child = (column.root.as_ref(), Rect { x, width, ..rect });
            x += width;
            child
        })
        .collect()
}

fn first_pane_id(node: &LayoutNode) -> Option<&PaneId> {
    match node {
        LayoutNode::Leaf(leaf) => Some(&leaf.pane_id),
        LayoutNode::Split(split) => {
            first_pane_id(&split.first).or_else(|| first_pane_id(&split.second))
        }
        LayoutNode::Stack(stack) => stack.pane_ids.first(),
        LayoutNode::Viewport(viewport) => viewport
            .columns
            .iter()
            .find_map(|column| first_pane_id(&column.root)),
    }
}

fn split_rect(rect: Rect, direction: LayoutDirection, ratio: f64) -> (Rect, Rect) {
    match direction {
        LayoutDirection::Horizontal => {
            if rect.width < 2.0 {
                return (
                    rect,
                    Rect {
                        width: 0.0,
                        height: 0.0,
                        ..rect
                    },
                );
            }
            let first_width = (rect.width * ratio).round().clamp(1.0, rect.width - 1.0);
            (
                Rect {
                    width: first_width,
                    ..rect
                },
                Rect {
                    x: rect.x + first_width,
                    width: rect.width - first_width,
                    ..rect
                },
            )
        }
        LayoutDirection::Vertical => {
            if rect.height < 2.0 {
                return (
                    rect,
                    Rect {
                        width: 0.0,
                        height: 0.0,
                        ..rect
                    },
                );
            }
            let first_height = (rect.height * ratio).round().clamp(1.0, rect.height - 1.0);
            (
                Rect {
                    height: first_height,
                    ..rect
                },
                Rect {
                    y: rect.y + first_height,
                    height: rect.height - first_height,
                    ..rect
                },
            )
        }
    }
}

fn walk_stack(
    pane_ids: &[PaneId],
    expanded: &PaneId,
    rect: Rect,
    workspace: &WorkspaceView,
    metrics: CellMetrics,
    out: &mut Vec<PaneGeometry>,
) {
    if pane_ids.is_empty() {
        return;
    }
    let expanded = if pane_ids.contains(&workspace.layout.active_pane_id) {
        &workspace.layout.active_pane_id
    } else {
        expanded
    };
    let expanded_index = pane_ids
        .iter()
        .position(|pane| pane == expanded)
        .unwrap_or(pane_ids.len() - 1);
    let header_height = TAB_HEIGHT;
    let visible_headers = ((rect.height / header_height).floor() as usize)
        .saturating_sub(1)
        .min(pane_ids.len() - 1);
    let mut before = 0usize;
    let mut after = 0usize;
    while before + after < visible_headers {
        let can_take_before = before < expanded_index;
        let can_take_after = after < pane_ids.len() - expanded_index - 1;
        if can_take_before && (!can_take_after || before <= after) {
            before += 1;
        } else if can_take_after {
            after += 1;
        } else {
            break;
        }
    }

    let expanded_height = (rect.height - (before + after) as f64 * header_height).max(0.0);
    let mut y = rect.y;
    for (index, pane) in pane_ids.iter().enumerate() {
        let height = if index == expanded_index {
            expanded_height
        } else if (index >= expanded_index - before && index < expanded_index)
            || (index > expanded_index && index <= expanded_index + after)
        {
            header_height
        } else {
            0.0
        };
        if height > 0.0 {
            push_pane(
                workspace,
                pane,
                Rect { y, height, ..rect },
                index != expanded_index,
                metrics,
                out,
            );
            y += height;
        }
    }
}

fn push_pane(
    workspace: &WorkspaceView,
    pane_id: &PaneId,
    rect: Rect,
    stack_header: bool,
    _metrics: CellMetrics,
    out: &mut Vec<PaneGeometry>,
) {
    let Some(pane) = workspace.pane(pane_id) else {
        return;
    };
    let tab_height = if !stack_header && !pane.tabs.is_empty() {
        TAB_HEIGHT.min(rect.height)
    } else {
        0.0
    };
    let content = if stack_header {
        Rect {
            x: rect.x,
            y: rect.y,
            width: 0.0,
            height: 0.0,
        }
    } else {
        Rect {
            x: rect.x,
            y: rect.y + tab_height,
            width: rect.width,
            height: (rect.height - tab_height).max(0.0),
        }
    };
    let tabs_width = (rect.width - SCREEN_ACTION_WIDTH).max(0.0);
    let tab_width = if pane.tabs.is_empty() {
        0.0
    } else {
        tabs_width / pane.tabs.len() as f64
    };
    let tabs = if tab_height > 0.0 {
        pane.tabs
            .iter()
            .enumerate()
            .map(|(index, tab)| TabHit {
                id: tab.id.clone(),
                rect: Rect {
                    x: rect.x + index as f64 * tab_width,
                    y: rect.y,
                    width: tab_width,
                    height: tab_height,
                },
                close_rect: Rect {
                    x: rect.x + (index + 1) as f64 * tab_width - SCREEN_CLOSE_SIZE.min(tab_width),
                    y: rect.y + (tab_height - SCREEN_CLOSE_SIZE).max(0.0) / 2.0,
                    width: SCREEN_CLOSE_SIZE.min(tab_width),
                    height: SCREEN_CLOSE_SIZE.min(tab_height),
                },
                terminal: match &tab.content {
                    TabContent::Terminal(terminal) => Some(terminal.clone()),
                    TabContent::Browser => None,
                },
            })
            .collect()
    } else {
        Vec::new()
    };
    out.push(PaneGeometry {
        pane: pane.id.clone(),
        rect,
        content,
        terminal: pane.active_terminal().cloned(),
        tabs,
        new_tab_button: (tab_height > 0.0).then_some(Rect {
            x: rect.x + tabs_width,
            y: rect.y,
            width: SCREEN_ACTION_WIDTH.min(rect.width),
            height: tab_height,
        }),
        stack_header,
    });
}

fn node_contains(node: &LayoutNode, pane: &PaneId) -> bool {
    match node {
        LayoutNode::Leaf(leaf) => &leaf.pane_id == pane,
        LayoutNode::Split(split) => {
            node_contains(&split.first, pane) || node_contains(&split.second, pane)
        }
        LayoutNode::Stack(stack) => stack.pane_ids.contains(pane),
        LayoutNode::Viewport(viewport) => viewport
            .columns
            .iter()
            .any(|column| node_contains(&column.root, pane)),
    }
}

/// `#rrggbb` or `#rgb` to cairo's 0..1 channels. Anything unparseable falls
/// back to mid grey, which makes a colour bug visible instead of hiding it.
fn parse_color(value: &str) -> (f64, f64, f64) {
    let hex = value.trim_start_matches('#');
    let expand = |slice: &str| u8::from_str_radix(slice, 16).ok();
    let (r, g, b) = match hex.len() {
        6 => (expand(&hex[0..2]), expand(&hex[2..4]), expand(&hex[4..6])),
        3 => (
            expand(&hex[0..1]).map(|v| v * 17),
            expand(&hex[1..2]).map(|v| v * 17),
            expand(&hex[2..3]).map(|v| v * 17),
        ),
        _ => (None, None, None),
    };
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => (
            f64::from(r) / 255.0,
            f64::from(g) / 255.0,
            f64::from(b) / 255.0,
        ),
        _ => (0.5, 0.5, 0.5),
    }
}

/// The palette the server resolves runs against, captured from a live session's
/// OSC 4 announcement. `spec/render.md` says runs carry resolved RGB with
/// palette indexes already applied, so a client that wants to restyle an ANSI
/// slot has to recognise the resolved value: these are the keys it matches.
/// A run recoloured by an application's own OSC 4 no longer matches, and is
/// deliberately left as the server sent it.
const SERVER_ANSI: [Rgb; 16] = [
    Rgb(0x1d, 0x1f, 0x21),
    Rgb(0xcc, 0x66, 0x66),
    Rgb(0xb5, 0xbd, 0x68),
    Rgb(0xf0, 0xc6, 0x74),
    Rgb(0x81, 0xa2, 0xbe),
    Rgb(0xb2, 0x94, 0xbb),
    Rgb(0x8a, 0xbe, 0xb7),
    Rgb(0xc5, 0xc8, 0xc6),
    Rgb(0x66, 0x66, 0x66),
    Rgb(0xd5, 0x4e, 0x53),
    Rgb(0xb9, 0xca, 0x4a),
    Rgb(0xe7, 0xc5, 0x47),
    Rgb(0x7a, 0xa6, 0xda),
    Rgb(0xc3, 0x97, 0xd8),
    Rgb(0x70, 0xc0, 0xb1),
    Rgb(0xea, 0xea, 0xea),
];

/// Substitutes a configured palette entry for the resolved colour the server
/// sent, leaving anything that matches no known slot untouched.
fn remap_palette(rgb: (f64, f64, f64), appearance: &TerminalAppearance) -> (f64, f64, f64) {
    let to_byte = |channel: f64| (channel * 255.0).round() as u8;
    let probe = Rgb(to_byte(rgb.0), to_byte(rgb.1), to_byte(rgb.2));
    SERVER_ANSI
        .iter()
        .position(|known| *known == probe)
        .and_then(|slot| appearance.palette[slot])
        .map_or(rgb, |color| color.cairo())
}

fn run_colors(
    run: &RenderRun,
    default_fg: &ColorHex,
    default_bg: &ColorHex,
    appearance: &TerminalAppearance,
) -> ((f64, f64, f64), (f64, f64, f64)) {
    let pick = |value: &Option<ColorHex>,
                fallback: &ColorHex,
                override_color: Option<Rgb>|
     -> (f64, f64, f64) {
        match (value, override_color) {
            // An explicit run colour is palette-resolved, so it may be remapped
            // but never replaced by the default override.
            (Some(color), _) => remap_palette(parse_color(color.as_str()), appearance),
            (None, Some(color)) => color.cairo(),
            (None, None) => remap_palette(parse_color(fallback.as_str()), appearance),
        }
    };
    let mut fg = pick(&run.fg, default_fg, appearance.foreground);
    let mut bg = pick(&run.bg, default_bg, appearance.background);
    if run.has_attr(RenderRun::ATTR_INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if run.has_attr(RenderRun::ATTR_FAINT) {
        fg = (fg.0 * 0.6, fg.1 * 0.6, fg.2 * 0.6);
    }
    (fg, bg)
}

pub fn build(handles: ViewHandles) -> DrawingArea {
    let ViewHandles {
        screens,
        attention,
        terminal_statuses,
        screen_terminals,
        theme,
        blink,
        tab_strip,
        scrollbar,
        search_state,
        preedit,
        im_cursor_location,
    } = handles;
    let area = DrawingArea::new();
    area.set_focusable(true);
    area.set_hexpand(true);
    area.set_vexpand(true);
    let image_cache = RefCell::new(ImageCache::new());

    area.set_draw_func(move |area, cr, width, height| {
        im_cursor_location.set(None);
        let chrome = theme.chrome();
        let (red, green, blue) = chrome.background.cairo();
        cr.set_source_rgb(red, green, blue);
        let _ = cr.paint();

        let screens = screens.borrow();
        let attention = attention.borrow();
        let terminal_statuses = terminal_statuses.borrow();
        let screen_terminals = screen_terminals.borrow();
        let mut image_cache = image_cache.borrow_mut();
        image_cache.retain(|(terminal, image_id, generation), _| {
            screens
                .grids
                .get(terminal)
                .and_then(|screen| screen.graphics.images.get(image_id))
                .is_some_and(|image| image.generation == *generation)
        });
        let metrics = cell_metrics(area, &theme);
        let geometries = pane_geometries(&screens, metrics, width, height);
        let Some(workspace) = screens.workspace.as_ref() else {
            return;
        };
        for geometry in &geometries {
            if geometry.rect.width <= 0.0 || geometry.rect.height <= 0.0 {
                continue;
            }
            let _ = cr.save();
            cr.rectangle(
                geometry.rect.x,
                geometry.rect.y,
                geometry.rect.width,
                geometry.rect.height,
            );
            cr.clip();

            if geometry.stack_header {
                draw_stack_header(
                    area,
                    cr,
                    workspace.pane(&geometry.pane),
                    geometry.rect,
                    &theme,
                );
            } else if let Some(terminal) = geometry.terminal.as_ref() {
                if let Some(screen) = screens.grids.get(terminal) {
                    let focused = geometry.pane == workspace.layout.active_pane_id
                        && screens.focused_terminal() == Some(terminal);
                    if focused {
                        if let Some(cursor) = screen.cursor.as_ref() {
                            im_cursor_location.set(Some(gdk::Rectangle::new(
                                (geometry.content.x + f64::from(cursor.x) * metrics.width).floor()
                                    as i32,
                                (geometry.content.y + f64::from(cursor.y) * metrics.height).floor()
                                    as i32,
                                metrics.width.ceil().max(1.0) as i32,
                                metrics.height.ceil().max(1.0) as i32,
                            )));
                        }
                    }
                    let search_query = search_state
                        .borrow()
                        .visible_query(terminal)
                        .map(str::to_string);
                    let (r, g, b) = theme
                        .terminal()
                        .background
                        .map_or_else(|| parse_color(screen.default_bg.as_str()), Rgb::cairo);
                    cr.set_source_rgb(r, g, b);
                    cr.rectangle(
                        geometry.content.x,
                        geometry.content.y,
                        geometry.content.width,
                        geometry.content.height,
                    );
                    let _ = cr.fill();
                    let _ = cr.save();
                    cr.rectangle(
                        geometry.content.x,
                        geometry.content.y,
                        geometry.content.width,
                        geometry.content.height,
                    );
                    cr.clip();
                    cr.translate(geometry.content.x, geometry.content.y);
                    draw_grid(
                        GridDrawContext {
                            area,
                            cr,
                            screen,
                            appearance: theme.terminal(),
                            metrics,
                            height: geometry.content.height,
                            blink: blink.get(),
                        },
                        terminal,
                        terminal_statuses.get(terminal),
                        search_query.as_deref(),
                        geometry.pane == workspace.layout.active_pane_id,
                        &theme,
                        &mut image_cache,
                    );
                    if focused {
                        if let Some(preedit) = preedit.borrow().as_ref() {
                            draw_preedit(area, cr, screen, metrics, &theme, preedit);
                        }
                    }
                    let _ = cr.restore();
                }
            } else {
                draw_browser_placeholder(area, cr, geometry.content, &theme);
            }

            if !geometry.tabs.is_empty() {
                draw_tabs(
                    TabStripDrawContext {
                        area,
                        cr,
                        colors: &chrome,
                        window_active: blink.get().window_active(),
                        spin_phase: blink.get().spin_phase(),
                        attention: &attention,
                    },
                    workspace.pane(&geometry.pane),
                    geometry,
                    tab_strip.hovered_tab().as_ref(),
                );
            }
            let focused = geometry.pane == workspace.layout.active_pane_id;
            if !focused {
                let (red, green, blue) = chrome.background.cairo();
                cr.set_source_rgba(red, green, blue, 1.0 - UNFOCUSED_PANE_OPACITY);
                cr.rectangle(
                    geometry.rect.x,
                    geometry.rect.y,
                    geometry.rect.width,
                    geometry.rect.height,
                );
                let _ = cr.fill();
            }
            if let Some(terminal) = geometry.terminal.as_ref() {
                if let Some(screen) = screens.grids.get(terminal) {
                    if !screen.at_bottom {
                        draw_scrollbar(cr, geometry.content, terminal, screen, &chrome, &scrollbar);
                    }
                }
            }
            let _ = cr.restore();
            draw_border(cr, geometry.rect, focused, &chrome);
        }
        if let Some(geometry) = screen_bar_geometry(&screens, width, height) {
            draw_screen_bar(
                TabStripDrawContext {
                    area,
                    cr,
                    colors: &chrome,
                    window_active: blink.get().window_active(),
                    spin_phase: blink.get().spin_phase(),
                    attention: &attention,
                },
                workspace,
                &geometry,
                tab_strip.hovered_screen().as_ref(),
                &screen_terminals,
            );
        }
    });
    area
}

fn draw_scrollbar(
    cr: &gtk4::cairo::Context,
    content: Rect,
    terminal: &TerminalId,
    screen: &Screen,
    colors: &ChromeColors,
    state: &ScrollbarState,
) {
    let active = state.is_active(terminal);
    let width = if active {
        SCROLLBAR_ACTIVE_WIDTH
    } else {
        SCROLLBAR_WIDTH
    };
    let Some(geometry) = scrollbar_geometry(
        content,
        screen.scrollback_rows,
        screen.size.rows,
        screen.viewport_offset,
        width,
    ) else {
        return;
    };
    let color = if active {
        colors.scrollbar_thumb_active_foreground
    } else {
        colors.scrollbar_thumb_foreground
    };
    let (red, green, blue) = color.cairo();
    cr.set_source_rgba(red, green, blue, if active { 0.95 } else { 0.78 });
    rounded_rectangle(cr, geometry.thumb, width / 2.0);
    let _ = cr.fill();
}

fn rounded_rectangle(cr: &gtk4::cairo::Context, rect: Rect, radius: f64) {
    let radius = radius.min(rect.width / 2.0).min(rect.height / 2.0).max(0.0);
    let right = rect.x + rect.width;
    let bottom = rect.y + rect.height;
    cr.new_sub_path();
    cr.arc(
        right - radius,
        rect.y + radius,
        radius,
        -std::f64::consts::FRAC_PI_2,
        0.0,
    );
    cr.arc(
        right - radius,
        bottom - radius,
        radius,
        0.0,
        std::f64::consts::FRAC_PI_2,
    );
    cr.arc(
        rect.x + radius,
        bottom - radius,
        radius,
        std::f64::consts::FRAC_PI_2,
        std::f64::consts::PI,
    );
    cr.arc(
        rect.x + radius,
        rect.y + radius,
        radius,
        std::f64::consts::PI,
        std::f64::consts::PI * 1.5,
    );
    cr.close_path();
}

struct TabStripDrawContext<'a> {
    area: &'a DrawingArea,
    cr: &'a gtk4::cairo::Context,
    colors: &'a ChromeColors,
    window_active: bool,
    spin_phase: usize,
    attention: &'a AttentionState,
}

fn draw_screen_bar(
    context: TabStripDrawContext<'_>,
    workspace: &WorkspaceView,
    geometry: &ScreenBarGeometry,
    hovered: Option<&ScreenId>,
    screen_terminals: &HashMap<ScreenId, Vec<TerminalId>>,
) {
    let TabStripDrawContext {
        area,
        cr,
        colors,
        window_active,
        attention,
        // Screen tabs roll up notification severity only; agent activity, and so
        // the spinner, belongs to the terminal tab that owns it.
        ..
    } = context;
    let background = if colors.tab_bar_is_opaque {
        colors.tab_bar_background
    } else {
        colors.background
    };
    let (red, green, blue) = background.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.rectangle(
        geometry.rect.x,
        geometry.rect.y,
        geometry.rect.width,
        geometry.rect.height,
    );
    let _ = cr.fill();

    let layout = area.create_pango_layout(None);
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(11.0 * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    layout.set_ellipsize(pango::EllipsizeMode::End);

    for (screen, hit) in workspace.screen_tabs.iter().zip(&geometry.tabs) {
        if screen.focused {
            let active_background = if window_active {
                colors.tab_active_background
            } else {
                colors.tab_active_unfocused_background
            };
            let (red, green, blue) = active_background.cairo();
            cr.set_source_rgb(red, green, blue);
            cr.rectangle(hit.rect.x, hit.rect.y, hit.rect.width, hit.rect.height);
            let _ = cr.fill();
        }

        let close_visible = hovered == Some(&screen.id);
        let indicator = screen_terminals
            .get(&screen.id)
            .map_or_else(AttentionIndicator::default, |terminals| {
                attention::screen_indicator(attention, terminals)
            });
        let trailing = screen_tab_trailing_width(close_visible, indicator);
        layout.set_width(
            ((hit.rect.width - trailing - 8.0).max(1.0) * f64::from(pango::SCALE)) as i32,
        );
        let fallback = format!("screen {}", screen.index + 1);
        layout.set_text(
            screen
                .name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(&fallback),
        );
        let foreground = if screen.focused && window_active {
            colors.tab_active_foreground
        } else if screen.focused {
            colors.tab_active_unfocused_foreground
        } else {
            colors.tab_foreground
        };
        let (red, green, blue) = foreground.cairo();
        cr.set_source_rgb(red, green, blue);
        let (_, logical) = layout.pixel_extents();
        let text_y = hit.rect.y + (hit.rect.height - f64::from(logical.height())) / 2.0
            - f64::from(logical.y());
        cr.move_to(hit.rect.x + 8.0, text_y);
        pangocairo::functions::show_layout(cr, &layout);

        let indicator_right = hit.rect.x + hit.rect.width
            - if close_visible {
                SCREEN_CLOSE_SIZE + 6.0
            } else {
                8.0
            };
        if let Some(level) = indicator.notification {
            draw_notification_marker(
                area,
                cr,
                indicator_right - ATTENTION_SLOT_SIZE / 2.0,
                hit.rect.y + hit.rect.height / 2.0,
                notification_color(colors, level),
            );
        }

        if close_visible {
            cr.set_source_rgb(red, green, blue);
            let center_x = hit.close_rect.x + hit.close_rect.width / 2.0;
            let center_y = hit.close_rect.y + hit.close_rect.height / 2.0;
            cr.set_line_width(1.25);
            cr.move_to(center_x - 3.0, center_y - 3.0);
            cr.line_to(center_x + 3.0, center_y + 3.0);
            cr.move_to(center_x + 3.0, center_y - 3.0);
            cr.line_to(center_x - 3.0, center_y + 3.0);
            let _ = cr.stroke();
        }
    }

    let fade_left = (geometry.rect.width - TAB_FADE_WIDTH).max(0.0);
    let (red, green, blue) = background.cairo();
    let gradient = gtk4::cairo::LinearGradient::new(fade_left, 0.0, geometry.rect.width, 0.0);
    gradient.add_color_stop_rgba(0.0, red, green, blue, 0.0);
    gradient.add_color_stop_rgba(0.60, red, green, blue, 0.0);
    gradient.add_color_stop_rgba(1.0, red, green, blue, 0.86);
    let _ = cr.set_source(&gradient);
    cr.rectangle(
        fade_left,
        geometry.rect.y,
        geometry.rect.width - fade_left,
        geometry.rect.height,
    );
    let _ = cr.fill();

    let (red, green, blue) = colors.tab_foreground.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.set_line_width(1.25);
    let center_x = geometry.new_button.x + geometry.new_button.width / 2.0;
    let center_y = geometry.new_button.y + geometry.new_button.height / 2.0;
    cr.move_to(center_x - 4.0, center_y);
    cr.line_to(center_x + 4.0, center_y);
    cr.move_to(center_x, center_y - 4.0);
    cr.line_to(center_x, center_y + 4.0);
    let _ = cr.stroke();
}

pub fn sidebar_scrim_visibility(value: f64, upper: f64, page_size: f64) -> (bool, bool) {
    let scrollable = upper > page_size;
    (
        scrollable && value > 0.0,
        scrollable && value < upper - page_size,
    )
}

pub fn build_sidebar_scrims(theme: Rc<Theme>, adjustment: &gtk4::Adjustment) -> DrawingArea {
    let scrims = DrawingArea::new();
    scrims.set_hexpand(true);
    scrims.set_vexpand(true);
    scrims.set_can_target(false);
    let draw_adjustment = adjustment.clone();
    scrims.set_draw_func(move |_, cr, width, height| {
        let colors = theme.chrome();
        let (red, green, blue) = colors.sidebar_background.cairo();
        let height = f64::from(height);
        let scrim_height = SIDEBAR_SCRIM_HEIGHT.min(height / 2.0);
        if scrim_height <= 0.0 {
            return;
        }

        let (show_top, show_bottom) = sidebar_scrim_visibility(
            draw_adjustment.value(),
            draw_adjustment.upper(),
            draw_adjustment.page_size(),
        );
        if show_top {
            let top = gtk4::cairo::LinearGradient::new(0.0, 0.0, 0.0, scrim_height);
            top.add_color_stop_rgba(0.0, red, green, blue, 1.0);
            top.add_color_stop_rgba(1.0, red, green, blue, 0.0);
            let _ = cr.set_source(&top);
            cr.rectangle(0.0, 0.0, f64::from(width), scrim_height);
            let _ = cr.fill();
        }

        if show_bottom {
            let bottom = gtk4::cairo::LinearGradient::new(0.0, height - scrim_height, 0.0, height);
            bottom.add_color_stop_rgba(0.0, red, green, blue, 0.0);
            bottom.add_color_stop_rgba(1.0, red, green, blue, 1.0);
            let _ = cr.set_source(&bottom);
            cr.rectangle(0.0, height - scrim_height, f64::from(width), scrim_height);
            let _ = cr.fill();
        }
    });
    {
        let scrims = scrims.downgrade();
        adjustment.connect_value_changed(move |_| {
            if let Some(scrims) = scrims.upgrade() {
                scrims.queue_draw();
            }
        });
    }
    {
        let scrims = scrims.downgrade();
        adjustment.connect_changed(move |_| {
            if let Some(scrims) = scrims.upgrade() {
                scrims.queue_draw();
            }
        });
    }
    scrims
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Neutral,
    Running,
    Attention,
    Done,
}

fn task_status(state: AgentState) -> Option<TaskStatus> {
    match state {
        AgentState::Working => Some(TaskStatus::Running),
        AgentState::Blocked => Some(TaskStatus::Attention),
        AgentState::Idle => Some(TaskStatus::Neutral),
        AgentState::Done => Some(TaskStatus::Done),
        AgentState::Unknown => None,
    }
}

fn notification_color(colors: &ChromeColors, level: NotificationLevel) -> Rgb {
    match level {
        NotificationLevel::Info => colors.notification_info,
        NotificationLevel::Warning => colors.notification_warning,
        NotificationLevel::Error => colors.notification_error,
    }
}

fn pane_tab_trailing_width(close_visible: bool, indicator: AttentionIndicator) -> f64 {
    let base = if close_visible {
        SCREEN_CLOSE_SIZE + 6.0
    } else {
        6.0
    };
    let notification = if indicator.notification.is_some() {
        ATTENTION_SLOT_SIZE
    } else {
        0.0
    };
    let agent = if indicator.agent.and_then(task_status).is_some() {
        ATTENTION_SLOT_SIZE
    } else {
        0.0
    };
    base + notification + agent
}

fn screen_tab_trailing_width(close_visible: bool, indicator: AttentionIndicator) -> f64 {
    let base = if close_visible {
        SCREEN_CLOSE_SIZE + 6.0
    } else {
        8.0
    };
    base + if indicator.notification.is_some() {
        ATTENTION_SLOT_SIZE
    } else {
        0.0
    }
}

fn draw_notification_marker(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    center_x: f64,
    center_y: f64,
    color: Rgb,
) {
    let layout = area.create_pango_layout(Some("\u{2022}"));
    let mut font = pango::FontDescription::from_string("Sans");
    font.set_absolute_size(NOTIFICATION_MARKER_SIZE * f64::from(pango::SCALE));
    layout.set_font_description(Some(&font));
    let (_, logical) = layout.pixel_extents();
    let (red, green, blue) = color.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.move_to(
        center_x - f64::from(logical.width()) / 2.0 - f64::from(logical.x()),
        center_y - f64::from(logical.height()) / 2.0 - f64::from(logical.y()),
    );
    pangocairo::functions::show_layout(cr, &layout);
}

pub fn draw_spokes_spinner(
    cr: &gtk4::cairo::Context,
    center_x: f64,
    center_y: f64,
    phase: usize,
    foreground: Rgb,
) {
    let (red, green, blue) = foreground.cairo();
    cr.set_line_width(1.25);
    cr.set_line_cap(gtk4::cairo::LineCap::Round);
    cr.new_path();
    for spoke in 0..12 {
        let angle = std::f64::consts::TAU * spoke as f64 / 12.0;
        let age = (spoke + 12 - phase % 12) % 12;
        let alpha = 0.18 + 0.82 * (12 - age) as f64 / 12.0;
        cr.set_source_rgba(red, green, blue, alpha);
        cr.move_to(center_x + angle.cos() * 3.0, center_y + angle.sin() * 3.0);
        cr.line_to(center_x + angle.cos() * 5.5, center_y + angle.sin() * 5.5);
        let _ = cr.stroke();
    }
}

pub fn draw_task_status_ring(
    cr: &gtk4::cairo::Context,
    center_x: f64,
    center_y: f64,
    status: TaskStatus,
    colors: &ChromeColors,
) {
    let (color, alpha) = match status {
        TaskStatus::Neutral => (colors.sidebar_dim_foreground, 0.8),
        TaskStatus::Running => (colors.accent, 1.0),
        TaskStatus::Attention => (Rgb(0xff, 0x6b, 0x33), 1.0),
        TaskStatus::Done => (Rgb(0x73, 0x9e, 0x80), 1.0),
    };
    let (red, green, blue) = color.cairo();
    cr.set_source_rgba(red, green, blue, alpha);
    cr.set_line_width(1.5);
    cr.new_path();
    cr.arc(center_x, center_y, 3.75, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
}

#[derive(Clone, Copy)]
struct GridDrawContext<'a> {
    area: &'a DrawingArea,
    cr: &'a gtk4::cairo::Context,
    screen: &'a Screen,
    appearance: &'a TerminalAppearance,
    metrics: CellMetrics,
    height: f64,
    blink: BlinkState,
}

fn draw_grid(
    context: GridDrawContext<'_>,
    terminal: &TerminalId,
    status: Option<&TerminalStatus>,
    search_query: Option<&str>,
    draw_selection: bool,
    theme: &Theme,
    image_cache: &mut ImageCache,
) {
    let GridDrawContext {
        cr,
        screen,
        appearance,
        metrics,
        height,
        blink,
        ..
    } = context;
    if !screen.is_initialized() {
        return;
    }
    let exit_summary = match status {
        Some(TerminalStatus::Exited { summary }) => Some(summary.as_deref()),
        _ => None,
    };
    draw_grid_backgrounds(cr, screen, appearance, metrics, height);
    draw_graphics(
        cr,
        terminal,
        screen,
        metrics,
        height,
        true,
        image_cache,
        theme.chrome().sidebar_dim_foreground,
    );
    if let Some(query) = search_query {
        draw_search_highlights(cr, screen, metrics, height, query, theme.chrome().accent);
    }
    if draw_selection && screen.selection.is_some() {
        let (r, g, b) = theme.chrome().selection_background.cairo();
        cr.set_source_rgb(r, g, b);
        selection_path(cr, screen, metrics);
        let _ = cr.fill();
    }
    let font = theme.font();
    draw_grid_text(context, &font, None);
    if draw_selection && screen.selection.is_some() {
        if let Some(foreground) = theme.chrome().selection_foreground {
            let _ = cr.save();
            selection_path(cr, screen, metrics);
            cr.clip();
            draw_grid_text(context, &font, Some(foreground));
            let _ = cr.restore();
        }
    }
    draw_graphics(
        cr,
        terminal,
        screen,
        metrics,
        height,
        false,
        image_cache,
        theme.chrome().sidebar_dim_foreground,
    );

    if screen.at_bottom {
        if let Some(summary) = exit_summary {
            draw_exited_marker(
                context,
                &font,
                summary,
                theme.chrome().sidebar_dim_foreground,
            );
        }
    }
    if exit_summary.is_some() {
        return;
    }

    if let Some(cursor) = &screen.cursor {
        if !cursor.visible {
            return;
        }
        let appearance = theme.terminal();
        // A configured shape or blink setting wins over the protocol's own,
        // which is what makes it a user preference rather than a hint.
        let style = appearance
            .cursor_shape
            .map_or(cursor.style, |shape| match shape {
                CursorShape::Block => RenderCursorStyle::Block,
                CursorShape::Bar => RenderCursorStyle::Bar,
                CursorShape::Underline => RenderCursorStyle::Underline,
            });
        let blinking = appearance.cursor_blink.unwrap_or(cursor.blink);
        let presentation = cursor_presentation(style, blinking, blink);
        if presentation == CursorPresentation::Hidden {
            return;
        }
        let x = f64::from(cursor.x) * metrics.width;
        let y = f64::from(cursor.y) * metrics.height;
        let (r, g, b) = appearance.cursor.map_or_else(
            || parse_color(cursor.color.as_ref().unwrap_or(&screen.default_fg).as_str()),
            Rgb::cairo,
        );
        cr.set_source_rgba(r, g, b, if blink.window_active { 0.75 } else { 0.5 });
        if presentation == CursorPresentation::Hollow {
            cr.set_line_width(1.0);
            cr.rectangle(x + 0.5, y + 0.5, metrics.width - 1.0, metrics.height - 1.0);
            let _ = cr.stroke();
            return;
        }
        // Snapping this isolated cursor rectangle is cosmetic. Grid cells tile
        // because their exact coordinates are filled together in shared paths.
        match style {
            RenderCursorStyle::Block => device_aligned_rectangle(
                cr,
                x,
                y,
                (f64::from(cursor.x) + 1.0) * metrics.width,
                (f64::from(cursor.y) + 1.0) * metrics.height,
            ),
            RenderCursorStyle::Underline => {
                device_aligned_rectangle(
                    cr,
                    x,
                    y + metrics.height - 2.0,
                    (f64::from(cursor.x) + 1.0) * metrics.width,
                    (f64::from(cursor.y) + 1.0) * metrics.height,
                );
            }
            RenderCursorStyle::Bar => device_aligned_rectangle(
                cr,
                x,
                y,
                x + 2.0,
                (f64::from(cursor.y) + 1.0) * metrics.height,
            ),
        }
        let _ = cr.fill();
    }
}

#[derive(Debug, PartialEq, Eq)]
struct PreeditSlice<'a> {
    text: &'a str,
    columns: std::ops::Range<u32>,
}

fn drawable_preedit(cursor_column: u32, available_columns: u32, text: &str) -> PreeditSlice<'_> {
    let mut byte_end = 0;
    let mut columns = 0u32;
    for (byte_index, grapheme) in text.grapheme_indices(true) {
        let width = unicode_column_count(grapheme);
        if columns.saturating_add(width) > available_columns {
            break;
        }
        columns = columns.saturating_add(width);
        byte_end = byte_index + grapheme.len();
    }
    PreeditSlice {
        text: &text[..byte_end],
        columns: cursor_column..cursor_column.saturating_add(columns),
    }
}

fn draw_preedit(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    metrics: CellMetrics,
    theme: &Theme,
    preedit: &PreeditState,
) {
    let Some(cursor) = screen.cursor.as_ref() else {
        return;
    };
    let cursor_column = u32::from(cursor.x);
    let available_columns = u32::from(screen.size.cols).saturating_sub(cursor_column);
    let drawable = drawable_preedit(cursor_column, available_columns, &preedit.text);
    if drawable.text.is_empty() || drawable.columns.is_empty() {
        return;
    }

    let y = f64::from(cursor.y) * metrics.height;
    let background = theme
        .terminal()
        .background
        .map_or_else(|| parse_color(screen.default_bg.as_str()), Rgb::cairo);
    cr.set_source_rgb(background.0, background.1, background.2);
    device_aligned_rectangle(
        cr,
        f64::from(drawable.columns.start) * metrics.width,
        y,
        f64::from(drawable.columns.end) * metrics.width,
        y + metrics.height,
    );
    let _ = cr.fill();

    let layout = area.create_pango_layout(None);
    layout.set_font_description(Some(&theme.font()));
    let attributes = pango::AttrList::new();
    attributes.insert(pango::AttrInt::new_underline(pango::Underline::Single));
    layout.set_attributes(Some(&attributes));
    let foreground = theme
        .terminal()
        .foreground
        .map_or_else(|| parse_color(screen.default_fg.as_str()), Rgb::cairo);
    cr.set_source_rgb(foreground.0, foreground.1, foreground.2);

    let placement = place_run_text(drawable.text, None);
    let use_fast_path = if placement.use_fast_path {
        layout.set_text(drawable.text);
        let layout_width = f64::from(layout.extents().1.width()) / f64::from(pango::SCALE);
        let grid_width = f64::from(placement.columns) * metrics.width;
        (layout_width - grid_width).abs() <= 0.5
    } else {
        false
    };
    if use_fast_path {
        cr.move_to(f64::from(drawable.columns.start) * metrics.width, y);
        pangocairo::functions::show_layout(cr, &layout);
    } else {
        for grapheme in placement.graphemes {
            layout.set_text(grapheme.text);
            cr.move_to(
                f64::from(drawable.columns.start.saturating_add(grapheme.column)) * metrics.width,
                y,
            );
            pangocairo::functions::show_layout(cr, &layout);
        }
    }
}

fn exited_marker_row(rows: &[Option<RenderRow>]) -> usize {
    let last_visible = rows.len().saturating_sub(1);
    rows.iter()
        .rposition(|row| {
            row.as_ref().is_some_and(|row| {
                row.runs
                    .iter()
                    .any(|run| run.text.chars().any(|character| !character.is_whitespace()))
            })
        })
        .map_or(0, |row| row.saturating_add(1).min(last_visible))
}

fn draw_exited_marker(
    context: GridDrawContext<'_>,
    font: &pango::FontDescription,
    summary: Option<&str>,
    color: Rgb,
) {
    let text = summary.map_or_else(
        || "[process exited]".to_string(),
        |summary| format!("[process exited: {summary}]"),
    );
    let layout = context.area.create_pango_layout(Some(&text));
    layout.set_font_description(Some(font));
    layout.set_single_paragraph_mode(true);
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_width(
        (f64::from(context.screen.size.cols) * context.metrics.width * f64::from(pango::SCALE))
            .floor() as i32,
    );
    let (red, green, blue) = color.cairo();
    context.cr.set_source_rgb(red, green, blue);
    context.cr.move_to(
        0.0,
        exited_marker_row(&context.screen.rows) as f64 * context.metrics.height,
    );
    pangocairo::functions::show_layout(context.cr, &layout);
}

fn draw_search_highlights(
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    metrics: CellMetrics,
    height: f64,
    query: &str,
    accent: Rgb,
) {
    let (red, green, blue) = accent.cairo();
    cr.set_source_rgba(red, green, blue, 0.25);
    for (row_index, row) in screen.rows.iter().enumerate() {
        let Some(row) = row else { continue };
        let y = row_index as f64 * metrics.height;
        if y >= height {
            break;
        }
        let (text, boundaries) = row_text_and_cell_boundaries(row);
        for (start, end) in search::find_line_matches(&text, query) {
            let Some((&start_column, &end_column)) = boundaries.get(start).zip(boundaries.get(end))
            else {
                continue;
            };
            if end_column <= start_column {
                continue;
            }
            cr.rectangle(
                f64::from(start_column) * metrics.width,
                y,
                f64::from(end_column - start_column) * metrics.width,
                metrics.height,
            );
        }
    }
    let _ = cr.fill();
}

fn row_text_and_cell_boundaries(row: &cmux::RenderRow) -> (String, Vec<u16>) {
    let mut text = String::new();
    let mut boundaries = vec![0];
    let mut column = 0u16;
    for run in &row.runs {
        let character_count = run.text.chars().count();
        if character_count == 0 {
            continue;
        }
        let cells = run
            .width_hint
            .unwrap_or_else(|| u16::try_from(character_count).unwrap_or(u16::MAX));
        text.push_str(&run.text);
        for index in 1..=character_count {
            let run_column = ((index as u64 * u64::from(cells))
                .saturating_add(character_count as u64 - 1)
                / character_count as u64)
                .min(u64::from(u16::MAX)) as u16;
            boundaries.push(column.saturating_add(run_column));
        }
        column = column.saturating_add(cells);
    }
    (text, boundaries)
}

pub fn graphic_geometry(
    placement: &RenderGraphicPlacement,
    image: &RenderGraphicImage,
    metrics: CellMetrics,
) -> Option<GraphicGeometry> {
    if !placement.viewport_visible
        || placement.source_width == 0
        || placement.source_height == 0
        || placement.source_x.checked_add(placement.source_width)? > image.width
        || placement.source_y.checked_add(placement.source_height)? > image.height
    {
        return None;
    }

    let source_width = f64::from(placement.source_width);
    let source_height = f64::from(placement.source_height);
    let (width, height) = match (placement.columns, placement.rows) {
        (0, 0) => (
            f64::from(placement.pixel_width),
            f64::from(placement.pixel_height),
        ),
        (columns, 0) => {
            let width = f64::from(columns) * metrics.width;
            (width, width * source_height / source_width)
        }
        (0, rows) => {
            let height = f64::from(rows) * metrics.height;
            (height * source_width / source_height, height)
        }
        (columns, rows) => (
            f64::from(columns) * metrics.width,
            f64::from(rows) * metrics.height,
        ),
    };
    if width <= 0.0 || height <= 0.0 || !width.is_finite() || !height.is_finite() {
        return None;
    }

    Some(GraphicGeometry {
        destination: Rect {
            x: f64::from(placement.viewport_col) * metrics.width + f64::from(placement.x_offset),
            y: f64::from(placement.viewport_row) * metrics.height + f64::from(placement.y_offset),
            width,
            height,
        },
        source: Rect {
            x: f64::from(placement.source_x),
            y: f64::from(placement.source_y),
            width: source_width,
            height: source_height,
        },
    })
}

fn decode_graphic_image(image: &RenderGraphicImage) -> Option<gdk_pixbuf::Pixbuf> {
    let width = i32::try_from(image.width).ok().filter(|width| *width > 0)?;
    let height = i32::try_from(image.height)
        .ok()
        .filter(|height| *height > 0)?;
    let channels = u32::from(image.format.channels()?);
    let rowstride = image.width.checked_mul(channels)?;
    let expected = usize::try_from(rowstride.checked_mul(image.height)?).ok()?;
    if image.data.len() != expected {
        return None;
    }
    let rowstride = i32::try_from(rowstride).ok().filter(|stride| *stride > 0)?;
    let bytes = gtk4::glib::Bytes::from_owned(image.data.clone());
    Some(gdk_pixbuf::Pixbuf::from_bytes(
        &bytes,
        gdk_pixbuf::Colorspace::Rgb,
        channels == 4,
        8,
        width,
        height,
        rowstride,
    ))
}

#[allow(clippy::too_many_arguments)]
fn draw_graphics(
    cr: &gtk4::cairo::Context,
    terminal: &TerminalId,
    screen: &Screen,
    metrics: CellMetrics,
    height: f64,
    behind_text: bool,
    image_cache: &mut ImageCache,
    placeholder_color: Rgb,
) {
    let mut placements = screen
        .graphics
        .placements
        .iter()
        .filter(|placement| (placement.z < 0) == behind_text)
        .collect::<Vec<_>>();
    placements.sort_by_key(|placement| {
        (
            placement.z,
            placement.image_id,
            placement.placement_id,
            placement.ordinal,
        )
    });

    for placement in placements {
        let Some(image) = screen.graphics.images.get(&placement.image_id) else {
            continue;
        };
        let Some(geometry) = graphic_geometry(placement, image, metrics) else {
            continue;
        };
        let viewport = Rect {
            x: 0.0,
            y: 0.0,
            width: f64::from(screen.size.cols) * metrics.width,
            height,
        };
        if !geometry.destination.intersects(viewport) {
            continue;
        }

        let key = (terminal.clone(), image.image_id, image.generation);
        let pixbuf = image_cache
            .entry(key)
            .or_insert_with(|| decode_graphic_image(image));
        if let Some(pixbuf) = pixbuf {
            draw_pixbuf(cr, pixbuf, geometry);
        } else {
            draw_graphic_placeholder(cr, geometry.destination, placeholder_color);
        }
    }
}

fn draw_pixbuf(cr: &gtk4::cairo::Context, pixbuf: &gdk_pixbuf::Pixbuf, geometry: GraphicGeometry) {
    let _ = cr.save();
    cr.rectangle(
        geometry.destination.x,
        geometry.destination.y,
        geometry.destination.width,
        geometry.destination.height,
    );
    cr.clip();
    cr.translate(geometry.destination.x, geometry.destination.y);
    cr.scale(
        geometry.destination.width / geometry.source.width,
        geometry.destination.height / geometry.source.height,
    );
    cr.set_source_pixbuf(pixbuf, -geometry.source.x, -geometry.source.y);
    cr.source().set_filter(gtk4::cairo::Filter::Bilinear);
    let _ = cr.paint();
    let _ = cr.restore();
}

fn draw_graphic_placeholder(cr: &gtk4::cairo::Context, rect: Rect, color: Rgb) {
    let (red, green, blue) = color.cairo();
    cr.set_source_rgba(red, green, blue, 0.85);
    cr.set_line_width(1.0);
    let x = rect.x + 0.5;
    let y = rect.y + 0.5;
    let width = (rect.width - 1.0).max(0.0);
    let height = (rect.height - 1.0).max(0.0);
    cr.rectangle(x, y, width, height);
    cr.move_to(x, y);
    cr.line_to(x + width, y + height);
    cr.move_to(x + width, y);
    cr.line_to(x, y + height);
    let _ = cr.stroke();
}

#[derive(Debug, PartialEq, Eq)]
struct GraphemePlacement<'a> {
    text: &'a str,
    column: u32,
}

#[derive(Debug, PartialEq, Eq)]
struct RunTextPlacement<'a> {
    columns: u32,
    graphemes: Vec<GraphemePlacement<'a>>,
    use_fast_path: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BlockElementRectangle {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl BlockElementRectangle {
    const EMPTY: Self = Self::new(0.0, 0.0, 0.0, 0.0);

    const fn new(x0: f64, y0: f64, x1: f64, y1: f64) -> Self {
        Self { x0, y0, x1, y1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct BlockElementGeometry {
    rectangles: [BlockElementRectangle; 3],
    rectangle_count: usize,
    alpha: f64,
}

impl BlockElementGeometry {
    const fn one(rectangle: BlockElementRectangle, alpha: f64) -> Self {
        Self {
            rectangles: [
                rectangle,
                BlockElementRectangle::EMPTY,
                BlockElementRectangle::EMPTY,
            ],
            rectangle_count: 1,
            alpha,
        }
    }

    const fn two(first: BlockElementRectangle, second: BlockElementRectangle) -> Self {
        Self {
            rectangles: [first, second, BlockElementRectangle::EMPTY],
            rectangle_count: 2,
            alpha: 1.0,
        }
    }

    const fn three(
        first: BlockElementRectangle,
        second: BlockElementRectangle,
        third: BlockElementRectangle,
    ) -> Self {
        Self {
            rectangles: [first, second, third],
            rectangle_count: 3,
            alpha: 1.0,
        }
    }

    fn rectangles(&self) -> &[BlockElementRectangle] {
        &self.rectangles[..self.rectangle_count]
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct FillStyle {
    color: (f64, f64, f64),
    alpha: f64,
}

#[derive(Debug, PartialEq)]
struct FillBucket {
    style: FillStyle,
    rectangles: Vec<BlockElementRectangle>,
}

fn bucket_fill_rectangle(
    buckets: &mut Vec<FillBucket>,
    style: FillStyle,
    rectangle: BlockElementRectangle,
) {
    if let Some(bucket) = buckets.iter_mut().find(|bucket| bucket.style == style) {
        bucket.rectangles.push(rectangle);
    } else {
        buckets.push(FillBucket {
            style,
            rectangles: vec![rectangle],
        });
    }
}

fn fill_rectangle_buckets(cr: &gtk4::cairo::Context, buckets: &[FillBucket]) {
    for bucket in buckets {
        cr.set_source_rgba(
            bucket.style.color.0,
            bucket.style.color.1,
            bucket.style.color.2,
            bucket.style.alpha,
        );
        cr.new_path();
        for rectangle in &bucket.rectangles {
            cr.rectangle(
                rectangle.x0,
                rectangle.y0,
                rectangle.x1 - rectangle.x0,
                rectangle.y1 - rectangle.y0,
            );
        }
        // Cairo computes coverage for the whole path, so shared cell edges do
        // not get antialiased and composited as two separate fills.
        let _ = cr.fill();
    }
}

// Font ink can cross cell edges, so block elements use explicit grid geometry
// and are filled in one path per style to tile without dark seams.
fn block_element_geometry(character: char) -> Option<BlockElementGeometry> {
    let rectangle = BlockElementRectangle::new;
    let upper_left = rectangle(0.0, 0.0, 0.5, 0.5);
    let upper_right = rectangle(0.5, 0.0, 1.0, 0.5);
    let lower_left = rectangle(0.0, 0.5, 0.5, 1.0);
    let lower_right = rectangle(0.5, 0.5, 1.0, 1.0);

    Some(match character {
        '\u{2580}' => BlockElementGeometry::one(rectangle(0.0, 0.0, 1.0, 0.5), 1.0),
        '\u{2581}'..='\u{2587}' => {
            let eighths = f64::from(u32::from(character) - 0x2580);
            BlockElementGeometry::one(rectangle(0.0, 1.0 - eighths / 8.0, 1.0, 1.0), 1.0)
        }
        '\u{2588}' => BlockElementGeometry::one(rectangle(0.0, 0.0, 1.0, 1.0), 1.0),
        '\u{2589}'..='\u{258f}' => {
            let eighths = f64::from(0x2590 - u32::from(character));
            BlockElementGeometry::one(rectangle(0.0, 0.0, eighths / 8.0, 1.0), 1.0)
        }
        '\u{2590}' => BlockElementGeometry::one(rectangle(0.5, 0.0, 1.0, 1.0), 1.0),
        '\u{2591}'..='\u{2593}' => {
            let alpha = f64::from(u32::from(character) - 0x2590) / 4.0;
            BlockElementGeometry::one(rectangle(0.0, 0.0, 1.0, 1.0), alpha)
        }
        '\u{2594}' => BlockElementGeometry::one(rectangle(0.0, 0.0, 1.0, 0.125), 1.0),
        '\u{2595}' => BlockElementGeometry::one(rectangle(0.875, 0.0, 1.0, 1.0), 1.0),
        '\u{2596}' => BlockElementGeometry::one(lower_left, 1.0),
        '\u{2597}' => BlockElementGeometry::one(lower_right, 1.0),
        '\u{2598}' => BlockElementGeometry::one(upper_left, 1.0),
        '\u{2599}' => BlockElementGeometry::three(upper_left, lower_left, lower_right),
        '\u{259a}' => BlockElementGeometry::two(upper_left, lower_right),
        '\u{259b}' => BlockElementGeometry::three(upper_left, upper_right, lower_left),
        '\u{259c}' => BlockElementGeometry::three(upper_left, upper_right, lower_right),
        '\u{259d}' => BlockElementGeometry::one(upper_right, 1.0),
        '\u{259e}' => BlockElementGeometry::two(upper_right, lower_left),
        '\u{259f}' => BlockElementGeometry::three(upper_right, lower_left, lower_right),
        _ => return None,
    })
}

fn unicode_column_count(text: &str) -> u32 {
    u32::try_from(text.width()).unwrap_or(u32::MAX)
}

fn unicode_text_column_count(text: &str) -> u32 {
    text.graphemes(true).fold(0u32, |columns, grapheme| {
        columns.saturating_add(unicode_column_count(grapheme))
    })
}

fn run_column_count(text: &str, width_hint: Option<u16>) -> u32 {
    width_hint
        .map(u32::from)
        .unwrap_or_else(|| unicode_text_column_count(text))
}

fn place_run_text(text: &str, width_hint: Option<u16>) -> RunTextPlacement<'_> {
    let mut graphemes = Vec::new();
    let mut column = 0u32;
    let mut all_single_width = true;

    for grapheme in text.graphemes(true) {
        let width = unicode_column_count(grapheme);
        graphemes.push(GraphemePlacement {
            text: grapheme,
            column,
        });
        column = column.saturating_add(width);
        all_single_width &= width == 1;
    }

    let columns = width_hint.map(u32::from).unwrap_or(column);
    let grapheme_count = u32::try_from(graphemes.len()).unwrap_or(u32::MAX);
    RunTextPlacement {
        columns,
        use_fast_path: all_single_width && columns == grapheme_count,
        graphemes,
    }
}

fn snap_to_device(cr: &gtk4::cairo::Context, x: f64, y: f64) -> (f64, f64) {
    let (device_x, device_y) = cr.user_to_device(x, y);
    cr.device_to_user(
        snap_to_pixel_grid(device_x, 1),
        snap_to_pixel_grid(device_y, 1),
    )
    .unwrap_or((x, y))
}

fn device_aligned_rectangle(
    cr: &gtk4::cairo::Context,
    left: f64,
    top: f64,
    right: f64,
    bottom: f64,
) {
    let (left, top) = snap_to_device(cr, left, top);
    let (right, bottom) = snap_to_device(cr, right, bottom);
    cr.rectangle(left, top, right - left, bottom - top);
}

fn draw_grid_backgrounds(
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    appearance: &TerminalAppearance,
    metrics: CellMetrics,
    height: f64,
) {
    let mut buckets = Vec::new();
    for (index, row) in screen.rows.iter().enumerate() {
        let Some(row) = row else { continue };
        let top = index as f64 * metrics.height;
        if top >= height {
            break;
        }
        let mut column = 0u32;
        for run in &row.runs {
            // The server's width hint wins whenever Unicode text does not map
            // one-to-one onto terminal grid columns.
            let cells = run_column_count(&run.text, run.width_hint);
            let end_column = column.saturating_add(cells);
            let (_, bg) = run_colors(run, &screen.default_fg, &screen.default_bg, appearance);
            let style = FillStyle {
                color: bg,
                alpha: 1.0,
            };
            for cell in column..end_column {
                bucket_fill_rectangle(
                    &mut buckets,
                    style,
                    BlockElementRectangle::new(
                        f64::from(cell) * metrics.width,
                        top,
                        f64::from(cell + 1) * metrics.width,
                        (index + 1) as f64 * metrics.height,
                    ),
                );
            }
            column = end_column;
        }
    }
    fill_rectangle_buckets(cr, &buckets);
}

fn draw_grid_text(
    context: GridDrawContext<'_>,
    font: &pango::FontDescription,
    foreground: Option<Rgb>,
) {
    let GridDrawContext {
        area,
        cr,
        screen,
        appearance,
        metrics,
        height,
        blink,
    } = context;
    let mut block_buckets = Vec::new();
    for (index, row) in screen.rows.iter().enumerate() {
        let Some(row) = row else { continue };
        let y = index as f64 * metrics.height;
        if y >= height {
            break;
        }
        let mut column = 0u32;
        for run in &row.runs {
            let should_draw = !run.has_attr(RenderRun::ATTR_INVISIBLE)
                && blinking_content_visible(run.has_attr(RenderRun::ATTR_BLINK), blink)
                && !run.text.trim().is_empty();
            let cells = if should_draw {
                let placement = place_run_text(&run.text, run.width_hint);
                let color = foreground.map(Rgb::cairo).unwrap_or_else(|| {
                    run_colors(run, &screen.default_fg, &screen.default_bg, appearance).0
                });
                for grapheme in &placement.graphemes {
                    let Some(geometry) = grapheme.text.chars().find_map(block_element_geometry)
                    else {
                        continue;
                    };
                    let left = f64::from(column.saturating_add(grapheme.column)) * metrics.width;
                    for rectangle in geometry.rectangles() {
                        bucket_fill_rectangle(
                            &mut block_buckets,
                            FillStyle {
                                color,
                                alpha: geometry.alpha,
                            },
                            BlockElementRectangle::new(
                                left + rectangle.x0 * metrics.width,
                                y + rectangle.y0 * metrics.height,
                                left + rectangle.x1 * metrics.width,
                                y + rectangle.y1 * metrics.height,
                            ),
                        );
                    }
                }
                placement.columns
            } else {
                run_column_count(&run.text, run.width_hint)
            };
            column = column.saturating_add(cells);
        }
    }
    fill_rectangle_buckets(cr, &block_buckets);

    let layout = area.create_pango_layout(None);
    let mut description = font.clone();
    for (index, row) in screen.rows.iter().enumerate() {
        let Some(row) = row else { continue };
        let y = index as f64 * metrics.height;
        if y >= height {
            break;
        }
        let mut column = 0u32;
        for run in &row.runs {
            let should_draw = !run.has_attr(RenderRun::ATTR_INVISIBLE)
                && blinking_content_visible(run.has_attr(RenderRun::ATTR_BLINK), blink)
                && !run.text.trim().is_empty();
            let cells = if should_draw {
                let placement = place_run_text(&run.text, run.width_hint);
                description.set_weight(if run.has_attr(RenderRun::ATTR_BOLD) {
                    pango::Weight::Bold
                } else {
                    pango::Weight::Normal
                });
                description.set_style(if run.has_attr(RenderRun::ATTR_ITALIC) {
                    pango::Style::Italic
                } else {
                    pango::Style::Normal
                });
                layout.set_font_description(Some(&description));
                let attributes = pango::AttrList::new();
                if let Some(underline) = &run.underline {
                    attributes.insert(pango::AttrInt::new_underline(match underline {
                        RenderUnderline::Double => pango::Underline::Double,
                        // Pango cannot preserve the other wire styles exactly.
                        _ => pango::Underline::Single,
                    }));
                }
                if run.has_attr(RenderRun::ATTR_STRIKETHROUGH) {
                    attributes.insert(pango::AttrInt::new_strikethrough(true));
                }
                layout.set_attributes(Some(&attributes));
                let color = foreground.map(Rgb::cairo).unwrap_or_else(|| {
                    run_colors(run, &screen.default_fg, &screen.default_bg, appearance).0
                });
                cr.set_source_rgb(color.0, color.1, color.2);
                let contains_block_element = run
                    .text
                    .chars()
                    .any(|character| block_element_geometry(character).is_some());
                let use_fast_path = if placement.use_fast_path && !contains_block_element {
                    layout.set_text(&run.text);
                    let layout_width =
                        f64::from(layout.extents().1.width()) / f64::from(pango::SCALE);
                    let grid_width = f64::from(placement.columns) * metrics.width;
                    (layout_width - grid_width).abs() <= 0.5
                } else {
                    false
                };
                if use_fast_path {
                    cr.move_to(f64::from(column) * metrics.width, y);
                    pangocairo::functions::show_layout(cr, &layout);
                } else {
                    for grapheme in placement.graphemes {
                        let x = f64::from(column.saturating_add(grapheme.column)) * metrics.width;
                        if grapheme
                            .text
                            .chars()
                            .find_map(block_element_geometry)
                            .is_none()
                        {
                            layout.set_text(grapheme.text);
                            cr.move_to(x, y);
                            pangocairo::functions::show_layout(cr, &layout);
                        }
                    }
                }
                placement.columns
            } else {
                run_column_count(&run.text, run.width_hint)
            };
            column = column.saturating_add(cells);
        }
    }
}

fn selection_path(cr: &gtk4::cairo::Context, screen: &Screen, metrics: CellMetrics) {
    cr.new_path();
    for row_index in 0..screen.rows.len() {
        let row = row_index as u16;
        let y = row_index as f64 * metrics.height;
        let mut run_start: Option<u16> = None;
        for column in 0..=screen.size.cols {
            let selected = column < screen.size.cols && screen.is_selected(row, column);
            match (selected, run_start) {
                (true, None) => run_start = Some(column),
                (false, Some(start)) => {
                    cr.rectangle(
                        f64::from(start) * metrics.width,
                        y,
                        f64::from(column - start) * metrics.width,
                        metrics.height,
                    );
                    run_start = None;
                }
                _ => {}
            }
        }
    }
}

fn draw_tabs(
    context: TabStripDrawContext<'_>,
    pane: Option<&PaneView>,
    geometry: &PaneGeometry,
    hovered: Option<&(PaneId, TabId)>,
) {
    let TabStripDrawContext {
        area,
        cr,
        colors,
        window_active,
        spin_phase,
        attention,
    } = context;
    let Some(pane) = pane else { return };
    let layout = area.create_pango_layout(None);
    let mut tab_font = pango::FontDescription::from_string("Sans");
    tab_font.set_absolute_size(11.0 * f64::from(pango::SCALE));
    layout.set_font_description(Some(&tab_font));
    layout.set_ellipsize(pango::EllipsizeMode::End);
    if colors.tab_bar_is_opaque {
        let (red, green, blue) = colors.tab_bar_background.cairo();
        cr.set_source_rgb(red, green, blue);
        cr.rectangle(
            geometry.rect.x,
            geometry.rect.y,
            geometry.rect.width,
            TAB_HEIGHT.min(geometry.rect.height),
        );
        let _ = cr.fill();
    }
    for (tab, hit) in pane.tabs.iter().zip(&geometry.tabs) {
        let active = pane.active_tab().is_some_and(|active| active.id == tab.id);
        if active {
            let background = if window_active {
                colors.tab_active_background
            } else {
                colors.tab_active_unfocused_background
            };
            let (red, green, blue) = background.cairo();
            cr.set_source_rgb(red, green, blue);
            cr.rectangle(hit.rect.x, hit.rect.y, hit.rect.width, hit.rect.height);
            let _ = cr.fill();
        }
        let close_visible =
            hovered.is_some_and(|(pane_id, tab_id)| pane_id == &geometry.pane && tab_id == &tab.id);
        let indicator = attention::tab_indicator(attention, &tab.content);
        let trailing = pane_tab_trailing_width(close_visible, indicator);
        layout.set_width(
            ((hit.rect.width - trailing - 6.0).max(1.0) * f64::from(pango::SCALE)) as i32,
        );
        layout.set_text(
            tab.name
                .as_deref()
                .filter(|name| !name.is_empty())
                .unwrap_or(match tab.content {
                    TabContent::Terminal(_) => "terminal",
                    TabContent::Browser => "browser",
                }),
        );
        let foreground = if active && window_active {
            colors.tab_active_foreground
        } else if active {
            colors.tab_active_unfocused_foreground
        } else {
            colors.tab_foreground
        };
        let (red, green, blue) = foreground.cairo();
        cr.set_source_rgb(red, green, blue);
        let (_, logical) = layout.pixel_extents();
        let text_y = hit.rect.y + (hit.rect.height - f64::from(logical.height())) / 2.0
            - f64::from(logical.y());
        cr.move_to(hit.rect.x + 6.0, text_y);
        pangocairo::functions::show_layout(cr, &layout);

        let mut indicator_right = hit.rect.x + hit.rect.width
            - if close_visible {
                SCREEN_CLOSE_SIZE + 6.0
            } else {
                6.0
            };
        if let Some(status) = indicator.agent.and_then(task_status) {
            let center_x = indicator_right - ATTENTION_SLOT_SIZE / 2.0;
            let center_y = hit.rect.y + hit.rect.height / 2.0;
            // The parity contract shows activity through the spokes spinner and
            // reserves the static ring for settled states.
            if status == TaskStatus::Running {
                draw_spokes_spinner(cr, center_x, center_y, spin_phase, colors.accent);
            } else {
                draw_task_status_ring(cr, center_x, center_y, status, colors);
            }
            indicator_right -= ATTENTION_SLOT_SIZE;
        }
        if let Some(level) = indicator.notification {
            draw_notification_marker(
                area,
                cr,
                indicator_right - ATTENTION_SLOT_SIZE / 2.0,
                hit.rect.y + hit.rect.height / 2.0,
                notification_color(colors, level),
            );
        }
        if close_visible {
            cr.set_source_rgb(red, green, blue);
            let center_x = hit.close_rect.x + hit.close_rect.width / 2.0;
            let center_y = hit.close_rect.y + hit.close_rect.height / 2.0;
            cr.set_line_width(1.25);
            cr.move_to(center_x - 3.0, center_y - 3.0);
            cr.line_to(center_x + 3.0, center_y + 3.0);
            cr.move_to(center_x + 3.0, center_y - 3.0);
            cr.line_to(center_x - 3.0, center_y + 3.0);
            let _ = cr.stroke();
        }
    }
    draw_tab_fade_mask(cr, geometry, colors);
    if let Some(button) = geometry.new_tab_button {
        let (red, green, blue) = colors.tab_foreground.cairo();
        cr.set_source_rgb(red, green, blue);
        cr.set_line_width(1.25);
        let center_x = button.x + button.width / 2.0;
        let center_y = button.y + button.height / 2.0;
        cr.move_to(center_x - 4.0, center_y);
        cr.line_to(center_x + 4.0, center_y);
        cr.move_to(center_x, center_y - 4.0);
        cr.line_to(center_x, center_y + 4.0);
        let _ = cr.stroke();
    }
}

fn draw_tab_fade_mask(cr: &gtk4::cairo::Context, geometry: &PaneGeometry, colors: &ChromeColors) {
    let width = TAB_FADE_WIDTH.min(geometry.rect.width.max(0.0));
    if width <= 0.0 {
        return;
    }
    let right = geometry.rect.x + geometry.rect.width;
    let left = right - width;
    let background = if colors.tab_bar_is_opaque {
        colors.tab_bar_background
    } else {
        colors.background
    };
    let (red, green, blue) = background.cairo();
    let gradient = gtk4::cairo::LinearGradient::new(left, 0.0, right, 0.0);
    gradient.add_color_stop_rgba(0.0, red, green, blue, 0.0);
    gradient.add_color_stop_rgba(0.60, red, green, blue, 0.0);
    gradient.add_color_stop_rgba(1.0, red, green, blue, 0.86);
    let _ = cr.set_source(&gradient);
    cr.rectangle(
        left,
        geometry.rect.y,
        width,
        TAB_HEIGHT.min(geometry.rect.height),
    );
    let _ = cr.fill();
}

fn draw_stack_header(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    pane: Option<&PaneView>,
    rect: Rect,
    theme: &Theme,
) {
    let colors = theme.chrome();
    if colors.tab_bar_is_opaque {
        let (red, green, blue) = colors.tab_bar_background.cairo();
        cr.set_source_rgb(red, green, blue);
        cr.rectangle(rect.x, rect.y, rect.width, rect.height);
        let _ = cr.fill();
    }
    let layout = area.create_pango_layout(None);
    let mut tab_font = pango::FontDescription::from_string("Sans");
    tab_font.set_absolute_size(11.0 * f64::from(pango::SCALE));
    layout.set_font_description(Some(&tab_font));
    layout.set_ellipsize(pango::EllipsizeMode::End);
    layout.set_width(((rect.width - 12.0).max(1.0) * f64::from(pango::SCALE)) as i32);
    layout.set_text(pane.and_then(|pane| pane.name.as_deref()).unwrap_or("pane"));
    let (red, green, blue) = colors.tab_foreground.cairo();
    cr.set_source_rgb(red, green, blue);
    let (_, logical) = layout.pixel_extents();
    let text_y =
        rect.y + (rect.height - f64::from(logical.height())) / 2.0 - f64::from(logical.y());
    cr.move_to(rect.x + 6.0, text_y);
    pangocairo::functions::show_layout(cr, &layout);
}

fn draw_browser_placeholder(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    rect: Rect,
    theme: &Theme,
) {
    let colors = theme.chrome();
    let (red, green, blue) = colors.background.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.rectangle(rect.x, rect.y, rect.width, rect.height);
    let _ = cr.fill();
    let layout = area.create_pango_layout(Some("browser tab - not supported"));
    layout.set_font_description(Some(&theme.font()));
    let (red, green, blue) = colors.sidebar_dim_foreground.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.move_to(rect.x + 12.0, rect.y + 12.0);
    pangocairo::functions::show_layout(cr, &layout);
}

fn draw_border(cr: &gtk4::cairo::Context, rect: Rect, focused: bool, colors: &ChromeColors) {
    let (red, green, blue) = colors.pane_separator.color.cairo();
    cr.set_source_rgba(red, green, blue, colors.pane_separator.alpha);
    cr.set_line_width(1.0);
    cr.rectangle(
        rect.x + 0.5,
        rect.y + 0.5,
        (rect.width - 1.0).max(0.0),
        (rect.height - 1.0).max(0.0),
    );
    let _ = cr.stroke();

    if !focused || !colors.draw_active_border {
        return;
    }
    let active = colors.border_active_foreground;
    let (red, green, blue) = active.cairo();
    cr.set_source_rgb(red, green, blue);
    cr.set_line_width(2.0);
    cr.rectangle(
        rect.x + 1.0,
        rect.y + 1.0,
        (rect.width - 2.0).max(0.0),
        (rect.height - 2.0).max(0.0),
    );
    let _ = cr.stroke();
}

/// Pixel position to grid cell, clamped to the viewport.
pub fn cell_at(metrics: CellMetrics, size: Size, x: f64, y: f64) -> (u16, u16) {
    let column = (x / metrics.width).floor().max(0.0) as u32;
    let row = (y / metrics.height).floor().max(0.0) as u32;
    (
        row.min(u32::from(size.rows.saturating_sub(1))) as u16,
        column.min(u32::from(size.cols)) as u16,
    )
}

/// How many whole cells fit in a pane content rectangle.
pub fn viewport_size(metrics: CellMetrics, width: f64, height: f64) -> Size {
    let cols = (width / metrics.width).floor().max(1.0) as u32;
    let rows = (height / metrics.height).floor().max(1.0) as u32;
    Size {
        cols: cols.min(u32::from(u16::MAX)) as u16,
        rows: rows.min(u32::from(u16::MAX)) as u16,
    }
}

pub fn is_copy_shortcut(key: gdk::Key, state: gdk::ModifierType) -> bool {
    state.contains(gdk::ModifierType::CONTROL_MASK)
        && state.contains(gdk::ModifierType::SHIFT_MASK)
        && matches!(key, gdk::Key::C | gdk::Key::c)
}

pub fn is_paste_shortcut(key: gdk::Key, state: gdk::ModifierType) -> bool {
    state.contains(gdk::ModifierType::CONTROL_MASK)
        && state.contains(gdk::ModifierType::SHIFT_MASK)
        && matches!(key, gdk::Key::V | gdk::Key::v)
}

pub fn paste_payload(text: &str) -> Option<&str> {
    (!text.is_empty()).then_some(text)
}

/// What a key press sends to a pane.
///
/// The bytes a key produces are not a property of the key. They depend on VT
/// state this frontend deliberately does not track: application cursor mode,
/// `modifyOtherKeys`, and the Kitty keyboard flags a coding agent turns on to
/// tell `Shift+Tab` and `Shift+Enter` apart from `Tab` and `Enter`. So input
/// follows the same rule as rendering and leaves the VT to the server: a key
/// the server's encoder can name travels as a chord and is encoded there
/// against the pane's live terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPress {
    /// A `terminal.input.keys` chord for the server to encode.
    Chord(String),
    /// Bytes for keys the chord grammar cannot name.
    Bytes(Vec<u8>),
}

/// Translates a GDK key press into pane input.
///
/// Returns None for keys this frontend does not handle, so GTK keeps its own
/// default handling for them.
pub fn key_press(key: gdk::Key, state: gdk::ModifierType) -> Option<KeyPress> {
    match key_chord(key, state) {
        Some(chord) => Some(KeyPress::Chord(chord)),
        None => key_to_bytes(key, state).map(KeyPress::Bytes),
    }
}

/// The key names the server's chord parser accepts.
fn chord_name(key: gdk::Key) -> Option<&'static str> {
    Some(match key {
        gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => "enter",
        // GDK reports Shift+Tab under its own keysym, which is the press
        // coding agents read as "switch mode".
        gdk::Key::Tab | gdk::Key::KP_Tab | gdk::Key::ISO_Left_Tab => "tab",
        gdk::Key::Escape => "escape",
        gdk::Key::BackSpace => "backspace",
        gdk::Key::Delete | gdk::Key::KP_Delete => "delete",
        gdk::Key::Insert | gdk::Key::KP_Insert => "insert",
        gdk::Key::Up | gdk::Key::KP_Up => "up",
        gdk::Key::Down | gdk::Key::KP_Down => "down",
        gdk::Key::Left | gdk::Key::KP_Left => "left",
        gdk::Key::Right | gdk::Key::KP_Right => "right",
        gdk::Key::Home | gdk::Key::KP_Home => "home",
        gdk::Key::End | gdk::Key::KP_End => "end",
        gdk::Key::Page_Up | gdk::Key::KP_Page_Up => "pageup",
        gdk::Key::Page_Down | gdk::Key::KP_Page_Down => "pagedown",
        gdk::Key::F1 => "f1",
        gdk::Key::F2 => "f2",
        gdk::Key::F3 => "f3",
        gdk::Key::F4 => "f4",
        gdk::Key::F5 => "f5",
        gdk::Key::F6 => "f6",
        gdk::Key::F7 => "f7",
        gdk::Key::F8 => "f8",
        gdk::Key::F9 => "f9",
        gdk::Key::F10 => "f10",
        gdk::Key::F11 => "f11",
        gdk::Key::F12 => "f12",
        _ => return None,
    })
}

/// Spells a character the way the chord parser names it, for the characters
/// the encoder has a physical key for. The server rejects a chord it cannot
/// parse, so anything outside that table is encoded locally instead.
fn chord_character(unicode: char) -> Option<String> {
    if unicode == ' ' {
        return Some("space".to_string());
    }
    matches!(
        unicode.to_ascii_lowercase(),
        'a'..='z'
            | '0'..='9'
            | '`'
            | '\\'
            | '['
            | ']'
            | ','
            | '='
            | '-'
            | '.'
            | '\''
            | ';'
            | '/'
    )
    .then(|| unicode.to_string())
}

fn key_chord(key: gdk::Key, state: gdk::ModifierType) -> Option<String> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gdk::ModifierType::ALT_MASK);
    // A layout that reports Shift+Tab as `ISO_Left_Tab` has already spent the
    // modifier on the keysym and leaves it out of the state.
    let shift = state.contains(gdk::ModifierType::SHIFT_MASK) || key == gdk::Key::ISO_Left_Tab;

    let name = match chord_name(key) {
        Some(name) => name.to_string(),
        // Plain text is the input method's to commit; a character becomes a
        // chord only once a modifier turns it into a control sequence.
        None if ctrl || alt => chord_character(key.to_unicode()?)?,
        None => return None,
    };

    let mut chord = String::new();
    if ctrl {
        chord.push_str("ctrl+");
    }
    if alt {
        chord.push_str("alt+");
    }
    if shift {
        chord.push_str("shift+");
    }
    chord.push_str(&name);
    Some(chord)
}

/// Bytes for keys the chord grammar cannot name: `Ctrl+@`, `Ctrl+^`, `Ctrl+_`,
/// and the characters a non-US layout produces that the encoder's physical-key
/// table has no entry for.
fn key_to_bytes(key: gdk::Key, state: gdk::ModifierType) -> Option<Vec<u8>> {
    let unicode = key.to_unicode()?;
    let base: Vec<u8> = if state.contains(gdk::ModifierType::CONTROL_MASK) {
        let upper = unicode.to_ascii_uppercase();
        if ('@'..='_').contains(&upper) {
            vec![(upper as u8) & 0x1f]
        } else {
            return None;
        }
    } else {
        let mut buffer = [0u8; 4];
        unicode.encode_utf8(&mut buffer).as_bytes().to_vec()
    };

    if state.contains(gdk::ModifierType::ALT_MASK) {
        let mut prefixed = vec![0x1b];
        prefixed.extend_from_slice(&base);
        return Some(prefixed);
    }
    Some(base)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use cmux::{
        LayoutDocument, LayoutLeaf, LayoutSplit, PaneId, RenderGraphicFormat, ScreenId, SplitId,
        TabId, TerminalId, WorkspaceId,
    };

    use super::*;
    use crate::screen::{PaneView, ScreenSet, ScreenTabView, TabContent, TabView, WorkspaceView};

    fn placement_offsets(placement: &RunTextPlacement<'_>) -> Vec<u32> {
        placement
            .graphemes
            .iter()
            .map(|grapheme| grapheme.column)
            .collect()
    }

    fn content_row(row: u16) -> Option<RenderRow> {
        Some(RenderRow {
            row,
            runs: vec![RenderRun {
                text: "output".to_string(),
                fg: None,
                bg: None,
                attrs: 0,
                underline: None,
                width_hint: Some(6),
            }],
        })
    }

    fn blank_row(row: u16) -> Option<RenderRow> {
        Some(RenderRow {
            row,
            runs: vec![RenderRun {
                text: " ".repeat(80),
                fg: None,
                bg: None,
                attrs: 0,
                underline: None,
                width_hint: None,
            }],
        })
    }

    #[test]
    fn exited_marker_uses_first_row_for_an_empty_viewport() {
        let rows = (0..24).map(blank_row).collect::<Vec<_>>();
        assert_eq!(exited_marker_row(&rows), 0);
    }

    #[test]
    fn exited_marker_follows_the_last_content_row() {
        let rows = vec![None, content_row(1), None, None];
        assert_eq!(exited_marker_row(&rows), 2);
    }

    #[test]
    fn exited_marker_clamps_to_a_full_viewports_last_row() {
        let rows = vec![content_row(0), content_row(1), content_row(2)];
        assert_eq!(exited_marker_row(&rows), 2);
    }

    #[test]
    fn sidebar_scrims_are_hidden_when_list_is_shorter_than_viewport() {
        assert_eq!(sidebar_scrim_visibility(0.0, 80.0, 100.0), (false, false));
    }

    #[test]
    fn sidebar_scrims_show_only_bottom_at_scroll_top() {
        assert_eq!(sidebar_scrim_visibility(0.0, 300.0, 100.0), (false, true));
    }

    #[test]
    fn sidebar_scrims_show_both_mid_scroll() {
        assert_eq!(sidebar_scrim_visibility(75.0, 300.0, 100.0), (true, true));
    }

    #[test]
    fn sidebar_scrims_show_only_top_at_scroll_bottom() {
        assert_eq!(sidebar_scrim_visibility(200.0, 300.0, 100.0), (true, false));
    }

    #[test]
    fn ascii_run_uses_consecutive_columns_and_fast_path() {
        let placement = place_run_text("abcdef", Some(6));

        assert_eq!(placement.columns, 6);
        assert_eq!(placement_offsets(&placement), vec![0, 1, 2, 3, 4, 5]);
        assert!(placement.use_fast_path);
    }

    #[test]
    fn cjk_run_places_each_grapheme_two_columns_apart() {
        let placement = place_run_text("中文中文", Some(8));

        assert_eq!(placement.columns, 8);
        assert_eq!(placement_offsets(&placement), vec![0, 2, 4, 6]);
        assert!(!placement.use_fast_path);
    }

    #[test]
    fn mixed_run_places_text_at_terminal_cell_offsets() {
        let placement = place_run_text("abc中文def", Some(10));

        assert_eq!(placement.columns, 10);
        assert_eq!(placement_offsets(&placement), vec![0, 1, 2, 3, 5, 7, 8, 9]);
        assert!(!placement.use_fast_path);
    }

    #[test]
    fn combining_mark_stays_with_its_preceding_cell() {
        let placement = place_run_text("e\u{301}x", Some(2));

        assert_eq!(placement.columns, 2);
        assert_eq!(placement_offsets(&placement), vec![0, 1]);
        assert_eq!(
            placement
                .graphemes
                .iter()
                .map(|grapheme| grapheme.text)
                .collect::<Vec<_>>(),
            vec!["e\u{301}", "x"]
        );
        assert!(placement.use_fast_path);
    }

    #[test]
    fn missing_width_hint_uses_unicode_column_widths() {
        let placement = place_run_text("a中b", None);

        assert_eq!(placement.columns, 4);
        assert_eq!(placement_offsets(&placement), vec![0, 1, 3]);
        assert!(!placement.use_fast_path);
    }

    #[test]
    fn full_block_covers_the_whole_cell() {
        let geometry = block_element_geometry('\u{2588}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[BlockElementRectangle::new(0.0, 0.0, 1.0, 1.0)]
        );
        assert_eq!(geometry.alpha, 1.0);
    }

    #[test]
    fn upper_half_block_covers_exactly_the_top_half() {
        let geometry = block_element_geometry('\u{2580}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[BlockElementRectangle::new(0.0, 0.0, 1.0, 0.5)]
        );
        assert_eq!(geometry.alpha, 1.0);
    }

    #[test]
    fn lower_one_eighth_block_is_anchored_at_the_bottom() {
        let geometry = block_element_geometry('\u{2581}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[BlockElementRectangle::new(0.0, 0.875, 1.0, 1.0)]
        );
        assert_eq!(geometry.alpha, 1.0);
    }

    #[test]
    fn left_one_eighth_block_is_anchored_at_the_left() {
        let geometry = block_element_geometry('\u{258f}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[BlockElementRectangle::new(0.0, 0.0, 0.125, 1.0)]
        );
        assert_eq!(geometry.alpha, 1.0);
    }

    #[test]
    fn diagonal_quadrants_are_two_disjoint_rectangles() {
        let geometry = block_element_geometry('\u{259a}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[
                BlockElementRectangle::new(0.0, 0.0, 0.5, 0.5),
                BlockElementRectangle::new(0.5, 0.5, 1.0, 1.0),
            ]
        );
        assert_eq!(geometry.alpha, 1.0);
    }

    #[test]
    fn medium_shade_covers_the_cell_at_half_alpha() {
        let geometry = block_element_geometry('\u{2592}').unwrap();

        assert_eq!(
            geometry.rectangles(),
            &[BlockElementRectangle::new(0.0, 0.0, 1.0, 1.0)]
        );
        assert_eq!(geometry.alpha, 0.5);
    }

    #[test]
    fn ordinary_character_has_no_block_geometry() {
        assert_eq!(block_element_geometry('A'), None);
    }

    const FRACTIONAL_DEVICE_SCALE: f64 = 4.0 / 3.0;
    const RASTER_CELL_METRICS: CellMetrics = CellMetrics {
        width: 8.830_078_125,
        height: 17.0,
        baseline: 13.0,
    };
    const RASTER_COLUMNS: u32 = 4;
    const RASTER_ROWS: usize = 2;
    const RASTER_FILL: (f64, f64, f64) = (1.0, 0.0, 0.0);
    const RASTER_FILL_PIXEL: u32 = 0x00ff_0000;

    fn fractional_scale_surface() -> (gtk4::cairo::ImageSurface, gtk4::cairo::Context) {
        let surface = gtk4::cairo::ImageSurface::create(gtk4::cairo::Format::Rgb24, 64, 64)
            .expect("create image surface");
        surface.set_device_scale(FRACTIONAL_DEVICE_SCALE, FRACTIONAL_DEVICE_SCALE);
        let cr = gtk4::cairo::Context::new(&surface).expect("create cairo context");
        cr.set_source_rgb(0.0, 0.0, 1.0);
        cr.paint().expect("paint contrasting background");
        (surface, cr)
    }

    fn assert_filled_grid_is_solid(surface: &mut gtk4::cairo::ImageSurface) {
        let stride = usize::try_from(surface.stride()).expect("positive image stride");
        surface.flush();
        let data = surface.data().expect("read image surface").to_vec();
        surface.finish();

        let right =
            (f64::from(RASTER_COLUMNS) * RASTER_CELL_METRICS.width * FRACTIONAL_DEVICE_SCALE)
                .floor() as usize;
        let bottom = (RASTER_ROWS as f64 * RASTER_CELL_METRICS.height * FRACTIONAL_DEVICE_SCALE)
            .floor() as usize;
        for y in 1..bottom {
            for x in 1..right {
                let offset = y * stride + x * 4;
                let pixel = u32::from_ne_bytes(
                    data[offset..offset + 4]
                        .try_into()
                        .expect("complete RGB24 pixel"),
                ) & 0x00ff_ffff;
                assert_eq!(
                    pixel, RASTER_FILL_PIXEL,
                    "intermediate pixel at device coordinate ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn cell_backgrounds_leave_no_seam_at_fractional_device_scale() {
        // Protects adjacent background runs and rows from dark fractional-scale seams.
        let background = ColorHex::parse("#ff0000").expect("valid literal");
        let run = || RenderRun {
            text: "  ".to_string(),
            fg: None,
            bg: Some(background.clone()),
            attrs: 0,
            underline: None,
            width_hint: Some(2),
        };
        let mut screen = Screen {
            size: Size {
                cols: RASTER_COLUMNS as u16,
                rows: RASTER_ROWS as u16,
            },
            ..Screen::default()
        };
        screen.rows = (0..RASTER_ROWS)
            .map(|row| {
                Some(RenderRow {
                    row: row as u16,
                    runs: vec![run(), run()],
                })
            })
            .collect();
        let (mut surface, cr) = fractional_scale_surface();

        draw_grid_backgrounds(
            &cr,
            &screen,
            &TerminalAppearance::default(),
            RASTER_CELL_METRICS,
            RASTER_ROWS as f64 * RASTER_CELL_METRICS.height,
        );

        drop(cr);
        assert_filled_grid_is_solid(&mut surface);
    }

    #[test]
    fn block_elements_leave_no_seam_at_fractional_device_scale() {
        // Protects adjacent block cells and rows from dark fractional-scale seams.
        let (mut surface, cr) = fractional_scale_surface();
        let mut buckets = Vec::new();
        let style = FillStyle {
            color: RASTER_FILL,
            alpha: 1.0,
        };
        for row in 0..RASTER_ROWS {
            for column in 0..RASTER_COLUMNS {
                bucket_fill_rectangle(
                    &mut buckets,
                    style,
                    BlockElementRectangle::new(
                        f64::from(column) * RASTER_CELL_METRICS.width,
                        row as f64 * RASTER_CELL_METRICS.height,
                        f64::from(column + 1) * RASTER_CELL_METRICS.width,
                        (row + 1) as f64 * RASTER_CELL_METRICS.height,
                    ),
                );
            }
        }

        fill_rectangle_buckets(&cr, &buckets);

        drop(cr);
        assert_filled_grid_is_solid(&mut surface);
    }

    #[test]
    fn fill_rectangles_group_by_color_and_alpha_across_rows_and_columns() {
        let red = (1.0, 0.0, 0.0);
        let blue = (0.0, 0.0, 1.0);
        let full_red = FillStyle {
            color: red,
            alpha: 1.0,
        };
        let mut buckets = Vec::new();
        let left = BlockElementRectangle::new(0.0, 0.0, 1.0, 1.0);
        let right = BlockElementRectangle::new(1.0, 0.0, 2.0, 1.0);
        let next_row = BlockElementRectangle::new(0.0, 1.0, 1.0, 2.0);

        bucket_fill_rectangle(&mut buckets, full_red, left);
        bucket_fill_rectangle(&mut buckets, full_red, right);
        bucket_fill_rectangle(&mut buckets, full_red, next_row);
        bucket_fill_rectangle(
            &mut buckets,
            FillStyle {
                color: blue,
                alpha: 1.0,
            },
            BlockElementRectangle::new(2.0, 0.0, 3.0, 1.0),
        );
        bucket_fill_rectangle(
            &mut buckets,
            FillStyle {
                color: red,
                alpha: 0.5,
            },
            BlockElementRectangle::new(1.0, 1.0, 2.0, 2.0),
        );

        assert_eq!(buckets.len(), 3);
        assert_eq!(buckets[0].style, full_red);
        assert_eq!(buckets[0].rectangles, vec![left, right, next_row]);
        assert_eq!(buckets[1].style.color, blue);
        assert_eq!(buckets[1].style.alpha, 1.0);
        assert_eq!(buckets[2].style.color, red);
        assert_eq!(buckets[2].style.alpha, 0.5);
    }

    #[test]
    fn preedit_that_fits_keeps_the_complete_text() {
        assert_eq!(
            drawable_preedit(3, 7, "pinyin"),
            PreeditSlice {
                text: "pinyin",
                columns: 3..9,
            }
        );
    }

    #[test]
    fn preedit_that_overflows_is_truncated_to_the_row() {
        assert_eq!(
            drawable_preedit(7, 3, "kana"),
            PreeditSlice {
                text: "kan",
                columns: 7..10,
            }
        );
    }

    #[test]
    fn wide_preedit_is_truncated_on_grapheme_boundaries() {
        assert_eq!(
            drawable_preedit(4, 5, "中文語"),
            PreeditSlice {
                text: "中文",
                columns: 4..8,
            }
        );
    }

    #[test]
    fn empty_preedit_has_an_empty_column_span() {
        assert_eq!(
            drawable_preedit(5, 10, ""),
            PreeditSlice {
                text: "",
                columns: 5..5,
            }
        );
    }

    fn pane(number: u8) -> PaneId {
        PaneId::parse(format!("pane_{number:032x}")).unwrap()
    }

    fn tab(number: u8) -> TabId {
        TabId::parse(format!("tab_{number:032x}")).unwrap()
    }

    fn terminal(number: u8) -> TerminalId {
        TerminalId::parse(format!("term_{number:032x}")).unwrap()
    }

    fn graphic_image() -> RenderGraphicImage {
        RenderGraphicImage {
            image_id: 41,
            generation: 2,
            width: 100,
            height: 50,
            format: RenderGraphicFormat::Rgba,
            data: vec![0; 100 * 50 * 4],
        }
    }

    fn graphic_placement() -> RenderGraphicPlacement {
        RenderGraphicPlacement {
            image_id: 41,
            placement_id: 7,
            ordinal: 1,
            x_offset: 3,
            y_offset: 4,
            source_x: 10,
            source_y: 5,
            source_width: 40,
            source_height: 20,
            columns: 2,
            rows: 3,
            grid_cols: 3,
            grid_rows: 4,
            pixel_width: 33,
            pixel_height: 44,
            viewport_col: -1,
            viewport_row: 2,
            viewport_visible: true,
            anchor_col: Some(8),
            anchor_row: Some(42),
            z: -2,
        }
    }

    fn split_workspace(two_tabs: bool) -> ScreenSet {
        let left = pane(1);
        let right = pane(2);
        let left_tab = tab(1);
        let right_tab = tab(2);
        let left_terminal = terminal(1);
        let right_terminal = terminal(2);
        let mut left_tabs = vec![TabView {
            id: left_tab.clone(),
            name: Some("left".to_string()),
            index: 0,
            focused: true,
            content: TabContent::Terminal(left_terminal),
        }];
        if two_tabs {
            left_tabs.push(TabView {
                id: tab(3),
                name: Some("web".to_string()),
                index: 1,
                focused: false,
                content: TabContent::Browser,
            });
        }
        let root = LayoutNode::Split(LayoutSplit {
            split_id: SplitId::parse(format!("split_{:032x}", 1)).unwrap(),
            direction: LayoutDirection::Horizontal,
            ratio: 0.5,
            first: Box::new(LayoutNode::Leaf(LayoutLeaf {
                pane_id: left.clone(),
                tab_ids: left_tabs.iter().map(|tab| tab.id.clone()).collect(),
                active_tab_id: Some(left_tab.clone()),
            })),
            second: Box::new(LayoutNode::Leaf(LayoutLeaf {
                pane_id: right.clone(),
                tab_ids: vec![right_tab.clone()],
                active_tab_id: Some(right_tab.clone()),
            })),
        });
        ScreenSet {
            workspace: Some(WorkspaceView {
                workspace_id: WorkspaceId::parse(format!("ws_{:032x}", 1)).unwrap(),
                screen_id: ScreenId::parse(format!("screen_{:032x}", 1)).unwrap(),
                screen_tabs: vec![ScreenTabView {
                    id: ScreenId::parse(format!("screen_{:032x}", 1)).unwrap(),
                    name: Some("main".to_string()),
                    index: 0,
                    focused: true,
                }],
                layout: LayoutDocument {
                    version: 1,
                    screen_id: ScreenId::parse(format!("screen_{:032x}", 1)).unwrap(),
                    active_pane_id: left.clone(),
                    zoomed_pane_id: None,
                    root,
                    extra: BTreeMap::new(),
                },
                panes: vec![
                    PaneView {
                        id: left,
                        name: None,
                        active_tab_id: Some(left_tab),
                        tabs: left_tabs,
                    },
                    PaneView {
                        id: right,
                        name: None,
                        active_tab_id: Some(right_tab),
                        tabs: vec![TabView {
                            id: tab(2),
                            name: Some("right".to_string()),
                            index: 0,
                            focused: true,
                            content: TabContent::Terminal(right_terminal),
                        }],
                    },
                ],
            }),
            grids: HashMap::new(),
        }
    }

    #[test]
    fn horizontal_split_tiles_and_sizes_both_terminals() {
        let screens = split_workspace(false);
        let metrics = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
        };
        let panes = pane_geometries(&screens, metrics, 1000, 600);
        assert_eq!(panes.len(), 2);
        assert_eq!(
            panes[0].rect,
            Rect {
                x: 0.0,
                y: 0.0,
                width: 500.0,
                height: 600.0
            }
        );
        assert_eq!(
            panes[1].rect,
            Rect {
                x: 500.0,
                y: 0.0,
                width: 500.0,
                height: 600.0
            }
        );
        let sizes = visible_terminal_sizes(&screens, metrics, 1000, 600);
        assert_eq!(sizes.len(), 2);
        assert!(sizes
            .iter()
            .all(|(_, size)| *size == Size { cols: 50, rows: 28 }));
    }

    #[test]
    fn divider_hit_testing_uses_six_pixel_region_and_nearest_line() {
        let screens = split_workspace(false);
        let dividers = split_dividers(&screens, 1000, 600);
        assert_eq!(dividers.len(), 1);
        assert_eq!(dividers[0].position, 500.0);
        assert_eq!(
            split_divider_at(&dividers, 497.0, 300.0).map(|hit| &hit.split_id),
            Some(&dividers[0].split_id)
        );
        assert!(split_divider_at(&dividers, 502.99, 300.0).is_some());
        assert!(split_divider_at(&dividers, 496.99, 300.0).is_none());
        assert!(split_divider_at(&dividers, 500.0, 600.0).is_none());

        let vertical = SplitDivider {
            direction: LayoutDirection::Vertical,
            position: 300.0,
            ..dividers[0].clone()
        };
        assert!(split_divider_at(std::slice::from_ref(&vertical), 200.0, 297.0).is_some());
        assert!(split_divider_at(std::slice::from_ref(&vertical), 200.0, 303.0).is_none());

        let mut nearer = dividers[0].clone();
        nearer.split_id = SplitId::parse(format!("split_{:032x}", 2)).unwrap();
        nearer.position = 501.0;
        let candidates = vec![dividers[0].clone(), nearer];
        assert_eq!(
            split_divider_at(&candidates, 500.75, 300.0).map(|hit| &hit.split_id),
            Some(&candidates[1].split_id)
        );
    }

    #[test]
    fn divider_ratio_uses_parent_axis_rounding_and_valid_clamps() {
        let screens = split_workspace(false);
        let horizontal = split_dividers(&screens, 1000, 600).remove(0);
        assert_eq!(split_ratio_at(&horizontal, 700.4, 100.0), Some(0.7));
        assert_eq!(split_ratio_at(&horizontal, -100.0, 100.0), Some(0.001));
        assert_eq!(split_ratio_at(&horizontal, 1200.0, 100.0), Some(0.999));

        let vertical = SplitDivider {
            direction: LayoutDirection::Vertical,
            split_rect: Rect {
                x: 25.0,
                y: 40.0,
                width: 300.0,
                height: 200.0,
            },
            position: 140.0,
            ..horizontal
        };
        assert_eq!(split_ratio_at(&vertical, 100.0, 90.0), Some(0.25));
        assert_eq!(split_ratio_at(&vertical, 100.0, 500.0), Some(0.995));
    }

    #[test]
    fn divider_throttle_limits_intermediate_updates_and_always_sends_final() {
        let mut throttle = DividerDragThrottle::default();
        assert!(throttle.should_send(Duration::ZERO, 0.5, false));
        assert!(!throttle.should_send(Duration::from_millis(10), 0.5, false));
        assert!(!throttle.should_send(Duration::from_millis(44), 0.6, false));
        assert!(throttle.should_send(Duration::from_millis(45), 0.6, false));
        assert!(!throttle.should_send(Duration::from_millis(60), 0.7, false));
        assert!(throttle.should_send(Duration::from_millis(60), 0.7, true));
        assert!(throttle.should_send(Duration::from_millis(60), 0.7, true));
    }

    #[test]
    fn spin_phase_advances_only_while_the_window_is_focused() {
        let mut blink = BlinkState::new(true);
        let start = blink.spin_phase();
        blink.tick(false);
        blink.tick(false);
        assert_eq!(blink.spin_phase(), start + 2);
        // A tick with no blinking content still redraws, because the spinner
        // moved even though nothing blinked.
        assert!(blink.tick(false));

        let mut unfocused = BlinkState::new(false);
        let idle = unfocused.spin_phase();
        unfocused.tick(true);
        assert_eq!(unfocused.spin_phase(), idle);
    }

    #[test]
    fn configured_line_height_and_spacing_grow_the_cell_and_recentre_text() {
        let base = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 16.0,
        };
        let appearance = TerminalAppearance {
            line_height: Some(1.5),
            letter_spacing: Some(2.0),
            ..TerminalAppearance::default()
        };
        let scaled = apply_cell_overrides(base, &appearance, 1);

        assert_eq!(scaled.width, 12.0);
        assert_eq!(scaled.height, 30.0);
        // The extra 10px is split above and below, so the glyph stays centred.
        assert_eq!(scaled.baseline, 21.0);

        let untouched = apply_cell_overrides(base, &TerminalAppearance::default(), 1);
        assert_eq!(untouched, base);
    }

    #[test]
    fn pixel_grid_snapping_respects_scale_and_tiles_adjacent_cells() {
        assert_eq!(snap_to_pixel_grid(8.3, 1), 8.0);
        assert_eq!(snap_to_pixel_grid(8.3, 2), 8.5);

        let cell_width = 8.830_078_125;
        for scale_factor in [1, 2] {
            let first_left = snap_to_pixel_grid(0.0, scale_factor);
            let first_right = snap_to_pixel_grid(cell_width, scale_factor);
            let second_left = snap_to_pixel_grid(cell_width, scale_factor);
            let second_right = snap_to_pixel_grid(cell_width * 2.0, scale_factor);

            assert_eq!(first_right, second_left);
            assert_eq!(
                (first_right - first_left) + (second_right - second_left),
                second_right - first_left
            );
        }
    }

    #[test]
    fn cell_height_snaps_after_line_height_and_recentres_the_baseline() {
        let base = CellMetrics {
            width: 8.830_078_125,
            height: 17.26,
            baseline: 13.5,
        };

        let scale_one = apply_cell_overrides(base, &TerminalAppearance::default(), 1);
        assert_eq!(scale_one.height, 17.0);
        assert_eq!(scale_one.baseline, 13.37);

        let scale_two = apply_cell_overrides(base, &TerminalAppearance::default(), 2);
        assert_eq!(scale_two.height, 17.5);
        assert_eq!(scale_two.baseline, 13.62);
        assert_eq!(scale_two.width, base.width);
    }

    #[test]
    fn out_of_range_line_height_is_clamped_rather_than_trusted() {
        let base = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 16.0,
        };
        let appearance = TerminalAppearance {
            line_height: Some(99.0),
            ..TerminalAppearance::default()
        };
        assert_eq!(
            apply_cell_overrides(base, &appearance, 1).height,
            20.0 * MAX_LINE_HEIGHT
        );
    }

    #[test]
    fn palette_remap_replaces_known_ansi_slots_only() {
        let mut appearance = TerminalAppearance::default();
        appearance.palette[1] = Some(Rgb(0x11, 0x22, 0x33));

        // The server's resolved ANSI red maps to the configured colour.
        let red = SERVER_ANSI[1].cairo();
        assert_eq!(
            remap_palette(red, &appearance),
            Rgb(0x11, 0x22, 0x33).cairo()
        );

        // A colour that is not a known slot is left exactly as the server sent
        // it, which is what keeps an application's own OSC 4 intact.
        let arbitrary = Rgb(0x12, 0x34, 0x56).cairo();
        assert_eq!(remap_palette(arbitrary, &appearance), arbitrary);

        // An unconfigured slot also passes through untouched.
        let green = SERVER_ANSI[2].cairo();
        assert_eq!(remap_palette(green, &appearance), green);
    }

    #[test]
    fn tab_strip_is_always_28_pixels_and_exposes_browser_hitbox() {
        let screens = split_workspace(true);
        let metrics = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
        };
        let panes = pane_geometries(&screens, metrics, 1000, 600);
        assert_eq!(panes[0].tabs.len(), 2);
        assert_eq!(panes[0].rect.y, 0.0);
        assert_eq!(panes[0].content.y, 28.0);
        assert_eq!(panes[0].tabs[1].terminal, None);
        assert!(panes[0].tabs[1].rect.contains(375.0, 14.0));
        assert_eq!(panes[0].new_tab_button.unwrap().width, 28.0);
        assert_eq!(
            pane_tab_trailing_width(
                false,
                AttentionIndicator {
                    notification: Some(NotificationLevel::Error),
                    agent: Some(AgentState::Working),
                },
            ),
            28.0
        );
        assert_eq!(pane_bar_hit(&panes[0], 486.0, 14.0), Some(PaneBarHit::New));
        let second_close = panes[0].tabs[1].close_rect;
        assert_eq!(
            pane_bar_hit(
                &panes[0],
                second_close.x + second_close.width / 2.0,
                second_close.y + second_close.height / 2.0,
            ),
            Some(PaneBarHit::Close(panes[0].tabs[1].id.clone()))
        );
        let sizes = visible_terminal_sizes(&screens, metrics, 1000, 600);
        assert_eq!(sizes[0].1, Size { cols: 50, rows: 28 });
        assert_eq!(sizes[1].1, Size { cols: 50, rows: 28 });
    }

    #[test]
    fn screen_bar_is_visible_if_and_only_if_workspace_has_multiple_screens() {
        assert!(!screen_bar_visible(0));
        assert!(!screen_bar_visible(1));
        assert!(screen_bar_visible(2));
        assert!(screen_bar_visible(10));
    }

    #[test]
    fn screen_bar_reserves_action_lane_and_prioritizes_close_hits() {
        let mut screens = split_workspace(false);
        let workspace = screens.workspace.as_mut().unwrap();
        workspace.screen_tabs.push(ScreenTabView {
            id: ScreenId::parse(format!("screen_{:032x}", 2)).unwrap(),
            name: Some("logs".to_string()),
            index: 1,
            focused: false,
        });

        let geometry = screen_bar_geometry(&screens, 428, 600).unwrap();
        let panes = pane_geometries(
            &screens,
            CellMetrics {
                width: 10.0,
                height: 20.0,
                baseline: 15.0,
            },
            428,
            600,
        );
        assert_eq!(panes[0].rect.y, 28.0);
        assert_eq!(geometry.rect.height, 28.0);
        assert_eq!(geometry.new_button.width, 28.0);
        assert_eq!(geometry.tabs.len(), 2);
        assert_eq!(geometry.tabs[0].rect.width, 200.0);
        assert_eq!(
            screen_tab_trailing_width(
                false,
                AttentionIndicator {
                    notification: Some(NotificationLevel::Warning),
                    agent: None,
                },
            ),
            19.0
        );
        assert_eq!(
            screen_bar_hit(&geometry, 414.0, 14.0),
            Some(ScreenBarHit::New)
        );

        let first = &geometry.tabs[0];
        let close_x = first.close_rect.x + first.close_rect.width / 2.0;
        assert_eq!(
            screen_bar_hit(&geometry, close_x, 14.0),
            Some(ScreenBarHit::Close(first.id.clone()))
        );
        assert_eq!(
            screen_bar_hit(&geometry, 20.0, 14.0),
            Some(ScreenBarHit::Tab(first.id.clone()))
        );
        assert_eq!(
            hovered_screen(&geometry, 220.0, 14.0),
            Some(geometry.tabs[1].id.clone())
        );
    }

    #[test]
    fn clipboard_shortcuts_require_control_and_shift() {
        let control_shift = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK;
        assert!(is_copy_shortcut(gdk::Key::c, control_shift));
        assert!(is_paste_shortcut(gdk::Key::V, control_shift));
        assert!(!is_paste_shortcut(
            gdk::Key::v,
            gdk::ModifierType::CONTROL_MASK
        ));
        assert!(!is_copy_shortcut(gdk::Key::v, control_shift));
    }

    fn chord(key: gdk::Key, state: gdk::ModifierType) -> Option<String> {
        match key_press(key, state) {
            Some(KeyPress::Chord(chord)) => Some(chord),
            _ => None,
        }
    }

    #[test]
    fn shift_tab_reaches_the_pane_instead_of_moving_gtk_focus() {
        // GDK reports Shift+Tab as ISO_Left_Tab, which has no Unicode value.
        // Left unhandled it falls through to GTK's focus navigation and the
        // pane never sees the press agents read as "switch mode".
        assert_eq!(
            chord(gdk::Key::ISO_Left_Tab, gdk::ModifierType::SHIFT_MASK),
            Some("shift+tab".to_string())
        );
        assert_eq!(
            chord(gdk::Key::ISO_Left_Tab, gdk::ModifierType::empty()),
            Some("shift+tab".to_string())
        );
        assert_eq!(
            chord(gdk::Key::Tab, gdk::ModifierType::SHIFT_MASK),
            Some("shift+tab".to_string())
        );
        assert_eq!(
            chord(gdk::Key::Tab, gdk::ModifierType::empty()),
            Some("tab".to_string())
        );
    }

    #[test]
    fn chords_carry_the_modifiers_the_encoder_needs() {
        assert_eq!(
            chord(gdk::Key::Right, gdk::ModifierType::CONTROL_MASK),
            Some("ctrl+right".to_string())
        );
        assert_eq!(
            chord(gdk::Key::Return, gdk::ModifierType::SHIFT_MASK),
            Some("shift+enter".to_string())
        );
        assert_eq!(
            chord(gdk::Key::c, gdk::ModifierType::CONTROL_MASK),
            Some("ctrl+c".to_string())
        );
        assert_eq!(
            chord(gdk::Key::b, gdk::ModifierType::ALT_MASK),
            Some("alt+b".to_string())
        );
        assert_eq!(
            chord(gdk::Key::space, gdk::ModifierType::CONTROL_MASK),
            Some("ctrl+space".to_string())
        );
        assert_eq!(
            chord(gdk::Key::F5, gdk::ModifierType::empty()),
            Some("f5".to_string())
        );
        assert_eq!(
            chord(gdk::Key::Escape, gdk::ModifierType::empty()),
            Some("escape".to_string())
        );
    }

    #[test]
    fn keys_outside_the_chord_grammar_are_encoded_locally() {
        // Plain text belongs to the input method, not to a chord.
        assert_eq!(
            key_press(gdk::Key::a, gdk::ModifierType::empty()),
            Some(KeyPress::Bytes(b"a".to_vec()))
        );
        // The encoder has no physical key for `@`, `^` or `_`.
        assert_eq!(
            key_press(gdk::Key::at, gdk::ModifierType::CONTROL_MASK),
            Some(KeyPress::Bytes(vec![0]))
        );
        assert_eq!(
            key_press(gdk::Key::underscore, gdk::ModifierType::CONTROL_MASK),
            Some(KeyPress::Bytes(vec![0x1f]))
        );
        assert_eq!(
            key_press(gdk::Key::eacute, gdk::ModifierType::ALT_MASK),
            Some(KeyPress::Bytes("\x1bé".as_bytes().to_vec()))
        );
        // Modifiers alone, and keys with no terminal meaning, stay with GTK.
        assert!(key_press(gdk::Key::Shift_L, gdk::ModifierType::empty()).is_none());
        assert!(key_press(gdk::Key::colon, gdk::ModifierType::CONTROL_MASK).is_none());
    }

    #[test]
    fn paste_payload_preserves_tui_input_bytes() {
        let text = "line one\r\n\x1b[200~line two\x1b[201~";
        assert_eq!(paste_payload(text), Some(text));
        assert_eq!(paste_payload(""), None);
    }

    #[test]
    fn mouse_move_throttle_reports_only_cell_changes() {
        let first = terminal(1);
        let second = terminal(2);
        let mut throttle = MouseMoveThrottle::default();

        assert!(throttle.should_report(&first, 2, 3));
        assert!(!throttle.should_report(&first, 2, 3));
        assert!(throttle.should_report(&first, 2, 4));
        assert!(throttle.should_report(&second, 2, 4));
        throttle.reset();
        assert!(throttle.should_report(&second, 2, 4));
    }

    #[test]
    fn graphic_geometry_scales_cell_axes_and_keeps_viewport_offsets() {
        let image = graphic_image();
        let placement = graphic_placement();
        let metrics = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
        };

        let geometry = graphic_geometry(&placement, &image, metrics).unwrap();
        assert_eq!(
            geometry,
            GraphicGeometry {
                destination: Rect {
                    x: -7.0,
                    y: 44.0,
                    width: 20.0,
                    height: 60.0,
                },
                source: Rect {
                    x: 10.0,
                    y: 5.0,
                    width: 40.0,
                    height: 20.0,
                },
            }
        );

        let mut width_only = placement.clone();
        width_only.rows = 0;
        let geometry = graphic_geometry(&width_only, &image, metrics).unwrap();
        assert_eq!(
            (geometry.destination.width, geometry.destination.height),
            (20.0, 10.0)
        );

        let mut height_only = placement.clone();
        height_only.columns = 0;
        let geometry = graphic_geometry(&height_only, &image, metrics).unwrap();
        assert_eq!(
            (geometry.destination.width, geometry.destination.height),
            (120.0, 60.0)
        );

        let mut native = placement;
        native.columns = 0;
        native.rows = 0;
        let geometry = graphic_geometry(&native, &image, metrics).unwrap();
        assert_eq!(
            (geometry.destination.width, geometry.destination.height),
            (33.0, 44.0)
        );
    }

    #[test]
    fn graphic_geometry_rejects_hidden_empty_and_out_of_bounds_sources() {
        let image = graphic_image();
        let metrics = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
        };
        let mut placement = graphic_placement();
        placement.viewport_visible = false;
        assert!(graphic_geometry(&placement, &image, metrics).is_none());

        placement.viewport_visible = true;
        placement.source_width = 0;
        assert!(graphic_geometry(&placement, &image, metrics).is_none());

        placement.source_width = 95;
        assert!(graphic_geometry(&placement, &image, metrics).is_none());
    }

    #[test]
    fn graphic_decode_rejects_bad_pixels_and_unsupported_formats_without_panicking() {
        let mut image = graphic_image();
        image.data.pop();
        assert!(decode_graphic_image(&image).is_none());

        image.format = RenderGraphicFormat::Unsupported("future-encoded".to_string());
        assert!(decode_graphic_image(&image).is_none());
    }

    #[test]
    fn blink_phase_and_unfocused_cursor_are_stable() {
        let mut blink = BlinkState::new(true);
        assert_eq!(
            cursor_presentation(RenderCursorStyle::Block, true, blink),
            CursorPresentation::Solid
        );
        assert!(blink.tick(true));
        assert_eq!(
            cursor_presentation(RenderCursorStyle::Block, true, blink),
            CursorPresentation::Hidden
        );
        assert!(!blinking_content_visible(true, blink));
        assert!(blinking_content_visible(false, blink));

        assert!(blink.set_window_active(false));
        assert_eq!(
            cursor_presentation(RenderCursorStyle::Block, true, blink),
            CursorPresentation::Hollow
        );
        assert_eq!(
            cursor_presentation(RenderCursorStyle::Bar, true, blink),
            CursorPresentation::Solid
        );
        assert!(blinking_content_visible(true, blink));
        assert!(!blink.tick(true));
    }

    #[test]
    fn scrollbar_thumb_tracks_absolute_viewport_offset_and_retained_ratio() {
        let content = Rect {
            x: 10.0,
            y: 20.0,
            width: 200.0,
            height: 100.0,
        };
        let top = scrollbar_geometry(content, 100, 20, 0, 4.0).unwrap();
        let middle = scrollbar_geometry(content, 100, 20, 50, 4.0).unwrap();
        let bottom = scrollbar_geometry(content, 100, 20, 100, 4.0).unwrap();

        assert_eq!(top.track.y, 23.0);
        assert_eq!(top.track.height, 94.0);
        assert_eq!(top.thumb.height, 24.0);
        assert_eq!(top.thumb.y, 23.0);
        assert_eq!(middle.thumb.y, 58.0);
        assert_eq!(bottom.thumb.y, 93.0);
        assert_eq!(bottom.thumb.x, 203.0);
        assert!(scrollbar_geometry(content, 0, 20, 0, 4.0).is_none());
    }

    #[test]
    fn scrollbar_drag_pixels_convert_to_signed_incremental_rows() {
        assert_eq!(scrollbar_drag_rows(35.0, 100, 94.0, 24.0), 50);
        assert_eq!(scrollbar_drag_rows(-14.0, 100, 94.0, 24.0), -20);
        assert_eq!(scrollbar_drag_rows(10.0, 100, 24.0, 24.0), 0);
        assert_eq!(scrollbar_drag_rows(f64::NAN, 100, 94.0, 24.0), 0);
    }

    #[test]
    fn runtime_font_size_adjustment_clamps_to_supported_range() {
        assert_eq!(clamp_font_size(5.0), MIN_FONT_SIZE);
        assert_eq!(clamp_font_size(33.0), MAX_FONT_SIZE);
        assert_eq!(adjusted_font_size(6.0, -1.0), MIN_FONT_SIZE);
        assert_eq!(adjusted_font_size(32.0, 1.0), MAX_FONT_SIZE);
        assert_eq!(adjusted_font_size(11.0, 1.0), 12.0);
        assert_eq!(clamp_font_size(f64::NAN), DEFAULT_FONT_SIZE);
    }
}
