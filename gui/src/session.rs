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
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use cmux::{
    Client, Config, CreateScreenOptions, Direction, LayoutNode, PaneId, RenderPatch,
    RenderSnapshot, ScreenId, ScrollOptions, Selector, Size, SplitId, SplitOptions,
    SplitRatioOptions, StreamPoll, TabContentId, TabId, TerminalAttachOptions,
    TerminalAttachmentItem, TerminalCreateOptions, TerminalId, TerminalMouseOptions,
    TextInputOptions, WorkspaceId,
};

use crate::screen::{PaneView, ScreenTabView, TabContent, TabView, WorkspaceView};

/// Short enough that resize and shutdown commands feel immediate without
/// turning an idle attachment into a busy loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug)]
struct RetrySchedule {
    interval: Duration,
    timeout: Duration,
    elapsed: Duration,
}

impl RetrySchedule {
    fn new(interval: Duration, timeout: Duration) -> Self {
        Self {
            interval,
            timeout,
            elapsed: Duration::ZERO,
        }
    }
}

impl Iterator for RetrySchedule {
    type Item = Duration;

    fn next(&mut self) -> Option<Self::Item> {
        if self.interval.is_zero() {
            return None;
        }
        let next = self.elapsed.checked_add(self.interval)?;
        if next > self.timeout {
            return None;
        }
        self.elapsed = next;
        Some(self.interval)
    }
}

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
        /// Includes SDK-decoded image pixels and full placement state.
        render: Box<RenderSnapshot>,
    },
    Patch {
        terminal: TerminalId,
        /// Includes image upserts/deletions and optional placement replacement.
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
    CreateWorkspace,
    RenameWorkspace {
        workspace: WorkspaceId,
        name: String,
    },
    CloseWorkspace {
        workspace: WorkspaceId,
    },
    MoveWorkspace {
        workspace: WorkspaceId,
        index: u32,
    },
    CreateScreen {
        workspace: WorkspaceId,
    },
    FocusScreen {
        workspace: WorkspaceId,
        screen: ScreenId,
    },
    RenameScreen {
        workspace: WorkspaceId,
        screen: ScreenId,
        name: String,
    },
    CloseScreen {
        workspace: WorkspaceId,
        screen: ScreenId,
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
    CreateTab {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
    },
    CloseTab {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        tab: TabId,
    },
    RenameTab {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        tab: TabId,
        name: String,
    },
    SplitPane {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        direction: Direction,
    },
    ClosePane {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
    },
    SetSplitRatio {
        workspace: WorkspaceId,
        screen: ScreenId,
        pane: PaneId,
        split: SplitId,
        ratio: f64,
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
    auto_start: bool,
    updates: async_channel::Sender<Update>,
) -> Worker {
    let (input_tx, input_rx) = mpsc::channel::<Input>();
    let (control_tx, control_rx) = mpsc::channel::<Control>();

    thread::spawn(move || {
        let config = match socket {
            Some(path) => Config::from_socket_path(path),
            None => Config::from_env_or_default_session(&session_name),
        };

        let client = match connect(&session_name, config, auto_start, &updates) {
            Ok(client) => client,
            Err(message) => {
                let _ = updates.send_blocking(Update::Error(message));
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

fn connect(
    session_name: &str,
    config: Config,
    auto_start: bool,
    updates: &async_channel::Sender<Update>,
) -> Result<Client, String> {
    let first_error = match Client::connect(config.clone()) {
        Ok(client) => return Ok(client),
        Err(error) => error,
    };

    if !auto_start || !session_is_unavailable(&first_error, &config.socket_path) {
        return Err(format!(
            "Could not connect to session '{session_name}': {first_error}"
        ));
    }

    let _ = updates.send_blocking(Update::Error(format!(
        "Starting cmux session '{session_name}'..."
    )));
    start_headless_session(session_name, &config.socket_path)?;

    let mut last_error = first_error.to_string();
    for delay in RetrySchedule::new(STARTUP_RETRY_INTERVAL, STARTUP_TIMEOUT) {
        thread::sleep(delay);
        match Client::connect(config.clone()) {
            Ok(client) => return Ok(client),
            Err(error) => last_error = error.to_string(),
        }
    }

    Err(format!(
        "Could not connect to session '{session_name}' after starting cmux and waiting {} seconds: \
         {last_error}",
        STARTUP_TIMEOUT.as_secs()
    ))
}

fn session_is_unavailable(error: &cmux::Error, socket_path: &Path) -> bool {
    let cmux::Error::Connection(message) = error else {
        return false;
    };
    if !socket_path.exists() {
        return true;
    }

    let message = message.to_ascii_lowercase();
    message.contains("connection refused") || message.contains("os error 111")
}

fn start_headless_session(session_name: &str, socket_path: &Path) -> Result<(), String> {
    let mut child = Command::new("cmux")
        .arg("--headless")
        .arg("--session")
        .arg(session_name)
        .arg("--socket")
        .arg(socket_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| {
            format!("Could not start cmux session '{session_name}': failed to run `cmux`: {error}")
        })?;

    // Reap a server that exits early; a successful headless session normally
    // keeps this thread parked until the GTK process itself exits.
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
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
            Input::CreateWorkspace => match session.create_workspace(None) {
                Ok(_) => refresh_after_focus(&client, &updates),
                Err(error) => {
                    let _ = updates.send_blocking(Update::Error(format!(
                        "workspace creation failed: {error}"
                    )));
                }
            },
            Input::RenameWorkspace { workspace, name } => {
                match session.workspace(Selector::id(workspace)).rename(name) {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "workspace rename failed: {error}"
                        )));
                    }
                }
            }
            Input::CloseWorkspace { workspace } => {
                match session.workspace(Selector::id(workspace)).close() {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "workspace close failed: {error}"
                        )));
                    }
                }
            }
            Input::MoveWorkspace { workspace, index } => {
                match session.workspace(Selector::id(workspace)).move_to(index) {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "workspace move failed: {error}"
                        )));
                    }
                }
            }
            Input::CreateScreen { workspace } => {
                match session
                    .workspace(Selector::id(workspace))
                    .create_screen(CreateScreenOptions::default())
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "screen creation failed: {error}"
                        )));
                    }
                }
            }
            Input::FocusScreen { workspace, screen } => {
                target = None;
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .focus()
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("screen focus failed: {error}")));
                    }
                }
            }
            Input::RenameScreen {
                workspace,
                screen,
                name,
            } => {
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .rename(name)
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("screen rename failed: {error}")));
                    }
                }
            }
            Input::CloseScreen { workspace, screen } => {
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .close()
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("screen close failed: {error}")));
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
            Input::CreateTab {
                workspace,
                screen,
                pane,
            } => {
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .create_terminal(TerminalCreateOptions::default())
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("tab creation failed: {error}")));
                    }
                }
            }
            Input::CloseTab {
                workspace,
                screen,
                pane,
                tab,
            } => {
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .tab(Selector::id(tab))
                    .close()
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("tab close failed: {error}")));
                    }
                }
            }
            Input::RenameTab {
                workspace,
                screen,
                pane,
                tab,
                name,
            } => {
                match session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .tab(Selector::id(tab))
                    .rename(name)
                {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("tab rename failed: {error}")));
                    }
                }
            }
            Input::SplitPane {
                workspace,
                screen,
                pane,
                direction,
            } => {
                let handle = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane));
                match handle.split(SplitOptions::new(direction)) {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("pane split failed: {error}")));
                    }
                }
            }
            Input::ClosePane {
                workspace,
                screen,
                pane,
            } => {
                let handle = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane));
                match handle.close() {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("pane close failed: {error}")));
                    }
                }
            }
            Input::SetSplitRatio {
                workspace,
                screen,
                pane,
                split,
                ratio,
            } => {
                let handle = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane));
                match handle.set_split_ratio(SplitRatioOptions {
                    split_id: split,
                    ratio,
                }) {
                    Ok(_) => refresh_after_focus(&client, &updates),
                    Err(error) => {
                        let _ = updates
                            .send_blocking(Update::Error(format!("pane resize failed: {error}")));
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
        let mut screen_tabs = Vec::new();

        for screen_handle in workspace
            .screens()
            .map_err(|error| format!("could not list screens: {error}"))?
        {
            let screen_snapshot = screen_handle
                .refresh()
                .map_err(|error| format!("could not read screen: {error}"))?;
            screen_tabs.push(ScreenTabView {
                id: screen_snapshot.id.clone(),
                name: screen_snapshot.name.clone(),
                index: screen_snapshot.index,
                focused: screen_snapshot.focused,
            });
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
                    screen_tabs: Vec::new(),
                    layout: screen_snapshot.layout,
                    panes,
                },
            ));
        }

        screen_tabs.sort_by_key(|screen| screen.index);
        for (_, view) in &mut views {
            view.screen_tabs.clone_from(&screen_tabs);
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

#[cfg(test)]
mod tests {
    use super::RetrySchedule;
    use std::time::Duration;

    #[test]
    fn startup_retry_schedule_covers_the_timeout_without_exceeding_it() {
        let delays = RetrySchedule::new(Duration::from_millis(200), Duration::from_secs(5))
            .collect::<Vec<_>>();

        assert_eq!(delays.len(), 25);
        assert!(delays
            .iter()
            .all(|delay| *delay == Duration::from_millis(200)));
        assert_eq!(delays.into_iter().sum::<Duration>(), Duration::from_secs(5));
    }

    #[test]
    fn startup_retry_schedule_stops_before_a_partial_interval() {
        let delays = RetrySchedule::new(Duration::from_millis(200), Duration::from_millis(450))
            .collect::<Vec<_>>();

        assert_eq!(delays, vec![Duration::from_millis(200); 2]);
    }

    #[test]
    fn startup_retry_schedule_rejects_a_zero_interval() {
        let delays = RetrySchedule::new(Duration::ZERO, Duration::from_secs(5)).collect::<Vec<_>>();

        assert!(delays.is_empty());
    }
}
