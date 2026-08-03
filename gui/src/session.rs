//! The protocol worker.
//!
//! The cmux resource SDK is blocking and GTK is single-threaded, so all
//! protocol work happens on worker threads and reaches the UI as messages on
//! an async channel. The UI thread never blocks on a socket.
//!
//! Two threads, because they need different connections:
//!
//! * the **stream thread** owns the terminal attachment. It polls with a short
//!   timeout rather than blocking forever on the iterator, which is what lets
//!   it act on resize and re-attach requests between frames — the viewer lease
//!   lives on the attachment's own connection and cannot be touched from
//!   anywhere else.
//! * the **control thread** sends input, scrolls and focuses workspaces over
//!   the shared control connection.

use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cmux::{
    Client, Config, RenderPatch, RenderSnapshot, ScrollOptions, Selector, Size, StreamPoll,
    TabId, TerminalAttachOptions, TerminalAttachmentItem, TerminalId, WorkspaceId,
};

/// How long the stream thread waits for a frame before checking for commands.
/// Short enough that a resize feels immediate, long enough not to spin.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Sent from the worker to the UI.
#[derive(Debug)]
pub enum Update {
    Connected { session_name: String },
    Workspaces(Vec<WorkspaceEntry>),
    Attached { terminal: TerminalId },
    Snapshot { terminal: TerminalId, render: Box<RenderSnapshot> },
    Patch { terminal: TerminalId, render: Box<RenderPatch> },
    Scroll { at_bottom: bool },
    Detached,
    Error(String),
}

/// Sent from the UI to the control thread.
#[derive(Debug)]
pub enum Input {
    Bytes(Vec<u8>),
    Scroll(i32),
    FocusWorkspace(WorkspaceId),
    RefreshWorkspaces,
    /// Routes later input to this terminal. Sent when the view re-attaches, so
    /// typing follows what is on screen.
    SetTarget(TerminalId),
    Stop,
}

/// Sent from the UI to the stream thread.
#[derive(Debug)]
pub enum Control {
    Resize(Size),
    Attach(TerminalId),
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceEntry {
    pub id: WorkspaceId,
    pub name: String,
    pub focused: bool,
    /// Terminals belonging to this workspace, in discovery order. Empty when
    /// the workspace has no terminal to attach to.
    pub terminals: Vec<TerminalId>,
}

pub struct Worker {
    pub input: mpsc::Sender<Input>,
    pub control: mpsc::Sender<Control>,
}

impl Worker {
    pub fn stop(&self) {
        let _ = self.input.send(Input::Stop);
        let _ = self.control.send(Control::Stop);
    }
}

pub fn spawn(
    session_name: String,
    socket: Option<PathBuf>,
    updates: async_channel::Sender<Update>,
) -> Worker {
    let (input_tx, input_rx) = mpsc::channel::<Input>();
    let (control_tx, control_rx) = mpsc::channel::<Control>();
    let control_for_stream = control_tx.clone();

    thread::spawn(move || {
        let config = match socket {
            Some(path) => Config::from_socket_path(path),
            None => Config::from_env_or_default_session(&session_name),
        };

        let client = match Client::connect(config) {
            Ok(client) => client,
            Err(error) => {
                let _ = updates.send_blocking(Update::Error(format!(
                    "could not connect to session '{session_name}': {error}. \
                     Start one with `cmux --headless --session {session_name}`."
                )));
                return;
            }
        };

        let _ = updates.send_blocking(Update::Connected { session_name });

        // Discover the topology before attaching so the first attach can target
        // a terminal the sidebar also knows about.
        let first_terminal = match publish_workspaces(&client, &updates) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = updates.send_blocking(Update::Error(error));
                None
            }
        };

        let control_client = client.clone();
        let control_updates = updates.clone();
        thread::spawn(move || control_loop(control_client, control_updates, input_rx));

        if let Some(terminal) = first_terminal {
            let _ = control_for_stream.send(Control::Attach(terminal));
        } else {
            let _ = updates.send_blocking(Update::Error(
                "the session has no terminals yet; create a workspace first".to_string(),
            ));
        }

        stream_loop(client, updates, control_rx);
    });

    Worker { input: input_tx, control: control_tx }
}

/// Owns the attachment and keeps it in step with the requested size and target.
fn stream_loop(
    client: Client,
    updates: async_channel::Sender<Update>,
    controls: mpsc::Receiver<Control>,
) {
    let session = client.session(Selector::current());
    let mut attachment: Option<cmux::TerminalAttachment> = None;
    let mut attached_to: Option<TerminalId> = None;
    let mut size = Size { cols: 100, rows: 30 };

    loop {
        // Commands first: a pending re-attach should not wait for a frame.
        loop {
            match controls.try_recv() {
                Ok(Control::Stop) => return,
                Ok(Control::Resize(next)) => {
                    if next == size {
                        continue;
                    }
                    size = next;
                    if let Some(stream) = attachment.as_mut() {
                        match stream.resize(size) {
                            // The server decides: `accepted` reports whether
                            // this viewer's request changed anything, and
                            // `size` is what it settled on. Reporting the
                            // server's answer beats assuming the request won.
                            Ok(result) => eprintln!(
                                "viewer resize {}x{} accepted={} effective={}x{}",
                                size.cols,
                                size.rows,
                                result.accepted,
                                result.size.cols,
                                result.size.rows
                            ),
                            Err(error) => {
                                let _ = updates
                                    .send_blocking(Update::Error(format!("resize failed: {error}")));
                            }
                        }
                    }
                }
                Ok(Control::Attach(terminal)) => {
                    if attached_to.as_ref() == Some(&terminal) {
                        continue;
                    }
                    // Dropping the old attachment closes its connection and
                    // releases the viewer lease.
                    attachment = None;
                    match open_attachment(&session, &terminal, size) {
                        Ok(stream) => {
                            attached_to = Some(terminal.clone());
                            attachment = Some(stream);
                            let _ = updates.send_blocking(Update::Attached { terminal });
                        }
                        Err(error) => {
                            attached_to = None;
                            let _ = updates.send_blocking(Update::Error(error));
                        }
                    }
                }
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => return,
            }
        }

        let Some(stream) = attachment.as_mut() else {
            thread::sleep(POLL_INTERVAL);
            continue;
        };

        match stream.next_timeout(POLL_INTERVAL) {
            Ok(StreamPoll::Item(item)) => match item.value {
                TerminalAttachmentItem::Snapshot { terminal_id, render } => {
                    let _ = updates.send_blocking(Update::Snapshot {
                        terminal: terminal_id,
                        render: Box::new(render),
                    });
                }
                TerminalAttachmentItem::Patch { terminal_id, render } => {
                    let _ = updates.send_blocking(Update::Patch {
                        terminal: terminal_id,
                        render: Box::new(render),
                    });
                }
                TerminalAttachmentItem::Scroll { scroll, .. } => {
                    let _ = updates.send_blocking(Update::Scroll { at_bottom: scroll.at_bottom });
                }
                TerminalAttachmentItem::Unknown { .. } => {}
            },
            Ok(StreamPoll::TimedOut) => {}
            Ok(StreamPoll::End) => {
                attachment = None;
                attached_to = None;
                let _ = updates.send_blocking(Update::Detached);
            }
            Err(error) => {
                attachment = None;
                attached_to = None;
                let _ = updates.send_blocking(Update::Error(format!("stream error: {error}")));
            }
        }
    }
}

fn open_attachment(
    session: &cmux::Session,
    terminal: &TerminalId,
    size: Size,
) -> Result<cmux::TerminalAttachment, String> {
    let handle = session.terminal(Selector::id(terminal.clone()));
    let mut stream = handle
        .attach(TerminalAttachOptions { size: Some(size), read_only: Some(false) })
        .map_err(|error| format!("attach failed: {error}"))?;

    // The viewer lease is what makes the server start sending render frames.
    // Without it the attachment opens and then immediately ends, which looks
    // exactly like a successful attach that draws nothing.
    stream.resize(size).map_err(|error| format!("viewer resize failed: {error}"))?;

    // The lease says "I am looking at this terminal at this size"; sizing
    // authority is what makes the server actually resize the PTY to match.
    // Not fatal: a read-only view of a terminal someone else sizes is still
    // useful, so a refusal is reported rather than aborting the attach.
    // Exclusive: this window is the only thing looking at the terminal, so it
    // should decide the size outright rather than negotiate with a participant
    // that is not on screen.
    if let Err(error) = stream.set_sizing(terminal, true, Some(true)) {
        eprintln!("exclusive sizing refused ({error}); retrying shared");
        if let Err(error) = stream.set_sizing(terminal, true, None) {
            eprintln!("sizing authority refused: {error}");
        }
    }
    Ok(stream)
}

fn control_loop(
    client: Client,
    updates: async_channel::Sender<Update>,
    inputs: mpsc::Receiver<Input>,
) {
    let session = client.session(Selector::current());
    let mut target: Option<TerminalId> = None;

    while let Ok(input) = inputs.recv() {
        match input {
            Input::Stop => break,
            Input::Bytes(bytes) => {
                let Some(terminal) = current_target(&session, &mut target, &updates) else {
                    continue;
                };
                if let Err(error) =
                    session.terminal(Selector::id(terminal)).write_bytes(&bytes)
                {
                    let _ =
                        updates.send_blocking(Update::Error(format!("write failed: {error}")));
                }
            }
            Input::Scroll(delta_rows) => {
                let Some(terminal) = current_target(&session, &mut target, &updates) else {
                    continue;
                };
                if let Err(error) = session
                    .terminal(Selector::id(terminal))
                    .scroll(ScrollOptions { delta_rows })
                {
                    let _ =
                        updates.send_blocking(Update::Error(format!("scroll failed: {error}")));
                }
            }
            Input::FocusWorkspace(id) => {
                if let Err(error) = session.workspace(Selector::id(id)).focus() {
                    let _ = updates.send_blocking(Update::Error(format!("focus failed: {error}")));
                }
            }
            Input::RefreshWorkspaces => {
                if let Err(error) = publish_workspaces(&client, &updates) {
                    let _ = updates.send_blocking(Update::Error(error));
                }
            }
            Input::SetTarget(terminal) => target = Some(terminal),
        }
    }
}

/// The terminal input is currently routed to. Set by the UI through
/// `SetTarget`-style attaches; falls back to the session's first terminal so a
/// key press before the first attach is not silently dropped.
fn current_target(
    session: &cmux::Session,
    cached: &mut Option<TerminalId>,
    updates: &async_channel::Sender<Update>,
) -> Option<TerminalId> {
    if let Some(terminal) = cached.clone() {
        return Some(terminal);
    }
    match session.terminals() {
        Ok(terminals) => {
            for terminal in terminals {
                if let Ok(snapshot) = terminal.refresh() {
                    *cached = Some(snapshot.id.clone());
                    return Some(snapshot.id);
                }
            }
            None
        }
        Err(error) => {
            let _ = updates
                .send_blocking(Update::Error(format!("could not list terminals: {error}")));
            None
        }
    }
}

/// Publishes the workspace list with each workspace's terminals, and returns
/// the terminal the window should attach to first.
///
/// Terminals are matched to workspaces through their tab: a terminal snapshot
/// carries `tab_id`, and walking workspace → screen → pane → tab yields the
/// tabs each workspace owns. The public API exposes no direct
/// workspace-to-terminal edge.
fn publish_workspaces(
    client: &Client,
    updates: &async_channel::Sender<Update>,
) -> Result<Option<TerminalId>, String> {
    let session = client.session(Selector::current());

    let mut terminals_by_tab: Vec<(TabId, TerminalId)> = Vec::new();
    for terminal in session
        .terminals()
        .map_err(|error| format!("could not list terminals: {error}"))?
    {
        if let Ok(snapshot) = terminal.refresh() {
            terminals_by_tab.push((snapshot.tab_id, snapshot.id));
        }
    }

    let workspaces = session
        .workspaces()
        .map_err(|error| format!("could not list workspaces: {error}"))?;

    let mut entries = Vec::new();
    let mut first = None;
    for workspace in &workspaces {
        let Ok(snapshot) = workspace.refresh() else { continue };

        let mut tabs = Vec::new();
        if let Ok(screens) = workspace.screens() {
            for screen in screens {
                let Ok(panes) = screen.panes() else { continue };
                for pane in panes {
                    let Ok(pane_tabs) = pane.tabs() else { continue };
                    for tab in pane_tabs {
                        if let Ok(tab_snapshot) = tab.refresh() {
                            tabs.push(tab_snapshot.id);
                        }
                    }
                }
            }
        }

        let terminals: Vec<TerminalId> = terminals_by_tab
            .iter()
            .filter(|(tab_id, _)| tabs.contains(tab_id))
            .map(|(_, terminal_id)| terminal_id.clone())
            .collect();

        if first.is_none() {
            first = terminals.first().cloned();
        }
        if snapshot.focused && !terminals.is_empty() {
            first = terminals.first().cloned();
        }

        entries.push(WorkspaceEntry {
            id: snapshot.id,
            name: snapshot.name,
            focused: snapshot.focused,
            terminals,
        });
    }

    // A session can hold terminals this walk did not reach; attaching to one
    // still beats showing an empty window.
    if first.is_none() {
        first = terminals_by_tab.first().map(|(_, terminal)| terminal.clone());
    }

    let _ = updates.send_blocking(Update::Workspaces(entries));
    Ok(first)
}
