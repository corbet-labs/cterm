//! Session state management

use crate::bridge::{PtyReader, PtyWriter};
use crate::error::Result;
use cterm_app::{TtyTransferApprovalRequest, TtyTransferDirection};
use cterm_core::screen::ScreenConfig;
use cterm_core::term::TerminalEvent;
#[cfg(unix)]
use cterm_core::Pty;
use cterm_core::{FileTransferAction, FileTransferCommand, PtyConfig, PtySize, Terminal};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::broadcast;

use super::tty_transfer::TtyTransferController;

/// Output chunk with timestamp
#[derive(Clone, Debug)]
pub struct OutputData {
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

/// Reply to an interactive SSH prompt (host key / password / passphrase).
#[derive(Clone, Debug, Default)]
pub struct PromptReply {
    /// For host-key prompts: whether the key was accepted.
    pub accept: bool,
    /// For password/passphrase prompts: the entered secret (None = cancelled).
    pub secret: Option<String>,
}

#[derive(Clone)]
struct TtyTransferPrompt {
    event: crate::proto::TtyFileTransferApprovalEvent,
    expires_at: tokio::time::Instant,
}

impl TtyTransferPrompt {
    fn event_with_remaining_time(
        &self,
        now: tokio::time::Instant,
    ) -> Option<crate::proto::TtyFileTransferApprovalEvent> {
        let remaining = self.expires_at.checked_duration_since(now)?;
        if remaining.is_zero() {
            return None;
        }
        let mut event = self.event.clone();
        event.expires_in_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        Some(event)
    }
}

/// Session state wrapping a Terminal instance
pub struct SessionState {
    /// The terminal instance
    terminal: RwLock<Terminal>,

    /// Session ID
    pub id: String,

    /// Broadcast sender for output data
    output_tx: broadcast::Sender<OutputData>,

    /// Broadcast sender for terminal events
    event_tx: broadcast::Sender<TerminalEvent>,

    /// Number of currently attached clients
    attached_clients: AtomicU32,

    /// User-set custom title (overrides OSC title for display)
    custom_title: RwLock<String>,

    /// Tab color override (CSS hex, e.g. "#ff0000")
    tab_color: RwLock<String>,

    /// Template name used to create this session
    template_name: RwLock<String>,

    /// Whether this session has an unacknowledged bell alert
    alerted: std::sync::atomic::AtomicBool,

    /// Human-readable session name (for latch named sessions)
    session_name: RwLock<Option<String>>,

    /// True while an SSH session is still establishing its connection (no PTY
    /// yet). Keeps the session from being reaped as "dead" during connect.
    connecting: std::sync::atomic::AtomicBool,

    /// Broadcast of interactive SSH prompts (host key / password / passphrase)
    /// raised during connect; consumed by `StreamEvents` and surfaced to the UI.
    prompt_tx: broadcast::Sender<crate::proto::SessionPromptEvent>,

    /// Pending prompts awaiting a `RespondPrompt`, keyed by prompt id.
    prompt_registry: parking_lot::Mutex<HashMap<String, std::sync::mpsc::Sender<PromptReply>>>,

    /// Monotonic counter for generating prompt ids.
    prompt_counter: AtomicU64,

    /// Per-session OSC 5113 authorization/filesystem actor. `None` records a
    /// fail-closed initialization failure.
    tty_transfer: OnceLock<Option<TtyTransferController>>,

    /// Whether new transfer commands may enter the non-serializable actor.
    tty_transfer_accepting: AtomicBool,

    /// Live OSC 5113 approval events for attached native clients.
    tty_transfer_prompt_tx: broadcast::Sender<crate::proto::TtyFileTransferApprovalEvent>,

    /// Current bounded approval set, replayed to reconnecting subscribers.
    tty_transfer_prompt_registry: parking_lot::Mutex<HashMap<u64, TtyTransferPrompt>>,

    /// Dedicated off-thread writer for the PTY master. All input and parser responses
    /// are routed here so blocking PTY writes never stall a tokio worker thread or run
    /// under the terminal lock. Initialized lazily in `start_reader`, once the PTY
    /// exists (which for SSH sessions is only after the connection is established).
    pty_writer: OnceLock<PtyWriter>,

    /// Fast-path flag used by the synchronized-update watchdog so idle
    /// sessions do not require taking the terminal lock.
    sync_update_active: AtomicBool,
}

impl SessionState {
    /// Install native cursor defaults before daemon-authoritative parsing starts.
    pub fn configure_cursor(&self, style: cterm_core::CursorStyle, blink: bool) {
        self.terminal
            .write()
            .screen_mut()
            .configure_cursor(style, blink);
    }

    /// Replace the palette used for daemon-authoritative OSC color replies.
    pub fn set_base_palette(&self, palette: cterm_core::ColorPalette) {
        self.terminal.write().set_base_palette(palette);
    }

    /// Update native frontend state and forward any enabled change reports.
    pub fn set_frontend_state(&self, state: cterm_core::FrontendState) {
        let responses = self.terminal.write().set_frontend_state_collecting(state);
        self.send_terminal_responses(responses);
    }

    /// Current state last reported by the native frontend.
    pub fn frontend_state(&self) -> cterm_core::FrontendState {
        self.terminal.read().screen().frontend_state()
    }

    /// Create a new session with the given configuration
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        size: PtySize,
        shell: Option<String>,
        args: Vec<String>,
        cwd: Option<std::path::PathBuf>,
        env: Vec<(String, String)>,
        term: Option<String>,
        scrollback_lines: usize,
    ) -> Result<Arc<Self>> {
        let size = size.normalized();
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let pty_config = PtyConfig {
            size,
            shell,
            args,
            cwd,
            env,
            term,
        };

        let screen_config = ScreenConfig { scrollback_lines };
        let terminal = Terminal::with_shell(cols, rows, screen_config, &pty_config)?;

        // Create broadcast channels
        let (output_tx, _) = broadcast::channel(1024);
        let (event_tx, _) = broadcast::channel(256);

        let state = Arc::new(Self {
            terminal: RwLock::new(terminal),
            id,
            output_tx,
            event_tx,
            attached_clients: AtomicU32::new(0),
            custom_title: RwLock::new(String::new()),
            tab_color: RwLock::new(String::new()),
            template_name: RwLock::new(String::new()),
            session_name: RwLock::new(None),
            alerted: std::sync::atomic::AtomicBool::new(false),
            connecting: std::sync::atomic::AtomicBool::new(false),
            prompt_tx: broadcast::channel(16).0,
            prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            prompt_counter: AtomicU64::new(0),
            tty_transfer: OnceLock::new(),
            tty_transfer_accepting: AtomicBool::new(true),
            tty_transfer_prompt_tx: broadcast::channel(32).0,
            tty_transfer_prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            pty_writer: OnceLock::new(),
            sync_update_active: AtomicBool::new(false),
        });

        Ok(state)
    }

    /// Create a placeholder session for a native SSH connection that is still
    /// being established. It has a screen but no PTY yet; [`Self::is_running`]
    /// reports it as alive (via the `connecting` flag) so it is not reaped while
    /// connecting. Call [`Self::spawn_ssh_connect`] to drive the connection.
    pub fn new_ssh_connecting(id: String, size: PtySize, scrollback_lines: usize) -> Arc<Self> {
        let size = size.normalized();
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let screen_config = ScreenConfig { scrollback_lines };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.screen_mut().set_cell_width_hint(size.cell_width());
        terminal
            .screen_mut()
            .set_cell_height_hint(size.cell_height());

        let (output_tx, _) = broadcast::channel(1024);
        let (event_tx, _) = broadcast::channel(256);
        let (prompt_tx, _) = broadcast::channel(16);

        Arc::new(Self {
            terminal: RwLock::new(terminal),
            id,
            output_tx,
            event_tx,
            attached_clients: AtomicU32::new(0),
            custom_title: RwLock::new(String::new()),
            tab_color: RwLock::new(String::new()),
            template_name: RwLock::new(String::new()),
            session_name: RwLock::new(None),
            alerted: std::sync::atomic::AtomicBool::new(false),
            connecting: std::sync::atomic::AtomicBool::new(true),
            prompt_tx,
            prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            prompt_counter: AtomicU64::new(0),
            tty_transfer: OnceLock::new(),
            tty_transfer_accepting: AtomicBool::new(true),
            tty_transfer_prompt_tx: broadcast::channel(32).0,
            tty_transfer_prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            pty_writer: OnceLock::new(),
            sync_update_active: AtomicBool::new(false),
        })
    }

    /// Drive the SSH connection on a background task. Interactive prompts (host
    /// key, password, passphrase) are surfaced via [`Self::subscribe_prompts`]
    /// and answered with [`Self::respond_prompt`]. On success the PTY is
    /// attached and the reader started; on failure a `ProcessExited` event is
    /// broadcast.
    pub fn spawn_ssh_connect(
        self: &Arc<Self>,
        mut ssh_config: cterm_core::SshConfig,
        size: PtySize,
    ) {
        let size = size.normalized();
        let state = Arc::clone(self);

        tokio::spawn(async move {
            // Bind interactive prompt callbacks to this session.
            ssh_config.host_key_prompt = Some(state.host_key_prompt_callback());
            ssh_config.password_prompt = Some(state.password_prompt_callback());
            ssh_config.passphrase_prompt = Some(state.passphrase_prompt_callback());

            let connect_state = Arc::clone(&state);
            let result = tokio::task::spawn_blocking(move || {
                let _ = &connect_state; // keep the session alive for callbacks
                cterm_core::Pty::connect_ssh(ssh_config, size)
            })
            .await;

            state.connecting.store(false, Ordering::Relaxed);

            match result {
                Ok(Ok(pty)) => {
                    state.terminal.write().set_pty(pty);
                    if let Err(e) = state.start_reader() {
                        log::error!("Failed to start SSH reader for {}: {}", state.id, e);
                    }
                }
                Ok(Err(e)) => {
                    log::warn!("SSH connect failed for {}: {}", state.id, e);
                    state
                        .process_output(format!("\r\nSSH connection failed: {e}\r\n").as_bytes())
                        .await;
                    state.broadcast_event(TerminalEvent::ProcessExited(1));
                }
                Err(e) => {
                    log::error!("SSH connect task panicked for {}: {}", state.id, e);
                    state.broadcast_event(TerminalEvent::ProcessExited(1));
                }
            }
        });
    }

    /// Whether this session is still establishing its SSH connection.
    pub fn is_connecting(&self) -> bool {
        self.connecting.load(Ordering::Relaxed)
    }

    /// Subscribe to interactive SSH prompts for this session.
    pub fn subscribe_prompts(&self) -> broadcast::Receiver<crate::proto::SessionPromptEvent> {
        self.prompt_tx.subscribe()
    }

    /// Emit a prompt and return a receiver that resolves when the client
    /// replies via [`Self::respond_prompt`]. Runs on the (blocking) connect
    /// thread, which parks on the returned receiver.
    fn emit_prompt(
        &self,
        event: crate::proto::SessionPromptEvent,
    ) -> std::sync::mpsc::Receiver<PromptReply> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.prompt_registry
            .lock()
            .insert(event.prompt_id.clone(), tx);
        let _ = self.prompt_tx.send(event);
        rx
    }

    /// Deliver a reply to a pending prompt. Returns false if unknown/expired.
    pub fn respond_prompt(&self, prompt_id: &str, reply: PromptReply) -> bool {
        if let Some(tx) = self.prompt_registry.lock().remove(prompt_id) {
            tx.send(reply).is_ok()
        } else {
            false
        }
    }

    /// Subscribe to current and future OSC 5113 consent requests.
    ///
    /// The live receiver is created while the registry is locked, then the
    /// bounded snapshot is copied. A request therefore appears in at least one
    /// side of the returned pair, never neither.
    pub fn subscribe_tty_transfer_prompts(
        &self,
    ) -> (
        Vec<crate::proto::TtyFileTransferApprovalEvent>,
        broadcast::Receiver<crate::proto::TtyFileTransferApprovalEvent>,
    ) {
        let registry = self.tty_transfer_prompt_registry.lock();
        let rx = self.tty_transfer_prompt_tx.subscribe();
        let now = tokio::time::Instant::now();
        let mut snapshot: Vec<_> = registry
            .values()
            .filter_map(|prompt| prompt.event_with_remaining_time(now))
            .collect();
        snapshot.sort_by_key(|event| event.request_id);
        (snapshot, rx)
    }

    /// Deliver an approval decision to the actor that owns the exact pending
    /// token. Queue admission alone is not reported as authorization success.
    pub async fn respond_tty_transfer_approval(&self, request_id: u64, approve: bool) -> bool {
        if !self
            .tty_transfer_prompt_registry
            .lock()
            .contains_key(&request_id)
        {
            return false;
        }
        let Some(controller) = self.tty_transfer.get().and_then(Option::as_ref) else {
            return false;
        };
        controller.respond(request_id, approve).await
    }

    /// Abort all OSC 5113 work and wait until filesystem staging has been
    /// discarded. Idempotent when the per-session executor never started.
    pub async fn shutdown_tty_transfers(&self) {
        if let Some(controller) = self.tty_transfer.get().and_then(Option::as_ref) {
            controller.shutdown().await;
        }
        self.tty_transfer_prompt_registry.lock().clear();
    }

    pub fn quiesce_tty_transfers(&self) {
        self.tty_transfer_accepting.store(false, Ordering::SeqCst);
        if let Some(controller) = self.tty_transfer.get().and_then(Option::as_ref) {
            controller.quiesce();
        }
    }

    pub fn resume_tty_transfers(&self) {
        self.tty_transfer_accepting.store(true, Ordering::SeqCst);
        if let Some(controller) = self.tty_transfer.get().and_then(Option::as_ref) {
            controller.resume();
        }
    }

    pub fn has_active_tty_transfers(&self) -> bool {
        self.tty_transfer
            .get()
            .and_then(Option::as_ref)
            .is_some_and(TtyTransferController::has_work)
    }

    pub(super) fn register_tty_transfer_prompt(
        &self,
        request: TtyTransferApprovalRequest,
        expires_at: tokio::time::Instant,
    ) {
        let direction = match request.direction {
            TtyTransferDirection::Send => crate::proto::TtyFileTransferDirection::Send,
            TtyTransferDirection::Receive => crate::proto::TtyFileTransferDirection::Receive,
        };
        let event = crate::proto::TtyFileTransferApprovalEvent {
            request_id: request.request_id,
            transfer_id: request.session_id,
            direction: direction as i32,
            paths: request.paths,
            expires_in_ms: 0,
            max_files: super::tty_transfer::DEFAULT_MAX_FILES_PER_SESSION as u32,
            max_file_bytes: super::tty_transfer::DEFAULT_MAX_FILE_BYTES,
            max_session_bytes: super::tty_transfer::DEFAULT_MAX_SESSION_BYTES,
        };
        let prompt = TtyTransferPrompt { event, expires_at };
        let mut registry = self.tty_transfer_prompt_registry.lock();
        let Some(event) = prompt.event_with_remaining_time(tokio::time::Instant::now()) else {
            return;
        };
        registry.insert(event.request_id, prompt);
        let _ = self.tty_transfer_prompt_tx.send(event);
    }

    pub(super) fn clear_tty_transfer_prompt(&self, request_id: u64) {
        self.tty_transfer_prompt_registry.lock().remove(&request_id);
    }

    fn next_prompt_id(&self) -> String {
        format!(
            "{}-{}",
            self.id,
            self.prompt_counter.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn host_key_prompt_callback(self: &Arc<Self>) -> cterm_core::HostKeyPrompt {
        let state = Arc::clone(self);
        Arc::new(move |req: cterm_core::HostKeyRequest| {
            let prompt_id = state.next_prompt_id();
            let kind = if req.changed {
                crate::proto::PromptKind::HostkeyChanged
            } else {
                crate::proto::PromptKind::HostkeyUnknown
            };
            let rx = state.emit_prompt(crate::proto::SessionPromptEvent {
                prompt_id,
                kind: kind as i32,
                host: req.host,
                port: req.port as u32,
                key_type: req.key_type,
                fingerprint: req.fingerprint,
                text: String::new(),
            });
            rx.recv().map(|r| r.accept).unwrap_or(false)
        })
    }

    fn password_prompt_callback(self: &Arc<Self>) -> cterm_core::PasswordPrompt {
        let state = Arc::clone(self);
        Arc::new(move |text: &str| {
            let prompt_id = state.next_prompt_id();
            let rx = state.emit_prompt(crate::proto::SessionPromptEvent {
                prompt_id,
                kind: crate::proto::PromptKind::Password as i32,
                host: String::new(),
                port: 0,
                key_type: String::new(),
                fingerprint: String::new(),
                text: text.to_string(),
            });
            rx.recv().ok().and_then(|r| r.secret)
        })
    }

    fn passphrase_prompt_callback(self: &Arc<Self>) -> cterm_core::PassphrasePrompt {
        let state = Arc::clone(self);
        Arc::new(move |path: &str| {
            let prompt_id = state.next_prompt_id();
            let rx = state.emit_prompt(crate::proto::SessionPromptEvent {
                prompt_id,
                kind: crate::proto::PromptKind::Passphrase as i32,
                host: String::new(),
                port: 0,
                key_type: String::new(),
                fingerprint: String::new(),
                text: format!("Enter passphrase for {path}"),
            });
            rx.recv().ok().and_then(|r| r.secret)
        })
    }

    /// Reconstruct a session from a raw PTY file descriptor (used during relaunch).
    ///
    /// # Safety
    /// The caller must ensure `fd` is a valid PTY master FD and `child_pid` is correct.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn from_raw_fd(
        id: String,
        fd: i32,
        child_pid: i32,
        cols: usize,
        rows: usize,
        custom_title: String,
        tab_color: String,
        template_name: String,
        scrollback_lines: usize,
    ) -> Result<Arc<Self>> {
        let pty = Pty::from_raw_fd(fd, child_pid);
        let screen_config = ScreenConfig { scrollback_lines };
        let mut terminal = Terminal::new(cols, rows, screen_config);
        terminal.set_pty(pty);

        let (output_tx, _) = broadcast::channel(1024);
        let (event_tx, _) = broadcast::channel(256);

        let state = Arc::new(Self {
            terminal: RwLock::new(terminal),
            id,
            output_tx,
            event_tx,
            attached_clients: AtomicU32::new(0),
            custom_title: RwLock::new(custom_title),
            tab_color: RwLock::new(tab_color),
            template_name: RwLock::new(template_name),
            session_name: RwLock::new(None),
            alerted: std::sync::atomic::AtomicBool::new(false),
            connecting: std::sync::atomic::AtomicBool::new(false),
            prompt_tx: broadcast::channel(16).0,
            prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            prompt_counter: AtomicU64::new(0),
            tty_transfer: OnceLock::new(),
            tty_transfer_accepting: AtomicBool::new(true),
            tty_transfer_prompt_tx: broadcast::channel(32).0,
            tty_transfer_prompt_registry: parking_lot::Mutex::new(HashMap::new()),
            pty_writer: OnceLock::new(),
            sync_update_active: AtomicBool::new(false),
        });

        Ok(state)
    }

    /// Start the PTY reader task
    pub fn start_reader(self: &Arc<Self>) -> Result<Arc<Self>> {
        let pty_reader = self.terminal.read().pty_reader();

        if let Some(reader) = pty_reader {
            self.tty_transfer
                .get_or_init(|| match TtyTransferController::spawn(self) {
                    Ok(controller) if !self.tty_transfer_accepting.load(Ordering::SeqCst) => {
                        controller.quiesce();
                        Some(controller)
                    }
                    Ok(controller) => Some(controller),
                    Err(error) => {
                        log::error!("OSC 5113 is unavailable for session {}: {error}", self.id);
                        None
                    }
                });
            // Now that a PTY exists, spin up the dedicated writer thread (owns its own
            // dup'd master fd). Idempotent: a no-op if already initialized.
            if let Some(file) = self.terminal.read().pty_writer() {
                let _ = self.pty_writer.set(PtyWriter::new(file, self.id.clone()));
            }

            let state = Arc::clone(self);
            let sync_state = Arc::downgrade(self);
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_millis(16));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    interval.tick().await;
                    let Some(state) = sync_state.upgrade() else {
                        break;
                    };
                    if !state.sync_update_active.load(Ordering::Relaxed) {
                        continue;
                    }
                    let mut term = state.terminal.write();
                    let expired = term.expire_synchronized_update();
                    state.sync_update_active.store(
                        term.synchronized_update_deadline().is_some(),
                        Ordering::Relaxed,
                    );
                    drop(term);
                    if expired {
                        state.broadcast_event(TerminalEvent::ContentChanged);
                    }
                }
            });
            // Spawn the reader task - it will run until the PTY closes
            tokio::spawn(async move {
                let pty_reader = PtyReader::new(reader);
                pty_reader.run(Arc::clone(&state)).await;
                // Notify subscribers that the process has exited
                log::debug!(
                    "PTY closed for session {}, broadcasting ProcessExited",
                    state.id
                );
                state.broadcast_event(TerminalEvent::ProcessExited(0));
            });
        }

        Ok(Arc::clone(self))
    }

    /// Increment the attached client count
    pub fn attach(&self) {
        self.attached_clients.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement the attached client count
    pub fn detach(&self) {
        // Stale UI cleanup can race after a failed reconnect. Keep detach
        // idempotent instead of wrapping the public count to `u32::MAX`.
        let _ = self
            .attached_clients
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |count| {
                Some(count.saturating_sub(1))
            });
    }

    /// Get the number of currently attached clients
    pub fn attached_clients(&self) -> u32 {
        self.attached_clients.load(Ordering::Relaxed)
    }

    /// Get the terminal dimensions
    pub fn dimensions(&self) -> (usize, usize) {
        let term = self.terminal.read();
        (term.cols(), term.rows())
    }

    /// Get the terminal title
    pub fn title(&self) -> String {
        self.terminal.read().title().to_string()
    }

    /// Get the user-set custom title
    pub fn custom_title(&self) -> String {
        self.custom_title.read().clone()
    }

    /// Set a custom title (empty string to clear)
    pub fn set_custom_title(&self, title: String) {
        *self.custom_title.write() = title;
    }

    /// Get the tab color override
    pub fn tab_color(&self) -> String {
        self.tab_color.read().clone()
    }

    /// Set the tab color override (empty string to clear)
    pub fn set_tab_color(&self, color: String) {
        *self.tab_color.write() = color;
    }

    /// Get the template name
    pub fn template_name(&self) -> String {
        self.template_name.read().clone()
    }

    /// Set the template name
    pub fn set_template_name(&self, name: String) {
        *self.template_name.write() = name;
    }

    /// Get the human-readable session name (for latch)
    pub fn session_name(&self) -> Option<String> {
        self.session_name.read().clone()
    }

    /// Set the human-readable session name
    pub fn set_session_name(&self, name: Option<String>) {
        *self.session_name.write() = name;
    }

    /// Whether this session has an unacknowledged bell alert.
    pub fn is_alerted(&self) -> bool {
        self.alerted.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Set the alerted state and broadcast a bell event if newly alerted.
    pub fn set_alerted(&self, alerted: bool) {
        let was_alerted = self
            .alerted
            .swap(alerted, std::sync::atomic::Ordering::Relaxed);
        if alerted && !was_alerted {
            self.broadcast_event(TerminalEvent::Bell);
        }
    }

    /// Check if the terminal is still running
    pub fn is_running(&self) -> bool {
        // A session still establishing its SSH connection has no PTY yet but
        // must not be treated as dead.
        self.connecting.load(Ordering::Relaxed) || self.terminal.write().is_running()
    }

    /// Get the child process ID
    pub fn child_pid(&self) -> Option<i32> {
        self.terminal.read().child_pid()
    }

    /// Check if a non-shell foreground process is running (PID-based).
    #[cfg(unix)]
    pub fn has_foreground_process(&self) -> bool {
        self.terminal.read().has_foreground_process()
    }

    /// Check if a non-shell foreground process is running (stub for non-Unix).
    #[cfg(not(unix))]
    pub fn has_foreground_process(&self) -> bool {
        false
    }

    /// Get the name of the foreground process (for display only).
    #[cfg(unix)]
    pub fn foreground_process_name(&self) -> Option<String> {
        self.terminal.read().foreground_process_name()
    }

    /// Get the name of the foreground process (stub for non-Unix).
    #[cfg(not(unix))]
    pub fn foreground_process_name(&self) -> Option<String> {
        None
    }

    /// Write input to the terminal.
    ///
    /// Routes through the dedicated PTY writer thread so the (potentially blocking)
    /// write never runs on a tokio worker thread or under the terminal lock.
    pub fn write_input(&self, data: &[u8]) -> Result<usize> {
        match self.pty_writer.get() {
            Some(writer) => writer.send(data),
            // No PTY writer yet (e.g. SSH session still connecting): fall back to a
            // direct write, which is a no-op unless a write_fn is configured.
            None => {
                self.terminal.write().write(data)?;
            }
        }
        Ok(data.len())
    }

    /// Resize the terminal
    pub fn resize(&self, cols: usize, rows: usize) {
        self.terminal.write().resize(cols, rows);
    }

    /// Resize the terminal with total pixel dimensions from the UI.
    pub fn resize_with_pixels(
        &self,
        cols: usize,
        rows: usize,
        pixel_width: u16,
        pixel_height: u16,
    ) {
        self.terminal
            .write()
            .resize_with_pixels(cols, rows, pixel_width, pixel_height);
    }

    /// Send a signal to the child process
    pub fn send_signal(&self, signal: i32) -> Result<()> {
        self.terminal.read().send_signal(signal)?;
        Ok(())
    }

    /// Process PTY output data.
    ///
    /// Parses under the terminal lock, then releases it BEFORE sending any
    /// parser-generated responses (DSR/DA/cursor reports) to the PTY writer thread.
    /// This guarantees the terminal lock is never held across a (potentially blocking)
    /// PTY write — the root cause of the daemon deadlock this avoids.
    pub async fn process_output(&self, data: &[u8]) -> Vec<TerminalEvent> {
        let (events, responses, commands) = {
            let mut term = self.terminal.write();
            let (events, responses, commands) = term.process_collecting_with_file_transfers(data);
            self.sync_update_active.store(
                term.synchronized_update_deadline().is_some(),
                Ordering::Relaxed,
            );
            (events, responses, commands)
        }; // terminal lock released here

        self.send_terminal_responses(responses);

        for command in commands {
            if !self.tty_transfer_accepting.load(Ordering::SeqCst) {
                self.send_tty_transfer_error(&command, "EBUSY:Daemon relaunch in progress");
                continue;
            }
            let Some(controller) = self.tty_transfer.get().and_then(Option::as_ref) else {
                self.send_tty_transfer_error(
                    &command,
                    "ENOTSUP:Local transfer executor is unavailable",
                );
                continue;
            };
            if let Err(command) = controller.submit(command).await {
                self.send_tty_transfer_error(&command, "EIO:Local transfer executor stopped");
            }
        }

        events
    }

    fn send_tty_transfer_error(&self, command: &FileTransferCommand, status: &str) {
        if command.quiet >= 2 {
            return;
        }
        let response = FileTransferCommand {
            action: FileTransferAction::Status,
            id: command.id.clone(),
            file_id: command.file_id.clone(),
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: None,
            name: None,
            status: Some(status.to_string()),
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        };
        if let Ok(bytes) = response.encode() {
            self.send_tty_transfer_response(&bytes);
        }
    }

    pub(super) fn send_tty_transfer_response(&self, response: &[u8]) {
        match self.pty_writer.get() {
            Some(writer) => writer.send(response),
            None => {
                if let Err(error) = self.terminal.write().write(response) {
                    log::error!("Failed to send OSC 5113 response to PTY: {error}");
                }
            }
        }
    }

    fn send_terminal_responses(&self, responses: Vec<Vec<u8>>) {
        if responses.is_empty() {
            return;
        }
        let response = responses.concat();
        match self.pty_writer.get() {
            Some(writer) => writer.send(&response),
            None => {
                // No PTY writer yet: write responses directly (no-op without a PTY).
                let mut term = self.terminal.write();
                if let Err(error) = term.write(&response) {
                    log::error!("Failed to send response to PTY: {error}");
                }
            }
        }
    }

    /// Broadcast output data to subscribers
    pub fn broadcast_output(&self, data: OutputData) {
        let _ = self.output_tx.send(data);
    }

    /// Broadcast a terminal event to subscribers
    pub fn broadcast_event(&self, event: TerminalEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Subscribe to output stream
    pub fn subscribe_output(&self) -> broadcast::Receiver<OutputData> {
        self.output_tx.subscribe()
    }

    /// Subscribe to event stream
    pub fn subscribe_events(&self) -> broadcast::Receiver<TerminalEvent> {
        self.event_tx.subscribe()
    }

    /// Handle a key press and return the escape sequence
    pub fn handle_key(
        &self,
        key: cterm_core::term::Key,
        modifiers: cterm_core::term::Modifiers,
    ) -> Option<Vec<u8>> {
        self.terminal.read().handle_key(key, modifiers)
    }

    /// Get a reference to the terminal (for reading screen state)
    pub fn with_terminal<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&Terminal) -> R,
    {
        let term = self.terminal.read();
        f(&term)
    }

    /// Get a mutable reference to the terminal
    pub fn with_terminal_mut<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut Terminal) -> R,
    {
        let mut term = self.terminal.write();
        f(&mut term)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_count_saturates_at_zero() {
        let session =
            SessionState::new_ssh_connecting("attachment-count".to_string(), PtySize::default(), 0);

        session.detach();
        assert_eq!(session.attached_clients(), 0);

        session.attach();
        session.detach();
        session.detach();
        assert_eq!(session.attached_clients(), 0);
    }

    #[test]
    fn cursor_defaults_are_authoritative_before_child_output() {
        let session =
            SessionState::new_ssh_connecting("cursor-defaults".to_string(), PtySize::default(), 0);
        session.configure_cursor(cterm_core::CursorStyle::Bar, false);

        let (_, responses) =
            session.with_terminal_mut(|terminal| terminal.process_collecting(b"\x1bP$q q\x1b\\"));

        assert_eq!(responses, vec![b"\x1bP1$r6 q\x1b\\".to_vec()]);
    }

    #[tokio::test]
    async fn transfer_prompt_is_replayed_and_exactly_one_decision_succeeds() {
        let session = SessionState::new_ssh_connecting(
            "tty-transfer-consent".to_string(),
            PtySize::default(),
            0,
        );
        let controller = TtyTransferController::spawn(&session).unwrap();
        assert!(session.tty_transfer.set(Some(controller)).is_ok());
        let (_, mut live) = session.subscribe_tty_transfer_prompts();

        session
            .process_output(b"\x1b]5113;ac=send;id=consent-test\x1b\\")
            .await;
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), live.recv())
            .await
            .expect("approval event timed out")
            .expect("approval channel closed");
        assert_eq!(event.transfer_id, "consent-test");
        assert_eq!(
            event.direction,
            crate::proto::TtyFileTransferDirection::Send as i32
        );
        assert!((1..=60_000).contains(&event.expires_in_ms));

        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        let (snapshot, _) = session.subscribe_tty_transfer_prompts();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].request_id, event.request_id);
        assert_eq!(snapshot[0].transfer_id, event.transfer_id);
        assert!(snapshot[0].expires_in_ms < event.expires_in_ms);
        assert!(
            session
                .respond_tty_transfer_approval(event.request_id, false)
                .await
        );
        assert!(
            !session
                .respond_tty_transfer_approval(event.request_id, true)
                .await
        );
        assert!(session.subscribe_tty_transfer_prompts().0.is_empty());
        session.shutdown_tty_transfers().await;
    }
}
