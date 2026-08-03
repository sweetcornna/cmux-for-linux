//! The protocol workers.
//!
//! The cmux resource SDK is blocking and GTK is single-threaded, so all
//! protocol work happens on worker threads and reaches the UI as messages on
//! an async channel. The UI thread never blocks on a socket.
//!
//! The control thread sends input and focus mutations over a shared client.
//! An attachment manager owns the visible-terminal set and gives every
//! attachment its own polling thread. A slow terminal therefore cannot add its
//! 50ms poll timeout to every other pane's latency.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cmux::{
    Client, Config, LayoutNode, PaneId, RenderPatch, RenderSnapshot, ScreenId, ScrollOptions,
    Selector, Size, StreamPoll, TabContentId, TabId, TerminalAttachOptions, TerminalAttachmentItem,
    TerminalId, TerminalMouseOptions, TextInputOptions, WorkspaceId,
};

use crate::screen::{PaneView, TabContent, TabView, WorkspaceView};

/// Short enough that resize and shutdown commands feel immediate without
/// turning an idle attachment into a busy loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug)]
pub enum Update {
    Connected {
        session_name: String,
    },
    Workspaces(Vec<WorkspaceEntry>),
    Attached {
        terminal: TerminalId,
    },
    Snapshot {
        terminal: TerminalId,
        render: Box<RenderSnapshot>,
    },
    Patch {
        terminal: TerminalId,
        render: Box<RenderPatch>,
    },
    Scroll {
        terminal: TerminalId,
        at_bottom: bool,
    },
    Detached {
        terminal: TerminalId,
    },
    Error(String),
}

#[derive(Debug)]
pub enum Input {
    Bytes(Vec<u8>),
    Paste(String),
    Scroll {
        terminal: TerminalId,
        delta_rows: i32,
    },
    Mouse {
        terminal: TerminalId,
        options: TerminalMouseOptions,
    },
    FocusWorkspace {
        workspace: WorkspaceId,
        target: Option<TerminalId>,
    },
    FocusPane {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        target: Option<TerminalId>,
    },
    FocusTab {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        tab: TabId,
        target: Option<TerminalId>,
    },
    RefreshWorkspaces,
    /// `None` is meaningful for a focused browser tab: input must not leak to
    /// the terminal that happened to be focused before it.
    SetTarget(Option<TerminalId>),
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentSpec {
    pub terminal: TerminalId,
    pub size: Size,
}

#[derive(Debug)]
pub enum Control {
    SyncAttachments(Vec<AttachmentSpec>),
    Stop,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WorkspaceEntry {
    pub id: WorkspaceId,
    pub name: String,
    pub color: Option<String>,
    pub focused: bool,
    pub terminals: Vec<TerminalId>,
    pub view: Option<WorkspaceView>,
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
        if let Err(error) = publish_workspaces(&client, &updates) {
            let _ = updates.send_blocking(Update::Error(error));
        }

        let control_client = client.clone();
        let control_updates = updates.clone();
        thread::spawn(move || control_loop(control_client, control_updates, input_rx));

        attachment_manager_loop(client, updates, control_rx);
    });

    Worker {
        input: input_tx,
        control: control_tx,
    }
}

struct AttachmentWorker {
    commands: mpsc::Sender<AttachmentCommand>,
}

enum AttachmentCommand {
    Resize(Size),
    Stop,
}

fn attachment_manager_loop(
    client: Client,
    updates: async_channel::Sender<Update>,
    controls: mpsc::Receiver<Control>,
) {
    let session = client.session(Selector::current());
    let mut workers: HashMap<TerminalId, AttachmentWorker> = HashMap::new();

    while let Ok(control) = controls.recv() {
        match control {
            Control::Stop => {
                for worker in workers.into_values() {
                    let _ = worker.commands.send(AttachmentCommand::Stop);
                }
                return;
            }
            Control::SyncAttachments(specs) => {
                let desired: HashMap<TerminalId, Size> = specs
                    .into_iter()
                    .map(|spec| (spec.terminal, spec.size))
                    .collect();
                let removed: Vec<TerminalId> = workers
                    .keys()
                    .filter(|terminal| !desired.contains_key(*terminal))
                    .cloned()
                    .collect();
                for terminal in removed {
                    if let Some(worker) = workers.remove(&terminal) {
                        let _ = worker.commands.send(AttachmentCommand::Stop);
                    }
                }

                for (terminal, size) in desired {
                    if let Some(worker) = workers.get(&terminal) {
                        let _ = worker.commands.send(AttachmentCommand::Resize(size));
                        continue;
                    }
                    let (command_tx, command_rx) = mpsc::channel();
                    let attachment_session = session.clone();
                    let attachment_updates = updates.clone();
                    let attachment_terminal = terminal.clone();
                    thread::spawn(move || {
                        attachment_loop(
                            attachment_session,
                            attachment_terminal,
                            size,
                            attachment_updates,
                            command_rx,
                        );
                    });
                    workers.insert(
                        terminal,
                        AttachmentWorker {
                            commands: command_tx,
                        },
                    );
                }
            }
        }
    }

    for worker in workers.into_values() {
        let _ = worker.commands.send(AttachmentCommand::Stop);
    }
}

fn attachment_loop(
    session: cmux::Session,
    terminal: TerminalId,
    initial_size: Size,
    updates: async_channel::Sender<Update>,
    commands: mpsc::Receiver<AttachmentCommand>,
) {
    let mut size = initial_size;
    loop {
        match drain_attachment_commands(&commands, &mut size, None, &terminal, &updates) {
            AttachmentAction::Continue => {}
            AttachmentAction::Stop => return,
        }

        let mut stream = match open_attachment(&session, &terminal, size) {
            Ok(stream) => stream,
            Err(error) => {
                let _ = updates.send_blocking(Update::Error(format!("{terminal:?}: {error}")));
                match commands.recv_timeout(RECONNECT_INTERVAL) {
                    Ok(AttachmentCommand::Resize(next)) => size = next,
                    Ok(AttachmentCommand::Stop) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        return;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
                continue;
            }
        };
        let _ = updates.send_blocking(Update::Attached {
            terminal: terminal.clone(),
        });

        loop {
            match drain_attachment_commands(
                &commands,
                &mut size,
                Some(&mut stream),
                &terminal,
                &updates,
            ) {
                AttachmentAction::Continue => {}
                AttachmentAction::Stop => return,
            }

            match stream.next_timeout(POLL_INTERVAL) {
                Ok(StreamPoll::Item(item)) => match item.value {
                    TerminalAttachmentItem::Snapshot {
                        terminal_id,
                        render,
                    } => {
                        let _ = updates.send_blocking(Update::Snapshot {
                            terminal: terminal_id,
                            render: Box::new(render),
                        });
                    }
                    TerminalAttachmentItem::Patch {
                        terminal_id,
                        render,
                    } => {
                        let _ = updates.send_blocking(Update::Patch {
                            terminal: terminal_id,
                            render: Box::new(render),
                        });
                    }
                    TerminalAttachmentItem::Scroll {
                        terminal_id,
                        scroll,
                    } => {
                        let _ = updates.send_blocking(Update::Scroll {
                            terminal: terminal_id,
                            at_bottom: scroll.at_bottom,
                        });
                    }
                    TerminalAttachmentItem::Unknown { .. } => {}
                },
                Ok(StreamPoll::TimedOut) => {}
                Ok(StreamPoll::End) => {
                    let _ = updates.send_blocking(Update::Detached {
                        terminal: terminal.clone(),
                    });
                    break;
                }
                Err(error) => {
                    let _ = updates.send_blocking(Update::Error(format!(
                        "stream error for {terminal:?}: {error}"
                    )));
                    break;
                }
            }
        }
    }
}

enum AttachmentAction {
    Continue,
    Stop,
}

fn drain_attachment_commands(
    commands: &mpsc::Receiver<AttachmentCommand>,
    size: &mut Size,
    mut stream: Option<&mut cmux::TerminalAttachment>,
    terminal: &TerminalId,
    updates: &async_channel::Sender<Update>,
) -> AttachmentAction {
    loop {
        match commands.try_recv() {
            Ok(AttachmentCommand::Stop) | Err(mpsc::TryRecvError::Disconnected) => {
                return AttachmentAction::Stop;
            }
            Ok(AttachmentCommand::Resize(next)) => {
                if next == *size {
                    continue;
                }
                *size = next;
                let Some(stream) = stream.as_deref_mut() else {
                    continue;
                };
                match stream.resize(next) {
                    Ok(result) => eprintln!(
                        "viewer {terminal:?} resize {}x{} accepted={} effective={}x{}",
                        next.cols, next.rows, result.accepted, result.size.cols, result.size.rows
                    ),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "resize failed for {terminal:?}: {error}"
                        )));
                    }
                }
            }
            Err(mpsc::TryRecvError::Empty) => return AttachmentAction::Continue,
        }
    }
}

fn open_attachment(
    session: &cmux::Session,
    terminal: &TerminalId,
    size: Size,
) -> Result<cmux::TerminalAttachment, String> {
    eprintln!(
        "opening viewer {terminal:?} at {}x{} cells",
        size.cols, size.rows
    );
    let handle = session.terminal(Selector::id(terminal.clone()));
    let mut stream = handle
        .attach(TerminalAttachOptions {
            size: Some(size),
            read_only: Some(false),
        })
        .map_err(|error| format!("attach failed: {error}"))?;

    // The viewer lease starts frame delivery. Without this call an attachment
    // can open and immediately end while looking deceptively successful.
    stream
        .resize(size)
        .map_err(|error| format!("viewer resize failed: {error}"))?;

    // Sizing authority, rather than the viewer lease alone, is what allows
    // this pane rectangle to become the PTY's authoritative cell geometry.
    if let Err(error) = stream.set_sizing(terminal, true, Some(true)) {
        eprintln!("exclusive sizing refused for {terminal:?} ({error}); retrying shared");
        if let Err(error) = stream.set_sizing(terminal, true, None) {
            eprintln!("sizing authority refused for {terminal:?}: {error}");
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
                let Some(terminal) = target.clone() else {
                    continue;
                };
                if let Err(error) = session.terminal(Selector::id(terminal)).write_bytes(&bytes) {
                    let _ = updates.send_blocking(Update::Error(format!("write failed: {error}")));
                }
            }
            Input::Paste(text) => {
                let Some(terminal) = target.clone() else {
                    continue;
                };
                if let Err(error) = session
                    .terminal(Selector::id(terminal))
                    .paste(TextInputOptions { text })
                {
                    let _ = updates.send_blocking(Update::Error(format!("paste failed: {error}")));
                }
            }
            Input::Scroll {
                terminal,
                delta_rows,
            } => {
                if let Err(error) = session
                    .terminal(Selector::id(terminal))
                    .scroll(ScrollOptions { delta_rows })
                {
                    let _ = updates.send_blocking(Update::Error(format!("scroll failed: {error}")));
                }
            }
            Input::Mouse { terminal, options } => {
                if let Err(error) = session.terminal(Selector::id(terminal)).mouse(options) {
                    let _ = updates
                        .send_blocking(Update::Error(format!("mouse input failed: {error}")));
                }
            }
            Input::FocusWorkspace {
                workspace,
                target: next,
            } => {
                target = next;
                match session.workspace(Selector::id(workspace)).focus() {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "workspace focus failed: {error}"
                        )));
                    }
                }
            }
            Input::FocusPane {
                workspace,
                screen,
                pane,
                target: next,
            } => {
                target = next;
                let handle = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane));
                match handle.focus() {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("pane focus failed: {error}")));
                    }
                }
            }
            Input::FocusTab {
                workspace,
                screen,
                pane,
                tab,
                target: next,
            } => {
                target = next;
                let handle = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .tab(Selector::id(tab));
                match handle.focus() {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("tab focus failed: {error}")));
                    }
                }
            }
            Input::RefreshWorkspaces => {
                if let Err(error) = publish_workspaces(&client, &updates) {
                    let _ = updates.send_blocking(Update::Error(error));
                }
            }
            Input::SetTarget(next) => target = next,
        }
    }
}

fn refresh_after_focus(client: &Client, updates: &async_channel::Sender<Update>) {
    if let Err(error) = publish_workspaces(client, updates) {
        let _ = updates.send_blocking(Update::Error(error));
    }
}

/// Publishes workspace catalog data and the focused screen's complete layout.
/// The tab snapshot already contains a typed content ID, so no terminal-to-tab
/// reconstruction or private protocol identity is needed.
fn publish_workspaces(
    client: &Client,
    updates: &async_channel::Sender<Update>,
) -> Result<(), String> {
    let session = client.session(Selector::current());
    let workspaces = session
        .workspaces()
        .map_err(|error| format!("could not list workspaces: {error}"))?;

    let mut entries = Vec::new();
    for workspace in &workspaces {
        let snapshot = workspace
            .refresh()
            .map_err(|error| format!("could not read workspace: {error}"))?;
        let mut terminals = Vec::new();
        let mut views = Vec::new();

        for screen_handle in workspace
            .screens()
            .map_err(|error| format!("could not list screens: {error}"))?
        {
            let screen_snapshot = screen_handle
                .refresh()
                .map_err(|error| format!("could not read screen: {error}"))?;
            let mut panes = Vec::new();
            for pane_handle in screen_handle
                .panes()
                .map_err(|error| format!("could not list panes: {error}"))?
            {
                let pane_snapshot = pane_handle
                    .refresh()
                    .map_err(|error| format!("could not read pane: {error}"))?;
                let mut tabs = Vec::new();
                for tab_handle in pane_handle
                    .tabs()
                    .map_err(|error| format!("could not list tabs: {error}"))?
                {
                    let tab = tab_handle
                        .refresh()
                        .map_err(|error| format!("could not read tab: {error}"))?;
                    let content = match tab.content_id {
                        TabContentId::Terminal(terminal) => {
                            if !terminals.contains(&terminal) {
                                terminals.push(terminal.clone());
                            }
                            TabContent::Terminal(terminal)
                        }
                        TabContentId::Browser(_) => TabContent::Browser,
                    };
                    tabs.push(TabView {
                        id: tab.id,
                        name: tab.name,
                        index: tab.index,
                        focused: tab.focused,
                        content,
                    });
                }
                tabs.sort_by_key(|tab| tab.index);
                let active_tab_id =
                    active_tab_for_pane(&screen_snapshot.layout.root, &pane_snapshot.id).or_else(
                        || {
                            tabs.iter()
                                .find(|tab| tab.focused)
                                .map(|tab| tab.id.clone())
                        },
                    );
                panes.push(PaneView {
                    id: pane_snapshot.id,
                    name: pane_snapshot.name,
                    active_tab_id,
                    tabs,
                });
            }
            views.push((
                screen_snapshot.focused,
                WorkspaceView {
                    workspace_id: snapshot.id.clone(),
                    screen_id: screen_snapshot.id,
                    layout: screen_snapshot.layout,
                    panes,
                },
            ));
        }

        let view = views
            .iter()
            .find(|(focused, _)| *focused)
            .or_else(|| views.first())
            .map(|(_, view)| view.clone());
        let color = ["color", "color_name", "color_hex", "workspace_color"]
            .into_iter()
            .find_map(|key| snapshot.extra.get(key).and_then(serde_json::Value::as_str))
            .map(str::to_string);
        entries.push(WorkspaceEntry {
            id: snapshot.id,
            name: snapshot.name,
            color,
            focused: snapshot.focused,
            terminals,
            view,
        });
    }

    let _ = updates.send_blocking(Update::Workspaces(entries));
    Ok(())
}

fn active_tab_for_pane(node: &LayoutNode, pane: &PaneId) -> Option<TabId> {
    match node {
        LayoutNode::Leaf(leaf) if &leaf.pane_id == pane => leaf.active_tab_id.clone(),
        LayoutNode::Leaf(_) | LayoutNode::Stack(_) => None,
        LayoutNode::Split(split) => active_tab_for_pane(&split.first, pane)
            .or_else(|| active_tab_for_pane(&split.second, pane)),
        LayoutNode::Viewport(viewport) => viewport
            .columns
            .iter()
            .find_map(|column| active_tab_for_pane(&column.root, pane)),
    }
}
