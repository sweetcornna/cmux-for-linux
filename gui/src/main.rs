//! cmux-gtk — a GTK4 frontend for cmux.
//!
//! The Linux GUI described in docs/linux-port.md: a window that speaks
//! `cmux.protocol/1` to a running session, lists its workspaces, and renders
//! the focused screen from the server's styled render streams.
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
    Application, ApplicationWindow, Box as GtkBox, DrawingArea, EventControllerKey,
    EventControllerScroll, EventControllerScrollFlags, GestureClick, GestureDrag, HeaderBar, Label,
    ListBox, ListBoxRow, Orientation, Paned, ScrolledWindow, SelectionMode,
};

use screen::ScreenSet;
use session::{AttachmentSpec, Control, Input, Update, Worker};

const APP_ID: &str = "com.github.sweetcornna.cmux-gtk";
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

fn sync_attachments(worker: &Worker, screens: &ScreenSet, terminal: &DrawingArea) {
    let metrics = view::cell_metrics(terminal);
    let specs = view::visible_terminal_sizes(screens, metrics, terminal.width(), terminal.height())
        .into_iter()
        .map(|(terminal, size)| AttachmentSpec { terminal, size })
        .collect();
    let _ = worker.control.send(Control::SyncAttachments(specs));
}

fn build_ui(application: &Application) {
    let args = parse_args();
    let screens = Rc::new(RefCell::new(ScreenSet::default()));
    let (update_tx, update_rx) = async_channel::unbounded::<Update>();
    let worker = Rc::new(session::spawn(
        args.session.clone(),
        args.socket.clone(),
        update_tx,
    ));

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
    let terminal = view::build(Rc::clone(&screens));

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
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let last = Cell::new((0i32, 0i32));
        terminal.connect_resize(move |area, width, height| {
            if last.replace((width, height)) == (width, height) {
                return;
            }
            sync_attachments(&worker, &screens.borrow(), area);
        });
    }

    // --- keyboard ---------------------------------------------------------
    let key_controller = EventControllerKey::new();
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let terminal_for_keys = terminal.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);
            let shift = state.contains(gdk::ModifierType::SHIFT_MASK);
            if ctrl && shift && matches!(key, gdk::Key::C | gdk::Key::c) {
                if let Some(text) = screens.borrow().selected_text() {
                    terminal_for_keys.clipboard().set_text(&text);
                }
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

    // --- scrollback -------------------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let scroll = EventControllerScroll::new(EventControllerScrollFlags::VERTICAL);
        scroll.connect_scroll(move |_, _, delta_y| {
            // The protocol uses positive rows for moving back into history,
            // opposite to GDK's positive-down wheel coordinate.
            let rows = -(delta_y.round() as i32) * SCROLL_ROWS;
            if rows != 0 {
                let _ = worker.input.send(Input::Scroll(rows));
            }
            gtk4::glib::Propagation::Stop
        });
        terminal.add_controller(scroll);
    }

    // --- pane and tab focus ----------------------------------------------
    {
        let worker = Rc::clone(&worker);
        let screens = Rc::clone(&screens);
        let terminal_for_click = terminal.clone();
        let click = GestureClick::new();
        click.connect_pressed(move |_, _, x, y| {
            terminal_for_click.grab_focus();
            let screens = screens.borrow();
            let Some(workspace) = screens.workspace.as_ref() else {
                return;
            };
            let geometries = view::pane_geometries(
                &screens,
                view::cell_metrics(&terminal_for_click),
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
        drag.connect_drag_begin(move |_, x, y| {
            terminal_for_begin.grab_focus();
            let metrics = view::cell_metrics(&terminal_for_begin);
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
        drag.connect_drag_update(move |gesture, dx, dy| {
            let Some((start_x, start_y)) = gesture.start_point() else {
                return;
            };
            let Some((terminal_id, start, content)) = anchor_for_update.borrow().clone() else {
                return;
            };
            let metrics = view::cell_metrics(&terminal_for_update);
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
            sync_attachments(&worker, &screens.borrow(), &terminal);
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
                            sync_attachments(&worker, &screens.borrow(), &terminal);
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
                            sync_attachments(&worker, &screens.borrow(), &terminal);
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
