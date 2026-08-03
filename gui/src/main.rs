//! cmux-gtk — a GTK4 frontend for cmux.
//!
//! The Linux GUI described in docs/linux-port.md: a window that speaks
//! `cmux.protocol/1` to a running session, lists its workspaces, and renders a
//! terminal from the server's styled render stream.
//!
//! The server stays the only terminal emulator. This process draws styled runs
//! and forwards input; it contains no VT parser and links no terminal
//! emulation library.

mod screen;
mod session;
mod view;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::gdk;
use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, EventControllerKey, EventControllerScroll,
    EventControllerScrollFlags, GestureDrag, HeaderBar, Label, ListBox, ListBoxRow, Orientation,
    Paned, ScrolledWindow, SelectionMode,
};

use screen::Screen;
use session::{Control, Input, Update};

const APP_ID: &str = "com.github.sweetcornna.cmux-gtk";

/// One wheel notch scrolls this many rows, matching the usual terminal step.
const SCROLL_ROWS: i32 = 3;

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
                     --probe runs the protocol worker without GTK and prints every\n\
                     update, which separates protocol failures from drawing ones."
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args { session, socket }
}

/// Runs the protocol worker without GTK and prints every update.
fn probe() -> gtk4::glib::ExitCode {
    let args = parse_args();
    let (tx, rx) = async_channel::unbounded::<Update>();
    let _worker = session::spawn(args.session.clone(), args.socket.clone(), tx);

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
            Ok(Update::Patch { render, .. }) => {
                println!("patch full_reset={} rows={}", render.full_reset, render.rows.len());
            }
            Ok(Update::Workspaces(list)) => {
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

fn build_ui(application: &Application) {
    let args = parse_args();

    let screen = Rc::new(RefCell::new(Screen::default()));
    let (update_tx, update_rx) = async_channel::unbounded::<Update>();
    let worker = Rc::new(session::spawn(args.session.clone(), args.socket.clone(), update_tx));

    let header = HeaderBar::new();
    let title = Label::new(Some(&format!("cmux — {}", args.session)));
    header.set_title_widget(Some(&title));

    let workspaces = ListBox::new();
    workspaces.set_selection_mode(SelectionMode::Single);
    workspaces.add_css_class("navigation-sidebar");

    let sidebar = ScrolledWindow::builder().child(&workspaces).width_request(200).build();
    let terminal = view::build(Rc::clone(&screen));

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

    // --- resize -----------------------------------------------------------
    // The terminal follows the widget: every allocation change recomputes how
    // many whole cells fit and tells the stream thread, which owns the viewer
    // lease. Duplicate sizes are dropped there.
    {
        let worker = Rc::clone(&worker);
        let last = Cell::new((0i32, 0i32));
        terminal.connect_resize(move |area, width, height| {
            if last.get() == (width, height) {
                return;
            }
            last.set((width, height));
            let metrics = view::cell_metrics(area);
            let size = view::viewport_size(metrics, width, height);
            eprintln!("widget {width}x{height}px -> {}x{} cells", size.cols, size.rows);
            let _ = worker.control.send(Control::Resize(size));
        });
    }

    // --- keyboard ---------------------------------------------------------
    let key_controller = EventControllerKey::new();
    {
        let worker = Rc::clone(&worker);
        let screen = Rc::clone(&screen);
        let terminal_for_keys = terminal.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            // Ctrl+Shift+C copies the selection instead of sending a control
            // byte, the way terminals conventionally resolve that conflict.
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if ctrl && shift && matches!(key, gdk::Key::C | gdk::Key::c) {
                if let Some(text) = screen.borrow().selected_text() {
                    terminal_for_keys.clipboard().set_text(&text);
                }
                return gtk4::glib::Propagation::Stop;
            }

            match view::key_to_bytes(key, state) {
                Some(bytes) => {
                    // Typing dismisses a selection, as in every terminal.
                    if screen.borrow().selection.is_some() {
                        screen.borrow_mut().clear_selection();
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

    // --- scrollback -------------------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(move |_, _, delta_y| {
            // Positive delta_y is downward; the protocol takes positive rows as
            // scrolling back into history, so the sign is inverted.
            let rows = -(delta_y.round() as i32) * SCROLL_ROWS;
            if rows != 0 {
                let _ = worker.input.send(Input::Scroll(rows));
            }
            gtk4::glib::Propagation::Stop
        });
        terminal.add_controller(scroll);
    }

    // --- selection --------------------------------------------------------
    {
        let screen = Rc::clone(&screen);
        let drag = GestureDrag::new();
        let anchor: Rc<Cell<(u16, u16)>> = Rc::new(Cell::new((0, 0)));

        let terminal_for_begin = terminal.clone();
        let screen_for_begin = Rc::clone(&screen);
        let anchor_for_begin = Rc::clone(&anchor);
        drag.connect_drag_begin(move |_, x, y| {
            terminal_for_begin.grab_focus();
            let metrics = view::cell_metrics(&terminal_for_begin);
            let size = screen_for_begin.borrow().size;
            anchor_for_begin.set(view::cell_at(metrics, size, x, y));
            screen_for_begin.borrow_mut().clear_selection();
            terminal_for_begin.queue_draw();
        });

        let terminal_for_update = terminal.clone();
        let anchor_for_update = Rc::clone(&anchor);
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((start_x, start_y)) = gesture.start_point() else { return };
            let metrics = view::cell_metrics(&terminal_for_update);
            let size = screen.borrow().size;
            let head = view::cell_at(metrics, size, start_x + dx, start_y + dy);
            screen.borrow_mut().set_selection(anchor_for_update.get(), head);
            terminal_for_update.queue_draw();
        });

        terminal.add_controller(drag);
    }

    // --- workspace switching ----------------------------------------------
    let entries: Rc<RefCell<Vec<session::WorkspaceEntry>>> = Rc::new(RefCell::new(Vec::new()));
    {
        let worker = Rc::clone(&worker);
        let entries = Rc::clone(&entries);
        let status = status.clone();
        workspaces.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let index = row.index();
            if index < 0 {
                return;
            }
            let entries = entries.borrow();
            let Some(entry) = entries.get(index as usize) else { return };

            let _ = worker.input.send(Input::FocusWorkspace(entry.id.clone()));
            match entry.terminals.first() {
                Some(terminal) => {
                    let _ = worker.control.send(Control::Attach(terminal.clone()));
                    let _ = worker.input.send(Input::SetTarget(terminal.clone()));
                }
                None => status.set_text(&format!(
                    "workspace '{}' has no terminal to attach to",
                    entry.name
                )),
            }
        });
    }

    // --- updates ----------------------------------------------------------
    {
        let screen = Rc::clone(&screen);
        let terminal = terminal.clone();
        let status = status.clone();
        let workspaces = workspaces.clone();
        let entries = Rc::clone(&entries);
        let worker = Rc::clone(&worker);
        gtk4::glib::spawn_future_local(async move {
            while let Ok(update) = update_rx.recv().await {
                match update {
                    Update::Connected { session_name } => {
                        status.set_text(&format!("connected to session '{session_name}'"));
                    }
                    Update::Workspaces(list) => {
                        // Rebuilding the list clears the selected row, so the
                        // periodic refresh must not touch it when nothing
                        // changed.
                        if *entries.borrow() == list {
                            continue;
                        }
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
                    Update::Attached { terminal: id } => {
                        status.set_text("attached");
                        let _ = worker.input.send(Input::SetTarget(id));
                        terminal.grab_focus();
                    }
                    Update::Snapshot { terminal: id, render } => {
                        screen.borrow_mut().apply_snapshot(id, *render);
                        terminal.queue_draw();
                    }
                    Update::Patch { terminal: id, render } => {
                        let result = screen.borrow_mut().apply_patch(&id, *render);
                        match result {
                            Ok(()) => terminal.queue_draw(),
                            // Losing sync means re-attaching, not drawing a
                            // half-updated grid; surface it instead of hiding it.
                            Err(error) => status.set_text(&format!("render desync: {error}")),
                        }
                    }
                    Update::Scroll { at_bottom } => {
                        screen.borrow_mut().apply_scroll(at_bottom);
                        status.set_text(if at_bottom { "attached" } else { "scrolled back" });
                        terminal.queue_draw();
                    }
                    Update::Detached => status.set_text("detached"),
                    Update::Error(message) => status.set_text(&message),
                }
            }
        });
    }

    // Workspaces change from outside this process — the "New cmux workspace
    // here" file-manager entry creates one, as does any other client — so the
    // sidebar re-reads the topology periodically rather than only at startup.
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
