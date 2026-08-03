//! cmux-gtk — a GTK4 frontend for cmux.
//!
//! Stage 1 of the Linux GUI described in docs/linux-port.md: a window that
//! speaks `cmux.protocol/1` to a running session, lists its workspaces, and
//! renders one terminal from the server's styled render stream.
//!
//! The server stays the only terminal emulator. This process draws styled runs
//! and forwards key presses; it contains no VT parser and links no terminal
//! emulation library.

mod screen;
mod session;
mod view;

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, EventControllerKey, HeaderBar, Label, ListBox,
    ListBoxRow, Orientation, Paned, ScrolledWindow, SelectionMode,
};

use screen::Screen;
use session::{Command, Update};

const APP_ID: &str = "com.github.sweetcornna.cmux-gtk";

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
                     cmux-gtk [--session <name>] [--socket <path>]\n\n\
                     Connects to a running cmux session. Start one with\n  \
                     cmux --headless --session <name>"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    Args { session, socket }
}

/// Runs the protocol worker without GTK and prints every update.
///
/// The render path is easier to diagnose without a display attached: this is
/// the same worker the window drives, so a failure reproduced here is a
/// protocol failure and not a drawing one.
fn probe() -> gtk4::glib::ExitCode {
    let args = parse_args();
    let (tx, rx) = async_channel::unbounded::<Update>();
    let _worker = session::spawn(args.session.clone(), args.socket.clone(), tx);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        match rx.recv_blocking() {
            Ok(Update::Snapshot { terminal, render }) => {
                println!(
                    "snapshot terminal={terminal:?} size={}x{} rows={} fg={} bg={}",
                    render.size.cols,
                    render.size.rows,
                    render.rows.len(),
                    render.default_fg.as_str(),
                    render.default_bg.as_str()
                );
            }
            Ok(Update::Patch { render, .. }) => {
                println!("patch full_reset={} rows={}", render.full_reset, render.rows.len());
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
    // The session name is parsed before GTK sees argv so `--session` is not
    // mistaken for a GApplication option.
    application.connect_activate(build_ui);
    application.run_with_args::<&str>(&[])
}

fn build_ui(application: &Application) {
    let args = parse_args();

    let screen = Rc::new(RefCell::new(Screen::default()));
    let (update_tx, update_rx) = async_channel::unbounded::<Update>();
    let worker = session::spawn(args.session.clone(), args.socket.clone(), update_tx);
    let commands = worker.commands;

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

    // Keyboard input goes to the attached terminal.
    let key_controller = EventControllerKey::new();
    {
        let commands = commands.clone();
        key_controller.connect_key_pressed(move |_, key, _, state| {
            match view::key_to_bytes(key, state) {
                Some(bytes) => {
                    let _ = commands.send(Command::Input(bytes));
                    gtk4::glib::Propagation::Stop
                }
                None => gtk4::glib::Propagation::Proceed,
            }
        });
    }
    terminal.add_controller(key_controller);

    // Clicking the grid focuses it, so typing goes to the PTY rather than the
    // sidebar.
    let click = gtk4::GestureClick::new();
    {
        let terminal = terminal.clone();
        click.connect_pressed(move |_, _, _, _| {
            terminal.grab_focus();
        });
    }
    terminal.add_controller(click);

    {
        let commands = commands.clone();
        let entries: Rc<RefCell<Vec<session::WorkspaceEntry>>> = Rc::new(RefCell::new(Vec::new()));
        let entries_for_select = Rc::clone(&entries);
        workspaces.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let index = row.index();
            if index < 0 {
                return;
            }
            if let Some(entry) = entries_for_select.borrow().get(index as usize) {
                let _ = commands.send(Command::SelectWorkspace(entry.id.clone()));
            }
        });

        let screen = Rc::clone(&screen);
        let terminal = terminal.clone();
        let status = status.clone();
        let workspaces = workspaces.clone();
        gtk4::glib::spawn_future_local(async move {
            while let Ok(update) = update_rx.recv().await {
                match update {
                    Update::Connected { session_name } => {
                        status.set_text(&format!("connected to session '{session_name}'"));
                    }
                    Update::Workspaces(list) => {
                        while let Some(child) = workspaces.first_child() {
                            workspaces.remove(&child);
                        }
                        for entry in &list {
                            let label = Label::new(Some(&if entry.name.is_empty() {
                                "(unnamed)".to_string()
                            } else {
                                entry.name.clone()
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
                    Update::Attached { .. } => {
                        status.set_text("attached");
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
                        terminal.queue_draw();
                    }
                    Update::Detached => status.set_text("detached"),
                    Update::Error(message) => status.set_text(&message),
                }
            }
        });
    }

    {
        let commands = commands.clone();
        window.connect_close_request(move |_| {
            let _ = commands.send(Command::Shutdown);
            gtk4::glib::Propagation::Proceed
        });
    }

    window.present();
    terminal.grab_focus();
}
