//! Cell-grid rendering and keyboard input.
//!
//! Drawing is deliberately literal: the server already resolved palettes, OSC
//! overrides and inverse-video source colours, so a run is filled with its
//! background and stamped with its text. Nothing here re-interprets terminal
//! semantics.

use std::cell::RefCell;
use std::rc::Rc;


use gtk4::pango;
use gtk4::prelude::*;
use gtk4::{DrawingArea, gdk};

use cmux::{ColorHex, RenderCursorStyle, RenderRun, RenderUnderline};

use crate::screen::Screen;

pub const FONT: &str = "monospace 11";

#[derive(Clone, Copy, Debug)]
pub struct CellMetrics {
    pub width: f64,
    pub height: f64,
    /// Distance from the top of a cell to the text baseline.
    #[allow(dead_code)]
    pub baseline: f64,
}

/// Measures one cell from the font itself rather than assuming a size, so the
/// grid lines up with whatever monospace face the desktop resolves.
pub fn cell_metrics(widget: &impl IsA<gtk4::Widget>) -> CellMetrics {
    let context = widget.as_ref().pango_context();
    let description = pango::FontDescription::from_string(FONT);
    context.set_font_description(Some(&description));
    let metrics = context.metrics(Some(&description), None);
    let width = f64::from(metrics.approximate_digit_width()) / f64::from(pango::SCALE);
    let ascent = f64::from(metrics.ascent()) / f64::from(pango::SCALE);
    let descent = f64::from(metrics.descent()) / f64::from(pango::SCALE);
    CellMetrics { width, height: ascent + descent, baseline: ascent }
}

/// `#rrggbb` or `#rgb` to cairo's 0..1 channels. Anything unparseable falls
/// back to mid grey, which is visible rather than invisible: a colour bug
/// should look wrong, not silently blend into the background.
fn parse_color(value: &str) -> (f64, f64, f64) {
    let hex = value.trim_start_matches('#');
    let expand = |slice: &str| u8::from_str_radix(slice, 16).ok();
    let (r, g, b) = match hex.len() {
        6 => (
            expand(&hex[0..2]),
            expand(&hex[2..4]),
            expand(&hex[4..6]),
        ),
        3 => (
            expand(&hex[0..1]).map(|v| v * 17),
            expand(&hex[1..2]).map(|v| v * 17),
            expand(&hex[2..3]).map(|v| v * 17),
        ),
        _ => (None, None, None),
    };
    match (r, g, b) {
        (Some(r), Some(g), Some(b)) => {
            (f64::from(r) / 255.0, f64::from(g) / 255.0, f64::from(b) / 255.0)
        }
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

pub fn build(screen: Rc<RefCell<Screen>>) -> DrawingArea {
    let area = DrawingArea::new();
    area.set_focusable(true);
    area.set_hexpand(true);
    area.set_vexpand(true);

    area.set_draw_func(move |area, cr, width, height| {
        let screen = screen.borrow();
        let metrics = cell_metrics(area);

        let (br, bg, bb) = parse_color(screen.default_bg.as_str());
        cr.set_source_rgb(br, bg, bb);
        let _ = cr.paint();

        if !screen.is_initialized() {
            return;
        }

        let layout = area.create_pango_layout(None);
        let mut description = pango::FontDescription::from_string(FONT);

        for (index, row) in screen.rows.iter().enumerate() {
            let Some(row) = row else { continue };
            let y = index as f64 * metrics.height;
            if y > f64::from(height) {
                break;
            }

            let mut column = 0u32;
            for run in &row.runs {
                // width_hint is authoritative: the server sends it exactly when
                // a wide grapheme or spacer makes the client's own Unicode
                // width calculation ambiguous.
                let cells = run
                    .width_hint
                    .map(u32::from)
                    .unwrap_or_else(|| run.text.chars().count() as u32);
                let x = f64::from(column) * metrics.width;
                let run_width = f64::from(cells) * metrics.width;

                let (fg, bg) = run_colors(run, &screen.default_fg, &screen.default_bg);

                cr.set_source_rgb(bg.0, bg.1, bg.2);
                cr.rectangle(x, y, run_width, metrics.height);
                let _ = cr.fill();

                if !run.has_attr(RenderRun::ATTR_INVISIBLE) && !run.text.trim().is_empty() {
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
                            // Pango has no dotted/dashed/curly distinction that
                            // maps cleanly here; a single underline is the
                            // closest honest approximation.
                            _ => pango::Underline::Single,
                        }));
                    }
                    if run.has_attr(RenderRun::ATTR_STRIKETHROUGH) {
                        attributes.insert(pango::AttrInt::new_strikethrough(true));
                    }
                    layout.set_attributes(Some(&attributes));
                    layout.set_text(&run.text);

                    cr.set_source_rgb(fg.0, fg.1, fg.2);
                    cr.move_to(x, y);
                    pangocairo::functions::show_layout(cr, &layout);
                }

                column += cells;
            }
        }

        if let Some(cursor) = &screen.cursor {
            if cursor.visible {
                let x = f64::from(cursor.x) * metrics.width;
                let y = f64::from(cursor.y) * metrics.height;
                let (r, g, b) = parse_color(
                    cursor.color.as_ref().unwrap_or(&screen.default_fg).as_str(),
                );
                cr.set_source_rgba(r, g, b, 0.75);
                match cursor.style {
                    RenderCursorStyle::Block => {
                        cr.rectangle(x, y, metrics.width, metrics.height);
                    }
                    RenderCursorStyle::Underline => {
                        cr.rectangle(x, y + metrics.height - 2.0, metrics.width, 2.0);
                    }
                    RenderCursorStyle::Bar => {
                        cr.rectangle(x, y, 2.0, metrics.height);
                    }
                }
                let _ = cr.fill();
            }
        }

        let _ = width;
    });

    area
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
                // Ctrl-A..Ctrl-Z and the handful of control codes above them.
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
