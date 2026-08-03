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
    Application, ApplicationWindow, Box as GtkBox, DrawingArea, EventControllerKey,
    EventControllerMotion, EventControllerScroll, EventControllerScrollFlags, GestureClick,
    GestureDrag, HeaderBar, Label, ListBox, ListBoxRow, Orientation, Paned, ScrolledWindow,
    SelectionMode,
};

use screen::ScreenSet;
use session::{AttachmentSpec, Control, Input, Update, Worker};

const APP_ID: &str = "com.github.sweetcornna.cmux-gtk";
const SCROLL_ROWS: i32 = 3;
const BLINK_INTERVAL: Duration = Duration::from_millis(500);

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

    let header = HeaderBar::new();
    let title = Label::new(Some(&format!("cmux — {}", args.session)));
    header.set_title_widget(Some(&title));

    let workspaces = ListBox::new();
    workspaces.set_selection_mode(SelectionMode::Single);
    workspaces.add_css_class("navigation-sidebar");
    let sidebar = ScrolledWindow::builder()
        .child(&workspaces)
        .width_request(200)
        .build();
    let terminal = view::build(Rc::clone(&screens), Rc::clone(&theme), Rc::clone(&blink));

    let status = Label::new(Some("connecting…"));
    status.set_xalign(0.0);
    status.set_margin_start(8);
    status.set_margin_end(8);
    status.set_margin_top(4);
    status.set_margin_bottom(4);
    status.add_css_class("dim-label");

    let terminal_side = GtkBox::new(Orientation::Vertical, 0);
    terminal_side.append(&terminal);
    terminal_side.append(&status);
    let paned = Paned::builder()
        .orientation(Orientation::Horizontal)
        .start_child(&sidebar)
        .end_child(&terminal_side)
        .position(200)
        .resize_start_child(false)
        .build();

    let window = ApplicationWindow::builder()
        .application(application)
        .title("cmux")
        .default_width(1100)
        .default_height(700)
        .child(&paned)
        .build();
    window.set_titlebar(Some(&header));

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
        let last = Cell::new((0i32, 0i32));
        terminal.connect_resize(move |area, width, height| {
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
        let status = status.clone();
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
            terminal.queue_draw();
            let _ = worker.input.send(Input::FocusWorkspace {
                workspace: entry.id,
                target,
            });
            if entry.terminals.is_empty() {
                status.set_text(&format!("workspace '{}' has no terminal tabs", entry.name));
            }
        });
    }

    // --- updates ----------------------------------------------------------
    {
        let screens = Rc::clone(&screens);
        let terminal = terminal.clone();
        let status = status.clone();
        let workspaces = workspaces.clone();
        let entries = Rc::clone(&entries);
        let worker = Rc::clone(&worker);
        let theme = Rc::clone(&theme);
        gtk4::glib::spawn_future_local(async move {
            while let Ok(update) = update_rx.recv().await {
                match update {
                    Update::Connected { session_name } => {
                        status.set_text(&format!("connected to session '{session_name}'"));
                    }
                    Update::Workspaces(list) => {
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
                                let name = if entry.name.is_empty() {
                                    "(unnamed)".to_string()
                                } else {
                                    entry.name.clone()
                                };
                                let label = Label::new(Some(&if entry.terminals.len() > 1 {
                                    format!("{name}  ·  {}", entry.terminals.len())
                                } else {
                                    name
                                }));
                                label.set_xalign(0.0);
                                label.set_margin_start(10);
                                label.set_margin_end(10);
                                label.set_margin_top(6);
                                label.set_margin_bottom(6);
                                let row = ListBoxRow::new();
                                row.set_child(Some(&label));
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
                        }
                        terminal.queue_draw();
                    }
                    Update::Attached { terminal: id } => {
                        eprintln!("viewer attached to {id:?}");
                        status.set_text("attached");
                        terminal.grab_focus();
                    }
                    Update::Snapshot {
                        terminal: id,
                        render,
                    } => {
                        if screens.borrow().contains_terminal(&id) {
                            screens.borrow_mut().apply_snapshot(id, *render);
                            terminal.queue_draw();
                        }
                    }
                    Update::Patch {
                        terminal: id,
                        render,
                    } => {
                        if screens.borrow().contains_terminal(&id) {
                            match screens.borrow_mut().apply_patch(&id, *render) {
                                Ok(()) => terminal.queue_draw(),
                                Err(error) => {
                                    status.set_text(&format!("render desync: {error}"));
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
                            status.set_text(if at_bottom {
                                "attached"
                            } else {
                                "scrolled back"
                            });
                        }
                        terminal.queue_draw();
                    }
                    Update::Detached { terminal: id } => {
                        if screens.borrow().contains_terminal(&id) {
                            status.set_text(&format!("detached from {id:?}; reconnecting"));
                        }
                    }
                    Update::Error(message) => status.set_text(&message),
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
