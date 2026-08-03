//! The protocol worker.
//!
//! The cmux resource SDK is blocking, and GTK is single-threaded, so all
//! protocol work happens on worker threads and reaches the UI as messages on
//! an async channel. The UI thread never blocks on a socket.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use cmux::{
    Config, Client, RenderPatch, RenderSnapshot, Selector, Size, TerminalAttachOptions,
    TerminalAttachmentItem, TerminalId, WorkspaceId,
};

/// Sent from the worker to the UI.
#[derive(Debug)]
pub enum Update {
    Connected { session_name: String },
    Workspaces(Vec<WorkspaceEntry>),
    Attached {
        #[allow(dead_code)]
        terminal: TerminalId,
    },
    Snapshot { terminal: TerminalId, render: Box<RenderSnapshot> },
    Patch { terminal: TerminalId, render: Box<RenderPatch> },
    Scroll { at_bottom: bool },
    Detached,
    Error(String),
}

/// Sent from the UI to the worker.
#[derive(Debug)]
pub enum Command {
    Input(Vec<u8>),
    #[allow(dead_code)]
    Resize(Size),
    SelectWorkspace(WorkspaceId),
    Shutdown,
}

#[derive(Clone, Debug)]
pub struct WorkspaceEntry {
    pub id: WorkspaceId,
    pub name: String,
    #[allow(dead_code)]
    pub focused: bool,
}

pub struct Worker {
    pub commands: mpsc::Sender<Command>,
}

pub fn spawn(
    session: String,
    socket: Option<PathBuf>,
    updates: async_channel::Sender<Update>,
) -> Worker {
    let (command_tx, command_rx) = mpsc::channel::<Command>();
    let attach_commands = command_tx.clone();

    thread::spawn(move || {
        let config = match socket {
            Some(path) => Config::from_socket_path(path),
            None => Config::from_env_or_default_session(&session),
        };

        let client = match Client::connect(config) {
            Ok(client) => client,
            Err(error) => {
                let _ = updates.send_blocking(Update::Error(format!(
                    "could not connect to session '{session}': {error}. \
                     Start one with `cmux --headless --session {session}`."
                )));
                return;
            }
        };

        let _ = updates.send_blocking(Update::Connected { session_name: session.clone() });

        run(client, updates, command_rx, attach_commands);
    });

    Worker { commands: command_tx }
}

fn run(
    client: Client,
    updates: async_channel::Sender<Update>,
    commands: mpsc::Receiver<Command>,
    _self_commands: mpsc::Sender<Command>,
) {
    let session = client.session(Selector::current());

    if let Err(error) = publish_workspaces(&session, &updates) {
        let _ = updates.send_blocking(Update::Error(error));
    }

    // Attach to whichever terminal the session currently exposes first. Picking
    // a terminal explicitly is the sidebar's job once workspace switching lands.
    let terminal = match first_terminal(&session) {
        Ok(Some(terminal)) => terminal,
        Ok(None) => {
            let _ = updates.send_blocking(Update::Error(
                "the session has no terminals yet; create a workspace first".to_string(),
            ));
            return;
        }
        Err(error) => {
            let _ = updates.send_blocking(Update::Error(error));
            return;
        }
    };

    let handle = session.terminal(Selector::id(terminal.clone()));
    let size = Size { cols: 100, rows: 30 };
    let mut attachment = match handle.attach(TerminalAttachOptions {
        size: Some(size),
        read_only: Some(false),
    }) {
        Ok(attachment) => attachment,
        Err(error) => {
            let _ = updates.send_blocking(Update::Error(format!("attach failed: {error}")));
            return;
        }
    };

    // The viewer lease is what makes the server start sending render frames.
    // Without it the attachment opens and then immediately ends, which looks
    // exactly like a successful attach that draws nothing.
    if let Err(error) = attachment.resize(size) {
        let _ = updates.send_blocking(Update::Error(format!("viewer resize failed: {error}")));
        return;
    }

    let _ = updates.send_blocking(Update::Attached { terminal: terminal.clone() });

    // Input and resize are control-plane calls on a separate connection, so
    // they run on their own thread while this one blocks on the render stream.
    let input_client = client.clone();
    let input_terminal = terminal.clone();
    let input_updates = updates.clone();
    thread::spawn(move || {
        let session = input_client.session(Selector::current());
        let handle = session.terminal(Selector::id(input_terminal));
        while let Ok(command) = commands.recv() {
            match command {
                Command::Input(bytes) => {
                    if let Err(error) = handle.write_bytes(&bytes) {
                        let _ = input_updates
                            .send_blocking(Update::Error(format!("write failed: {error}")));
                    }
                }
                Command::Resize(_) => {
                    // The viewer lease lives on the attachment's connection,
                    // which this thread does not own. Resize is driven from the
                    // stream thread instead; see the note in main.rs.
                }
                Command::SelectWorkspace(id) => {
                    let workspace = session.workspace(Selector::id(id));
                    if let Err(error) = workspace.focus() {
                        let _ = input_updates
                            .send_blocking(Update::Error(format!("focus failed: {error}")));
                    }
                }
                Command::Shutdown => break,
            }
        }
    });

    // The typed stream yields envelopes carrying a sequence and cursor; only
    // the value matters here, since this frontend rebuilds from a fresh
    // snapshot on reconnect rather than resuming from a cursor.
    for item in attachment.by_ref() {
        match item.map(|envelope| envelope.value) {
            Ok(TerminalAttachmentItem::Snapshot { terminal_id, render }) => {
                let _ = updates.send_blocking(Update::Snapshot {
                    terminal: terminal_id,
                    render: Box::new(render),
                });
            }
            Ok(TerminalAttachmentItem::Patch { terminal_id, render }) => {
                let _ = updates.send_blocking(Update::Patch {
                    terminal: terminal_id,
                    render: Box::new(render),
                });
            }
            Ok(TerminalAttachmentItem::Scroll { scroll, .. }) => {
                let _ = updates.send_blocking(Update::Scroll { at_bottom: scroll.at_bottom });
            }
            Ok(TerminalAttachmentItem::Unknown { .. }) => {}
            Err(error) => {
                let _ = updates.send_blocking(Update::Error(format!("stream error: {error}")));
                break;
            }
        }
    }

    let _ = updates.send_blocking(Update::Detached);
}

fn publish_workspaces(
    session: &cmux::Session,
    updates: &async_channel::Sender<Update>,
) -> Result<(), String> {
    let workspaces = session
        .workspaces()
        .map_err(|error| format!("could not list workspaces: {error}"))?;

    let mut entries = Vec::new();
    for workspace in &workspaces {
        let snapshot = workspace
            .refresh()
            .map_err(|error| format!("could not read a workspace: {error}"))?;
        entries.push(WorkspaceEntry {
            id: snapshot.id.clone(),
            name: snapshot.name.clone(),
            focused: snapshot.focused,
        });
    }

    let _ = updates.send_blocking(Update::Workspaces(entries));
    Ok(())
}

fn first_terminal(session: &cmux::Session) -> Result<Option<TerminalId>, String> {
    let terminals = session
        .terminals()
        .map_err(|error| format!("could not list terminals: {error}"))?;
    for terminal in terminals {
        let snapshot = terminal
            .refresh()
            .map_err(|error| format!("could not read a terminal: {error}"))?;
        return Ok(Some(snapshot.id));
    }
    Ok(None)
}
