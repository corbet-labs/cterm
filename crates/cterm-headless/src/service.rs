//! gRPC TerminalService implementation

use crate::convert::{
    cell_to_proto, cursor_to_proto, event_to_proto, extra_cursors_to_proto, modes_to_proto,
    proto_to_cursor_style, proto_to_frontend_state, proto_to_key, proto_to_modifiers,
    proto_to_palette, screen_to_proto, screen_to_text, terminal_images_to_proto,
    visible_rows_to_proto,
};
use crate::proto::terminal_service_server::TerminalService;
use crate::proto::*;
use crate::session::SessionManager;
#[cfg(unix)]
use libc;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Notify;
use tokio_stream::{
    wrappers::errors::BroadcastStreamRecvError, wrappers::BroadcastStream, Stream, StreamExt,
};
use tonic::{Request, Response, Status};

/// TerminalService implementation
pub struct TerminalServiceImpl {
    session_manager: Arc<SessionManager>,
    /// Notifier used to trigger server shutdown from the shutdown RPC
    shutdown_notify: Arc<Notify>,
    /// Unique identifier for this daemon instance
    daemon_id: String,
    /// Stable logical identity configured for this daemon endpoint.
    daemon_identity: String,
    /// Managed authentication key. Debug formatting is redacted by its type.
    daemon_auth_secret: Option<cterm_proto::DaemonAuthSecret>,
    /// Time when the daemon was started
    start_time: Instant,
    /// Number of clients that have performed a handshake
    client_count: AtomicU32,
    /// Number of active output streams (proxy for connected clients)
    active_streams: Arc<AtomicU32>,
    /// Socket path (needed for relaunch)
    socket_path: String,
    /// Default scrollback lines (needed for relaunch)
    scrollback_lines: usize,
}

impl TerminalServiceImpl {
    /// Create a new TerminalService with a shutdown notifier
    pub fn new(session_manager: Arc<SessionManager>, shutdown_notify: Arc<Notify>) -> Self {
        Self {
            session_manager,
            shutdown_notify,
            daemon_id: uuid::Uuid::new_v4().to_string(),
            daemon_identity: "cterm".to_string(),
            daemon_auth_secret: None,
            start_time: Instant::now(),
            client_count: AtomicU32::new(0),
            active_streams: Arc::new(AtomicU32::new(0)),
            socket_path: String::new(),
            scrollback_lines: 10000,
        }
    }

    /// Set daemon endpoint details needed for handshakes and relaunch.
    pub fn set_server_config(
        &mut self,
        socket_path: String,
        daemon_identity: String,
        daemon_auth_secret: Option<cterm_proto::DaemonAuthSecret>,
        scrollback_lines: usize,
    ) {
        self.socket_path = socket_path;
        self.daemon_identity = daemon_identity;
        self.daemon_auth_secret = daemon_auth_secret;
        self.scrollback_lines = scrollback_lines;
    }
}

#[tonic::async_trait]
impl TerminalService for TerminalServiceImpl {
    // ========================================================================
    // Session Management
    // ========================================================================

    async fn create_session(
        &self,
        request: Request<CreateSessionRequest>,
    ) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();

        let base_palette = match req.base_palette.as_ref() {
            Some(palette) => Some(
                proto_to_palette(palette)
                    .ok_or_else(|| Status::invalid_argument("invalid frontend palette"))?,
            ),
            None => None,
        };
        let frontend_state = proto_to_frontend_state(req.theme_appearance, req.window_visibility)
            .ok_or_else(|| Status::invalid_argument("invalid frontend state"))?;
        let cursor_style = match req.cursor_style {
            Some(style) => proto_to_cursor_style(style)
                .ok_or_else(|| Status::invalid_argument("invalid cursor style"))?,
            None => cterm_core::CursorStyle::Block,
        };
        let cursor_blink = req.cursor_blink.unwrap_or(true);

        let cols = req.cols.max(1) as usize;
        let rows = req.rows.max(1) as usize;
        let size = cterm_core::PtySize {
            cols: cols.min(u16::MAX as usize) as u16,
            rows: rows.min(u16::MAX as usize) as u16,
            pixel_width: req.pixel_width.min(u16::MAX as u32) as u16,
            pixel_height: req.pixel_height.min(u16::MAX as u32) as u16,
        }
        .normalized();

        // Native SSH session: open a puressh connection instead of a local shell.
        if let Some(ssh) = req.ssh {
            let local_forwards = ssh
                .local_forwards
                .into_iter()
                .map(|f| cterm_core::ssh::LocalForward {
                    local_port: f.local_port as u16,
                    remote_host: f.remote_host,
                    remote_port: f.remote_port as u16,
                })
                .collect();
            let mut ssh_config = cterm_core::SshConfig {
                host: ssh.host,
                port: ssh.port as u16,
                username: ssh.username,
                identity_files: ssh.identity_files.into_iter().map(PathBuf::from).collect(),
                term: req.term,
                remote_command: ssh.remote_command,
                local_forwards,
                jump_host: ssh.jump_host,
                agent_forward: ssh.agent_forward,
                x11_forward: ssh.x11_forward,
                // Interactive shell PTY; compression is left off (small,
                // latency-sensitive traffic).
                compress: false,
                host_key_prompt: None,
                password_prompt: None,
                passphrase_prompt: None,
            };
            // Default TERM if the client did not specify one.
            if ssh_config.term.is_none() {
                ssh_config.term = Some("xterm-256color".to_string());
            }

            let session = self
                .session_manager
                .create_ssh_session_with_size_and_palette(
                    size,
                    ssh_config,
                    base_palette,
                    frontend_state,
                    cursor_style,
                    cursor_blink,
                )
                .map_err(Status::from)?;

            return Ok(Response::new(CreateSessionResponse {
                session_id: session.id.clone(),
                cols: u32::from(size.cols),
                rows: u32::from(size.rows),
            }));
        }

        let env: Vec<(String, String)> = req.env.into_iter().collect();

        let session = self
            .session_manager
            .create_session_with_size_and_palette(
                size,
                req.shell,
                req.args,
                req.cwd.map(PathBuf::from),
                env,
                req.term,
                base_palette,
                frontend_state,
                cursor_style,
                cursor_blink,
            )
            .map_err(Status::from)?;

        Ok(Response::new(CreateSessionResponse {
            session_id: session.id.clone(),
            cols: u32::from(size.cols),
            rows: u32::from(size.rows),
        }))
    }

    async fn list_sessions(
        &self,
        _request: Request<ListSessionsRequest>,
    ) -> Result<Response<ListSessionsResponse>, Status> {
        let sessions = self.session_manager.list_sessions();

        let session_infos: Vec<SessionInfo> = sessions
            .iter()
            .map(|s| {
                let (cols, rows) = s.dimensions();
                SessionInfo {
                    session_id: s.id.clone(),
                    cols: cols as u32,
                    rows: rows as u32,
                    title: s.title(),
                    running: s.is_running(),
                    child_pid: s.child_pid().unwrap_or(0),
                    attached_clients: s.attached_clients(),
                    custom_title: s.custom_title(),
                    tab_color: s.tab_color(),
                    template_name: s.template_name(),
                    has_foreground_process: s.has_foreground_process(),
                    foreground_process_name: s.foreground_process_name().unwrap_or_default(),
                    alerted: s.is_alerted(),
                }
            })
            .collect();

        Ok(Response::new(ListSessionsResponse {
            sessions: session_infos,
        }))
    }

    async fn get_session(
        &self,
        request: Request<GetSessionRequest>,
    ) -> Result<Response<GetSessionResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let (cols, rows) = session.dimensions();
        let info = SessionInfo {
            session_id: session.id.clone(),
            cols: cols as u32,
            rows: rows as u32,
            title: session.title(),
            running: session.is_running(),
            child_pid: session.child_pid().unwrap_or(0),
            attached_clients: session.attached_clients(),
            custom_title: session.custom_title(),
            tab_color: session.tab_color(),
            template_name: session.template_name(),
            has_foreground_process: session.has_foreground_process(),
            foreground_process_name: session.foreground_process_name().unwrap_or_default(),
            alerted: session.is_alerted(),
        };

        Ok(Response::new(GetSessionResponse {
            session: Some(info),
        }))
    }

    async fn destroy_session(
        &self,
        request: Request<DestroySessionRequest>,
    ) -> Result<Response<DestroySessionResponse>, Status> {
        let req = request.into_inner();
        self.session_manager
            .destroy_session(&req.session_id, req.signal)
            .map_err(Status::from)?;

        Ok(Response::new(DestroySessionResponse { success: true }))
    }

    // ========================================================================
    // Session Metadata
    // ========================================================================

    async fn set_session_title(
        &self,
        request: Request<SetSessionTitleRequest>,
    ) -> Result<Response<SetSessionTitleResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.set_custom_title(req.custom_title);

        Ok(Response::new(SetSessionTitleResponse { success: true }))
    }

    async fn set_session_metadata(
        &self,
        request: Request<SetSessionMetadataRequest>,
    ) -> Result<Response<SetSessionMetadataResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let mask = req.fields_mask;
        if mask & 1 != 0 {
            session.set_custom_title(req.custom_title);
        }
        if mask & 2 != 0 {
            session.set_tab_color(req.tab_color);
        }
        if mask & 4 != 0 {
            session.set_template_name(req.template_name);
        }

        Ok(Response::new(SetSessionMetadataResponse { success: true }))
    }

    async fn set_session_palette(
        &self,
        request: Request<SetSessionPaletteRequest>,
    ) -> Result<Response<SetSessionPaletteResponse>, Status> {
        let req = request.into_inner();
        let palette = req
            .palette
            .as_ref()
            .and_then(proto_to_palette)
            .ok_or_else(|| Status::invalid_argument("invalid frontend palette"))?;
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;
        session.set_base_palette(palette);
        Ok(Response::new(SetSessionPaletteResponse { success: true }))
    }

    async fn set_session_frontend_state(
        &self,
        request: Request<SetSessionFrontendStateRequest>,
    ) -> Result<Response<SetSessionFrontendStateResponse>, Status> {
        let req = request.into_inner();
        if req.theme_appearance.is_none() && req.window_visibility.is_none() {
            return Err(Status::invalid_argument("frontend state update is empty"));
        }
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;
        let current = session.frontend_state();
        let (current_theme, current_visibility) =
            cterm_proto::convert::frontend_state_to_proto(current);
        let state = proto_to_frontend_state(
            req.theme_appearance.unwrap_or(current_theme),
            req.window_visibility.unwrap_or(current_visibility),
        )
        .ok_or_else(|| Status::invalid_argument("invalid frontend state"))?;
        session.set_frontend_state(state);
        Ok(Response::new(SetSessionFrontendStateResponse {
            success: true,
        }))
    }

    // ========================================================================
    // Input
    // ========================================================================

    async fn write_input(
        &self,
        request: Request<WriteInputRequest>,
    ) -> Result<Response<WriteInputResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let bytes_written = session.write_input(&req.data).map_err(Status::from)?;

        Ok(Response::new(WriteInputResponse {
            bytes_written: bytes_written as u32,
        }))
    }

    async fn stream_input(
        &self,
        request: Request<tonic::Streaming<WriteInputRequest>>,
    ) -> Result<Response<StreamInputResponse>, Status> {
        let mut stream = request.into_inner();
        let session_manager = Arc::clone(&self.session_manager);
        let mut session = None;
        let mut total_bytes: u64 = 0;

        while let Some(msg) = stream.next().await {
            let msg = msg?;

            // Resolve the session lazily on the first message; subsequent
            // messages must use the same session_id.
            if session.is_none() {
                session = Some(
                    session_manager
                        .get_session(&msg.session_id)
                        .map_err(Status::from)?,
                );
            } else if let Some(ref s) = session {
                if s.id != msg.session_id {
                    return Err(Status::invalid_argument(
                        "session_id mismatch within stream",
                    ));
                }
            }

            let s = session.as_ref().unwrap();
            let n = s.write_input(&msg.data).map_err(Status::from)?;
            total_bytes += n as u64;
        }

        Ok(Response::new(StreamInputResponse {
            total_bytes_written: total_bytes,
        }))
    }

    async fn send_key(
        &self,
        request: Request<SendKeyRequest>,
    ) -> Result<Response<SendKeyResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let key = req
            .key
            .as_ref()
            .and_then(proto_to_key)
            .ok_or_else(|| Status::invalid_argument("Invalid key"))?;

        let modifiers = req
            .modifiers
            .as_ref()
            .map(proto_to_modifiers)
            .unwrap_or_default();

        let sequence = session.handle_key(key, modifiers).unwrap_or_default();

        // Write the sequence to the PTY
        if !sequence.is_empty() {
            session.write_input(&sequence).map_err(Status::from)?;
        }

        Ok(Response::new(SendKeyResponse { sequence }))
    }

    // ========================================================================
    // Output Streaming
    // ========================================================================

    type StreamOutputStream =
        Pin<Box<dyn Stream<Item = Result<OutputChunk, Status>> + Send + 'static>>;

    async fn stream_output(
        &self,
        request: Request<StreamOutputRequest>,
    ) -> Result<Response<Self::StreamOutputStream>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.attach();
        self.active_streams.fetch_add(1, Ordering::Relaxed);

        let rx = session.subscribe_output();
        let session_id = req.session_id.clone();
        let session_detach = session.clone();
        let active_streams = Arc::clone(&self.active_streams);
        let session_manager = Arc::clone(&self.session_manager);
        let shutdown_notify = Arc::clone(&self.shutdown_notify);
        let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
            Ok(data) => Some(Ok(OutputChunk {
                data: data.data,
                timestamp_ms: data.timestamp_ms,
            })),
            Err(BroadcastStreamRecvError::Lagged(count)) => {
                log::warn!(
                    "stream_output: client lagged, dropped {} messages for session {}. \
                         Client terminal state may be stale until new output arrives.",
                    count,
                    session_id,
                );
                None
            }
        });

        // Wrap the stream to detach and check auto-shutdown when the client disconnects
        let stream = StreamNotify::new(stream, move || {
            session_detach.detach();
            let prev = active_streams.fetch_sub(1, Ordering::Relaxed);
            if prev == 1 && session_manager.session_count() == 0 && session_manager.had_sessions() {
                log::info!("No sessions and no connected clients, shutting down daemon");
                shutdown_notify.notify_one();
            }
        });

        Ok(Response::new(Box::pin(stream)))
    }

    // ========================================================================
    // Screen State
    // ========================================================================

    async fn get_screen(
        &self,
        request: Request<GetScreenRequest>,
    ) -> Result<Response<GetScreenResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let response =
            session.with_terminal(|term| screen_to_proto(term.screen(), req.include_scrollback));

        Ok(Response::new(response))
    }

    async fn get_cell(
        &self,
        request: Request<GetCellRequest>,
    ) -> Result<Response<GetCellResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let cell = session.with_terminal(|term| {
            term.screen()
                .get_cell(req.row as usize, req.col as usize)
                .cloned()
        });

        let cell = cell.ok_or_else(|| Status::out_of_range("Cell position out of range"))?;

        Ok(Response::new(GetCellResponse {
            cell: Some(cell_to_proto(&cell)),
        }))
    }

    async fn get_cursor(
        &self,
        request: Request<GetCursorRequest>,
    ) -> Result<Response<GetCursorResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let cursor = session.with_terminal(|term| {
            let screen = term.screen();
            cursor_to_proto(screen)
        });

        Ok(Response::new(GetCursorResponse {
            cursor: Some(cursor),
        }))
    }

    async fn get_screen_text(
        &self,
        request: Request<GetScreenTextRequest>,
    ) -> Result<Response<GetScreenTextResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let lines = session.with_terminal(|term| {
            screen_to_text(
                term.screen(),
                req.include_scrollback,
                req.start_row,
                req.end_row,
            )
        });

        Ok(Response::new(GetScreenTextResponse { lines }))
    }

    // ========================================================================
    // Control
    // ========================================================================

    async fn resize(
        &self,
        request: Request<ResizeRequest>,
    ) -> Result<Response<ResizeResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.resize_with_pixels(
            req.cols.max(1) as usize,
            req.rows.max(1) as usize,
            req.pixel_width.min(u16::MAX as u32) as u16,
            req.pixel_height.min(u16::MAX as u32) as u16,
        );

        Ok(Response::new(ResizeResponse { success: true }))
    }

    async fn send_signal(
        &self,
        request: Request<SendSignalRequest>,
    ) -> Result<Response<SendSignalResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.send_signal(req.signal).map_err(Status::from)?;

        Ok(Response::new(SendSignalResponse { success: true }))
    }

    async fn clear_alert(
        &self,
        request: Request<ClearAlertRequest>,
    ) -> Result<Response<ClearAlertResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.set_alerted(false);

        Ok(Response::new(ClearAlertResponse {}))
    }

    // ========================================================================
    // Event Streaming
    // ========================================================================

    type StreamEventsStream =
        Pin<Box<dyn Stream<Item = Result<TerminalEvent, Status>> + Send + 'static>>;

    async fn stream_events(
        &self,
        request: Request<StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let rx = session.subscribe_events();
        let session_id = req.session_id.clone();
        let events = BroadcastStream::new(rx).filter_map(move |result| match result {
            Ok(event) => event_to_proto(&event).map(Ok),
            Err(BroadcastStreamRecvError::Lagged(count)) => {
                log::warn!(
                    "stream_events: client lagged, dropped {} events for session {}",
                    count,
                    session_id,
                );
                None
            }
        });

        // Merge in interactive SSH prompts (host key / password / passphrase),
        // which the client answers via RespondPrompt.
        let prompt_rx = session.subscribe_prompts();
        let prompts = BroadcastStream::new(prompt_rx).filter_map(|result| match result {
            Ok(prompt) => Some(Ok(TerminalEvent {
                event: Some(terminal_event::Event::SessionPrompt(prompt)),
            })),
            Err(_) => None,
        });

        let stream = events.merge(prompts);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn respond_prompt(
        &self,
        request: Request<RespondPromptRequest>,
    ) -> Result<Response<RespondPromptResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let success = session.respond_prompt(
            &req.prompt_id,
            crate::session::PromptReply {
                accept: req.accept,
                secret: req.secret,
            },
        );

        Ok(Response::new(RespondPromptResponse { success }))
    }

    // ========================================================================
    // Connection Management (new RPCs)
    // ========================================================================

    async fn handshake(
        &self,
        request: Request<HandshakeRequest>,
    ) -> Result<Response<HandshakeResponse>, Status> {
        let req = request.into_inner();
        log::info!(
            "Client connected: {} (version {})",
            req.client_id,
            req.client_version
        );

        self.client_count.fetch_add(1, Ordering::Relaxed);

        let hostname = gethostname();

        let mut response = HandshakeResponse {
            daemon_id: self.daemon_id.clone(),
            daemon_version: env!("CARGO_PKG_VERSION").to_string(),
            is_local: true,
            hostname,
            protocol_version: cterm_proto::PROTOCOL_VERSION,
            daemon_identity: self.daemon_identity.clone(),
            daemon_auth_proof: Vec::new(),
        };
        if let Some(secret) = &self.daemon_auth_secret {
            if req.daemon_auth_challenge.len() != cterm_proto::DAEMON_AUTH_CHALLENGE_BYTES {
                return Err(Status::invalid_argument(
                    "managed daemon handshake requires a fresh 32-byte challenge",
                ));
            }
            response.daemon_auth_proof =
                cterm_proto::managed_daemon_auth_proof(secret, &req, &response);
        }

        Ok(Response::new(response))
    }

    async fn attach_session(
        &self,
        request: Request<AttachSessionRequest>,
    ) -> Result<Response<AttachSessionResponse>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        session.attach();

        // Resize to client dimensions if provided
        if req.cols > 0 && req.rows > 0 {
            session.resize(req.cols as usize, req.rows as usize);
        }

        let (cols, rows) = session.dimensions();
        let info = SessionInfo {
            session_id: session.id.clone(),
            cols: cols as u32,
            rows: rows as u32,
            title: session.title(),
            running: session.is_running(),
            child_pid: session.child_pid().unwrap_or(0),
            attached_clients: session.attached_clients(),
            custom_title: session.custom_title(),
            tab_color: session.tab_color(),
            template_name: session.template_name(),
            has_foreground_process: session.has_foreground_process(),
            foreground_process_name: session.foreground_process_name().unwrap_or_default(),
            alerted: session.is_alerted(),
        };

        let initial_screen = if req.want_screen_snapshot {
            Some(session.with_terminal(|term| screen_to_proto(term.screen(), true)))
        } else {
            None
        };

        Ok(Response::new(AttachSessionResponse {
            session: Some(info),
            initial_screen,
        }))
    }

    async fn detach_session(
        &self,
        request: Request<DetachSessionRequest>,
    ) -> Result<Response<DetachSessionResponse>, Status> {
        let req = request.into_inner();

        // Decrement attached count if session still exists
        if let Ok(session) = self.session_manager.get_session(&req.session_id) {
            session.detach();
        }

        if !req.keep_running {
            // Destroy the session
            self.session_manager
                .destroy_session(&req.session_id, None)
                .map_err(Status::from)?;
        }

        Ok(Response::new(DetachSessionResponse { success: true }))
    }

    // ========================================================================
    // Screen Update Streaming
    // ========================================================================

    type StreamScreenUpdatesStream =
        Pin<Box<dyn Stream<Item = Result<ScreenUpdate, Status>> + Send + 'static>>;

    async fn stream_screen_updates(
        &self,
        request: Request<StreamScreenUpdatesRequest>,
    ) -> Result<Response<Self::StreamScreenUpdatesStream>, Status> {
        let req = request.into_inner();
        let session = self
            .session_manager
            .get_session(&req.session_id)
            .map_err(Status::from)?;

        let session_id = req.session_id.clone();
        let rx = session.subscribe_events();
        let session_ref = session.clone();
        let mut seq: u64 = 0;
        let incremental = req.incremental;

        // For incremental mode, maintain per-subscriber cache of last-sent state.
        // Since both daemon and client run full terminal emulation, we only need
        // to send the rows that actually changed.
        let mut cached_rows: Vec<Row> = if incremental {
            session_ref.with_terminal(|term| visible_rows_to_proto(term.screen()))
        } else {
            Vec::new()
        };
        let mut cached_cursor: Option<CursorPosition> = if incremental {
            Some(session_ref.with_terminal(|term| cursor_to_proto(term.screen())))
        } else {
            None
        };
        let mut cached_modes: Option<TerminalModes> = if incremental {
            Some(session_ref.with_terminal(|term| modes_to_proto(term.screen())))
        } else {
            None
        };
        let mut cached_images: Vec<cterm_proto::proto::TerminalImage> = if incremental {
            session_ref.with_terminal(|term| terminal_images_to_proto(term.screen()))
        } else {
            Vec::new()
        };
        let mut cached_extra_cursors: Option<ExtraCursorsUpdate> = if incremental {
            Some(session_ref.with_terminal(|term| extra_cursors_to_proto(term.screen())))
        } else {
            None
        };
        // After a lag event, force a full screen resync
        let mut needs_full_resync = false;

        let stream = BroadcastStream::new(rx).filter_map(move |result| match result {
            Ok(event) => {
                if matches!(event, cterm_core::term::TerminalEvent::ContentChanged) {
                    seq += 1;

                    if !incremental || needs_full_resync {
                        // Non-incremental mode or resync after lag: send full screen
                        let screen_data =
                            session_ref.with_terminal(|term| screen_to_proto(term.screen(), false));

                        if incremental {
                            // Rebuild cache after resync
                            cached_rows = screen_data.visible_rows.clone();
                            cached_cursor = screen_data.cursor;
                            cached_modes = screen_data.modes.clone();
                            cached_images = screen_data.images.clone();
                            cached_extra_cursors = Some(ExtraCursorsUpdate {
                                cursors: screen_data.extra_cursors.clone(),
                                colors: screen_data.extra_cursor_colors,
                            });
                            needs_full_resync = false;
                        }

                        Some(Ok(ScreenUpdate {
                            session_id: session_id.clone(),
                            sequence: seq,
                            update_type: Some(screen_update::UpdateType::FullScreen(
                                FullScreenUpdate {
                                    screen: Some(screen_data),
                                },
                            )),
                        }))
                    } else {
                        // Incremental mode: diff current screen against cache
                        let (
                            dirty_rows,
                            new_rows,
                            cur_cursor,
                            cur_modes,
                            cur_images,
                            cur_extra_cursors,
                        ) = session_ref.with_terminal(|term| {
                            let screen = term.screen();
                            let current_rows = visible_rows_to_proto(screen);
                            let cursor = cursor_to_proto(screen);
                            let modes = modes_to_proto(screen);
                            let images = terminal_images_to_proto(screen);
                            let extra_cursors = extra_cursors_to_proto(screen);

                            // Find rows that changed
                            let mut dirty = Vec::new();
                            let height = current_rows.len();
                            let old_height = cached_rows.len();

                            for i in 0..height {
                                let changed = if i >= old_height {
                                    true // new row (screen grew)
                                } else {
                                    current_rows[i] != cached_rows[i]
                                };
                                if changed {
                                    dirty.push(DirtyRow {
                                        row_index: i as u32,
                                        cells: current_rows[i].cells.clone(),
                                        wrapped: current_rows[i].wrapped,
                                        shell_prompt: current_rows[i].shell_prompt,
                                        command_start: current_rows[i].command_start,
                                        command_end: current_rows[i].command_end,
                                    });
                                }
                            }

                            (dirty, current_rows, cursor, modes, images, extra_cursors)
                        });

                        // Check cursor and modes changes
                        let cursor_changed = cached_cursor.as_ref() != Some(&cur_cursor);
                        let modes_changed = cached_modes.as_ref() != Some(&cur_modes);
                        let images_changed = cached_images != cur_images;
                        let extra_cursors_changed =
                            cached_extra_cursors.as_ref() != Some(&cur_extra_cursors);

                        // Update cache
                        cached_rows = new_rows;

                        if dirty_rows.is_empty()
                            && !cursor_changed
                            && !modes_changed
                            && !images_changed
                            && !extra_cursors_changed
                        {
                            // Nothing actually changed (e.g. selection-only update)
                            return None;
                        }

                        // If most rows changed, send full screen instead
                        let height = cached_rows.len();
                        if dirty_rows.len() > height * 3 / 4 {
                            cached_cursor = Some(cur_cursor);
                            cached_modes = Some(cur_modes);
                            cached_images = cur_images.clone();
                            cached_extra_cursors = Some(cur_extra_cursors.clone());
                            let drcs_fonts = session_ref.with_terminal(|term| {
                                cterm_proto::convert::screen::drcs_fonts_to_proto(term.screen())
                            });
                            let screen_data = FullScreenUpdate {
                                screen: Some(GetScreenResponse {
                                    cols: if height > 0 {
                                        cached_rows[0].cells.len() as u32
                                    } else {
                                        0
                                    },
                                    rows: height as u32,
                                    cursor: cached_cursor,
                                    visible_rows: cached_rows.clone(),
                                    scrollback: Vec::new(),
                                    title: session_ref.title(),
                                    modes: cached_modes.clone(),
                                    drcs_fonts,
                                    images: cur_images,
                                    extra_cursors: cur_extra_cursors.cursors,
                                    extra_cursor_colors: cur_extra_cursors.colors,
                                }),
                            };
                            return Some(Ok(ScreenUpdate {
                                session_id: session_id.clone(),
                                sequence: seq,
                                update_type: Some(screen_update::UpdateType::FullScreen(
                                    screen_data,
                                )),
                            }));
                        }

                        // Send dirty rows with optional cursor/modes
                        let cursor_update = if cursor_changed {
                            cached_cursor = Some(cur_cursor);
                            Some(cur_cursor)
                        } else {
                            None
                        };
                        let modes_update = if modes_changed {
                            cached_modes = Some(cur_modes.clone());
                            Some(cur_modes)
                        } else {
                            None
                        };
                        let images_update = if images_changed {
                            cached_images = cur_images.clone();
                            Some(TerminalImages { images: cur_images })
                        } else {
                            None
                        };
                        let extra_cursors_update = if extra_cursors_changed {
                            cached_extra_cursors = Some(cur_extra_cursors.clone());
                            Some(cur_extra_cursors)
                        } else {
                            None
                        };

                        Some(Ok(ScreenUpdate {
                            session_id: session_id.clone(),
                            sequence: seq,
                            update_type: Some(screen_update::UpdateType::DirtyRows(
                                DirtyRowsUpdate {
                                    rows: dirty_rows,
                                    cursor: cursor_update,
                                    modes: modes_update,
                                    images: images_update,
                                    extra_cursors: extra_cursors_update,
                                },
                            )),
                        }))
                    }
                } else if matches!(event, cterm_core::term::TerminalEvent::TitleChanged(_)) {
                    seq += 1;
                    let title = session_ref.title();
                    Some(Ok(ScreenUpdate {
                        session_id: session_id.clone(),
                        sequence: seq,
                        update_type: Some(screen_update::UpdateType::Title(TitleUpdate { title })),
                    }))
                } else {
                    None
                }
            }
            Err(BroadcastStreamRecvError::Lagged(count)) => {
                log::warn!(
                    "stream_screen_updates: client lagged, dropped {} events for session {}",
                    count,
                    session_id,
                );
                if incremental {
                    // Force full resync on next event to ensure client state is correct
                    needs_full_resync = true;
                }
                None
            }
        });

        Ok(Response::new(Box::pin(stream)))
    }

    // ========================================================================
    // Daemon Management
    // ========================================================================

    async fn get_daemon_info(
        &self,
        _request: Request<GetDaemonInfoRequest>,
    ) -> Result<Response<GetDaemonInfoResponse>, Status> {
        let hostname = gethostname();

        Ok(Response::new(GetDaemonInfoResponse {
            daemon_id: self.daemon_id.clone(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            hostname,
            session_count: self.session_manager.session_count() as u32,
            client_count: self.client_count.load(Ordering::Relaxed),
            uptime_secs: self.start_time.elapsed().as_secs(),
        }))
    }

    async fn shutdown(
        &self,
        request: Request<ShutdownRequest>,
    ) -> Result<Response<ShutdownResponse>, Status> {
        let req = request.into_inner();

        if !req.force && self.session_manager.session_count() > 0 {
            return Ok(Response::new(ShutdownResponse {
                success: false,
                reason: "Active sessions exist. Use force=true to override.".to_string(),
            }));
        }

        log::info!("Shutdown requested (force={})", req.force);

        // If force=true and sessions exist, destroy them all first
        if req.force {
            let sessions = self.session_manager.list_sessions();
            for session in &sessions {
                if let Err(e) = self.session_manager.destroy_session(&session.id, None) {
                    log::warn!(
                        "Failed to destroy session {} during shutdown: {}",
                        session.id,
                        e
                    );
                }
            }
        }

        // Trigger actual server shutdown
        self.shutdown_notify.notify_one();

        Ok(Response::new(ShutdownResponse {
            success: true,
            reason: String::new(),
        }))
    }

    async fn relaunch_daemon(
        &self,
        request: Request<RelaunchDaemonRequest>,
    ) -> Result<Response<RelaunchDaemonResponse>, Status> {
        if self.daemon_auth_secret.is_some() {
            return Err(Status::failed_precondition(
                "daemon relaunch is disabled in managed mode",
            ));
        }

        #[cfg(not(unix))]
        {
            let _ = request;
            return Ok(Response::new(RelaunchDaemonResponse {
                success: false,
                reason: "Relaunch is only supported on Unix".to_string(),
            }));
        }

        #[cfg(unix)]
        {
            let req = request.into_inner();
            let binary_path = if req.binary_path.is_empty() {
                None
            } else {
                Some(req.binary_path.as_str())
            };

            log::info!(
                "Relaunch requested (binary: {})",
                binary_path.unwrap_or("<current>")
            );

            // perform_relaunch calls exec() and does not return on success
            match crate::relaunch::perform_relaunch(
                &self.session_manager,
                &self.socket_path,
                &self.daemon_identity,
                self.scrollback_lines,
                binary_path,
            ) {
                Ok(()) => {
                    // Should not reach here — exec replaces the process
                    unreachable!("exec should not return on success");
                }
                Err(e) => {
                    log::error!("Relaunch failed: {}", e);
                    Ok(Response::new(RelaunchDaemonResponse {
                        success: false,
                        reason: e,
                    }))
                }
            }
        }
    }
}

/// A stream wrapper that calls a callback when dropped (i.e. when the client disconnects).
struct StreamNotify<F: FnOnce()> {
    inner: Pin<Box<dyn Stream<Item = Result<OutputChunk, Status>> + Send>>,
    on_drop: Option<F>,
}

impl<F: FnOnce()> StreamNotify<F> {
    fn new<S>(inner: S, on_drop: F) -> Self
    where
        S: Stream<Item = Result<OutputChunk, Status>> + Send + 'static,
    {
        Self {
            inner: Box::pin(inner),
            on_drop: Some(on_drop),
        }
    }
}

impl<F: FnOnce()> Drop for StreamNotify<F> {
    fn drop(&mut self) {
        if let Some(f) = self.on_drop.take() {
            f();
        }
    }
}

// SAFETY: Both fields are Unpin — Pin<Box<...>> is always Unpin, and Option<F> is Unpin.
impl<F: FnOnce()> Unpin for StreamNotify<F> {}

impl<F: FnOnce()> Stream for StreamNotify<F> {
    type Item = Result<OutputChunk, Status>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

fn gethostname() -> String {
    #[cfg(unix)]
    {
        let mut buf = [0u8; 256];
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut _, buf.len()) } == 0 {
            let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
            String::from_utf8_lossy(&buf[..len]).to_string()
        } else {
            "unknown".to_string()
        }
    }
    #[cfg(not(unix))]
    {
        "unknown".to_string()
    }
}
