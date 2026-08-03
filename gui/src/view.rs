//! Cell-grid rendering, pane layout, and keyboard input.
//!
//! Drawing is deliberately literal: the server already resolved palettes, OSC
//! overrides and inverse-video source colours. This module only places the
//! server's layout rectangles and stamps its styled runs inside them.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::time::Duration;

use cmux::{
    ColorHex, LayoutDirection, LayoutNode, LayoutViewport, PaneId, RenderCursorStyle, RenderRun,
    RenderUnderline, Size, SplitId, TabId, TerminalId,
};
use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{gdk, DrawingArea};

use crate::config::{
    ChromeColors, ChromeMode, Rgb, Settings, ThemeOverrides, DEFAULT_DARK_BACKGROUND,
    DEFAULT_LIGHT_BACKGROUND,
};
use crate::screen::{PaneView, Screen, ScreenSet, TabContent, WorkspaceView};

const TAB_HEIGHT: f64 = 28.0;
const TAB_FADE_WIDTH: f64 = 100.0;
const SIDEBAR_SCRIM_HEIGHT: f64 = 50.0;
const UNFOCUSED_PANE_OPACITY: f64 = 0.70;
const DIVIDER_HIT_SIZE: f64 = 6.0;
const DIVIDER_THROTTLE_INTERVAL: Duration = Duration::from_millis(45);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlinkState {
    window_active: bool,
    phase_visible: bool,
}

impl BlinkState {
    pub fn new(window_active: bool) -> Self {
        Self {
            window_active,
            phase_visible: true,
        }
    }

    pub fn set_window_active(&mut self, active: bool) -> bool {
        let changed = self.window_active != active || !self.phase_visible;
        self.window_active = active;
        self.phase_visible = true;
        changed
    }

    pub fn tick(&mut self, has_blinking_content: bool) -> bool {
        if self.window_active && has_blinking_content {
            self.phase_visible = !self.phase_visible;
            true
        } else if !self.phase_visible {
            self.phase_visible = true;
            true
        } else {
            false
        }
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

pub struct Theme {
    font: pango::FontDescription,
    chrome_mode: ChromeMode,
    overrides: ThemeOverrides,
    chrome: RefCell<ChromeColors>,
}

impl Theme {
    pub fn new(settings: Settings) -> Self {
        let fallback = match settings.chrome {
            ChromeMode::Light => DEFAULT_LIGHT_BACKGROUND,
            ChromeMode::Auto | ChromeMode::Dark => DEFAULT_DARK_BACKGROUND,
        };
        Self {
            font: pango::FontDescription::from_string(&settings.font),
            chrome_mode: settings.chrome,
            overrides: settings.theme,
            chrome: RefCell::new(ChromeColors::derive(
                fallback,
                settings.chrome,
                settings.theme,
            )),
        }
    }

    pub fn refresh_chrome(&self, background: Rgb) -> bool {
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
}

#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f64,
    pub height: f64,
    /// Distance from the top of a cell to the text baseline.
    #[allow(dead_code)]
    pub baseline: f64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
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

#[derive(Clone, Debug)]
pub struct TabHit {
    pub id: TabId,
    pub rect: Rect,
    pub terminal: Option<TerminalId>,
}

#[derive(Clone, Debug)]
pub struct PaneGeometry {
    pub pane: PaneId,
    pub rect: Rect,
    pub content: Rect,
    pub terminal: Option<TerminalId>,
    pub tabs: Vec<TabHit>,
    pub stack_header: bool,
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
    context.set_font_description(Some(&theme.font));
    let metrics = context.metrics(Some(&theme.font), None);
    let width = f64::from(metrics.approximate_digit_width()) / f64::from(pango::SCALE);
    let ascent = f64::from(metrics.ascent()) / f64::from(pango::SCALE);
    let descent = f64::from(metrics.descent()) / f64::from(pango::SCALE);
    CellMetrics {
        width,
        height: ascent + descent,
        baseline: ascent,
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
    let area = Rect {
        x: 0.0,
        y: 0.0,
        width: f64::from(width.max(0)),
        height: f64::from(height.max(0)),
    };
    let mut panes = Vec::new();
    if let Some(zoomed) = workspace.layout.zoomed_pane_id.as_ref() {
        push_pane(workspace, zoomed, area, false, metrics, &mut panes);
    } else {
        walk_layout(&workspace.layout.root, area, workspace, metrics, &mut panes);
    }
    panes
}

pub fn split_dividers(screens: &ScreenSet, width: i32, height: i32) -> Vec<SplitDivider> {
    let Some(workspace) = screens.workspace.as_ref() else {
        return Vec::new();
    };
    if workspace.layout.zoomed_pane_id.is_some() {
        return Vec::new();
    }
    let area = Rect {
        x: 0.0,
        y: 0.0,
        width: f64::from(width.max(0)),
        height: f64::from(height.max(0)),
    };
    let mut dividers = Vec::new();
    walk_dividers(
        &workspace.layout.root,
        area,
        &workspace.layout.active_pane_id,
        &mut dividers,
    );
    dividers
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
    let tab_width = if pane.tabs.is_empty() {
        0.0
    } else {
        rect.width / pane.tabs.len() as f64
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

fn run_colors(
    run: &RenderRun,
    default_fg: &ColorHex,
    default_bg: &ColorHex,
) -> ((f64, f64, f64), (f64, f64, f64)) {
    let pick = |value: &Option<ColorHex>, fallback: &ColorHex| {
        parse_color(value.as_ref().unwrap_or(fallback).as_str())
    };
    let mut fg = pick(&run.fg, default_fg);
    let mut bg = pick(&run.bg, default_bg);
    if run.has_attr(RenderRun::ATTR_INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if run.has_attr(RenderRun::ATTR_FAINT) {
        fg = (fg.0 * 0.6, fg.1 * 0.6, fg.2 * 0.6);
    }
    (fg, bg)
}

pub fn build(
    screens: Rc<RefCell<ScreenSet>>,
    theme: Rc<Theme>,
    blink: Rc<Cell<BlinkState>>,
) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_focusable(true);
    area.set_hexpand(true);
    area.set_vexpand(true);

    area.set_draw_func(move |area, cr, width, height| {
        let chrome = theme.chrome();
        let (red, green, blue) = chrome.background.cairo();
        cr.set_source_rgb(red, green, blue);
        let _ = cr.paint();

        let screens = screens.borrow();
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
                    let (r, g, b) = parse_color(screen.default_bg.as_str());
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
                        area,
                        cr,
                        screen,
                        metrics,
                        geometry.content.height,
                        geometry.pane == workspace.layout.active_pane_id,
                        &theme,
                        blink.get(),
                    );
                    let _ = cr.restore();
                }
            } else {
                draw_browser_placeholder(area, cr, geometry.content, &theme);
            }

            if !geometry.tabs.is_empty() {
                draw_tabs(
                    area,
                    cr,
                    workspace.pane(&geometry.pane),
                    geometry,
                    &chrome,
                    blink.get().window_active(),
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
            let _ = cr.restore();
            draw_border(cr, geometry.rect, focused, &chrome);
        }
    });
    area
}

pub fn build_sidebar_scrims(theme: Rc<Theme>) -> DrawingArea {
    let scrims = DrawingArea::new();
    scrims.set_hexpand(true);
    scrims.set_vexpand(true);
    scrims.set_can_target(false);
    scrims.set_draw_func(move |_, cr, width, height| {
        let colors = theme.chrome();
        let (red, green, blue) = colors.sidebar_background.cairo();
        let height = f64::from(height);
        let scrim_height = SIDEBAR_SCRIM_HEIGHT.min(height / 2.0);
        if scrim_height <= 0.0 {
            return;
        }

        let top = gtk4::cairo::LinearGradient::new(0.0, 0.0, 0.0, scrim_height);
        top.add_color_stop_rgba(0.0, red, green, blue, 1.0);
        top.add_color_stop_rgba(1.0, red, green, blue, 0.0);
        let _ = cr.set_source(&top);
        cr.rectangle(0.0, 0.0, f64::from(width), scrim_height);
        let _ = cr.fill();

        let bottom = gtk4::cairo::LinearGradient::new(0.0, height - scrim_height, 0.0, height);
        bottom.add_color_stop_rgba(0.0, red, green, blue, 0.0);
        bottom.add_color_stop_rgba(1.0, red, green, blue, 1.0);
        let _ = cr.set_source(&bottom);
        cr.rectangle(0.0, height - scrim_height, f64::from(width), scrim_height);
        let _ = cr.fill();
    });
    scrims
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TaskStatus {
    Neutral,
    Running,
    Attention,
    Done,
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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
    cr.arc(center_x, center_y, 3.75, 0.0, std::f64::consts::TAU);
    let _ = cr.stroke();
}

fn draw_grid(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    metrics: CellMetrics,
    height: f64,
    draw_selection: bool,
    theme: &Theme,
    blink: BlinkState,
) {
    if !screen.is_initialized() {
        return;
    }
    draw_grid_backgrounds(cr, screen, metrics, height);
    if draw_selection && screen.selection.is_some() {
        let (r, g, b) = theme.chrome().selection_background.cairo();
        cr.set_source_rgb(r, g, b);
        selection_path(cr, screen, metrics);
        let _ = cr.fill();
    }
    draw_grid_text(area, cr, screen, metrics, height, &theme.font, None, blink);
    if draw_selection && screen.selection.is_some() {
        if let Some(foreground) = theme.chrome().selection_foreground {
            let _ = cr.save();
            selection_path(cr, screen, metrics);
            cr.clip();
            draw_grid_text(
                area,
                cr,
                screen,
                metrics,
                height,
                &theme.font,
                Some(foreground),
                blink,
            );
            let _ = cr.restore();
        }
    }

    if let Some(cursor) = &screen.cursor {
        if !cursor.visible {
            return;
        }
        let presentation = cursor_presentation(cursor.style, cursor.blink, blink);
        if presentation == CursorPresentation::Hidden {
            return;
        }
        let x = f64::from(cursor.x) * metrics.width;
        let y = f64::from(cursor.y) * metrics.height;
        let (r, g, b) = parse_color(cursor.color.as_ref().unwrap_or(&screen.default_fg).as_str());
        cr.set_source_rgba(r, g, b, if blink.window_active { 0.75 } else { 0.5 });
        if presentation == CursorPresentation::Hollow {
            cr.set_line_width(1.0);
            cr.rectangle(x + 0.5, y + 0.5, metrics.width - 1.0, metrics.height - 1.0);
            let _ = cr.stroke();
            return;
        }
        match cursor.style {
            RenderCursorStyle::Block => cr.rectangle(x, y, metrics.width, metrics.height),
            RenderCursorStyle::Underline => {
                cr.rectangle(x, y + metrics.height - 2.0, metrics.width, 2.0);
            }
            RenderCursorStyle::Bar => cr.rectangle(x, y, 2.0, metrics.height),
        }
        let _ = cr.fill();
    }
}

fn draw_grid_backgrounds(
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    metrics: CellMetrics,
    height: f64,
) {
    for (index, row) in screen.rows.iter().enumerate() {
        let Some(row) = row else { continue };
        let y = index as f64 * metrics.height;
        if y >= height {
            break;
        }
        let mut column = 0u32;
        for run in &row.runs {
            // The server's width hint wins whenever Unicode text does not map
            // one-to-one onto terminal grid columns.
            let cells = run
                .width_hint
                .map(u32::from)
                .unwrap_or_else(|| run.text.chars().count() as u32);
            let x = f64::from(column) * metrics.width;
            let run_width = f64::from(cells) * metrics.width;
            let (_, bg) = run_colors(run, &screen.default_fg, &screen.default_bg);
            cr.set_source_rgb(bg.0, bg.1, bg.2);
            cr.rectangle(x, y, run_width, metrics.height);
            let _ = cr.fill();
            column += cells;
        }
    }
}

fn draw_grid_text(
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    screen: &Screen,
    metrics: CellMetrics,
    height: f64,
    font: &pango::FontDescription,
    foreground: Option<Rgb>,
    blink: BlinkState,
) {
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
            let cells = run
                .width_hint
                .map(u32::from)
                .unwrap_or_else(|| run.text.chars().count() as u32);
            if !run.has_attr(RenderRun::ATTR_INVISIBLE)
                && blinking_content_visible(run.has_attr(RenderRun::ATTR_BLINK), blink)
                && !run.text.trim().is_empty()
            {
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
                layout.set_text(&run.text);
                let color = foreground
                    .map(Rgb::cairo)
                    .unwrap_or_else(|| run_colors(run, &screen.default_fg, &screen.default_bg).0);
                cr.set_source_rgb(color.0, color.1, color.2);
                cr.move_to(f64::from(column) * metrics.width, y);
                pangocairo::functions::show_layout(cr, &layout);
            }
            column += cells;
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
    area: &DrawingArea,
    cr: &gtk4::cairo::Context,
    pane: Option<&PaneView>,
    geometry: &PaneGeometry,
    colors: &ChromeColors,
    window_active: bool,
) {
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
        layout.set_width(((hit.rect.width - 12.0).max(1.0) * f64::from(pango::SCALE)) as i32);
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
    }
    draw_tab_fade_mask(cr, geometry, colors);
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
    layout.set_font_description(Some(&theme.font));
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

/// Translates a GDK key press into the bytes a PTY expects.
///
/// Returns None for keys this frontend does not handle, so GTK keeps its own
/// default handling for them.
pub fn key_to_bytes(key: gdk::Key, state: gdk::ModifierType) -> Option<Vec<u8>> {
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
    let alt = state.contains(gdk::ModifierType::ALT_MASK);

    let base: Vec<u8> = match key {
        gdk::Key::Return | gdk::Key::KP_Enter => vec![b'\r'],
        gdk::Key::BackSpace => vec![0x7f],
        gdk::Key::Tab => vec![b'\t'],
        gdk::Key::Escape => vec![0x1b],
        gdk::Key::Up => b"\x1b[A".to_vec(),
        gdk::Key::Down => b"\x1b[B".to_vec(),
        gdk::Key::Right => b"\x1b[C".to_vec(),
        gdk::Key::Left => b"\x1b[D".to_vec(),
        gdk::Key::Home => b"\x1b[H".to_vec(),
        gdk::Key::End => b"\x1b[F".to_vec(),
        gdk::Key::Page_Up => b"\x1b[5~".to_vec(),
        gdk::Key::Page_Down => b"\x1b[6~".to_vec(),
        gdk::Key::Delete => b"\x1b[3~".to_vec(),
        _ => {
            let unicode = key.to_unicode()?;
            if ctrl {
                let upper = unicode.to_ascii_uppercase();
                if ('@'..='_').contains(&upper) {
                    vec![(upper as u8) & 0x1f]
                } else if unicode == ' ' {
                    vec![0]
                } else {
                    return None;
                }
            } else {
                let mut buffer = [0u8; 4];
                unicode.encode_utf8(&mut buffer).as_bytes().to_vec()
            }
        }
    };

    if alt {
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
        LayoutDocument, LayoutLeaf, LayoutSplit, PaneId, ScreenId, SplitId, TabId, TerminalId,
        WorkspaceId,
    };

    use super::*;
    use crate::screen::{PaneView, ScreenSet, TabContent, TabView, WorkspaceView};

    fn pane(number: u8) -> PaneId {
        PaneId::parse(format!("pane_{number:032x}")).unwrap()
    }

    fn tab(number: u8) -> TabId {
        TabId::parse(format!("tab_{number:032x}")).unwrap()
    }

    fn terminal(number: u8) -> TerminalId {
        TerminalId::parse(format!("term_{number:032x}")).unwrap()
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
    fn tab_strip_is_always_28_points_and_exposes_browser_hitbox() {
        let screens = split_workspace(true);
        let metrics = CellMetrics {
            width: 10.0,
            height: 20.0,
            baseline: 15.0,
        };
        let panes = pane_geometries(&screens, metrics, 1000, 600);
        assert_eq!(panes[0].tabs.len(), 2);
        assert_eq!(panes[0].content.y, 28.0);
        assert_eq!(panes[0].tabs[1].terminal, None);
        assert!(panes[0].tabs[1].rect.contains(375.0, 10.0));
        let sizes = visible_terminal_sizes(&screens, metrics, 1000, 600);
        assert_eq!(sizes[0].1, Size { cols: 50, rows: 28 });
        assert_eq!(sizes[1].1, Size { cols: 50, rows: 28 });
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
}
