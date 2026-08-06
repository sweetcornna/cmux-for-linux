//! The protocol workers.
//!
//! The cmux resource SDK is blocking and GTK is single-threaded, so all
//! protocol work happens on worker threads and reaches the UI as messages on
//! an async channel. The UI thread never blocks on a socket.
//!
//! The control thread sends input and focus mutations over a shared client.
//! A session-event thread coalesces workspace-tree changes into topology
//! publications. An attachment manager owns the visible-terminal set and gives
//! every attachment its own polling thread. A slow terminal therefore cannot add
//! its 50ms poll timeout to every other pane's latency.

use std::collections::{HashMap, HashSet};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cmux::{
    AgentId, AgentSnapshot, Client, Config, CreateScreenOptions, Direction, EventStreamOptions,
    LayoutNode, NotificationId, NotificationSnapshot, PaneId, ReadHistoryOptions, RenderPatch,
    RenderSnapshot, ResourceChange, ResourceEntitySnapshot, ResourceKind, ResourceReference,
    ScreenId, ScrollOptions, Selector, SessionEvent, SessionEventStream, Size, SplitId,
    SplitOptions, SplitRatioOptions, StreamPoll, TabContentId, TabId, TerminalAttachOptions,
    TerminalAttachmentItem, TerminalCreateOptions, TerminalExit, TerminalExitOutcome, TerminalId,
    TerminalLifecycle, TerminalMouseOptions, TerminalSnapshot, TextInputOptions, WorkspaceId,
};

use crate::attention::AttentionState;
use crate::screen::{PaneView, ScreenTabView, TabContent, TabView, WorkspaceView};
use crate::search::{self, SearchRequest, SearchResults};

/// Short enough that resize and shutdown commands feel immediate without
/// turning an idle attachment into a busy loop.
const POLL_INTERVAL: Duration = Duration::from_millis(50);
const TOPOLOGY_REFRESH_INTERVAL: Duration = Duration::from_millis(125);
const TOPOLOGY_ERROR_THRESHOLD: u8 = 3;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const MAX_ATTACHMENT_OVERFLOW_REATTACHES: u8 = 3;
const STARTUP_RETRY_INTERVAL: Duration = Duration::from_millis(200);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const SESSION_PROBE_TIMEOUT: Duration = Duration::from_millis(300);

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

#[derive(Clone, Copy, Debug)]
struct TopologyRefreshSchedule {
    interval: Duration,
    next_allowed: Option<Instant>,
    pending: bool,
    consecutive_failures: u8,
}

impl TopologyRefreshSchedule {
    fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_allowed: None,
            pending: false,
            consecutive_failures: 0,
        }
    }

    fn request(&mut self, now: Instant) -> bool {
        if self.next_allowed.is_none_or(|deadline| now >= deadline) {
            self.next_allowed = Some(now + self.interval);
            self.pending = false;
            true
        } else {
            self.pending = true;
            false
        }
    }

    fn take_due(&mut self, now: Instant) -> bool {
        if self.pending && self.next_allowed.is_some_and(|deadline| now >= deadline) {
            self.next_allowed = Some(now + self.interval);
            self.pending = false;
            true
        } else {
            false
        }
    }

    fn poll_timeout(&self, now: Instant, maximum: Duration) -> Duration {
        if self.pending {
            self.next_allowed
                .map(|deadline| deadline.saturating_duration_since(now).min(maximum))
                .unwrap_or(maximum)
        } else {
            maximum
        }
    }

    fn record_publish_success(&mut self) {
        self.consecutive_failures = 0;
    }

    fn record_publish_failure(&mut self) -> bool {
        self.pending = true;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.consecutive_failures == TOPOLOGY_ERROR_THRESHOLD
    }
}

fn resource_kind_affects_topology(resource: ResourceKind) -> bool {
    match resource {
        ResourceKind::Session
        | ResourceKind::Workspace
        | ResourceKind::Screen
        | ResourceKind::Pane
        | ResourceKind::Tab
        | ResourceKind::Terminal => true,
        ResourceKind::Machine
        | ResourceKind::Browser
        | ResourceKind::Client
        | ResourceKind::Notification
        | ResourceKind::Agent
        | ResourceKind::PairingRequest
        | ResourceKind::FrontendProjection
        | ResourceKind::SidebarView => false,
    }
}

fn resource_change_affects_topology(change: &ResourceChange) -> bool {
    match change {
        ResourceChange::Upsert { resource, .. } | ResourceChange::Delete { resource, .. } => {
            resource_kind_affects_topology(*resource)
        }
        ResourceChange::Unknown { .. } => true,
    }
}

fn resource_kind_affects_attention(resource: ResourceKind) -> bool {
    matches!(resource, ResourceKind::Notification | ResourceKind::Agent)
}

fn resource_change_affects_attention(change: &ResourceChange) -> bool {
    match change {
        ResourceChange::Upsert { resource, .. } | ResourceChange::Delete { resource, .. } => {
            resource_kind_affects_attention(*resource)
        }
        ResourceChange::Unknown { .. } => false,
    }
}

fn session_event_affects_topology(event: &SessionEvent) -> bool {
    match event {
        SessionEvent::Snapshot(_) => true,
        SessionEvent::Delta(delta) => delta.changes.iter().any(resource_change_affects_topology),
        SessionEvent::Unknown { .. } => true,
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TerminalStatus {
    Launching,
    Running,
    Exited { summary: Option<String> },
}

fn exit_summary(exit: Option<&TerminalExit>) -> Option<String> {
    exit.map(|exit| match &exit.outcome {
        TerminalExitOutcome::Exit { code } => format!("status {code}"),
        TerminalExitOutcome::Signal {
            signal,
            core_dumped,
        } => {
            let suffix = if *core_dumped { " (core dumped)" } else { "" };
            format!("signal {signal}{suffix}")
        }
        TerminalExitOutcome::Unknown { reason } => reason.clone(),
    })
}

fn terminal_status(terminal: &TerminalSnapshot) -> TerminalStatus {
    match terminal.lifecycle {
        TerminalLifecycle::Launching => TerminalStatus::Launching,
        TerminalLifecycle::Running => TerminalStatus::Running,
        TerminalLifecycle::Exited => TerminalStatus::Exited {
            summary: exit_summary(terminal.exit.as_ref()),
        },
    }
}

#[derive(Debug)]
pub enum Update {
    Connected {
        session: SessionEntry,
    },
    Switching {
        session_name: String,
    },
    Disconnected {
        session_name: String,
        message: String,
    },
    Sessions(Vec<SessionEntry>),
    SwitchFailed {
        session_name: String,
        message: String,
    },
    Workspaces(Vec<WorkspaceEntry>),
    Attention(AttentionState),
    TerminalStatuses(HashMap<TerminalId, TerminalStatus>),
    TerminalCwds(HashMap<TerminalId, String>),
    GitBranch {
        directory: PathBuf,
        branch: Option<String>,
    },
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
        offset: u64,
        at_bottom: bool,
    },
    SearchResults(SearchResults),
    Detached {
        terminal: TerminalId,
    },
    Error(String),
}

#[derive(Debug)]
pub struct StampedUpdate {
    pub generation: u64,
    pub update: Update,
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
        target: Option<TerminalId>,
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
    Search(SearchRequest),
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
    pub screens: Vec<ScreenEntry>,
    pub view: Option<WorkspaceView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScreenEntry {
    pub id: ScreenId,
    pub terminals: Vec<TerminalId>,
    pub active_terminal: Option<TerminalId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionEntry {
    pub name: String,
    pub socket_path: PathBuf,
}

pub struct RoutedSender<T> {
    sender: Arc<Mutex<Option<mpsc::Sender<T>>>>,
}

impl<T> RoutedSender<T> {
    fn new() -> Self {
        Self {
            sender: Arc::new(Mutex::new(None)),
        }
    }

    pub fn send(&self, value: T) -> Result<(), mpsc::SendError<T>> {
        let sender = self.sender.lock().ok().and_then(|sender| sender.clone());
        match sender {
            Some(sender) => sender.send(value),
            None => Err(mpsc::SendError(value)),
        }
    }

    fn bind(&self, sender: mpsc::Sender<T>) {
        if let Ok(mut current) = self.sender.lock() {
            *current = Some(sender);
        }
    }

    fn clear(&self) {
        if let Ok(mut current) = self.sender.lock() {
            current.take();
        }
    }
}

pub struct Worker {
    pub input: RoutedSender<Input>,
    pub control: RoutedSender<Control>,
    supervisor: mpsc::Sender<SupervisorCommand>,
}

impl Worker {
    pub fn refresh_sessions(&self) {
        let _ = self.supervisor.send(SupervisorCommand::RefreshSessions);
    }

    pub fn switch_session(&self, session: SessionEntry) {
        let _ = self.supervisor.send(SupervisorCommand::Switch(session));
    }

    pub fn new_session(&self, name: String) {
        let _ = self.supervisor.send(SupervisorCommand::NewSession(name));
    }

    pub fn stop(&self) {
        let _ = self.supervisor.send(SupervisorCommand::Stop);
    }
}

pub fn spawn(
    session_name: String,
    socket: Option<PathBuf>,
    auto_start: bool,
    updates: async_channel::Sender<StampedUpdate>,
) -> Worker {
    let socket_path =
        socket.unwrap_or_else(|| Config::from_env_or_default_session(&session_name).socket_path);
    let initial = SessionEntry {
        name: session_name,
        socket_path,
    };
    let input = RoutedSender::new();
    let control = RoutedSender::new();
    let (supervisor_tx, supervisor_rx) = mpsc::channel();

    let supervisor_input = RoutedSender {
        sender: Arc::clone(&input.sender),
    };
    let supervisor_control = RoutedSender {
        sender: Arc::clone(&control.sender),
    };
    let runtime_events = supervisor_tx.clone();
    thread::spawn(move || {
        supervisor_loop(
            initial,
            auto_start,
            updates,
            supervisor_rx,
            runtime_events,
            supervisor_input,
            supervisor_control,
        )
    });

    Worker {
        input,
        control,
        supervisor: supervisor_tx,
    }
}

#[derive(Debug)]
enum SupervisorCommand {
    RefreshSessions,
    Switch(SessionEntry),
    NewSession(String),
    RuntimeDisconnected { generation: u64, message: String },
    Stop,
}

#[derive(Clone)]
struct UpdateSink {
    generation: u64,
    sender: async_channel::Sender<StampedUpdate>,
}

impl UpdateSink {
    fn send_blocking(&self, update: Update) -> Result<(), async_channel::SendError<StampedUpdate>> {
        self.sender.send_blocking(StampedUpdate {
            generation: self.generation,
            update,
        })
    }
}

fn send_update(updates: &async_channel::Sender<StampedUpdate>, generation: u64, update: Update) {
    let _ = updates.send_blocking(StampedUpdate { generation, update });
}

struct Runtime {
    input: mpsc::Sender<Input>,
    control: mpsc::Sender<Control>,
    session_event_stop: mpsc::Sender<()>,
    control_join: JoinHandle<()>,
    attachment_join: JoinHandle<()>,
    session_event_join: JoinHandle<()>,
}

impl Runtime {
    fn stop(self) {
        let _ = self.input.send(Input::Stop);
        let _ = self.control.send(Control::Stop);
        let _ = self.session_event_stop.send(());
        let _ = self.control_join.join();
        let _ = self.attachment_join.join();
        let _ = self.session_event_join.join();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct RebuildPlan {
    previous: SessionEntry,
    target: SessionEntry,
}

impl RebuildPlan {
    fn new(previous: &SessionEntry, target: SessionEntry) -> Option<Self> {
        (previous != &target).then(|| Self {
            previous: previous.clone(),
            target,
        })
    }

    fn commit(self, connected: SessionEntry) -> SessionEntry {
        debug_assert_eq!(self.target.socket_path, connected.socket_path);
        connected
    }

    fn rollback(self) -> SessionEntry {
        self.previous
    }
}

fn supervisor_loop(
    initial: SessionEntry,
    initial_auto_start: bool,
    updates: async_channel::Sender<StampedUpdate>,
    commands: mpsc::Receiver<SupervisorCommand>,
    runtime_events: mpsc::Sender<SupervisorCommand>,
    input: RoutedSender<Input>,
    control: RoutedSender<Control>,
) {
    let mut generation = 0;
    let mut current = initial;
    let mut runtime = match start_runtime(
        &current,
        initial_auto_start,
        generation,
        &updates,
        &runtime_events,
        &input,
        &control,
    ) {
        Ok((connected, runtime)) => {
            current = connected;
            Some(runtime)
        }
        Err(message) => {
            send_update(&updates, generation, Update::Error(message));
            None
        }
    };
    let mut reconnect_at: Option<Instant> = None;

    loop {
        let command = match reconnect_at {
            Some(deadline) => {
                match commands.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(command) => command,
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        match start_runtime(
                            &current,
                            false,
                            generation,
                            &updates,
                            &runtime_events,
                            &input,
                            &control,
                        ) {
                            Ok((connected, next_runtime)) => {
                                current = connected;
                                runtime = Some(next_runtime);
                                reconnect_at = None;
                            }
                            Err(message) => {
                                send_update(&updates, generation, Update::Error(message));
                                reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
                            }
                        }
                        continue;
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            None => match commands.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };

        match command {
            SupervisorCommand::RefreshSessions => match enumerate_sessions(&current) {
                Ok(sessions) => send_update(&updates, generation, Update::Sessions(sessions)),
                Err(message) => send_update(&updates, generation, Update::Error(message)),
            },
            SupervisorCommand::Switch(target) => {
                switch_runtime(
                    target,
                    false,
                    &mut current,
                    &mut runtime,
                    &mut reconnect_at,
                    &mut generation,
                    &updates,
                    &runtime_events,
                    &input,
                    &control,
                );
            }
            SupervisorCommand::NewSession(name) => {
                let existing = enumerate_sessions(&current)
                    .ok()
                    .and_then(|sessions| sessions.into_iter().find(|session| session.name == name));
                let (target, auto_start) = existing.map_or_else(
                    || {
                        (
                            SessionEntry {
                                socket_path: session_socket_path(&current.socket_path, &name),
                                name,
                            },
                            true,
                        )
                    },
                    |session| (session, false),
                );
                switch_runtime(
                    target,
                    auto_start,
                    &mut current,
                    &mut runtime,
                    &mut reconnect_at,
                    &mut generation,
                    &updates,
                    &runtime_events,
                    &input,
                    &control,
                );
            }
            SupervisorCommand::RuntimeDisconnected {
                generation: disconnected_generation,
                message,
            } => {
                if disconnected_generation != generation || runtime.is_none() {
                    continue;
                }
                generation = generation.wrapping_add(1);
                send_update(
                    &updates,
                    generation,
                    Update::Disconnected {
                        session_name: current.name.clone(),
                        message,
                    },
                );
                input.clear();
                control.clear();
                if let Some(runtime) = runtime.take() {
                    runtime.stop();
                }
                reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
            }
            SupervisorCommand::Stop => break,
        }
    }

    input.clear();
    control.clear();
    if let Some(runtime) = runtime {
        runtime.stop();
    }
}

#[allow(clippy::too_many_arguments)]
fn switch_runtime(
    target: SessionEntry,
    auto_start: bool,
    current: &mut SessionEntry,
    runtime: &mut Option<Runtime>,
    reconnect_at: &mut Option<Instant>,
    generation: &mut u64,
    updates: &async_channel::Sender<StampedUpdate>,
    runtime_events: &mpsc::Sender<SupervisorCommand>,
    input: &RoutedSender<Input>,
    control: &RoutedSender<Control>,
) {
    let Some(plan) = RebuildPlan::new(current, target) else {
        return;
    };
    *generation = generation.wrapping_add(1);
    send_update(
        updates,
        *generation,
        Update::Switching {
            session_name: plan.target.name.clone(),
        },
    );
    input.clear();
    control.clear();
    if let Some(previous_runtime) = runtime.take() {
        previous_runtime.stop();
    }
    *reconnect_at = None;

    match start_runtime(
        &plan.target,
        auto_start,
        *generation,
        updates,
        runtime_events,
        input,
        control,
    ) {
        Ok((connected, next_runtime)) => {
            *current = plan.commit(connected);
            *runtime = Some(next_runtime);
        }
        Err(message) => {
            let failed_name = plan.target.name.clone();
            let rollback = plan.rollback();
            *current = rollback.clone();
            *generation = generation.wrapping_add(1);
            match start_runtime(
                &rollback,
                true,
                *generation,
                updates,
                runtime_events,
                input,
                control,
            ) {
                Ok((connected, previous_runtime)) => {
                    *current = connected;
                    *runtime = Some(previous_runtime);
                    send_update(
                        updates,
                        *generation,
                        Update::SwitchFailed {
                            session_name: failed_name,
                            message,
                        },
                    );
                }
                Err(rollback_error) => {
                    send_update(
                        updates,
                        *generation,
                        Update::SwitchFailed {
                            session_name: failed_name,
                            message: format!(
                                "{message}; could not restore session '{}': {rollback_error}",
                                rollback.name
                            ),
                        },
                    );
                    *reconnect_at = Some(Instant::now() + RECONNECT_INTERVAL);
                }
            }
        }
    }
}

fn start_runtime(
    requested: &SessionEntry,
    auto_start: bool,
    generation: u64,
    updates: &async_channel::Sender<StampedUpdate>,
    runtime_events: &mpsc::Sender<SupervisorCommand>,
    input_route: &RoutedSender<Input>,
    control_route: &RoutedSender<Control>,
) -> Result<(SessionEntry, Runtime), String> {
    let sink = UpdateSink {
        generation,
        sender: updates.clone(),
    };
    let config = Config::from_socket_path(&requested.socket_path);
    let client = connect(&requested.name, config, auto_start, &sink)?;
    let name = connected_session_name(&client).unwrap_or_else(|| requested.name.clone());
    let connected = SessionEntry {
        name,
        socket_path: requested.socket_path.clone(),
    };
    let session_events = client
        .session(Selector::current())
        .events(EventStreamOptions::default())
        .map_err(|error| format!("could not open session event stream: {error}"))?;
    let (input_tx, input_rx) = mpsc::channel::<Input>();
    let (control_tx, control_rx) = mpsc::channel::<Control>();
    let (session_event_stop_tx, session_event_stop_rx) = mpsc::channel();
    let (attention_clear_tx, attention_clear_rx) = mpsc::channel();
    let control_client = client.clone();
    let control_updates = sink.clone();
    let control_join = thread::spawn(move || {
        control_loop(
            control_client,
            control_updates,
            input_rx,
            attention_clear_tx,
        )
    });
    let session_event_client = client.clone();
    let session_event_updates = sink.clone();
    let session_event_runtime_events = runtime_events.clone();
    let session_event_join = thread::spawn(move || {
        session_event_loop(
            session_event_client,
            session_events,
            session_event_updates,
            session_event_stop_rx,
            attention_clear_rx,
            generation,
            session_event_runtime_events,
        )
    });
    let attachment_updates = sink.clone();
    let attachment_events = runtime_events.clone();
    let attachment_join = thread::spawn(move || {
        attachment_manager_loop(
            client,
            attachment_updates,
            control_rx,
            generation,
            attachment_events,
        )
    });
    input_route.bind(input_tx.clone());
    control_route.bind(control_tx.clone());
    let runtime = Runtime {
        input: input_tx,
        control: control_tx,
        session_event_stop: session_event_stop_tx,
        control_join,
        attachment_join,
        session_event_join,
    };
    let _ = sink.send_blocking(Update::Connected {
        session: connected.clone(),
    });

    Ok((connected, runtime))
}

fn session_event_loop(
    client: Client,
    mut stream: SessionEventStream,
    updates: UpdateSink,
    stop: mpsc::Receiver<()>,
    attention_clears: mpsc::Receiver<TerminalId>,
    generation: u64,
    runtime_events: mpsc::Sender<SupervisorCommand>,
) {
    let mut refreshes = TopologyRefreshSchedule::new(TOPOLOGY_REFRESH_INTERVAL);
    let mut notifications = HashMap::<NotificationId, NotificationSnapshot>::new();
    let mut agents = HashMap::<AgentId, AgentSnapshot>::new();
    let mut terminal_cwds = HashMap::<TerminalId, String>::new();
    // Lifecycle comes from the resource stream, so it can both suppress stale
    // agent activity and explain dead panes without another protocol request.
    let mut terminal_statuses = HashMap::<TerminalId, TerminalStatus>::new();
    let mut published_attention = None;
    let mut published_terminal_statuses = None;

    loop {
        if session_event_stop_requested(&stop) {
            return;
        }

        let now = Instant::now();
        if refreshes.take_due(now) {
            publish_event_topology(&client, &updates, &mut refreshes);
        }
        let mut cleared = false;
        while let Ok(terminal) = attention_clears.try_recv() {
            cleared |= clear_terminal_notifications(&mut notifications, &terminal);
        }
        if cleared {
            publish_attention(
                &notifications,
                &agents,
                &terminal_statuses,
                &mut published_attention,
                &updates,
            );
        }

        let timeout = refreshes.poll_timeout(Instant::now(), POLL_INTERVAL);
        match stream.next_timeout(timeout) {
            Ok(StreamPoll::Item(item)) => {
                // Liveness first: an agent record and its terminal's lifecycle
                // can change in one delta, and the agent must be judged against
                // the newer lifecycle rather than the previous one.
                let liveness_changed =
                    apply_terminal_liveness_event(&item.value, &mut terminal_statuses);
                if liveness_changed {
                    publish_terminal_statuses(
                        &terminal_statuses,
                        &mut published_terminal_statuses,
                        &updates,
                    );
                }
                if apply_attention_event(&item.value, &mut notifications, &mut agents)
                    || liveness_changed
                {
                    publish_attention(
                        &notifications,
                        &agents,
                        &terminal_statuses,
                        &mut published_attention,
                        &updates,
                    );
                }
                if apply_terminal_cwd_event(&item.value, &mut terminal_cwds) {
                    let _ = updates.send_blocking(Update::TerminalCwds(terminal_cwds.clone()));
                }
                if session_event_affects_topology(&item.value) && refreshes.request(Instant::now())
                {
                    if session_event_stop_requested(&stop) {
                        return;
                    }
                    publish_event_topology(&client, &updates, &mut refreshes);
                }
            }
            Ok(StreamPoll::TimedOut) => {}
            Ok(StreamPoll::End) => {
                let _ = runtime_events.send(SupervisorCommand::RuntimeDisconnected {
                    generation,
                    message: "session event stream ended".to_string(),
                });
                return;
            }
            Err(error) => {
                let _ = runtime_events.send(SupervisorCommand::RuntimeDisconnected {
                    generation,
                    message: format!("session event stream error: {error}"),
                });
                return;
            }
        }
    }
}

fn apply_terminal_cwd_event(
    event: &SessionEvent,
    terminal_cwds: &mut HashMap<TerminalId, String>,
) -> bool {
    match event {
        SessionEvent::Snapshot(event) => {
            let next = event
                .snapshot
                .terminals
                .iter()
                .filter_map(|terminal| {
                    terminal
                        .cwd
                        .as_ref()
                        .filter(|cwd| !cwd.is_empty())
                        .map(|cwd| (terminal.id.clone(), cwd.clone()))
                })
                .collect::<HashMap<_, _>>();
            if *terminal_cwds == next {
                false
            } else {
                *terminal_cwds = next;
                true
            }
        }
        SessionEvent::Delta(delta) => {
            let mut changed = false;
            for change in &delta.changes {
                changed |= apply_terminal_cwd_change(change, terminal_cwds);
            }
            changed
        }
        SessionEvent::Unknown { .. } => false,
    }
}

/// Tracks terminal lifecycle from the same resource stream the cwd map uses.
/// The running subset is derived from this map for attention calculations.
fn apply_terminal_liveness_event(
    event: &SessionEvent,
    statuses: &mut HashMap<TerminalId, TerminalStatus>,
) -> bool {
    match event {
        SessionEvent::Snapshot(event) => {
            let next = event
                .snapshot
                .terminals
                .iter()
                .map(|terminal| (terminal.id.clone(), terminal_status(terminal)))
                .collect::<HashMap<_, _>>();
            if *statuses == next {
                false
            } else {
                *statuses = next;
                true
            }
        }
        SessionEvent::Delta(delta) => {
            let mut changed = false;
            for change in &delta.changes {
                changed |= match change {
                    ResourceChange::Upsert {
                        resource: ResourceKind::Terminal,
                        value: ResourceEntitySnapshot::Terminal(terminal),
                        ..
                    } => {
                        let status = terminal_status(terminal);
                        statuses
                            .insert(terminal.id.clone(), status.clone())
                            .as_ref()
                            != Some(&status)
                    }
                    ResourceChange::Delete {
                        resource: ResourceKind::Terminal,
                        id: ResourceReference::Terminal(terminal),
                        ..
                    } => statuses.remove(terminal).is_some(),
                    _ => false,
                };
            }
            changed
        }
        SessionEvent::Unknown { .. } => false,
    }
}

fn publish_terminal_statuses(
    statuses: &HashMap<TerminalId, TerminalStatus>,
    published: &mut Option<HashMap<TerminalId, TerminalStatus>>,
    updates: &UpdateSink,
) {
    if published.as_ref() == Some(statuses) {
        return;
    }
    *published = Some(statuses.clone());
    let _ = updates.send_blocking(Update::TerminalStatuses(statuses.clone()));
}

fn apply_terminal_cwd_change(
    change: &ResourceChange,
    terminal_cwds: &mut HashMap<TerminalId, String>,
) -> bool {
    match change {
        ResourceChange::Upsert {
            resource: ResourceKind::Terminal,
            value: ResourceEntitySnapshot::Terminal(terminal),
            ..
        } => match terminal.cwd.as_ref().filter(|cwd| !cwd.is_empty()) {
            Some(cwd) => {
                terminal_cwds
                    .insert(terminal.id.clone(), cwd.clone())
                    .as_ref()
                    != Some(cwd)
            }
            None => terminal_cwds.remove(&terminal.id).is_some(),
        },
        ResourceChange::Delete {
            resource: ResourceKind::Terminal,
            id: ResourceReference::Terminal(terminal),
            ..
        } => terminal_cwds.remove(terminal).is_some(),
        _ => false,
    }
}

fn apply_attention_event(
    event: &SessionEvent,
    notifications: &mut HashMap<NotificationId, NotificationSnapshot>,
    agents: &mut HashMap<AgentId, AgentSnapshot>,
) -> bool {
    match event {
        SessionEvent::Snapshot(event) => {
            notifications.clear();
            notifications.extend(
                event
                    .snapshot
                    .notifications
                    .iter()
                    .cloned()
                    .map(|notification| (notification.id.clone(), notification)),
            );
            agents.clear();
            agents.extend(
                event
                    .snapshot
                    .agents
                    .iter()
                    .cloned()
                    .map(|agent| (agent.id.clone(), agent)),
            );
            true
        }
        SessionEvent::Delta(delta) => {
            let mut affected = false;
            for change in &delta.changes {
                affected |= apply_attention_change(change, notifications, agents);
            }
            affected
        }
        SessionEvent::Unknown { .. } => false,
    }
}

fn apply_attention_change(
    change: &ResourceChange,
    notifications: &mut HashMap<NotificationId, NotificationSnapshot>,
    agents: &mut HashMap<AgentId, AgentSnapshot>,
) -> bool {
    if !resource_change_affects_attention(change) {
        return false;
    }
    match change {
        ResourceChange::Upsert {
            resource: ResourceKind::Notification,
            value: ResourceEntitySnapshot::Notification(notification),
            ..
        } => {
            notifications.insert(notification.id.clone(), notification.clone());
            true
        }
        ResourceChange::Upsert {
            resource: ResourceKind::Agent,
            value: ResourceEntitySnapshot::Agent(agent),
            ..
        } => {
            agents.insert(agent.id.clone(), agent.clone());
            true
        }
        ResourceChange::Delete {
            resource: ResourceKind::Notification,
            id: ResourceReference::Notification(notification),
            ..
        } => notifications.remove(notification).is_some(),
        ResourceChange::Delete {
            resource: ResourceKind::Agent,
            id: ResourceReference::Agent(agent),
            ..
        } => agents.remove(agent).is_some(),
        _ => false,
    }
}

fn clear_terminal_notifications(
    notifications: &mut HashMap<NotificationId, NotificationSnapshot>,
    terminal: &TerminalId,
) -> bool {
    let mut changed = false;
    for notification in notifications.values_mut() {
        if notification.unread && notification.terminal_id.as_ref() == Some(terminal) {
            notification.unread = false;
            changed = true;
        }
    }
    changed
}

fn publish_attention(
    notifications: &HashMap<NotificationId, NotificationSnapshot>,
    agents: &HashMap<AgentId, AgentSnapshot>,
    statuses: &HashMap<TerminalId, TerminalStatus>,
    published: &mut Option<AttentionState>,
    updates: &UpdateSink,
) {
    let next = AttentionState::from_resources_with_liveness(
        notifications.values(),
        agents.values(),
        |terminal| matches!(statuses.get(terminal), Some(TerminalStatus::Running)),
    );
    if published.as_ref() == Some(&next) {
        return;
    }
    *published = Some(next.clone());
    let _ = updates.send_blocking(Update::Attention(next));
}

fn session_event_stop_requested(stop: &mpsc::Receiver<()>) -> bool {
    match stop.try_recv() {
        Ok(()) | Err(mpsc::TryRecvError::Disconnected) => true,
        Err(mpsc::TryRecvError::Empty) => false,
    }
}

fn publish_event_topology(
    client: &Client,
    updates: &UpdateSink,
    refreshes: &mut TopologyRefreshSchedule,
) {
    match publish_workspaces(client, updates) {
        Ok(()) => refreshes.record_publish_success(),
        Err(error) => {
            if refreshes.record_publish_failure() {
                let _ = updates
                    .send_blocking(Update::Error(format!("topology refresh failed: {error}")));
            }
        }
    }
}

fn connected_session_name(client: &Client) -> Option<String> {
    client
        .current_session()
        .refresh()
        .ok()
        .and_then(|snapshot| snapshot.name)
        .filter(|name| !name.is_empty())
}

fn session_socket_path(current_socket: &Path, session_name: &str) -> PathBuf {
    current_socket.with_file_name(format!("{session_name}.sock"))
}

fn socket_candidates(directory: &Path, active_socket: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = fs::read_dir(directory).map_err(|error| {
        format!(
            "Could not read cmux session directory '{}': {error}",
            directory.display()
        )
    })?;
    let mut candidates = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        #[cfg(unix)]
        let is_socket = entry
            .file_type()
            .is_ok_and(|file_type| file_type.is_socket());
        #[cfg(not(unix))]
        let is_socket = false;
        candidates.push((entry.path(), is_socket));
    }
    Ok(parse_socket_candidates(
        candidates,
        directory,
        active_socket,
    ))
}

fn parse_socket_candidates(
    candidates: impl IntoIterator<Item = (PathBuf, bool)>,
    directory: &Path,
    active_socket: &Path,
) -> Vec<PathBuf> {
    let mut paths = candidates
        .into_iter()
        .filter(|(path, is_socket)| {
            *is_socket && path.extension().and_then(|extension| extension.to_str()) == Some("sock")
        })
        .map(|(path, _)| path)
        .collect::<Vec<_>>();
    if active_socket.parent() == Some(directory) && !paths.iter().any(|path| path == active_socket)
    {
        paths.push(active_socket.to_path_buf());
    }
    paths.sort();
    paths.dedup();
    paths
}

fn enumerate_sessions(active: &SessionEntry) -> Result<Vec<SessionEntry>, String> {
    let directory = active.socket_path.parent().ok_or_else(|| {
        format!(
            "Session socket '{}' has no parent directory",
            active.socket_path.display()
        )
    })?;
    let mut sessions = Vec::new();
    for socket_path in socket_candidates(directory, &active.socket_path)? {
        let config = Config::from_socket_path(&socket_path).with_timeout(SESSION_PROBE_TIMEOUT);
        let Ok(client) = Client::connect(config) else {
            continue;
        };
        let fallback = socket_path
            .file_stem()
            .and_then(|name| name.to_str())
            .unwrap_or("unnamed")
            .to_string();
        sessions.push(SessionEntry {
            name: connected_session_name(&client).unwrap_or(fallback),
            socket_path,
        });
    }
    sessions.sort_by(|left, right| {
        left.name
            .cmp(&right.name)
            .then_with(|| left.socket_path.cmp(&right.socket_path))
    });
    Ok(sessions)
}

fn connect(
    session_name: &str,
    config: Config,
    auto_start: bool,
    updates: &UpdateSink,
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
    join: JoinHandle<()>,
}

#[derive(Debug)]
enum AttachmentStreamOutcome {
    Ended,
    Overflow,
    Errored(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AttachmentOutcomeAction {
    Detach { remember: bool },
    Reattach,
    Rebuild,
}

fn attachment_outcome_action(
    outcome: &AttachmentStreamOutcome,
    terminal_is_desired: bool,
) -> AttachmentOutcomeAction {
    match outcome {
        AttachmentStreamOutcome::Ended => AttachmentOutcomeAction::Detach {
            remember: terminal_is_desired,
        },
        AttachmentStreamOutcome::Overflow if terminal_is_desired => {
            AttachmentOutcomeAction::Reattach
        }
        AttachmentStreamOutcome::Overflow => AttachmentOutcomeAction::Detach { remember: false },
        AttachmentStreamOutcome::Errored(_) => AttachmentOutcomeAction::Rebuild,
    }
}

struct AttachmentCompletion {
    terminal: TerminalId,
    outcome: AttachmentStreamOutcome,
}

#[derive(Clone, Copy, Debug)]
struct AttachmentOverflowRetry {
    attempts: u8,
    retry_at: Option<Instant>,
}

fn next_attachment_overflow_retry(attempts: u8, now: Instant) -> Option<AttachmentOverflowRetry> {
    (attempts < MAX_ATTACHMENT_OVERFLOW_REATTACHES).then(|| AttachmentOverflowRetry {
        attempts: attempts + 1,
        retry_at: Some(now + RECONNECT_INTERVAL),
    })
}

enum AttachmentCommand {
    Resize(Size),
    Stop,
}

fn attachment_manager_loop(
    client: Client,
    updates: UpdateSink,
    controls: mpsc::Receiver<Control>,
    generation: u64,
    runtime_events: mpsc::Sender<SupervisorCommand>,
) {
    let session = client.session(Selector::current());
    let mut workers: HashMap<TerminalId, AttachmentWorker> = HashMap::new();
    let mut desired: HashMap<TerminalId, Size> = HashMap::new();
    let mut satisfied = HashSet::<TerminalId>::new();
    let mut overflow_retries = HashMap::<TerminalId, AttachmentOverflowRetry>::new();
    let (completion_tx, completion_rx) = mpsc::channel::<AttachmentCompletion>();

    loop {
        while let Ok(completion) = completion_rx.try_recv() {
            if let Some(worker) = workers.remove(&completion.terminal) {
                let _ = worker.join.join();
            }
            let terminal_is_desired = desired.contains_key(&completion.terminal);
            match attachment_outcome_action(&completion.outcome, terminal_is_desired) {
                AttachmentOutcomeAction::Detach { remember } => {
                    let _ = updates.send_blocking(Update::Detached {
                        terminal: completion.terminal.clone(),
                    });
                    overflow_retries.remove(&completion.terminal);
                    if remember {
                        satisfied.insert(completion.terminal);
                    }
                }
                AttachmentOutcomeAction::Reattach => {
                    let _ = updates.send_blocking(Update::Detached {
                        terminal: completion.terminal.clone(),
                    });
                    let attempts = overflow_retries
                        .get(&completion.terminal)
                        .map_or(0, |retry| retry.attempts);
                    if let Some(retry) = next_attachment_overflow_retry(attempts, Instant::now()) {
                        overflow_retries.insert(completion.terminal, retry);
                    } else {
                        let message = format!(
                            "attachment overflow for {:?}; stopped after {} reattach attempts",
                            completion.terminal, MAX_ATTACHMENT_OVERFLOW_REATTACHES
                        );
                        let _ = updates.send_blocking(Update::Error(message));
                        overflow_retries.remove(&completion.terminal);
                        satisfied.insert(completion.terminal);
                    }
                }
                AttachmentOutcomeAction::Rebuild => {
                    let AttachmentStreamOutcome::Errored(message) = completion.outcome else {
                        unreachable!("only stream errors rebuild the runtime");
                    };
                    let _ = updates.send_blocking(Update::Error(message.clone()));
                    let _ = runtime_events.send(SupervisorCommand::RuntimeDisconnected {
                        generation,
                        message,
                    });
                }
            }
        }

        let now = Instant::now();
        let ready: Vec<TerminalId> = overflow_retries
            .iter()
            .filter(|(terminal, retry)| {
                desired.contains_key(*terminal)
                    && retry.retry_at.is_some_and(|retry_at| now >= retry_at)
            })
            .map(|(terminal, _)| terminal.clone())
            .collect();
        for terminal in ready {
            let Some(size) = desired.get(&terminal).copied() else {
                continue;
            };
            let Some(retry) = overflow_retries.get_mut(&terminal) else {
                continue;
            };
            retry.retry_at = None;
            let worker =
                spawn_attachment_worker(&session, terminal.clone(), size, &updates, &completion_tx);
            workers.insert(terminal, worker);
        }

        match controls.recv_timeout(POLL_INTERVAL) {
            Ok(Control::Stop) => {
                break;
            }
            Ok(Control::SyncAttachments(specs)) => {
                desired = specs
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
                        let _ = worker.join.join();
                    }
                }
                satisfied.retain(|terminal| desired.contains_key(terminal));
                overflow_retries.retain(|terminal, _| desired.contains_key(terminal));

                for (terminal, size) in &desired {
                    if let Some(worker) = workers.get(terminal) {
                        let _ = worker.commands.send(AttachmentCommand::Resize(*size));
                        continue;
                    }
                    if satisfied.contains(terminal) || overflow_retries.contains_key(terminal) {
                        continue;
                    }
                    let worker = spawn_attachment_worker(
                        &session,
                        terminal.clone(),
                        *size,
                        &updates,
                        &completion_tx,
                    );
                    workers.insert(terminal.clone(), worker);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    for worker in workers.into_values() {
        let _ = worker.commands.send(AttachmentCommand::Stop);
        let _ = worker.join.join();
    }
}

fn spawn_attachment_worker(
    session: &cmux::Session,
    terminal: TerminalId,
    size: Size,
    updates: &UpdateSink,
    completions: &mpsc::Sender<AttachmentCompletion>,
) -> AttachmentWorker {
    let (command_tx, command_rx) = mpsc::channel();
    let attachment_session = session.clone();
    let attachment_updates = updates.clone();
    let attachment_terminal = terminal.clone();
    let attachment_completions = completions.clone();
    let join = thread::spawn(move || {
        if let Some(outcome) = attachment_loop(
            attachment_session,
            attachment_terminal.clone(),
            size,
            attachment_updates,
            command_rx,
        ) {
            let _ = attachment_completions.send(AttachmentCompletion {
                terminal: attachment_terminal,
                outcome,
            });
        }
    });
    AttachmentWorker {
        commands: command_tx,
        join,
    }
}

fn attachment_loop(
    session: cmux::Session,
    terminal: TerminalId,
    initial_size: Size,
    updates: UpdateSink,
    commands: mpsc::Receiver<AttachmentCommand>,
) -> Option<AttachmentStreamOutcome> {
    let mut size = initial_size;
    match drain_attachment_commands(&commands, &mut size, None, &terminal, &updates) {
        AttachmentAction::Continue => {}
        AttachmentAction::Stop => return None,
    }

    let mut stream = match open_attachment(&session, &terminal, size) {
        Ok(stream) => stream,
        Err(error) => {
            let message = format!("{terminal:?}: {error}");
            return Some(AttachmentStreamOutcome::Errored(message));
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
            AttachmentAction::Stop => return None,
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
                        offset: scroll.offset,
                        at_bottom: scroll.at_bottom,
                    });
                }
                TerminalAttachmentItem::Unknown { kind, .. } if kind == "overflow" => {
                    return Some(AttachmentStreamOutcome::Overflow);
                }
                TerminalAttachmentItem::Unknown { .. } => {}
            },
            Ok(StreamPoll::TimedOut) => {}
            Ok(StreamPoll::End) => return Some(AttachmentStreamOutcome::Ended),
            Err(error) => {
                if matches!(
                    &error,
                    cmux::Error::StreamEnded { reason, .. } if reason == "gap"
                ) {
                    return Some(AttachmentStreamOutcome::Overflow);
                }
                let message = format!("stream error for {terminal:?}: {error}");
                return Some(AttachmentStreamOutcome::Errored(message));
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
    updates: &UpdateSink,
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
    updates: UpdateSink,
    inputs: mpsc::Receiver<Input>,
    attention_clears: mpsc::Sender<TerminalId>,
) {
    let session = client.session(Selector::current());
    let mut target: Option<TerminalId> = None;
    let mut history_cache: HashMap<TerminalId, Vec<String>> = HashMap::new();

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
            Input::Search(request) => {
                match search_terminal(&session, &mut history_cache, request) {
                    Ok(results) => {
                        let _ = updates.send_blocking(Update::SearchResults(results));
                    }
                    Err(error) => {
                        let _ = updates.send_blocking(Update::Error(format!(
                            "scrollback search failed: {error}"
                        )));
                    }
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
                if let Err(error) = session.workspace(Selector::id(workspace)).focus() {
                    let _ = updates
                        .send_blocking(Update::Error(format!("workspace focus failed: {error}")));
                } else if let Some(terminal) = target.clone() {
                    let _ = attention_clears.send(terminal);
                }
            }
            Input::CreateWorkspace => {
                if let Err(error) = session.create_workspace(None) {
                    let _ = updates.send_blocking(Update::Error(format!(
                        "workspace creation failed: {error}"
                    )));
                }
            }
            Input::RenameWorkspace { workspace, name } => {
                if let Err(error) = session.workspace(Selector::id(workspace)).rename(name) {
                    let _ = updates
                        .send_blocking(Update::Error(format!("workspace rename failed: {error}")));
                }
            }
            Input::CloseWorkspace { workspace } => {
                if let Err(error) = session.workspace(Selector::id(workspace)).close() {
                    let _ = updates
                        .send_blocking(Update::Error(format!("workspace close failed: {error}")));
                }
            }
            Input::MoveWorkspace { workspace, index } => {
                if let Err(error) = session.workspace(Selector::id(workspace)).move_to(index) {
                    let _ = updates
                        .send_blocking(Update::Error(format!("workspace move failed: {error}")));
                }
            }
            Input::CreateScreen { workspace } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .create_screen(CreateScreenOptions::default())
                {
                    let _ = updates
                        .send_blocking(Update::Error(format!("screen creation failed: {error}")));
                }
            }
            Input::FocusScreen {
                workspace,
                screen,
                target: next,
            } => {
                target = next;
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .focus()
                {
                    let _ = updates
                        .send_blocking(Update::Error(format!("screen focus failed: {error}")));
                } else if let Some(terminal) = target.clone() {
                    let _ = attention_clears.send(terminal);
                }
            }
            Input::RenameScreen {
                workspace,
                screen,
                name,
            } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .rename(name)
                {
                    let _ = updates
                        .send_blocking(Update::Error(format!("screen rename failed: {error}")));
                }
            }
            Input::CloseScreen { workspace, screen } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .close()
                {
                    let _ = updates
                        .send_blocking(Update::Error(format!("screen close failed: {error}")));
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
                if let Err(error) = handle.focus() {
                    let _ =
                        updates.send_blocking(Update::Error(format!("pane focus failed: {error}")));
                } else if let Some(terminal) = target.clone() {
                    let _ = attention_clears.send(terminal);
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
                if let Err(error) = handle.focus() {
                    let _ =
                        updates.send_blocking(Update::Error(format!("tab focus failed: {error}")));
                } else if let Some(terminal) = target.clone() {
                    let _ = attention_clears.send(terminal);
                }
            }
            Input::CreateTab {
                workspace,
                screen,
                pane,
            } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .create_terminal(TerminalCreateOptions::default())
                {
                    let _ = updates
                        .send_blocking(Update::Error(format!("tab creation failed: {error}")));
                }
            }
            Input::CloseTab {
                workspace,
                screen,
                pane,
                tab,
            } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .tab(Selector::id(tab))
                    .close()
                {
                    let _ =
                        updates.send_blocking(Update::Error(format!("tab close failed: {error}")));
                }
            }
            Input::RenameTab {
                workspace,
                screen,
                pane,
                tab,
                name,
            } => {
                if let Err(error) = session
                    .workspace(Selector::id(workspace))
                    .screen(Selector::id(screen))
                    .pane(Selector::id(pane))
                    .tab(Selector::id(tab))
                    .rename(name)
                {
                    let _ =
                        updates.send_blocking(Update::Error(format!("tab rename failed: {error}")));
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
                if let Err(error) = handle.split(SplitOptions::new(direction)) {
                    let _ =
                        updates.send_blocking(Update::Error(format!("pane split failed: {error}")));
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
                if let Err(error) = handle.close() {
                    let _ =
                        updates.send_blocking(Update::Error(format!("pane close failed: {error}")));
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
                if let Err(error) = handle.set_split_ratio(SplitRatioOptions {
                    split_id: split,
                    ratio,
                }) {
                    let _ = updates
                        .send_blocking(Update::Error(format!("pane resize failed: {error}")));
                }
            }
            Input::SetTarget(next) => target = next,
        }
    }
}

fn search_terminal(
    session: &cmux::Session,
    history_cache: &mut HashMap<TerminalId, Vec<String>>,
    request: SearchRequest,
) -> Result<SearchResults, String> {
    let cache_is_stale = history_cache
        .get(&request.terminal)
        .is_none_or(|rows| rows.len() != request.expected_history_rows as usize);
    if request.refresh || cache_is_stale {
        let handle = session.terminal(Selector::id(request.terminal.clone()));
        let mut before = None;
        let mut pages = Vec::new();
        loop {
            let page = handle
                .read_history(ReadHistoryOptions {
                    before,
                    limit: Some(10_000),
                    styled: Some(false),
                })
                .map_err(|error| error.to_string())?;
            let next = page.next;
            pages.push((
                page.start,
                page.rows.iter().map(search::row_text).collect::<Vec<_>>(),
            ));
            let Some(next) = next else { break };
            if before == Some(next) {
                return Err("history pagination cursor did not advance".to_string());
            }
            before = Some(next);
        }
        pages.sort_by_key(|(start, _)| *start);
        let mut history = Vec::new();
        for (start, rows) in pages {
            if start != history.len() as u64 {
                return Err("retained history changed while it was being paged".to_string());
            }
            history.extend(rows);
        }
        history_cache.insert(request.terminal.clone(), history);
    }

    let history = history_cache
        .get(&request.terminal)
        .expect("history cache is populated above");
    let document = search::complete_document(history, &request.viewport, request.viewport_offset);
    Ok(SearchResults {
        generation: request.generation,
        terminal: request.terminal,
        query: request.query.clone(),
        history_rows: history.len() as u64,
        matches: search::find_matches(&document, &request.query),
    })
}

/// Publishes workspace catalog data and the focused screen's complete layout.
/// The tab snapshot already contains a typed content ID, so no terminal-to-tab
/// reconstruction or private protocol identity is needed.
fn publish_workspaces(client: &Client, updates: &UpdateSink) -> Result<(), String> {
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
        let mut screens = Vec::new();

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
            let mut screen_terminals = Vec::new();
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
                            if !screen_terminals.contains(&terminal) {
                                screen_terminals.push(terminal.clone());
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
            let view = WorkspaceView {
                workspace_id: snapshot.id.clone(),
                screen_id: screen_snapshot.id.clone(),
                screen_tabs: Vec::new(),
                layout: screen_snapshot.layout,
                panes,
            };
            let active_terminal = view.active_terminal().cloned();
            if let Some(index) = active_terminal.as_ref().and_then(|active| {
                screen_terminals
                    .iter()
                    .position(|terminal| terminal == active)
            }) {
                screen_terminals.swap(0, index);
            }
            screens.push(ScreenEntry {
                id: screen_snapshot.id,
                terminals: screen_terminals,
                active_terminal,
            });
            views.push((screen_snapshot.focused, view));
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
            screens,
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
    use super::{
        attachment_outcome_action, exit_summary, next_attachment_overflow_retry,
        parse_socket_candidates, resource_change_affects_attention,
        resource_change_affects_topology, resource_kind_affects_attention,
        resource_kind_affects_topology, session_event_affects_topology, session_socket_path,
        AttachmentOutcomeAction, AttachmentStreamOutcome, RebuildPlan, RetrySchedule, RoutedSender,
        SessionEntry, TopologyRefreshSchedule, MAX_ATTACHMENT_OVERFLOW_REATTACHES,
        RECONNECT_INTERVAL,
    };
    use cmux::{
        AgentId, Cursor, Document, MachineId, NotificationId, ResourceChange, ResourceKind,
        ResourceReference, SessionDeltaEvent, SessionEvent, TerminalExit, TerminalExitOutcome,
        WorkspaceId,
    };
    use std::path::{Path, PathBuf};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    fn terminal_exit(outcome: TerminalExitOutcome) -> TerminalExit {
        TerminalExit {
            outcome,
            exited_at: 1,
            revision: 2,
        }
    }

    #[test]
    fn terminal_exit_summary_describes_status_signal_and_unknown_outcomes() {
        let status = terminal_exit(TerminalExitOutcome::Exit { code: 17 });
        let signal = terminal_exit(TerminalExitOutcome::Signal {
            signal: 9,
            core_dumped: false,
        });
        let core_dumped = terminal_exit(TerminalExitOutcome::Signal {
            signal: 11,
            core_dumped: true,
        });
        let unknown = terminal_exit(TerminalExitOutcome::Unknown {
            reason: "host-process-ended-before-adoption".to_string(),
        });

        assert_eq!(exit_summary(Some(&status)).as_deref(), Some("status 17"));
        assert_eq!(exit_summary(Some(&signal)).as_deref(), Some("signal 9"));
        assert_eq!(
            exit_summary(Some(&core_dumped)).as_deref(),
            Some("signal 11 (core dumped)")
        );
        assert_eq!(
            exit_summary(Some(&unknown)).as_deref(),
            Some("host-process-ended-before-adoption")
        );
        assert_eq!(exit_summary(None), None);
    }

    #[test]
    fn normal_attachment_end_while_desired_detaches_and_is_remembered() {
        assert_eq!(
            attachment_outcome_action(&AttachmentStreamOutcome::Ended, true),
            AttachmentOutcomeAction::Detach { remember: true }
        );
    }

    #[test]
    fn normal_attachment_end_after_removal_detaches_without_memory() {
        assert_eq!(
            attachment_outcome_action(&AttachmentStreamOutcome::Ended, false),
            AttachmentOutcomeAction::Detach { remember: false }
        );
    }

    #[test]
    fn attachment_stream_error_rebuilds_runtime() {
        assert_eq!(
            attachment_outcome_action(
                &AttachmentStreamOutcome::Errored("transport closed".to_string()),
                true
            ),
            AttachmentOutcomeAction::Rebuild
        );
    }

    #[test]
    fn attachment_overflow_while_desired_reattaches_only_attachment() {
        assert_eq!(
            attachment_outcome_action(&AttachmentStreamOutcome::Overflow, true),
            AttachmentOutcomeAction::Reattach
        );
    }

    #[test]
    fn attachment_overflow_reattach_schedule_is_delayed_and_bounded() {
        let start = Instant::now();
        let mut attempts = 0;
        for _ in 0..MAX_ATTACHMENT_OVERFLOW_REATTACHES {
            let retry = next_attachment_overflow_retry(attempts, start).unwrap();
            attempts = retry.attempts;
            assert_eq!(retry.retry_at, Some(start + RECONNECT_INTERVAL));
        }
        assert!(next_attachment_overflow_retry(attempts, start).is_none());
    }

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

    #[test]
    fn topology_refresh_schedule_coalesces_a_burst_and_publishes_the_trailing_state() {
        let start = Instant::now();
        let interval = Duration::from_millis(125);
        let mut schedule = TopologyRefreshSchedule::new(interval);

        assert!(schedule.request(start));
        assert!(!schedule.request(start + Duration::from_millis(20)));
        assert!(!schedule.request(start + Duration::from_millis(100)));
        assert_eq!(
            schedule.poll_timeout(
                start + Duration::from_millis(100),
                Duration::from_millis(50)
            ),
            Duration::from_millis(25)
        );
        assert!(!schedule.take_due(start + Duration::from_millis(124)));
        assert!(schedule.take_due(start + interval));
        assert!(!schedule.take_due(start + Duration::from_secs(1)));
    }

    #[test]
    fn topology_refresh_schedule_keeps_spacing_subsequent_bursts() {
        let start = Instant::now();
        let interval = Duration::from_millis(125);
        let mut schedule = TopologyRefreshSchedule::new(interval);

        assert!(schedule.request(start));
        assert!(!schedule.request(start + Duration::from_millis(10)));
        assert!(schedule.take_due(start + interval));
        assert!(!schedule.request(start + interval + Duration::from_millis(10)));
        assert!(schedule.take_due(start + interval + interval));
    }

    #[test]
    fn topology_publish_failures_rearm_retries_and_report_only_the_third_failure() {
        let start = Instant::now();
        let interval = Duration::from_millis(125);
        let mut schedule = TopologyRefreshSchedule::new(interval);

        assert!(schedule.request(start));
        assert!(!schedule.record_publish_failure());
        assert!(!schedule.take_due(start + Duration::from_millis(124)));
        assert!(schedule.take_due(start + interval));
        assert!(!schedule.record_publish_failure());
        assert!(schedule.take_due(start + interval * 2));
        assert!(schedule.record_publish_failure());
        assert!(schedule.take_due(start + interval * 3));
        assert!(!schedule.record_publish_failure());
    }

    #[test]
    fn successful_topology_publish_resets_the_failure_counter() {
        let mut schedule = TopologyRefreshSchedule::new(Duration::from_millis(125));

        assert!(!schedule.record_publish_failure());
        assert!(!schedule.record_publish_failure());
        schedule.record_publish_success();
        assert!(!schedule.record_publish_failure());
        assert!(!schedule.record_publish_failure());
        assert!(schedule.record_publish_failure());
    }

    #[test]
    fn resource_kind_classification_matches_rendered_topology() {
        for resource in [
            ResourceKind::Session,
            ResourceKind::Workspace,
            ResourceKind::Screen,
            ResourceKind::Pane,
            ResourceKind::Tab,
            ResourceKind::Terminal,
        ] {
            assert!(resource_kind_affects_topology(resource));
        }
        for resource in [
            ResourceKind::Machine,
            ResourceKind::Browser,
            ResourceKind::Client,
            ResourceKind::Notification,
            ResourceKind::Agent,
            ResourceKind::PairingRequest,
            ResourceKind::FrontendProjection,
            ResourceKind::SidebarView,
        ] {
            assert!(!resource_kind_affects_topology(resource));
        }
    }

    #[test]
    fn attention_classification_is_separate_from_topology() {
        let notification = ResourceChange::Delete {
            sequence: 1,
            resource: ResourceKind::Notification,
            id: ResourceReference::Notification(
                NotificationId::parse("notification_00000000000000000000000000000001").unwrap(),
            ),
        };
        let agent = ResourceChange::Delete {
            sequence: 2,
            resource: ResourceKind::Agent,
            id: ResourceReference::Agent(
                AgentId::parse("agent_00000000000000000000000000000001").unwrap(),
            ),
        };

        assert!(resource_kind_affects_attention(ResourceKind::Notification));
        assert!(resource_kind_affects_attention(ResourceKind::Agent));
        assert!(!resource_kind_affects_attention(ResourceKind::Tab));
        assert!(resource_change_affects_attention(&notification));
        assert!(resource_change_affects_attention(&agent));
        assert!(!resource_change_affects_topology(&notification));
        assert!(!resource_change_affects_topology(&agent));
    }

    #[test]
    fn unknown_resource_changes_and_session_events_are_topology_affecting() {
        let raw = Document::from_serializable(&serde_json::json!({"future": true})).unwrap();
        let change = ResourceChange::Unknown {
            kind: "future_change".to_string(),
            raw: raw.clone(),
        };
        assert!(resource_change_affects_topology(&change));

        let delta = SessionEvent::Delta(SessionDeltaEvent {
            cursor: Cursor {
                generation: "generation".to_string(),
                revision: 2,
            },
            previous_revision: 1,
            revision: 2,
            changes: vec![change],
        });
        assert!(session_event_affects_topology(&delta));
        assert!(session_event_affects_topology(&SessionEvent::Unknown {
            kind: "future_event".to_string(),
            raw,
        }));
    }

    #[test]
    fn deltas_refresh_only_when_a_change_affects_topology() {
        let event = |changes| {
            SessionEvent::Delta(SessionDeltaEvent {
                cursor: Cursor {
                    generation: "generation".to_string(),
                    revision: 2,
                },
                previous_revision: 1,
                revision: 2,
                changes,
            })
        };
        let machine = ResourceChange::Delete {
            sequence: 1,
            resource: ResourceKind::Machine,
            id: ResourceReference::Machine(
                MachineId::parse("machine_00000000000000000000000000000000").unwrap(),
            ),
        };
        let workspace = ResourceChange::Delete {
            sequence: 2,
            resource: ResourceKind::Workspace,
            id: ResourceReference::Workspace(
                WorkspaceId::parse("ws_00000000000000000000000000000000").unwrap(),
            ),
        };

        assert!(!session_event_affects_topology(&event(Vec::new())));
        assert!(!session_event_affects_topology(&event(vec![
            machine.clone()
        ])));
        assert!(session_event_affects_topology(&event(vec![
            machine, workspace
        ])));
    }

    #[test]
    fn rebuild_plan_commits_target_and_rolls_back_previous() {
        let previous = SessionEntry {
            name: "main".into(),
            socket_path: "/run/cmux/main.sock".into(),
        };
        let target = SessionEntry {
            name: "logs".into(),
            socket_path: "/run/cmux/logs.sock".into(),
        };
        let plan = RebuildPlan::new(&previous, target.clone()).unwrap();

        assert_eq!(plan.clone().commit(target.clone()), target);
        assert_eq!(plan.rollback(), previous);
        assert!(RebuildPlan::new(&previous, previous.clone()).is_none());
    }

    #[test]
    fn routed_sender_drops_commands_while_unbound_and_rebinds_cleanly() {
        let route = RoutedSender::new();
        assert_eq!(route.send(1), Err(mpsc::SendError(1)));

        let (first_tx, first_rx) = mpsc::channel();
        route.bind(first_tx);
        route.send(2).unwrap();
        assert_eq!(first_rx.recv().unwrap(), 2);

        route.clear();
        assert_eq!(route.send(3), Err(mpsc::SendError(3)));
        let (second_tx, second_rx) = mpsc::channel();
        route.bind(second_tx);
        route.send(4).unwrap();
        assert_eq!(second_rx.recv().unwrap(), 4);
    }

    #[test]
    fn socket_candidate_parser_keeps_only_sockets_and_includes_custom_active_path() {
        let directory = Path::new("/run/user/1000/cmux-tui-1000");
        let alpha = directory.join("alpha.sock");
        let active = directory.join("current.custom");
        let candidates = vec![
            (alpha.clone(), true),
            (directory.join("ignored.txt"), true),
            (directory.join("regular.sock"), false),
        ];
        let mut expected = vec![alpha, active.clone()];
        expected.sort();
        assert_eq!(
            parse_socket_candidates(candidates, directory, &active),
            expected
        );
    }

    #[test]
    fn new_session_socket_is_a_sibling_of_the_active_socket() {
        assert_eq!(
            session_socket_path(Path::new("/run/user/1000/cmux-tui-1000/main.sock"), "logs"),
            PathBuf::from("/run/user/1000/cmux-tui-1000/logs.sock")
        );
    }
}
