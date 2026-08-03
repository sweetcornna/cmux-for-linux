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
use std::time::Duration;

use cmux::{InputModifier, MouseButton, TerminalId, TerminalMouseKind, TerminalMouseOptions};
use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{
    Align, Application, ApplicationWindow, Box as GtkBox, CenterBox, CssProvider, DrawingArea,
    EventControllerKey, EventControllerMotion, EventControllerScroll, EventControllerScrollFlags,
    GestureClick, GestureDrag, Label, ListBox, ListBoxRow, Orientation, Overlay, PackType, Paned,
    ScrolledWindow, SelectionMode, WindowControls,
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
                     Connects to a running cmux session. Start one with\n  \
                     cmux --headless --session <name>\n\n\
                     --probe runs the protocol workers without GTK and prints every\n\
                     update, which separates protocol failures from drawing ones."
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
    let worker = session::spawn(args.session.clone(), args.socket.clone(), tx);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(Update::Snapshot { terminal, render }) => println!(
                "snapshot terminal={terminal:?} size={}x{} rows={} fg={} bg={}",
                render.size.cols,
                render.size.rows,
                render.rows.len(),
                render.default_fg.as_str(),
                render.default_bg.as_str()
            ),
            Ok(Update::Patch { terminal, render }) => println!(
                "patch terminal={terminal:?} full_reset={} rows={}",
                render.full_reset,
                render.rows.len()
            ),
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

fn workspace_row(entry: &session::WorkspaceEntry, theme: Rc<view::Theme>) -> ListBoxRow {
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

    let row = ListBoxRow::new();
    row.add_css_class("workspace-row");
    row.set_child(Some(&overlay));
    let drag = GestureDrag::new();
    {
        let row = row.clone();
        drag.connect_drag_begin(move |_, _, _| row.add_css_class("dragging"));
    }
    {
        let row = row.clone();
        drag.connect_drag_end(move |_, _, _| row.remove_css_class("dragging"));
    }
    row.add_controller(drag);
    row
}

fn build_ui(application: &Application) {
    let args = parse_args();
    let theme = Rc::new(view::Theme::new(config::load()));
    let screens = Rc::new(RefCell::new(ScreenSet::default()));
    let (update_tx, update_rx) = async_channel::unbounded::<Update>();
    let worker = Rc::new(session::spawn(
        args.session.clone(),
        args.socket.clone(),
        update_tx,
    ));
    let blink = Rc::new(Cell::new(view::BlinkState::new(false)));

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
    titlebar.set_start_widget(Some(&controls_start));
    titlebar.set_end_widget(Some(&controls_end));

    let workspaces = ListBox::new();
    workspaces.set_selection_mode(SelectionMode::Single);
    workspaces.add_css_class("workspace-list");
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
    let terminal = view::build(Rc::clone(&screens), Rc::clone(&theme), Rc::clone(&blink));

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
        key_controller.connect_key_pressed(move |_, key, _, state| {
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

    // --- pointer motion ---------------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let theme = Rc::clone(&theme);
        let pointer_for_enter = Rc::clone(&pointer);
        let pointer_for_motion = Rc::clone(&pointer);
        let pointer_for_leave = Rc::clone(&pointer);
        let throttle_for_enter = Rc::clone(&move_throttle);
        let throttle_for_motion = Rc::clone(&move_throttle);
        let throttle_for_leave = Rc::clone(&move_throttle);
        let terminal_for_motion = terminal.clone();
        let motion = EventControllerMotion::new();
        motion.connect_enter(move |_, x, y| {
            pointer_for_enter.set(Some((x, y)));
            throttle_for_enter.borrow_mut().reset();
        });
        motion.connect_motion(move |controller, x, y| {
            pointer_for_motion.set(Some((x, y)));
            let state = controller.current_event_state();
            let held = mouse_button_held(state);
            if held && state.contains(gdk::ModifierType::SHIFT_MASK) {
                return;
            }
            let metrics = view::cell_metrics(&terminal_for_motion, &theme);
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
        let click = GestureClick::new();
        click.set_button(0);
        click.connect_pressed(move |gesture, _, x, y| {
            terminal_for_click.grab_focus();
            let screens = screens.borrow();
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
            if let Some(tab) = pane.tabs.iter().find(|tab| tab.rect.contains(x, y)) {
                let _ = worker.input.send(Input::FocusTab {
                    workspace: workspace.workspace_id.clone(),
                    screen: workspace.screen_id.clone(),
                    pane: pane.pane.clone(),
                    tab: tab.id.clone(),
                    target: tab.terminal.clone(),
                });
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
            let button_number = gesture.current_button();
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
        let anchor: Rc<RefCell<Option<DragAnchor>>> = Rc::new(RefCell::new(None));

        let terminal_for_begin = terminal.clone();
        let screens_for_begin = Rc::clone(&screens);
        let anchor_for_begin = Rc::clone(&anchor);
        let theme_for_begin = Rc::clone(&theme);
        drag.connect_drag_begin(move |_, x, y| {
            terminal_for_begin.grab_focus();
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
        let anchor_for_update = Rc::clone(&anchor);
        let theme_for_update = Rc::clone(&theme);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let Some((terminal_id, start, content)) = anchor_for_update.borrow().clone() else {
                return;
            };
            let metrics = view::cell_metrics(&terminal_for_update, &theme_for_update);
            let mut screens = screens.borrow_mut();
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
        terminal.add_controller(drag);
    }

    // --- workspace switching ---------------------------------------------
    let entries: Rc<RefCell<Vec<session::WorkspaceEntry>>> = Rc::new(RefCell::new(Vec::new()));
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
                            while let Some(child) = workspaces.first_child() {
                                workspaces.remove(&child);
                            }
                            for entry in &list {
                                let row = workspace_row(entry, Rc::clone(&theme));
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
}
