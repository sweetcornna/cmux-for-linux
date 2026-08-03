//! cmux-gtk — a GTK4 frontend for cmux.
//!
//! The Linux GUI described in docs/linux-port.md: a window that speaks
//! `cmux.protocol/1` to a running session, lists its workspaces, and renders
//! the focused screen from the server's styled render streams.
//!
//! The server stays the only terminal emulator. This process draws styled runs
//! and forwards input; it contains no VT parser and links no terminal
//! emulation library.

mod config;
mod screen;
mod session;
mod view;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use cmux::{
    Direction, InputModifier, LayoutDirection, MouseButton, PaneId, ScreenId, TerminalId,
    TerminalMouseKind, TerminalMouseOptions, WorkspaceId,
};
use gtk4::prelude::*;
use gtk4::{gdk, gio};
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, Button, CenterBox, CssProvider,
    DrawingArea, Entry, EventControllerKey, EventControllerMotion, EventControllerScroll,
    EventControllerScrollFlags, GestureClick, GestureDrag, Label, ListBox, ListBoxRow, Orientation,
    Overlay, PackType, Paned, Popover, PopoverMenu, ScrolledWindow, SelectionMode, Widget,
    WindowControls,
};

use screen::ScreenSet;
use session::{AttachmentSpec, Control, Input, Update, Worker};

const APP_ID: &str = "com.github.sweetcornna.cmux-gtk";
const SCROLL_ROWS: i32 = 3;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);
const SIDEBAR_WIDTH: i32 = 240;
const SIDEBAR_MAX_WIDTH: i32 = 600;

struct MouseTarget {
    terminal: TerminalId,
    row: u16,
    column: u16,
    can_scroll_locally: bool,
}

#[derive(Clone)]
struct PaneTarget {
    workspace: WorkspaceId,
    screen: ScreenId,
    pane: PaneId,
}

#[derive(Clone)]
struct TabTarget {
    pane: PaneTarget,
    tab: cmux::TabId,
    terminal: Option<TerminalId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum RenameTarget {
    Screen {
        workspace: WorkspaceId,
        screen: ScreenId,
    },
    Tab {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        tab: cmux::TabId,
    },
    Workspace(WorkspaceId),
}

struct RenamePrompt {
    popover: Popover,
    entry: Entry,
    target: Rc<RefCell<Option<RenameTarget>>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PrefixAction {
    NewTab,
    NextTab,
    PrevTab,
    CloseTab,
    SplitRight,
    SplitDown,
    ClosePane,
    RenameScreen,
    RenameWorkspace,
    CloseScreen,
}

struct DividerDrag {
    target: PaneTarget,
    divider: view::SplitDivider,
    start_x: f64,
    start_y: f64,
    started: Instant,
    throttle: view::DividerDragThrottle,
}

struct Args {
    session: String,
    socket: Option<PathBuf>,
}

fn parse_args() -> Args {
    let mut session = std::env::var("CMUX_SESSION").unwrap_or_else(|_| "main".to_string());
    let mut socket = None;
    let mut argv = std::env::args().skip(1);
    while let Some(argument) = argv.next() {
        match argument.as_str() {
            "--session" => {
                if let Some(value) = argv.next() {
                    session = value;
                }
            }
            "--socket" => socket = argv.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!(
                    "cmux-gtk — GTK4 frontend for cmux\n\n\
                     USAGE\n  \
                     cmux-gtk [--session <name>] [--socket <path>] [--probe]\n\n\
                     Connects to a cmux session, starting a headless one when needed.\n\n\
                     --probe runs the protocol workers without GTK or automatic session\n\
                     startup and prints every update, which separates protocol failures\n\
                     from drawing ones."
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args { session, socket }
}

fn probe() -> gtk4::glib::ExitCode {
    let args = parse_args();
    let (tx, rx) = async_channel::unbounded::<Update>();
    let worker = session::spawn(args.session.clone(), args.socket.clone(), false, tx);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(Update::Snapshot { terminal, render }) => {
                let (images, placements) = render.graphics.as_ref().map_or((0, 0), |graphics| {
                    (
                        graphics.images.as_ref().map_or(0, Vec::len),
                        graphics.placements.len(),
                    )
                });
                println!(
                    "snapshot terminal={terminal:?} size={}x{} rows={} images={} placements={} fg={} bg={}",
                    render.size.cols,
                    render.size.rows,
                    render.rows.len(),
                    images,
                    placements,
                    render.default_fg.as_str(),
                    render.default_bg.as_str()
                );
            }
            Ok(Update::Patch { terminal, render }) => {
                let (images, placements, removed) =
                    render.graphics.as_ref().map_or((0, 0, 0), |graphics| {
                        (
                            graphics.images.as_ref().map_or(0, Vec::len),
                            graphics.placements.as_ref().map_or(0, Vec::len),
                            graphics.removed_image_ids.as_ref().map_or(0, Vec::len),
                        )
                    });
                println!(
                    "patch terminal={terminal:?} full_reset={} rows={} image_upserts={} placements={} image_removals={}",
                    render.full_reset,
                    render.rows.len(),
                    images,
                    placements,
                    removed
                );
            }
            Ok(Update::Workspaces(list)) => {
                let specs = list
                    .iter()
                    .find(|entry| entry.focused)
                    .or_else(|| list.first())
                    .and_then(|entry| entry.view.as_ref())
                    .into_iter()
                    .flat_map(|view| &view.panes)
                    .filter_map(|pane| pane.active_terminal().cloned())
                    .map(|terminal| AttachmentSpec {
                        terminal,
                        size: cmux::Size {
                            cols: 100,
                            rows: 30,
                        },
                    })
                    .collect();
                let _ = worker.control.send(Control::SyncAttachments(specs));
                for entry in list {
                    println!(
                        "workspace {:?} focused={} terminals={}",
                        entry.name,
                        entry.focused,
                        entry.terminals.len()
                    );
                }
            }
            Ok(other) => println!("{other:?}"),
            Err(error) => {
                println!("channel closed: {error}");
                break;
            }
        }
    }
    gtk4::glib::ExitCode::SUCCESS
}

fn main() -> gtk4::glib::ExitCode {
    if std::env::args().any(|argument| argument == "--probe") {
        return probe();
    }
    let application = Application::builder().application_id(APP_ID).build();
    application.connect_activate(build_ui);
    // Arguments are parsed before GTK sees argv so `--session` is not mistaken
    // for a GApplication option.
    application.run_with_args::<&str>(&[])
}

fn sync_attachments(
    worker: &Worker,
    screens: &ScreenSet,
    terminal: &DrawingArea,
    theme: &view::Theme,
) {
    let metrics = view::cell_metrics(terminal, theme);
    let specs = view::visible_terminal_sizes(screens, metrics, terminal.width(), terminal.height())
        .into_iter()
        .map(|(terminal, size)| AttachmentSpec { terminal, size })
        .collect();
    let _ = worker.control.send(Control::SyncAttachments(specs));
}

fn mouse_target(
    screens: &ScreenSet,
    terminal: &DrawingArea,
    metrics: view::CellMetrics,
    x: f64,
    y: f64,
) -> Option<MouseTarget> {
    let pane = view::pane_geometries(screens, metrics, terminal.width(), terminal.height())
        .into_iter()
        .find(|pane| pane.content.contains(x, y) && pane.terminal.is_some())?;
    let terminal_id = pane.terminal?;
    let screen = screens.grids.get(&terminal_id)?;
    let (row, column) = view::cell_at(metrics, screen.size, x - pane.content.x, y - pane.content.y);
    Some(MouseTarget {
        terminal: terminal_id,
        row,
        column: column.min(screen.size.cols.saturating_sub(1)),
        can_scroll_locally: screen.scrollback_rows > 0 || !screen.at_bottom,
    })
}

fn focused_pane_target(screens: &ScreenSet) -> Option<PaneTarget> {
    let workspace = screens.workspace.as_ref()?;
    Some(PaneTarget {
        workspace: workspace.workspace_id.clone(),
        screen: workspace.screen_id.clone(),
        pane: workspace.layout.active_pane_id.clone(),
    })
}

fn focused_tab_target(screens: &ScreenSet) -> Option<TabTarget> {
    let workspace = screens.workspace.as_ref()?;
    let pane = workspace.active_pane()?;
    let tab = pane.active_tab()?;
    Some(TabTarget {
        pane: PaneTarget {
            workspace: workspace.workspace_id.clone(),
            screen: workspace.screen_id.clone(),
            pane: pane.id.clone(),
        },
        tab: tab.id.clone(),
        terminal: match &tab.content {
            screen::TabContent::Terminal(terminal) => Some(terminal.clone()),
            screen::TabContent::Browser => None,
        },
    })
}

fn adjacent_tab_target(screens: &ScreenSet, offset: i32) -> Option<TabTarget> {
    let workspace = screens.workspace.as_ref()?;
    let pane = workspace.active_pane()?;
    let tab = pane.adjacent_tab(offset)?;
    Some(TabTarget {
        pane: PaneTarget {
            workspace: workspace.workspace_id.clone(),
            screen: workspace.screen_id.clone(),
            pane: pane.id.clone(),
        },
        tab: tab.id.clone(),
        terminal: match &tab.content {
            screen::TabContent::Terminal(terminal) => Some(terminal.clone()),
            screen::TabContent::Browser => None,
        },
    })
}

fn divider_at(
    screens: &ScreenSet,
    terminal: &DrawingArea,
    x: f64,
    y: f64,
) -> Option<view::SplitDivider> {
    let dividers = view::split_dividers(screens, terminal.width(), terminal.height());
    view::split_divider_at(&dividers, x, y).cloned()
}

fn resize_cursor(direction: LayoutDirection) -> &'static str {
    match direction {
        LayoutDirection::Horizontal => "col-resize",
        LayoutDirection::Vertical => "row-resize",
    }
}

fn send_split(worker: &Worker, target: &PaneTarget, direction: Direction) {
    let _ = worker.input.send(Input::SplitPane {
        workspace: target.workspace.clone(),
        screen: target.screen.clone(),
        pane: target.pane.clone(),
        direction,
    });
}

fn send_close(worker: &Worker, target: &PaneTarget) {
    let _ = worker.input.send(Input::ClosePane {
        workspace: target.workspace.clone(),
        screen: target.screen.clone(),
        pane: target.pane.clone(),
    });
}

fn send_focus_tab(worker: &Worker, target: &TabTarget) {
    let _ = worker.input.send(Input::FocusTab {
        workspace: target.pane.workspace.clone(),
        screen: target.pane.screen.clone(),
        pane: target.pane.pane.clone(),
        tab: target.tab.clone(),
        target: target.terminal.clone(),
    });
}

fn prefix_action(key: gdk::Key) -> Option<PrefixAction> {
    match key {
        gdk::Key::t => Some(PrefixAction::NewTab),
        gdk::Key::Tab => Some(PrefixAction::NextTab),
        gdk::Key::ISO_Left_Tab => Some(PrefixAction::PrevTab),
        gdk::Key::x => Some(PrefixAction::CloseTab),
        gdk::Key::percent => Some(PrefixAction::SplitRight),
        gdk::Key::quotedbl => Some(PrefixAction::SplitDown),
        gdk::Key::X => Some(PrefixAction::ClosePane),
        gdk::Key::comma => Some(PrefixAction::RenameScreen),
        gdk::Key::dollar => Some(PrefixAction::RenameWorkspace),
        gdk::Key::ampersand => Some(PrefixAction::CloseScreen),
        _ => None,
    }
}

fn rename_submission(text: &str) -> Option<String> {
    let name = text.trim();
    (!name.is_empty()).then(|| name.to_string())
}

fn workspace_drop_index(
    source: usize,
    delta_y: f64,
    row_height: f64,
    count: usize,
) -> Option<usize> {
    if source >= count || count == 0 || !delta_y.is_finite() || row_height <= 0.0 {
        return None;
    }
    let delta = (delta_y / row_height).round() as isize;
    Some((source as isize + delta).clamp(0, count.saturating_sub(1) as isize) as usize)
}

fn build_rename_prompt(worker: Rc<Worker>, toast: Label) -> RenamePrompt {
    let entry = Entry::new();
    entry.set_max_length(120);
    entry.set_width_chars(24);
    entry.add_css_class("rename-entry");
    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&entry);
    let popover = Popover::new();
    popover.add_css_class("cmux-menu");
    popover.add_css_class("rename-prompt");
    popover.set_autohide(true);
    popover.set_has_arrow(true);
    popover.set_child(Some(&content));
    let target = Rc::new(RefCell::new(None::<RenameTarget>));

    {
        let target = Rc::clone(&target);
        let worker = Rc::clone(&worker);
        let popover = popover.clone();
        let toast = toast.clone();
        entry.connect_activate(move |entry| {
            let Some(name) = rename_submission(entry.text().as_str()) else {
                set_toast(&toast, "Name cannot be empty");
                return;
            };
            let Some(target) = target.borrow_mut().take() else {
                return;
            };
            let input = match target {
                RenameTarget::Screen { workspace, screen } => Input::RenameScreen {
                    workspace,
                    screen,
                    name,
                },
                RenameTarget::Tab {
                    workspace,
                    screen,
                    pane,
                    tab,
                } => Input::RenameTab {
                    workspace,
                    screen,
                    pane,
                    tab,
                    name,
                },
                RenameTarget::Workspace(workspace) => Input::RenameWorkspace { workspace, name },
            };
            let _ = worker.input.send(input);
            popover.popdown();
        });
    }
    {
        let target = Rc::clone(&target);
        let popover = popover.clone();
        let keys = EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            if key != gdk::Key::Escape {
                return gtk4::glib::Propagation::Proceed;
            }
            target.borrow_mut().take();
            popover.popdown();
            gtk4::glib::Propagation::Stop
        });
        entry.add_controller(keys);
    }
    {
        let target = Rc::clone(&target);
        popover.connect_closed(move |_| {
            target.borrow_mut().take();
        });
    }

    RenamePrompt {
        popover,
        entry,
        target,
    }
}

fn open_rename_prompt(
    prompt: &RenamePrompt,
    parent: &impl IsA<Widget>,
    pointing_to: gdk::Rectangle,
    target: RenameTarget,
    current_name: &str,
) {
    prompt.popover.popdown();
    if prompt.popover.parent().is_some() {
        prompt.popover.unparent();
    }
    prompt.popover.set_parent(parent);
    prompt.popover.set_pointing_to(Some(&pointing_to));
    prompt.entry.set_text(current_name);
    prompt.entry.select_region(0, -1);
    *prompt.target.borrow_mut() = Some(target);
    prompt.popover.popup();
    prompt.entry.grab_focus();
}

fn send_split_ratio(
    worker: &Worker,
    target: &PaneTarget,
    divider: &view::SplitDivider,
    ratio: f64,
) {
    let _ = worker.input.send(Input::SetSplitRatio {
        workspace: target.workspace.clone(),
        screen: target.screen.clone(),
        pane: target.pane.clone(),
        split: divider.split_id.clone(),
        ratio,
    });
}

fn input_modifiers(state: gdk::ModifierType) -> Vec<InputModifier> {
    let mut modifiers = Vec::new();
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        modifiers.push(InputModifier::Shift);
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        modifiers.push(InputModifier::Control);
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        modifiers.push(InputModifier::Alt);
    }
    if state.intersects(
        gdk::ModifierType::META_MASK
            | gdk::ModifierType::SUPER_MASK
            | gdk::ModifierType::HYPER_MASK,
    ) {
        modifiers.push(InputModifier::Meta);
    }
    modifiers
}

fn mouse_button(button: u32) -> Option<MouseButton> {
    match button {
        1 => Some(MouseButton::Left),
        2 => Some(MouseButton::Middle),
        3 => Some(MouseButton::Right),
        _ => None,
    }
}

fn mouse_button_held(state: gdk::ModifierType) -> bool {
    state.intersects(
        gdk::ModifierType::BUTTON1_MASK
            | gdk::ModifierType::BUTTON2_MASK
            | gdk::ModifierType::BUTTON3_MASK,
    )
}

fn clamp_sidebar_width(requested: i32, window_width: i32) -> i32 {
    let maximum = (window_width.max(0) / 3).min(SIDEBAR_MAX_WIDTH);
    if maximum < SIDEBAR_WIDTH {
        maximum
    } else {
        requested.clamp(SIDEBAR_WIDTH, maximum)
    }
}

fn clamp_sidebar(paned: &Paned, window: &ApplicationWindow) {
    if window.width() <= 0 {
        return;
    }
    let position = clamp_sidebar_width(paned.position(), window.width());
    if position != paned.position() {
        paned.set_position(position);
    }
}

fn set_toast(toast: &Label, message: &str) {
    toast.set_text(message);
    toast.set_visible(true);
}

fn clear_toast(toast: &Label) {
    toast.set_visible(false);
}

fn refresh_chrome(
    screens: &ScreenSet,
    theme: &view::Theme,
    provider: &CssProvider,
    terminal: &DrawingArea,
    sidebar_scrims: &DrawingArea,
    workspaces: &ListBox,
) {
    let Some(background) = screens
        .focused_screen()
        .and_then(|screen| config::Rgb::parse(screen.default_bg.as_str()))
    else {
        return;
    };
    if theme.refresh_chrome(background) {
        provider.load_from_data(&theme.css());
        terminal.queue_draw();
        sidebar_scrims.queue_draw();
        workspaces.queue_draw();
    }
}

fn workspace_row(
    entry: &session::WorkspaceEntry,
    theme: Rc<view::Theme>,
    worker: Rc<Worker>,
    rename_prompt: Rc<RenamePrompt>,
    drop_indicators: Rc<RefCell<Vec<GtkBox>>>,
    workspace_count: usize,
) -> ListBoxRow {
    let title = if entry.name.is_empty() {
        "(unnamed)"
    } else {
        &entry.name
    };
    let label = Label::new(Some(title));
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
    label.set_single_line_mode(true);
    label.add_css_class("workspace-title");

    let content = GtkBox::new(Orientation::Horizontal, 8);
    content.add_css_class("workspace-content");
    content.append(&label);
    if entry.terminals.len() > 1 {
        let count = Label::new(Some(&entry.terminals.len().to_string()));
        count.add_css_class("workspace-badge");
        content.append(&count);
    }

    let overlay = Overlay::new();
    overlay.set_child(Some(&content));
    let base_color = entry.color.as_deref().and_then(|value| {
        config::Rgb::parse(value).or_else(|| config::workspace_palette_color(value))
    });
    if let Some(base_color) = base_color {
        let rail = DrawingArea::new();
        rail.set_content_width(3);
        rail.set_halign(Align::Start);
        rail.set_valign(Align::Fill);
        rail.set_can_target(false);
        rail.add_css_class("workspace-rail");
        rail.set_draw_func(move |_, cr, width, height| {
            let chrome = theme.chrome();
            let color = chrome
                .workspace_rail
                .unwrap_or_else(|| config::workspace_display_color(base_color, chrome.is_light));
            let (red, green, blue) = color.cairo();
            let width = f64::from(width);
            let height = f64::from(height);
            cr.set_source_rgba(red, green, blue, 0.95);
            let radius = 1.5_f64.min(width / 2.0).min(height / 2.0);
            cr.new_sub_path();
            cr.arc(
                width - radius,
                radius,
                radius,
                -std::f64::consts::FRAC_PI_2,
                0.0,
            );
            cr.arc(
                width - radius,
                height - radius,
                radius,
                0.0,
                std::f64::consts::FRAC_PI_2,
            );
            cr.arc(
                radius,
                height - radius,
                radius,
                std::f64::consts::FRAC_PI_2,
                std::f64::consts::PI,
            );
            cr.arc(
                radius,
                radius,
                radius,
                std::f64::consts::PI,
                std::f64::consts::PI * 1.5,
            );
            cr.close_path();
            let _ = cr.fill();
        });
        overlay.add_overlay(&rail);
    }

    let close = Button::from_icon_name("window-close-symbolic");
    close.add_css_class("workspace-close");
    close.set_halign(Align::End);
    close.set_valign(Align::Center);
    close.set_tooltip_text(Some("Close workspace"));
    {
        let worker = Rc::clone(&worker);
        let workspace = entry.id.clone();
        close.connect_clicked(move |_| {
            let _ = worker.input.send(Input::CloseWorkspace {
                workspace: workspace.clone(),
            });
        });
    }
    overlay.add_overlay(&close);

    let drop_indicator = GtkBox::new(Orientation::Horizontal, 0);
    drop_indicator.add_css_class("drop-indicator");
    drop_indicator.set_halign(Align::Fill);
    drop_indicator.set_valign(Align::Start);
    drop_indicator.set_can_target(false);
    drop_indicator.set_visible(false);
    overlay.add_overlay(&drop_indicator);
    drop_indicators.borrow_mut().push(drop_indicator);

    let row = ListBoxRow::new();
    row.add_css_class("workspace-row");
    row.set_child(Some(&overlay));

    let rename_click = GestureClick::new();
    rename_click.set_button(1);
    {
        let label = label.clone();
        let rename_prompt = Rc::clone(&rename_prompt);
        let workspace = entry.id.clone();
        let current_name = entry.name.clone();
        rename_click.connect_pressed(move |_, press_count, _, _| {
            if press_count != 2 {
                return;
            }
            open_rename_prompt(
                &rename_prompt,
                &label,
                gdk::Rectangle::new(0, 0, label.width().max(1), label.height().max(1)),
                RenameTarget::Workspace(workspace.clone()),
                &current_name,
            );
        });
    }
    label.add_controller(rename_click);

    let drag = GestureDrag::new();
    drag.set_button(1);
    {
        let row = row.clone();
        drag.connect_drag_begin(move |_, _, _| row.add_css_class("dragging"));
    }
    {
        let row = row.clone();
        let drop_indicators = Rc::clone(&drop_indicators);
        drag.connect_drag_update(move |_, _, delta_y| {
            for indicator in drop_indicators.borrow().iter() {
                indicator.set_visible(false);
            }
            let Ok(source) = usize::try_from(row.index()) else {
                return;
            };
            let row_height = f64::from((row.height() + 2).max(1));
            let Some(target) = workspace_drop_index(source, delta_y, row_height, workspace_count)
            else {
                return;
            };
            if target == source {
                return;
            }
            if let Some(indicator) = drop_indicators.borrow().get(target) {
                indicator.set_valign(if target < source {
                    Align::Start
                } else {
                    Align::End
                });
                indicator.set_visible(true);
            }
        });
    }
    {
        let row = row.clone();
        let drop_indicators = Rc::clone(&drop_indicators);
        let worker = Rc::clone(&worker);
        let workspace = entry.id.clone();
        drag.connect_drag_end(move |_, _, delta_y| {
            row.remove_css_class("dragging");
            for indicator in drop_indicators.borrow().iter() {
                indicator.set_visible(false);
            }
            let Ok(source) = usize::try_from(row.index()) else {
                return;
            };
            let row_height = f64::from((row.height() + 2).max(1));
            let Some(target) = workspace_drop_index(source, delta_y, row_height, workspace_count)
            else {
                return;
            };
            if target != source {
                let _ = worker.input.send(Input::MoveWorkspace {
                    workspace: workspace.clone(),
                    index: target as u32,
                });
            }
        });
    }
    row.add_controller(drag);
    row
}

fn build_ui(application: &Application) {
    let args = parse_args();
    let theme = Rc::new(view::Theme::new(config::load()));
    let screens = Rc::new(RefCell::new(ScreenSet::default()));
    let entries: Rc<RefCell<Vec<session::WorkspaceEntry>>> = Rc::new(RefCell::new(Vec::new()));
    let (update_tx, update_rx) = async_channel::unbounded::<Update>();
    let worker = Rc::new(session::spawn(
        args.session.clone(),
        args.socket.clone(),
        true,
        update_tx,
    ));
    let blink = Rc::new(Cell::new(view::BlinkState::new(false)));
    let tab_strip = Rc::new(view::TabStripState::default());

    let css_provider = CssProvider::new();
    css_provider.load_from_data(&theme.css());
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &css_provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let titlebar = CenterBox::new();
    titlebar.add_css_class("cmux-titlebar");
    let title = Label::new(Some(&format!("cmux — {}", args.session)));
    title.add_css_class("cmux-title");
    titlebar.set_center_widget(Some(&title));
    let controls_start = WindowControls::new(PackType::Start);
    let controls_end = WindowControls::new(PackType::End);
    let new_workspace = Button::from_icon_name("list-add-symbolic");
    new_workspace.add_css_class("titlebar-action");
    new_workspace.set_tooltip_text(Some("New workspace"));
    {
        let worker = Rc::clone(&worker);
        new_workspace.connect_clicked(move |_| {
            let _ = worker.input.send(Input::CreateWorkspace);
        });
    }
    let titlebar_actions = GtkBox::new(Orientation::Horizontal, 0);
    titlebar_actions.add_css_class("titlebar-actions");
    titlebar_actions.append(&new_workspace);
    titlebar_actions.append(&controls_end);
    titlebar.set_start_widget(Some(&controls_start));
    titlebar.set_end_widget(Some(&titlebar_actions));

    let workspaces = ListBox::new();
    workspaces.set_selection_mode(SelectionMode::Single);
    workspaces.add_css_class("workspace-list");
    let drop_indicators: Rc<RefCell<Vec<GtkBox>>> = Rc::new(RefCell::new(Vec::new()));
    let sidebar_scroll = ScrolledWindow::builder()
        .child(&workspaces)
        .hscrollbar_policy(gtk4::PolicyType::Never)
        .build();
    sidebar_scroll.add_css_class("sidebar-surface");
    let sidebar_scrims = view::build_sidebar_scrims(Rc::clone(&theme));
    let sidebar = Overlay::new();
    sidebar.add_css_class("sidebar-surface");
    sidebar.set_child(Some(&sidebar_scroll));
    sidebar.add_overlay(&sidebar_scrims);
    let terminal = view::build(
        Rc::clone(&screens),
        Rc::clone(&theme),
        Rc::clone(&blink),
        Rc::clone(&tab_strip),
    );

    let toast = Label::new(None);
    toast.add_css_class("toast");
    toast.set_halign(Align::Center);
    toast.set_valign(Align::End);
    toast.set_wrap(true);
    toast.set_visible(false);
    let terminal_overlay = Overlay::new();
    terminal_overlay.set_child(Some(&terminal));
    terminal_overlay.add_overlay(&toast);
    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&sidebar)
        .end_child(&terminal_overlay)
        .position(SIDEBAR_WIDTH)
        .resize_start_child(false)
        .shrink_start_child(true)
        .wide_handle(false)
        .build();
    paned.add_css_class("cmux-split");

    let window = ApplicationWindow::builder()
        .application(application)
        .title("cmux")
        .default_width(1000)
        .default_height(700)
        .child(&paned)
        .build();
    window.add_css_class("cmux-window");
    window.set_size_request(300, 200);
    window.set_titlebar(Some(&titlebar));
    let rename_prompt = Rc::new(build_rename_prompt(Rc::clone(&worker), toast.clone()));

    let context_target = Rc::new(RefCell::new(None::<PaneTarget>));
    let pane_menu_model = gio::Menu::new();
    pane_menu_model.append(Some("Split right"), Some("pane.split-right"));
    pane_menu_model.append(Some("Split down"), Some("pane.split-down"));
    pane_menu_model.append(Some("Close pane"), Some("pane.close"));
    let pane_menu = PopoverMenu::from_model(Some(&pane_menu_model));
    pane_menu.add_css_class("cmux-menu");
    pane_menu.set_has_arrow(false);
    pane_menu.set_parent(&terminal);

    let pane_actions = gio::SimpleActionGroup::new();
    let split_right = gio::SimpleAction::new("split-right", None);
    {
        let worker = Rc::clone(&worker);
        let context_target = Rc::clone(&context_target);
        split_right.connect_activate(move |_, _| {
            if let Some(target) = context_target.borrow().as_ref() {
                send_split(&worker, target, Direction::Right);
            }
        });
    }
    pane_actions.add_action(&split_right);
    let split_down = gio::SimpleAction::new("split-down", None);
    {
        let worker = Rc::clone(&worker);
        let context_target = Rc::clone(&context_target);
        split_down.connect_activate(move |_, _| {
            if let Some(target) = context_target.borrow().as_ref() {
                send_split(&worker, target, Direction::Down);
            }
        });
    }
    pane_actions.add_action(&split_down);
    let close_pane = gio::SimpleAction::new("close", None);
    {
        let worker = Rc::clone(&worker);
        let context_target = Rc::clone(&context_target);
        close_pane.connect_activate(move |_, _| {
            if let Some(target) = context_target.borrow().as_ref() {
                send_close(&worker, target);
            }
        });
    }
    pane_actions.add_action(&close_pane);
    terminal.insert_action_group("pane", Some(&pane_actions));

    {
        let window = window.clone();
        let adjusting = Cell::new(false);
        paned.connect_position_notify(move |paned| {
            if adjusting.replace(true) {
                return;
            }
            clamp_sidebar(paned, &window);
            adjusting.set(false);
        });
    }

    // --- focus and blink phase -------------------------------------------
    {
        let blink = Rc::clone(&blink);
        let terminal = terminal.clone();
        window.connect_is_active_notify(move |window| {
            let mut state = blink.get();
            if state.set_window_active(window.is_active()) {
                blink.set(state);
                terminal.queue_draw();
            }
        });
    }
    {
        let blink = Rc::clone(&blink);
        let screens = Rc::clone(&screens);
        let terminal = terminal.downgrade();
        gtk4::glib::timeout_add_local(BLINK_INTERVAL, move || {
            let Some(terminal) = terminal.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            let mut state = blink.get();
            if state.tick(view::needs_blink(&screens.borrow())) {
                blink.set(state);
                terminal.queue_draw();
            }
            gtk4::glib::ControlFlow::Continue
        });
    }

    // --- resize -----------------------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let theme = Rc::clone(&theme);
        let paned = paned.clone();
        let window = window.clone();
        let last = Cell::new((0i32, 0i32));
        terminal.connect_resize(move |area, width, height| {
            clamp_sidebar(&paned, &window);
            if last.replace((width, height)) == (width, height) {
                return;
            }
            sync_attachments(&worker, &screens.borrow(), area, &theme);
        });
    }

    // --- keyboard ---------------------------------------------------------
    let key_controller = EventControllerKey::new();
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let terminal_for_keys = terminal.clone();
        let toast = toast.clone();
        let rename_prompt = Rc::clone(&rename_prompt);
        let entries = Rc::clone(&entries);
        let prefix_armed = Cell::new(false);
        key_controller.connect_key_pressed(move |_, key, _, state| {
            let is_prefix = state.contains(gdk::ModifierType::CONTROL_MASK)
                && !state.intersects(
                    gdk::ModifierType::SHIFT_MASK
                        | gdk::ModifierType::ALT_MASK
                        | gdk::ModifierType::SUPER_MASK,
                )
                && matches!(key, gdk::Key::B | gdk::Key::b);
            if prefix_armed.replace(false) {
                if is_prefix {
                    let _ = worker.input.send(Input::Bytes(vec![0x02]));
                    return gtk4::glib::Propagation::Stop;
                }
                let action = prefix_action(key);
                let screens_ref = screens.borrow();
                match action {
                    Some(PrefixAction::NewTab) => {
                        if let Some(target) = focused_pane_target(&screens_ref) {
                            let _ = worker.input.send(Input::CreateTab {
                                workspace: target.workspace,
                                screen: target.screen,
                                pane: target.pane,
                            });
                        } else {
                            set_toast(&toast, "No focused pane for a new tab");
                        }
                    }
                    Some(PrefixAction::NextTab) | Some(PrefixAction::PrevTab) => {
                        let offset = if action == Some(PrefixAction::NextTab) {
                            1
                        } else {
                            -1
                        };
                        if let Some(target) = adjacent_tab_target(&screens_ref, offset) {
                            send_focus_tab(&worker, &target);
                        } else {
                            set_toast(&toast, "No focused tab to switch");
                        }
                    }
                    Some(PrefixAction::CloseTab) => {
                        if let Some(target) = focused_tab_target(&screens_ref) {
                            let _ = worker.input.send(Input::CloseTab {
                                workspace: target.pane.workspace,
                                screen: target.pane.screen,
                                pane: target.pane.pane,
                                tab: target.tab,
                            });
                        } else {
                            set_toast(&toast, "No focused tab to close");
                        }
                    }
                    Some(PrefixAction::SplitRight) | Some(PrefixAction::SplitDown) => {
                        if let Some(target) = focused_pane_target(&screens_ref) {
                            let direction = if action == Some(PrefixAction::SplitRight) {
                                Direction::Right
                            } else {
                                Direction::Down
                            };
                            send_split(&worker, &target, direction);
                        } else {
                            set_toast(&toast, "No focused pane to split");
                        }
                    }
                    Some(PrefixAction::ClosePane) => {
                        if let Some(target) = focused_pane_target(&screens_ref) {
                            send_close(&worker, &target);
                        } else {
                            set_toast(&toast, "No focused pane to close");
                        }
                    }
                    Some(PrefixAction::RenameScreen) => {
                        if let Some(workspace) = screens_ref.workspace.as_ref() {
                            let screen = workspace
                                .screen_tabs
                                .iter()
                                .find(|screen| screen.id == workspace.screen_id);
                            if let Some(screen) = screen {
                                let geometry = view::screen_bar_geometry(
                                    &screens_ref,
                                    terminal_for_keys.width(),
                                    terminal_for_keys.height(),
                                );
                                let rect = geometry
                                    .as_ref()
                                    .and_then(|geometry| {
                                        geometry.tabs.iter().find(|tab| tab.id == screen.id)
                                    })
                                    .map(|tab| {
                                        gdk::Rectangle::new(
                                            tab.rect.x as i32,
                                            tab.rect.y as i32,
                                            tab.rect.width.max(1.0) as i32,
                                            tab.rect.height.max(1.0) as i32,
                                        )
                                    })
                                    .unwrap_or_else(|| gdk::Rectangle::new(0, 0, 1, 1));
                                open_rename_prompt(
                                    &rename_prompt,
                                    &terminal_for_keys,
                                    rect,
                                    RenameTarget::Screen {
                                        workspace: workspace.workspace_id.clone(),
                                        screen: screen.id.clone(),
                                    },
                                    screen.name.as_deref().unwrap_or(""),
                                );
                            }
                        }
                    }
                    Some(PrefixAction::RenameWorkspace) => {
                        if let Some(workspace) = screens_ref.workspace.as_ref() {
                            let entry = entries
                                .borrow()
                                .iter()
                                .find(|entry| entry.id == workspace.workspace_id)
                                .cloned();
                            if let Some(entry) = entry {
                                open_rename_prompt(
                                    &rename_prompt,
                                    &terminal_for_keys,
                                    gdk::Rectangle::new(0, 0, 1, 1),
                                    RenameTarget::Workspace(entry.id),
                                    &entry.name,
                                );
                            }
                        }
                    }
                    Some(PrefixAction::CloseScreen) => {
                        if let Some(workspace) = screens_ref.workspace.as_ref() {
                            let _ = worker.input.send(Input::CloseScreen {
                                workspace: workspace.workspace_id.clone(),
                                screen: workspace.screen_id.clone(),
                            });
                        } else {
                            set_toast(&toast, "No focused screen to close");
                        }
                    }
                    None => {}
                }
                return gtk4::glib::Propagation::Stop;
            }
            if is_prefix {
                prefix_armed.set(true);
                return gtk4::glib::Propagation::Stop;
            }
            if view::is_copy_shortcut(key, state) {
                if let Some(text) = screens.borrow().selected_text() {
                    terminal_for_keys.clipboard().set_text(&text);
                }
                return gtk4::glib::Propagation::Stop;
            }
            if view::is_paste_shortcut(key, state) {
                let clipboard = terminal_for_keys.clipboard();
                let worker = Rc::clone(&worker);
                let screens = Rc::clone(&screens);
                let terminal = terminal_for_keys.clone();
                gtk4::glib::spawn_future_local(async move {
                    let Ok(Some(text)) = clipboard.read_text_future().await else {
                        return;
                    };
                    let Some(text) = view::paste_payload(text.as_str()) else {
                        return;
                    };
                    if let Some(screen) = screens.borrow_mut().focused_screen_mut() {
                        screen.clear_selection();
                    }
                    terminal.queue_draw();
                    let _ = worker.input.send(Input::Paste(text.to_string()));
                });
                return gtk4::glib::Propagation::Stop;
            }

            match view::key_to_bytes(key, state) {
                Some(bytes) => {
                    let had_selection = screens
                        .borrow()
                        .focused_screen()
                        .is_some_and(|screen| screen.selection.is_some());
                    if had_selection {
                        if let Some(screen) = screens.borrow_mut().focused_screen_mut() {
                            screen.clear_selection();
                        }
                        terminal_for_keys.queue_draw();
                    }
                    let _ = worker.input.send(Input::Bytes(bytes));
                    gtk4::glib::Propagation::Stop
                }
                None => gtk4::glib::Propagation::Proceed,
            }
        });
    }
    terminal.add_controller(key_controller);

    let pointer = Rc::new(Cell::new(None::<(f64, f64)>));
    let move_throttle = Rc::new(RefCell::new(view::MouseMoveThrottle::default()));
    let divider_drag = Rc::new(RefCell::new(None::<DividerDrag>));
    let divider_drag_direction = Rc::new(Cell::new(None::<LayoutDirection>));
    let suppress_mouse_release = Rc::new(Cell::new(false));

    // --- pointer motion ---------------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let theme_for_enter = Rc::clone(&theme);
        let theme_for_motion = Rc::clone(&theme);
        let pointer_for_enter = Rc::clone(&pointer);
        let pointer_for_motion = Rc::clone(&pointer);
        let pointer_for_leave = Rc::clone(&pointer);
        let throttle_for_enter = Rc::clone(&move_throttle);
        let throttle_for_motion = Rc::clone(&move_throttle);
        let throttle_for_leave = Rc::clone(&move_throttle);
        let screens_for_enter = Rc::clone(&screens);
        let screens_for_motion = Rc::clone(&screens);
        let drag_direction_for_motion = Rc::clone(&divider_drag_direction);
        let drag_direction_for_leave = Rc::clone(&divider_drag_direction);
        let tab_strip_for_enter = Rc::clone(&tab_strip);
        let tab_strip_for_motion = Rc::clone(&tab_strip);
        let tab_strip_for_leave = Rc::clone(&tab_strip);
        let terminal_for_enter = terminal.clone();
        let terminal_for_motion = terminal.clone();
        let terminal_for_leave = terminal.clone();
        let motion = EventControllerMotion::new();
        motion.connect_enter(move |_, x, y| {
            pointer_for_enter.set(Some((x, y)));
            throttle_for_enter.borrow_mut().reset();
            let screens = screens_for_enter.borrow();
            let hovered_screen = view::screen_bar_geometry(
                &screens,
                terminal_for_enter.width(),
                terminal_for_enter.height(),
            )
            .and_then(|geometry| view::hovered_screen(&geometry, x, y));
            let panes = view::pane_geometries(
                &screens,
                view::cell_metrics(&terminal_for_enter, &theme_for_enter),
                terminal_for_enter.width(),
                terminal_for_enter.height(),
            );
            let hovered_tab = view::hovered_pane_tab(&panes, x, y);
            let screen_changed = tab_strip_for_enter.set_hovered_screen(hovered_screen);
            let tab_changed = tab_strip_for_enter.set_hovered_tab(hovered_tab);
            if screen_changed || tab_changed {
                terminal_for_enter.queue_draw();
            }
            let direction =
                divider_at(&screens, &terminal_for_enter, x, y).map(|divider| divider.direction);
            terminal_for_enter.set_cursor_from_name(direction.map(resize_cursor));
        });
        motion.connect_motion(move |controller, x, y| {
            pointer_for_motion.set(Some((x, y)));
            let screens_ref = screens_for_motion.borrow();
            let hovered_screen = view::screen_bar_geometry(
                &screens_ref,
                terminal_for_motion.width(),
                terminal_for_motion.height(),
            )
            .and_then(|geometry| view::hovered_screen(&geometry, x, y));
            let panes = view::pane_geometries(
                &screens_ref,
                view::cell_metrics(&terminal_for_motion, &theme_for_motion),
                terminal_for_motion.width(),
                terminal_for_motion.height(),
            );
            let hovered_tab = view::hovered_pane_tab(&panes, x, y);
            let screen_changed = tab_strip_for_motion.set_hovered_screen(hovered_screen);
            let tab_changed = tab_strip_for_motion.set_hovered_tab(hovered_tab);
            if screen_changed || tab_changed {
                terminal_for_motion.queue_draw();
            }
            drop(screens_ref);
            let state = controller.current_event_state();
            let held = mouse_button_held(state);
            let direction = drag_direction_for_motion.get().or_else(|| {
                (!held)
                    .then(|| {
                        divider_at(&screens_for_motion.borrow(), &terminal_for_motion, x, y)
                            .map(|divider| divider.direction)
                    })
                    .flatten()
            });
            terminal_for_motion.set_cursor_from_name(direction.map(resize_cursor));
            if direction.is_some() {
                throttle_for_motion.borrow_mut().reset();
                return;
            }
            if held && state.contains(gdk::ModifierType::SHIFT_MASK) {
                return;
            }
            let metrics = view::cell_metrics(&terminal_for_motion, &theme_for_motion);
            let Some(target) = mouse_target(&screens.borrow(), &terminal_for_motion, metrics, x, y)
            else {
                if !held {
                    throttle_for_motion.borrow_mut().reset();
                }
                return;
            };
            if !held
                && !throttle_for_motion.borrow_mut().should_report(
                    &target.terminal,
                    target.row,
                    target.column,
                )
            {
                return;
            }
            let _ = worker.input.send(Input::Mouse {
                terminal: target.terminal,
                options: TerminalMouseOptions {
                    kind: TerminalMouseKind::Move,
                    row: target.row,
                    column: target.column,
                    button: None,
                    delta_rows: None,
                    modifiers: input_modifiers(state),
                },
            });
        });
        motion.connect_leave(move |_| {
            pointer_for_leave.set(None);
            throttle_for_leave.borrow_mut().reset();
            let screen_changed = tab_strip_for_leave.set_hovered_screen(None);
            let tab_changed = tab_strip_for_leave.set_hovered_tab(None);
            if screen_changed || tab_changed {
                terminal_for_leave.queue_draw();
            }
            terminal_for_leave
                .set_cursor_from_name(drag_direction_for_leave.get().map(resize_cursor));
        });
        terminal.add_controller(motion);
    }

    // --- scrollback and application wheel input --------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let theme = Rc::clone(&theme);
        let pointer = Rc::clone(&pointer);
        let terminal_for_scroll = terminal.clone();
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(move |controller, _, delta_y| {
            let delta_rows = (delta_y.round() as i32) * SCROLL_ROWS;
            let Some((x, y)) = pointer.get() else {
                return gtk4::glib::Propagation::Stop;
            };
            if divider_at(&screens.borrow(), &terminal_for_scroll, x, y).is_some() {
                return gtk4::glib::Propagation::Stop;
            }
            let metrics = view::cell_metrics(&terminal_for_scroll, &theme);
            let Some(target) = mouse_target(&screens.borrow(), &terminal_for_scroll, metrics, x, y)
            else {
                return gtk4::glib::Propagation::Stop;
            };
            if delta_rows == 0 {
                return gtk4::glib::Propagation::Stop;
            }
            if target.can_scroll_locally {
                // Viewport scrolling uses positive rows for history, opposite
                // to GDK's positive-down wheel direction.
                let _ = worker.input.send(Input::Scroll {
                    terminal: target.terminal,
                    delta_rows: -delta_rows,
                });
            } else {
                // The TUI can inspect its VT directly and synthesize arrows
                // when mouse tracking is off. This client receives opaque VT
                // state, so matching that fallback would require forbidden VT
                // parsing; the server safely drops untracked wheel input.
                let _ = worker.input.send(Input::Mouse {
                    terminal: target.terminal,
                    options: TerminalMouseOptions {
                        kind: TerminalMouseKind::Wheel,
                        row: target.row,
                        column: target.column,
                        button: None,
                        delta_rows: Some(delta_rows),
                        modifiers: input_modifiers(controller.current_event_state()),
                    },
                });
            }
            gtk4::glib::Propagation::Stop
        });
        terminal.add_controller(scroll);
    }

    // --- pane and tab focus ----------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let theme = Rc::clone(&theme);
        let worker_for_release = Rc::clone(&worker);
        let screens_for_release = Rc::clone(&screens);
        let theme_for_release = Rc::clone(&theme);
        let terminal_for_click = terminal.clone();
        let terminal_for_release = terminal.clone();
        let context_target_for_click = Rc::clone(&context_target);
        let pane_menu_for_click = pane_menu.clone();
        let rename_prompt_for_click = Rc::clone(&rename_prompt);
        let suppress_for_click = Rc::clone(&suppress_mouse_release);
        let suppress_for_release = Rc::clone(&suppress_mouse_release);
        let click = GestureClick::new();
        click.set_button(0);
        click.connect_pressed(move |gesture, press_count, x, y| {
            terminal_for_click.grab_focus();
            suppress_for_click.set(false);
            let screens = screens.borrow();
            let button_number = gesture.current_button();
            if let Some(geometry) = view::screen_bar_geometry(
                &screens,
                terminal_for_click.width(),
                terminal_for_click.height(),
            ) {
                if let Some(hit) = view::screen_bar_hit(&geometry, x, y) {
                    let Some(workspace) = screens.workspace.as_ref() else {
                        return;
                    };
                    suppress_for_click.set(true);
                    match hit {
                        view::ScreenBarHit::New if button_number == 1 => {
                            let _ = worker.input.send(Input::CreateScreen {
                                workspace: workspace.workspace_id.clone(),
                            });
                        }
                        view::ScreenBarHit::Close(screen) if button_number == 1 => {
                            let _ = worker.input.send(Input::CloseScreen {
                                workspace: workspace.workspace_id.clone(),
                                screen,
                            });
                        }
                        view::ScreenBarHit::Tab(screen) if button_number == 2 => {
                            let _ = worker.input.send(Input::CloseScreen {
                                workspace: workspace.workspace_id.clone(),
                                screen,
                            });
                        }
                        view::ScreenBarHit::Tab(screen)
                            if button_number == 1 && press_count == 2 =>
                        {
                            let screen_entry = workspace
                                .screen_tabs
                                .iter()
                                .find(|entry| entry.id == screen);
                            let tab = geometry.tabs.iter().find(|tab| tab.id == screen);
                            if let (Some(screen_entry), Some(tab)) = (screen_entry, tab) {
                                open_rename_prompt(
                                    &rename_prompt_for_click,
                                    &terminal_for_click,
                                    gdk::Rectangle::new(
                                        tab.rect.x as i32,
                                        tab.rect.y as i32,
                                        tab.rect.width.max(1.0) as i32,
                                        tab.rect.height.max(1.0) as i32,
                                    ),
                                    RenameTarget::Screen {
                                        workspace: workspace.workspace_id.clone(),
                                        screen,
                                    },
                                    screen_entry.name.as_deref().unwrap_or(""),
                                );
                            }
                        }
                        view::ScreenBarHit::Tab(screen) if button_number == 1 => {
                            let _ = worker.input.send(Input::FocusScreen {
                                workspace: workspace.workspace_id.clone(),
                                screen,
                            });
                        }
                        _ => {}
                    }
                    return;
                }
            }
            if divider_at(&screens, &terminal_for_click, x, y).is_some() {
                suppress_for_click.set(true);
                return;
            }
            let Some(workspace) = screens.workspace.as_ref() else {
                return;
            };
            let geometries = view::pane_geometries(
                &screens,
                view::cell_metrics(&terminal_for_click, &theme),
                terminal_for_click.width(),
                terminal_for_click.height(),
            );
            let Some(pane) = geometries.iter().find(|pane| pane.rect.contains(x, y)) else {
                return;
            };
            let pane_bar_hit = view::pane_bar_hit(pane, x, y);
            if let Some(hit) = pane_bar_hit.as_ref() {
                match hit {
                    view::PaneBarHit::New if button_number == 1 => {
                        let _ = worker.input.send(Input::CreateTab {
                            workspace: workspace.workspace_id.clone(),
                            screen: workspace.screen_id.clone(),
                            pane: pane.pane.clone(),
                        });
                    }
                    view::PaneBarHit::Close(tab) if button_number == 1 => {
                        let _ = worker.input.send(Input::CloseTab {
                            workspace: workspace.workspace_id.clone(),
                            screen: workspace.screen_id.clone(),
                            pane: pane.pane.clone(),
                            tab: tab.clone(),
                        });
                    }
                    view::PaneBarHit::Tab(tab) if button_number == 2 => {
                        let _ = worker.input.send(Input::CloseTab {
                            workspace: workspace.workspace_id.clone(),
                            screen: workspace.screen_id.clone(),
                            pane: pane.pane.clone(),
                            tab: tab.clone(),
                        });
                    }
                    view::PaneBarHit::Tab(tab) if button_number == 1 && press_count == 2 => {
                        let tab_view = workspace
                            .pane(&pane.pane)
                            .and_then(|pane| pane.tabs.iter().find(|entry| entry.id == *tab));
                        let hit = pane.tabs.iter().find(|entry| entry.id == *tab);
                        if let (Some(tab_view), Some(hit)) = (tab_view, hit) {
                            open_rename_prompt(
                                &rename_prompt_for_click,
                                &terminal_for_click,
                                gdk::Rectangle::new(
                                    hit.rect.x as i32,
                                    hit.rect.y as i32,
                                    hit.rect.width.max(1.0) as i32,
                                    hit.rect.height.max(1.0) as i32,
                                ),
                                RenameTarget::Tab {
                                    workspace: workspace.workspace_id.clone(),
                                    screen: workspace.screen_id.clone(),
                                    pane: pane.pane.clone(),
                                    tab: tab.clone(),
                                },
                                tab_view.name.as_deref().unwrap_or(""),
                            );
                        }
                    }
                    view::PaneBarHit::Tab(tab) if button_number == 1 => {
                        if let Some(hit) = pane.tabs.iter().find(|entry| entry.id == *tab) {
                            let _ = worker.input.send(Input::FocusTab {
                                workspace: workspace.workspace_id.clone(),
                                screen: workspace.screen_id.clone(),
                                pane: pane.pane.clone(),
                                tab: tab.clone(),
                                target: hit.terminal.clone(),
                            });
                        }
                    }
                    _ => {}
                }
                if button_number == 1 || button_number == 2 {
                    suppress_for_click.set(true);
                    return;
                }
            }
            if button_number == 3 {
                *context_target_for_click.borrow_mut() = Some(PaneTarget {
                    workspace: workspace.workspace_id.clone(),
                    screen: workspace.screen_id.clone(),
                    pane: pane.pane.clone(),
                });
                suppress_for_click.set(true);
                pane_menu_for_click.set_pointing_to(Some(&gdk::Rectangle::new(
                    x.floor() as i32,
                    y.floor() as i32,
                    1,
                    1,
                )));
                pane_menu_for_click.popup();
                return;
            }
            let target = workspace
                .pane(&pane.pane)
                .and_then(|pane| pane.active_terminal())
                .cloned();
            let _ = worker.input.send(Input::FocusPane {
                workspace: workspace.workspace_id.clone(),
                screen: workspace.screen_id.clone(),
                pane: pane.pane.clone(),
                target,
            });
            let state = gesture.current_event_state();
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                return;
            }
            let Some(button) = mouse_button(button_number) else {
                return;
            };
            let metrics = view::cell_metrics(&terminal_for_click, &theme);
            let Some(target) = mouse_target(&screens, &terminal_for_click, metrics, x, y) else {
                return;
            };
            let _ = worker.input.send(Input::Mouse {
                terminal: target.terminal,
                options: TerminalMouseOptions {
                    kind: TerminalMouseKind::Down,
                    row: target.row,
                    column: target.column,
                    button: Some(button),
                    delta_rows: None,
                    modifiers: input_modifiers(state),
                },
            });
        });
        click.connect_released(move |gesture, _, x, y| {
            let button_number = gesture.current_button();
            if button_number == 3 || suppress_for_release.replace(false) {
                return;
            }
            let state = gesture.current_event_state();
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                return;
            }
            let Some(button) = mouse_button(button_number) else {
                return;
            };
            let metrics = view::cell_metrics(&terminal_for_release, &theme_for_release);
            let Some(target) = mouse_target(
                &screens_for_release.borrow(),
                &terminal_for_release,
                metrics,
                x,
                y,
            ) else {
                return;
            };
            let _ = worker_for_release.input.send(Input::Mouse {
                terminal: target.terminal,
                options: TerminalMouseOptions {
                    kind: TerminalMouseKind::Up,
                    row: target.row,
                    column: target.column,
                    button: Some(button),
                    delta_rows: None,
                    modifiers: input_modifiers(state),
                },
            });
        });
        terminal.add_controller(click);
    }

    // --- selection --------------------------------------------------------
    {
        type DragAnchor = (cmux::TerminalId, (u16, u16), view::Rect);
        let screens = Rc::clone(&screens);
        let drag = GestureDrag::new();
        drag.set_button(1);
        let anchor: Rc<RefCell<Option<DragAnchor>>> = Rc::new(RefCell::new(None));

        let terminal_for_begin = terminal.clone();
        let screens_for_begin = Rc::clone(&screens);
        let anchor_for_begin = Rc::clone(&anchor);
        let theme_for_begin = Rc::clone(&theme);
        let divider_drag_for_begin = Rc::clone(&divider_drag);
        let drag_direction_for_begin = Rc::clone(&divider_drag_direction);
        drag.connect_drag_begin(move |_, x, y| {
            terminal_for_begin.grab_focus();
            if let Some(divider) =
                divider_at(&screens_for_begin.borrow(), &terminal_for_begin, x, y)
            {
                let target = screens_for_begin
                    .borrow()
                    .workspace
                    .as_ref()
                    .map(|workspace| PaneTarget {
                        workspace: workspace.workspace_id.clone(),
                        screen: workspace.screen_id.clone(),
                        pane: divider.pane.clone(),
                    });
                if let Some(target) = target {
                    *anchor_for_begin.borrow_mut() = None;
                    drag_direction_for_begin.set(Some(divider.direction));
                    terminal_for_begin.set_cursor_from_name(Some(resize_cursor(divider.direction)));
                    *divider_drag_for_begin.borrow_mut() = Some(DividerDrag {
                        target,
                        divider,
                        start_x: x,
                        start_y: y,
                        started: Instant::now(),
                        throttle: view::DividerDragThrottle::default(),
                    });
                    return;
                }
            }
            *divider_drag_for_begin.borrow_mut() = None;
            drag_direction_for_begin.set(None);
            terminal_for_begin.set_cursor_from_name(None);
            let metrics = view::cell_metrics(&terminal_for_begin, &theme_for_begin);
            let geometries = view::pane_geometries(
                &screens_for_begin.borrow(),
                metrics,
                terminal_for_begin.width(),
                terminal_for_begin.height(),
            );
            let Some(pane) = geometries
                .into_iter()
                .find(|pane| pane.content.contains(x, y) && pane.terminal.is_some())
            else {
                *anchor_for_begin.borrow_mut() = None;
                return;
            };
            let terminal_id = pane.terminal.expect("filtered terminal geometry");
            let mut screens = screens_for_begin.borrow_mut();
            let Some(screen) = screens.grids.get_mut(&terminal_id) else {
                *anchor_for_begin.borrow_mut() = None;
                return;
            };
            let cell = view::cell_at(metrics, screen.size, x - pane.content.x, y - pane.content.y);
            screen.clear_selection();
            *anchor_for_begin.borrow_mut() = Some((terminal_id, cell, pane.content));
            terminal_for_begin.queue_draw();
        });

        let terminal_for_update = terminal.clone();
        let screens_for_update = Rc::clone(&screens);
        let anchor_for_update = Rc::clone(&anchor);
        let theme_for_update = Rc::clone(&theme);
        let divider_drag_for_update = Rc::clone(&divider_drag);
        let worker_for_update = Rc::clone(&worker);
        drag.connect_drag_update(move |gesture, dx, dy| {
            if let Some(resize) = divider_drag_for_update.borrow_mut().as_mut() {
                if let Some(ratio) =
                    view::split_ratio_at(&resize.divider, resize.start_x + dx, resize.start_y + dy)
                {
                    let elapsed = resize.started.elapsed();
                    if resize.throttle.should_send(elapsed, ratio, false) {
                        send_split_ratio(
                            &worker_for_update,
                            &resize.target,
                            &resize.divider,
                            ratio,
                        );
                    }
                }
                return;
            }
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let Some((terminal_id, start, content)) = anchor_for_update.borrow().clone() else {
                return;
            };
            let metrics = view::cell_metrics(&terminal_for_update, &theme_for_update);
            let mut screens = screens_for_update.borrow_mut();
            let Some(screen) = screens.grids.get_mut(&terminal_id) else {
                return;
            };
            let head = view::cell_at(
                metrics,
                screen.size,
                start_x + dx - content.x,
                start_y + dy - content.y,
            );
            screen.set_selection(start, head);
            terminal_for_update.queue_draw();
        });

        let terminal_for_end = terminal.clone();
        let screens_for_end = Rc::clone(&screens);
        let divider_drag_for_end = Rc::clone(&divider_drag);
        let drag_direction_for_end = Rc::clone(&divider_drag_direction);
        let worker_for_end = Rc::clone(&worker);
        drag.connect_drag_end(move |_, dx, dy| {
            let Some(mut resize) = divider_drag_for_end.borrow_mut().take() else {
                return;
            };
            let x = resize.start_x + dx;
            let y = resize.start_y + dy;
            if let Some(ratio) = view::split_ratio_at(&resize.divider, x, y) {
                let elapsed = resize.started.elapsed();
                if resize.throttle.should_send(elapsed, ratio, true) {
                    send_split_ratio(&worker_for_end, &resize.target, &resize.divider, ratio);
                }
            }
            drag_direction_for_end.set(None);
            let direction = divider_at(&screens_for_end.borrow(), &terminal_for_end, x, y)
                .map(|divider| divider.direction);
            terminal_for_end.set_cursor_from_name(direction.map(resize_cursor));
        });
        terminal.add_controller(drag);
    }

    // --- workspace switching ---------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let entries = Rc::clone(&entries);
        let screens = Rc::clone(&screens);
        let terminal = terminal.clone();
        let theme = Rc::clone(&theme);
        let toast = toast.clone();
        let css_provider = css_provider.clone();
        let sidebar_scrims = sidebar_scrims.clone();
        let workspaces_for_theme = workspaces.clone();
        workspaces.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let Ok(index) = usize::try_from(row.index()) else {
                return;
            };
            let Some(entry) = entries.borrow().get(index).cloned() else {
                return;
            };
            let target = entry
                .view
                .as_ref()
                .and_then(|view| view.active_terminal())
                .cloned();
            screens.borrow_mut().set_workspace(entry.view.clone());
            sync_attachments(&worker, &screens.borrow(), &terminal, &theme);
            refresh_chrome(
                &screens.borrow(),
                &theme,
                &css_provider,
                &terminal,
                &sidebar_scrims,
                &workspaces_for_theme,
            );
            terminal.queue_draw();
            let _ = worker.input.send(Input::FocusWorkspace {
                workspace: entry.id,
                target,
            });
            if entry.terminals.is_empty() {
                set_toast(
                    &toast,
                    &format!("Workspace '{}' has no terminal tabs", entry.name),
                );
            } else {
                clear_toast(&toast);
            }
        });
    }

    // --- updates ----------------------------------------------------------
    {
        let screens = Rc::clone(&screens);
        let terminal = terminal.clone();
        let toast = toast.clone();
        let workspaces = workspaces.clone();
        let entries = Rc::clone(&entries);
        let worker = Rc::clone(&worker);
        let theme = Rc::clone(&theme);
        let css_provider = css_provider.clone();
        let sidebar_scrims = sidebar_scrims.clone();
        let rename_prompt = Rc::clone(&rename_prompt);
        let drop_indicators = Rc::clone(&drop_indicators);
        gtk4::glib::spawn_future_local(async move {
            while let Ok(update) = update_rx.recv().await {
                match update {
                    Update::Connected { session_name } => {
                        eprintln!("connected to session '{session_name}'");
                        clear_toast(&toast);
                    }
                    Update::Workspaces(list) => {
                        let empty = list.is_empty();
                        let focused = list
                            .iter()
                            .find(|entry| entry.focused)
                            .or_else(|| list.first())
                            .cloned();
                        let changed = *entries.borrow() != list;
                        if changed {
                            drop_indicators.borrow_mut().clear();
                            while let Some(child) = workspaces.first_child() {
                                workspaces.remove(&child);
                            }
                            for entry in &list {
                                let row = workspace_row(
                                    entry,
                                    Rc::clone(&theme),
                                    Rc::clone(&worker),
                                    Rc::clone(&rename_prompt),
                                    Rc::clone(&drop_indicators),
                                    list.len(),
                                );
                                workspaces.append(&row);
                            }
                            *entries.borrow_mut() = list;
                        }

                        if let Some(entry) = focused {
                            let target = entry
                                .view
                                .as_ref()
                                .and_then(|view| view.active_terminal())
                                .cloned();
                            screens.borrow_mut().set_workspace(entry.view.clone());
                            let _ = worker.input.send(Input::SetTarget(target));
                            sync_attachments(&worker, &screens.borrow(), &terminal, &theme);
                            refresh_chrome(
                                &screens.borrow(),
                                &theme,
                                &css_provider,
                                &terminal,
                                &sidebar_scrims,
                                &workspaces,
                            );
                            if changed {
                                let index = entries
                                    .borrow()
                                    .iter()
                                    .position(|candidate| candidate.id == entry.id);
                                if let Some(row) =
                                    index.and_then(|index| workspaces.row_at_index(index as i32))
                                {
                                    workspaces.select_row(Some(&row));
                                }
                            }
                        } else {
                            screens.borrow_mut().set_workspace(None);
                            sync_attachments(&worker, &screens.borrow(), &terminal, &theme);
                            if empty {
                                set_toast(&toast, "The session has no workspaces");
                            }
                        }
                        terminal.queue_draw();
                    }
                    Update::Attached { terminal: id } => {
                        eprintln!("viewer attached to {id:?}");
                        clear_toast(&toast);
                        terminal.grab_focus();
                    }
                    Update::Snapshot {
                        terminal: id,
                        render,
                    } => {
                        if screens.borrow().contains_terminal(&id) {
                            screens.borrow_mut().apply_snapshot(id, *render);
                            refresh_chrome(
                                &screens.borrow(),
                                &theme,
                                &css_provider,
                                &terminal,
                                &sidebar_scrims,
                                &workspaces,
                            );
                            terminal.queue_draw();
                        }
                    }
                    Update::Patch {
                        terminal: id,
                        render,
                    } => {
                        if screens.borrow().contains_terminal(&id) {
                            match screens.borrow_mut().apply_patch(&id, *render) {
                                Ok(()) => {
                                    refresh_chrome(
                                        &screens.borrow(),
                                        &theme,
                                        &css_provider,
                                        &terminal,
                                        &sidebar_scrims,
                                        &workspaces,
                                    );
                                    terminal.queue_draw();
                                }
                                Err(error) => {
                                    set_toast(&toast, &format!("Render desync: {error}"));
                                }
                            }
                        }
                    }
                    Update::Scroll {
                        terminal: id,
                        at_bottom,
                    } => {
                        let focused = screens.borrow().focused_terminal() == Some(&id);
                        screens.borrow_mut().apply_scroll(&id, at_bottom);
                        if focused {
                            clear_toast(&toast);
                        }
                        terminal.queue_draw();
                    }
                    Update::Detached { terminal: id } => {
                        if screens.borrow().contains_terminal(&id) {
                            set_toast(&toast, &format!("Detached from {id:?}; reconnecting"));
                        }
                    }
                    Update::Error(message) => set_toast(&toast, &message),
                }
            }
        });
    }

    // Other clients can mutate the workspace tree, so periodically refreshing
    // the catalog is also how foreign splits and tab changes reach this view.
    {
        let worker = Rc::clone(&worker);
        gtk4::glib::timeout_add_seconds_local(3, move || {
            let _ = worker.input.send(Input::RefreshWorkspaces);
            gtk4::glib::ControlFlow::Continue
        });
    }

    {
        let worker = Rc::clone(&worker);
        window.connect_close_request(move |_| {
            worker.stop();
            gtk4::glib::Propagation::Proceed
        });
    }

    window.present();
    terminal.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidebar_width_is_clamped_to_metrics_and_one_third() {
        assert_eq!(clamp_sidebar_width(100, 1000), 240);
        assert_eq!(clamp_sidebar_width(400, 1000), 333);
        assert_eq!(clamp_sidebar_width(900, 2400), 600);
        assert_eq!(clamp_sidebar_width(240, 699), 233);
        assert_eq!(clamp_sidebar_width(240, 300), 100);
    }

    #[test]
    fn rename_submission_rejects_blank_names_and_trims_valid_input() {
        assert_eq!(rename_submission(""), None);
        assert_eq!(rename_submission("  \t "), None);
        assert_eq!(
            rename_submission("  build logs  "),
            Some("build logs".to_string())
        );
    }

    #[test]
    fn workspace_drop_index_rounds_to_rows_and_clamps_to_catalog() {
        assert_eq!(workspace_drop_index(2, -90.0, 30.0, 5), Some(0));
        assert_eq!(workspace_drop_index(2, -16.0, 30.0, 5), Some(1));
        assert_eq!(workspace_drop_index(2, 14.0, 30.0, 5), Some(2));
        assert_eq!(workspace_drop_index(2, 16.0, 30.0, 5), Some(3));
        assert_eq!(workspace_drop_index(2, 900.0, 30.0, 5), Some(4));
        assert_eq!(workspace_drop_index(5, 0.0, 30.0, 5), None);
        assert_eq!(workspace_drop_index(0, f64::NAN, 30.0, 5), None);
    }

    #[test]
    fn prefix_actions_match_the_tui_default_keymap() {
        assert_eq!(prefix_action(gdk::Key::t), Some(PrefixAction::NewTab));
        assert_eq!(prefix_action(gdk::Key::Tab), Some(PrefixAction::NextTab));
        assert_eq!(
            prefix_action(gdk::Key::ISO_Left_Tab),
            Some(PrefixAction::PrevTab)
        );
        assert_eq!(prefix_action(gdk::Key::x), Some(PrefixAction::CloseTab));
        assert_eq!(
            prefix_action(gdk::Key::comma),
            Some(PrefixAction::RenameScreen)
        );
        assert_eq!(
            prefix_action(gdk::Key::dollar),
            Some(PrefixAction::RenameWorkspace)
        );
        assert_eq!(
            prefix_action(gdk::Key::ampersand),
            Some(PrefixAction::CloseScreen)
        );
    }
}
