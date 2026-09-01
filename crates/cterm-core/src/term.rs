//! Terminal - Main terminal state combining screen and parser
//!
//! Provides a high-level interface for terminal emulation.

use crate::color::ColorPalette;
use crate::dnd::DndCommand;
use crate::kitty_file_transfer::FileTransferCommand;
use crate::kitty_graphics::GraphicsAnimationTick;
use crate::parser::Parser;
use crate::pty::{Pty, PtyConfig, PtyError, PtySize};
use crate::screen::{
    ClipboardOperation, ColorQuery, DesktopNotificationAction, FrontendState, Screen, ScreenConfig,
    SearchResult,
};
use crate::{KeyEventKind, KeyEventMetadata, KeyboardEnhancementFlags};
use std::time::{Duration, Instant};

const APPLICATION_SYNC_UPDATE_TIMEOUT: Duration = Duration::from_secs(1);
// Far fewer than 256 syntactically valid OSC 5113 commands fit in this many
// bytes. Draining between chunks therefore preserves the Screen queue's hard
// bound without silently losing a burst from one large PTY read.
const FILE_TRANSFER_DRAIN_CHUNK_BYTES: usize = 1024;

/// Events emitted by the terminal
#[derive(Debug, Clone)]
pub enum TerminalEvent {
    /// Terminal title changed
    TitleChanged(String),
    /// Bell was rung
    Bell,
    /// Process exited with code
    ProcessExited(u32),
    /// Terminal content changed (needs redraw)
    ContentChanged,
    /// Clipboard operation requested (OSC 52)
    ClipboardRequest(ClipboardOperation),
    /// Terminal application requested a native desktop notification.
    DesktopNotification(DesktopNotificationAction),
    /// Terminal application changed a native Kitty OSC 72 drag session.
    DndCommand(DndCommand),
}

/// Terminal configuration
#[derive(Debug, Clone, Default)]
pub struct TerminalConfig {
    /// Screen configuration
    pub screen: ScreenConfig,
    /// PTY configuration
    pub pty: PtyConfig,
}

/// Callback for writing data when no PTY is present (e.g., daemon mode)
pub type WriteFn = Box<dyn Fn(&[u8]) -> Result<(), PtyError> + Send + Sync>;

/// Terminal instance managing screen, parser, and PTY
pub struct Terminal {
    screen: Screen,
    parser: Parser,
    pty: Option<Pty>,
    write_fn: Option<WriteFn>,
    last_title: String,
    synchronized_update_deadline: Option<Instant>,
    content_change_pending: bool,
}

impl Terminal {
    /// Create a new terminal with the given dimensions
    pub fn new(cols: usize, rows: usize, config: ScreenConfig) -> Self {
        Self {
            screen: Screen::new(cols, rows, config),
            parser: Parser::new(),
            pty: None,
            write_fn: None,
            last_title: String::new(),
            synchronized_update_deadline: None,
            content_change_pending: false,
        }
    }

    /// Create a terminal and spawn a shell
    pub fn with_shell(
        cols: usize,
        rows: usize,
        screen_config: ScreenConfig,
        pty_config: &PtyConfig,
    ) -> Result<Self, PtyError> {
        let cols = cols.clamp(1, u16::MAX as usize);
        let rows = rows.clamp(1, u16::MAX as usize);
        let mut config = pty_config.clone();
        config.size.cols = cols as u16;
        config.size.rows = rows as u16;
        config.size = config.size.normalized();

        let pty = Pty::new(&config)?;

        let mut screen = Screen::new(cols, rows, screen_config);
        screen.set_cell_width_hint(config.size.cell_width());
        screen.set_cell_height_hint(config.size.cell_height());

        Ok(Self {
            screen,
            parser: Parser::new(),
            pty: Some(pty),
            write_fn: None,
            last_title: String::new(),
            synchronized_update_deadline: None,
            content_change_pending: false,
        })
    }

    /// Get a reference to the screen
    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    /// Get a mutable reference to the screen
    pub fn screen_mut(&mut self) -> &mut Screen {
        &mut self.screen
    }

    /// Advance terminal-driven Kitty image animation independently of PTY I/O.
    pub fn advance_graphics_animations(&mut self, now: Duration) -> GraphicsAnimationTick {
        self.parser
            .advance_graphics_animations(&mut self.screen, now)
    }

    /// Get the PTY if available
    pub fn pty(&self) -> Option<&Pty> {
        self.pty.as_ref()
    }

    /// Get a mutable reference to the PTY if available
    pub fn pty_mut(&mut self) -> Option<&mut Pty> {
        self.pty.as_mut()
    }

    /// Set the PTY for this terminal
    pub fn set_pty(&mut self, pty: Pty) {
        self.pty = Some(pty);
    }

    /// Set a write callback for daemon/remote mode (used instead of PTY)
    pub fn set_write_fn(&mut self, write_fn: WriteFn) {
        self.write_fn = Some(write_fn);
    }

    /// Take the PTY out of the terminal, returning it if present.
    ///
    /// This is used when closing a tab to ensure the PTY is dropped promptly,
    /// which closes the master FD and unblocks any background read threads.
    pub fn take_pty(&mut self) -> Option<Pty> {
        self.pty.take()
    }

    /// Process input from the PTY and update the screen
    pub fn process(&mut self, data: &[u8]) -> Vec<TerminalEvent> {
        let (events, responses) = self.process_collecting(data);

        // Send any pending responses back to the PTY inline. This is the in-process
        // path (GUI); the daemon uses `process_collecting` and writes responses through
        // a dedicated off-thread writer so a blocking PTY write can't stall it.
        for response in responses {
            if let Err(e) = self.write(&response) {
                log::error!("Failed to send response to PTY: {}", e);
            }
        }

        events
    }

    /// Parse output mirrored from a daemon-owned PTY without sending terminal
    /// query replies a second time. The daemon is the authoritative responder.
    pub fn process_mirror(&mut self, data: &[u8]) -> Vec<TerminalEvent> {
        let (events, _responses) = self.process_collecting(data);
        // ctermd is the sole OSC 5113 authority. Native mirrors must not retain
        // a second queue of raw commands that they are forbidden to execute.
        self.screen.take_kitty_file_transfer_commands();
        events
    }

    /// Parse input and update the screen, RETURNING any pending PTY responses
    /// (e.g. DSR/DA/cursor reports) instead of writing them back to the PTY.
    ///
    /// Daemon mode uses this so the (potentially blocking) PTY write happens off the
    /// async worker thread and outside the terminal lock. Callers MUST write the
    /// returned responses back to the PTY to keep terminal queries functioning.
    pub fn process_collecting(&mut self, data: &[u8]) -> (Vec<TerminalEvent>, Vec<Vec<u8>>) {
        let (events, responses, _) = self.process_collecting_inner(data, false);
        (events, responses)
    }

    /// Parse daemon-owned PTY output and losslessly drain validated OSC 5113
    /// commands alongside ordinary terminal events and query responses.
    pub fn process_collecting_with_file_transfers(
        &mut self,
        data: &[u8],
    ) -> (Vec<TerminalEvent>, Vec<Vec<u8>>, Vec<FileTransferCommand>) {
        self.process_collecting_inner(data, true)
    }

    fn process_collecting_inner(
        &mut self,
        data: &[u8],
        drain_file_transfers: bool,
    ) -> (Vec<TerminalEvent>, Vec<Vec<u8>>, Vec<FileTransferCommand>) {
        let mut events = Vec::new();
        let mut file_transfers = if drain_file_transfers {
            self.screen.take_kitty_file_transfer_commands()
        } else {
            Vec::new()
        };

        let sync_generation = self.screen.sync_update_generation();
        if drain_file_transfers {
            for chunk in data.chunks(FILE_TRANSFER_DRAIN_CHUNK_BYTES) {
                self.parser.parse(&mut self.screen, chunk);
                file_transfers.extend(self.screen.take_kitty_file_transfer_commands());
            }
        } else {
            self.parser.parse(&mut self.screen, data);
        }

        if self.screen.modes.application_sync_updates {
            if self.screen.sync_update_generation() != sync_generation
                || self.synchronized_update_deadline.is_none()
            {
                self.synchronized_update_deadline =
                    Some(Instant::now() + APPLICATION_SYNC_UPDATE_TIMEOUT);
            }
        } else {
            self.synchronized_update_deadline = None;
        }

        // Collect any pending responses for the caller to write back to the PTY
        let mut responses = if self.screen.has_pending_responses() {
            self.screen.take_pending_responses()
        } else {
            Vec::new()
        };

        // Emit clipboard operation events
        if self.screen.has_clipboard_ops() {
            for op in self.screen.take_clipboard_ops() {
                events.push(TerminalEvent::ClipboardRequest(op));
            }
        }

        if self.screen.has_notifications() {
            for notification in self.screen.take_notifications() {
                events.push(TerminalEvent::DesktopNotification(notification));
            }
        }

        if self.screen.has_dnd_commands() {
            for command in self.screen.take_dnd_commands() {
                events.push(TerminalEvent::DndCommand(command));
            }
        }

        if self.screen.has_color_queries() {
            for (target, dynamic_color) in self.screen.take_color_queries() {
                let color = dynamic_color.unwrap_or_else(|| self.screen.base_query_color(target));
                responses.push(Self::color_query_response(target, color));
            }
        }

        // Check for bell
        if self.screen.bell {
            self.screen.bell = false;
            events.push(TerminalEvent::Bell);
        }

        // Check for title change
        if self.screen.title != self.last_title {
            self.last_title = self.screen.title.clone();
            events.push(TerminalEvent::TitleChanged(self.last_title.clone()));
        }

        // Coalesce rendering while an application synchronized update is in
        // progress. Other events and protocol replies are never delayed.
        if !data.is_empty() {
            self.content_change_pending = true;
        }
        let content_changed = if self.screen.modes.application_sync_updates {
            self.expire_synchronized_update_at(Instant::now())
        } else {
            std::mem::take(&mut self.content_change_pending)
        };
        if content_changed {
            events.push(TerminalEvent::ContentChanged);
        }

        (events, responses, file_transfers)
    }

    fn color_query_response(target: ColorQuery, color: crate::color::Rgb) -> Vec<u8> {
        match target {
            ColorQuery::Palette(index) => {
                format!("\x1b]4;{index};{}\x1b\\", color.to_osc_spec()).into_bytes()
            }
            _ => format!("\x1b]{};{}\x1b\\", target.osc_code(), color.to_osc_spec()).into_bytes(),
        }
    }

    /// Set the frontend theme used to answer OSC 10-12 color queries.
    pub fn set_base_palette(&mut self, palette: ColorPalette) {
        self.screen.set_base_palette(palette);
    }

    /// Update state owned by an in-process native frontend and send any
    /// application-requested change reports through this terminal's PTY.
    pub fn set_frontend_state(&mut self, state: FrontendState) {
        for response in self.set_frontend_state_collecting(state) {
            if let Err(error) = self.write(&response) {
                log::error!("Failed to send frontend state report to PTY: {error}");
            }
        }
    }

    /// Update frontend-owned state while returning reports for a daemon-owned
    /// PTY, keeping potentially blocking writes outside the terminal lock.
    pub fn set_frontend_state_collecting(&mut self, state: FrontendState) -> Vec<Vec<u8>> {
        self.screen.set_theme_appearance(state.appearance);
        self.screen.set_window_visibility(state.visibility);
        self.screen.take_pending_responses()
    }

    /// Deadline for the active application synchronized update, if any.
    /// Frontends use this to drive the fail-safe even when PTY output stops.
    pub fn synchronized_update_deadline(&self) -> Option<Instant> {
        self.screen
            .modes
            .application_sync_updates
            .then_some(self.synchronized_update_deadline)
            .flatten()
    }

    /// End an application synchronized update once its one-second fail-safe
    /// expires. Returns true when deferred screen damage should be rendered.
    pub fn expire_synchronized_update(&mut self) -> bool {
        self.expire_synchronized_update_at(Instant::now())
    }

    fn expire_synchronized_update_at(&mut self, now: Instant) -> bool {
        if !self.screen.modes.application_sync_updates
            || self
                .synchronized_update_deadline
                .is_none_or(|deadline| now < deadline)
        {
            return false;
        }

        self.screen.set_application_sync_updates(false);
        self.synchronized_update_deadline = None;
        std::mem::take(&mut self.content_change_pending)
    }

    /// Write input to the PTY (keyboard input)
    pub fn write(&mut self, data: &[u8]) -> Result<(), PtyError> {
        if let Some(ref mut pty) = self.pty {
            pty.write(data)?;
        } else if let Some(ref write_fn) = self.write_fn {
            write_fn(data)?;
        }
        Ok(())
    }

    /// Write a string to the PTY
    pub fn write_str(&mut self, s: &str) -> Result<(), PtyError> {
        self.write(s.as_bytes())
    }

    /// Send clipboard data as OSC 52 response
    pub fn send_clipboard_response(
        &mut self,
        selection: crate::screen::ClipboardSelection,
        data: &[u8],
    ) -> Result<(), PtyError> {
        use crate::screen::ClipboardSelection;
        use base64::Engine;

        let selection_char = match selection {
            ClipboardSelection::Clipboard => 'c',
            ClipboardSelection::Primary => 'p',
            ClipboardSelection::Select => 's',
        };

        let encoded = base64::engine::general_purpose::STANDARD.encode(data);
        let response = format!("\x1b]52;{};{}\x07", selection_char, encoded);
        self.write(response.as_bytes())
    }

    /// Resize the terminal
    pub fn resize(&mut self, cols: usize, rows: usize) {
        let pixel_width = (self.screen.cell_width_hint() * cols as f64)
            .round()
            .clamp(1.0, u16::MAX as f64) as u16;
        let pixel_height = (self.screen.cell_height_hint() * rows as f64)
            .round()
            .clamp(1.0, u16::MAX as f64) as u16;
        self.resize_with_pixels(cols, rows, pixel_width, pixel_height);
    }

    /// Resize the screen and PTY with total pixel dimensions supplied by the UI.
    pub fn resize_with_pixels(
        &mut self,
        cols: usize,
        rows: usize,
        pixel_width: u16,
        pixel_height: u16,
    ) {
        let cols = cols.clamp(1, u16::MAX as usize);
        let rows = rows.clamp(1, u16::MAX as usize);
        // Older callers only know about rows and columns. Preserve the current
        // measured cell geometry for those callers instead of reverting to the
        // conservative creation-time default on every resize.
        let pixel_width = if pixel_width == 0 {
            (self.screen.cell_width_hint() * cols as f64)
                .round()
                .clamp(1.0, u16::MAX as f64) as u16
        } else {
            pixel_width
        };
        let pixel_height = if pixel_height == 0 {
            (self.screen.cell_height_hint() * rows as f64)
                .round()
                .clamp(1.0, u16::MAX as f64) as u16
        } else {
            pixel_height
        };
        let size = PtySize {
            cols: cols as u16,
            rows: rows as u16,
            pixel_width,
            pixel_height,
        }
        .normalized();
        self.screen.resize(cols, rows);
        self.screen.set_cell_width_hint(size.cell_width());
        self.screen.set_cell_height_hint(size.cell_height());
        self.parser.refresh_graphics_viewport(&mut self.screen);
        if let Some(ref pty) = self.pty {
            let _ = pty.resize_with_size(size);
        }
    }

    /// Check if the process is still running
    pub fn is_running(&mut self) -> bool {
        if let Some(ref mut pty) = self.pty {
            return pty.is_running();
        }
        false
    }

    /// Send a signal to the child process
    pub fn send_signal(&self, signal: i32) -> Result<(), PtyError> {
        if let Some(ref pty) = self.pty {
            return pty.send_signal(signal).map_err(PtyError::Io);
        }
        Err(PtyError::NotRunning)
    }

    /// Get an independent blocking reader for the PTY output.
    pub fn pty_reader(&self) -> Option<Box<dyn std::io::Read + Send>> {
        self.pty.as_ref().and_then(|p| p.try_clone_reader().ok())
    }

    /// Get a cloned writer for the PTY master (local PTYs only).
    ///
    /// A duplicated local PTY master fd is bidirectional, so this returns a `File`
    /// suitable for writing input/responses back to the child. The daemon uses this to
    /// drive a dedicated writer thread, keeping blocking PTY writes off the async
    /// worker pool. Returns `None` for SSH-backed terminals (no local fd).
    pub fn pty_writer(&self) -> Option<std::fs::File> {
        self.pty.as_ref().and_then(|p| p.try_clone_writer())
    }

    /// Get the child process ID
    pub fn child_pid(&self) -> Option<i32> {
        self.pty.as_ref().map(|p| p.child_pid())
    }

    /// Check if there's a foreground process running (other than the shell)
    #[cfg(unix)]
    pub fn has_foreground_process(&self) -> bool {
        self.pty
            .as_ref()
            .map(|p| p.has_foreground_process())
            .unwrap_or(false)
    }

    /// Get the name of the foreground process (if any)
    #[cfg(unix)]
    pub fn foreground_process_name(&self) -> Option<String> {
        self.pty.as_ref().and_then(|p| p.foreground_process_name())
    }

    /// Get the shell-reported working directory, falling back to Unix process inspection.
    pub fn foreground_cwd(&self) -> Option<std::path::PathBuf> {
        if let Some(path) = self.screen.current_working_directory() {
            return Some(path.to_path_buf());
        }

        #[cfg(unix)]
        return self.pty.as_ref().and_then(Pty::foreground_cwd);

        #[cfg(not(unix))]
        None
    }

    /// Get terminal width
    pub fn cols(&self) -> usize {
        self.screen.width()
    }

    /// Get terminal height
    pub fn rows(&self) -> usize {
        self.screen.height()
    }

    /// Get current title
    pub fn title(&self) -> &str {
        &self.screen.title
    }

    /// Scroll viewport up (into scrollback)
    pub fn scroll_viewport_up(&mut self, lines: usize) {
        let max_offset = self.screen.scrollback().len();
        self.screen.scroll_offset = (self.screen.scroll_offset + lines).min(max_offset);
        self.parser.refresh_graphics_viewport(&mut self.screen);
    }

    /// Scroll viewport down (towards bottom)
    pub fn scroll_viewport_down(&mut self, lines: usize) {
        self.screen.scroll_offset = self.screen.scroll_offset.saturating_sub(lines);
        self.parser.refresh_graphics_viewport(&mut self.screen);
    }

    /// Reset viewport to bottom
    pub fn scroll_viewport_to_bottom(&mut self) {
        self.screen.scroll_offset = 0;
        self.parser.refresh_graphics_viewport(&mut self.screen);
    }

    /// Scroll to the closest shell prompt above the viewport.
    pub fn scroll_to_previous_prompt(&mut self) -> bool {
        let changed = self.screen.scroll_to_previous_prompt();
        if changed {
            self.parser.refresh_graphics_viewport(&mut self.screen);
        }
        changed
    }

    /// Scroll to the closest shell prompt below the viewport.
    pub fn scroll_to_next_prompt(&mut self) -> bool {
        let changed = self.screen.scroll_to_next_prompt();
        if changed {
            self.parser.refresh_graphics_viewport(&mut self.screen);
        }
        changed
    }

    /// Return the output delimited by the latest OSC 133 C/D pair.
    pub fn last_command_output(&self) -> Option<String> {
        self.screen.last_command_output()
    }

    /// Check if viewport is at bottom
    pub fn is_at_bottom(&self) -> bool {
        self.screen.scroll_offset == 0
    }

    /// Search for text in scrollback and visible buffer
    pub fn find(&self, pattern: &str, case_sensitive: bool, regex: bool) -> Vec<SearchResult> {
        self.screen.find(pattern, case_sensitive, regex)
    }

    /// Scroll to show a specific line from find results
    pub fn scroll_to_line(&mut self, line_idx: usize) {
        self.screen.scroll_offset = self.screen.line_to_scroll_offset(line_idx);
        self.parser.refresh_graphics_viewport(&mut self.screen);
    }

    /// Handle keyboard input and generate appropriate escape sequences
    pub fn handle_key(&self, key: Key, modifiers: Modifiers) -> Option<Vec<u8>> {
        self.handle_key_event(key, modifiers, KeyEventKind::Press)
    }

    /// Handle a physical key event, honoring the active kitty keyboard mode.
    pub fn handle_key_event(
        &self,
        key: Key,
        modifiers: Modifiers,
        kind: KeyEventKind,
    ) -> Option<Vec<u8>> {
        self.handle_key_event_with_metadata(key, modifiers, kind, KeyEventMetadata::default())
    }

    /// Handle a physical key event together with native layout and text data.
    pub fn handle_key_event_with_metadata(
        &self,
        key: Key,
        modifiers: Modifiers,
        kind: KeyEventKind,
        metadata: KeyEventMetadata<'_>,
    ) -> Option<Vec<u8>> {
        let flags = self.screen.keyboard_enhancement_flags();
        let report_events = flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES);
        let report_all = flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
        let disambiguate =
            report_all || flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES);

        if kind == KeyEventKind::Release && !report_events {
            return None;
        }

        match key {
            Key::Char(c) => {
                let needs_escape = report_all
                    || modifiers.intersects(
                        Modifiers::CTRL
                            | Modifiers::ALT
                            | Modifiers::SUPER
                            | Modifiers::HYPER
                            | Modifiers::META,
                    );
                if disambiguate && needs_escape {
                    Some(csi_u_key(c as u32, modifiers, kind, flags, metadata))
                } else if kind == KeyEventKind::Release {
                    None
                } else if let Some(sequence) =
                    modify_other_key(c, modifiers, self.screen.modes.modify_other_keys)
                {
                    Some(sequence)
                } else {
                    self.handle_legacy_key(key, modifiers)
                }
            }
            Key::Enter | Key::Tab | Key::Backspace => {
                if report_all {
                    let codepoint = match key {
                        Key::Enter => 13,
                        Key::Tab => 9,
                        Key::Backspace => 127,
                        _ => unreachable!(),
                    };
                    Some(csi_u_key(codepoint, modifiers, kind, flags, metadata))
                } else if kind == KeyEventKind::Release {
                    None
                } else {
                    self.handle_legacy_key(key, modifiers)
                }
            }
            Key::Escape => {
                if disambiguate || report_events {
                    Some(csi_u_key(27, modifiers, kind, flags, metadata))
                } else if kind == KeyEventKind::Release {
                    None
                } else {
                    self.handle_legacy_key(key, modifiers)
                }
            }
            Key::Up => enhanced_cursor_key(
                b'A',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Down => enhanced_cursor_key(
                b'B',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Right => enhanced_cursor_key(
                b'C',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Left => enhanced_cursor_key(
                b'D',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Home => enhanced_cursor_key(
                b'H',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::End => enhanced_cursor_key(
                b'F',
                modifiers,
                kind,
                disambiguate || report_events,
                report_events,
            )
            .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::PageUp => enhanced_tilde_key(5, modifiers, kind, report_events)
                .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::PageDown => enhanced_tilde_key(6, modifiers, kind, report_events)
                .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Insert => enhanced_tilde_key(2, modifiers, kind, report_events)
                .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::Delete => enhanced_tilde_key(3, modifiers, kind, report_events)
                .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::F(n @ 13..=35) => Some(csi_u_key(
                57363 + u32::from(n),
                modifiers,
                kind,
                flags,
                metadata,
            )),
            Key::F(n) => enhanced_function_key(n, modifiers, kind, report_events)
                .or_else(|| self.handle_non_release_legacy(key, modifiers, kind)),
            Key::NumpadDigit(_)
            | Key::NumpadDecimal
            | Key::NumpadDivide
            | Key::NumpadMultiply
            | Key::NumpadSubtract
            | Key::NumpadAdd
            | Key::NumpadEnter => {
                if disambiguate || report_events {
                    kitty_numpad_code(key)
                        .map(|code| csi_u_key(code, modifiers, kind, flags, metadata))
                } else {
                    self.handle_non_release_legacy(key, modifiers, kind)
                }
            }
            Key::Named(named) => {
                if named.is_modifier() && !report_all {
                    None
                } else {
                    Some(csi_u_key(
                        named.kitty_code(),
                        modifiers,
                        kind,
                        flags,
                        metadata,
                    ))
                }
            }
        }
    }

    /// Encode text delivered without a reliable physical-key identity.
    ///
    /// IME and dead-key APIs can produce such commits. In all-key mode the
    /// physical event has already been reported, so raw text is suppressed;
    /// associated-text mode uses Kitty key number zero for the commit.
    pub fn handle_text_input(&self, text: &str) -> Vec<u8> {
        let flags = self.screen.keyboard_enhancement_flags();
        if !flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES) {
            return text.as_bytes().to_vec();
        }
        if !flags.contains(KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT) {
            return Vec::new();
        }

        encode_pure_text_event(text).unwrap_or_default()
    }

    /// Encode the release paired with a key press that the UI already sent as
    /// an enhanced event. Release-time modifiers are authoritative, but a
    /// character remains a CSI-u event even if Ctrl/Alt was released first.
    pub fn handle_reported_key_release(&self, key: Key, modifiers: Modifiers) -> Option<Vec<u8>> {
        self.handle_reported_key_release_with_metadata(key, modifiers, KeyEventMetadata::default())
    }

    /// Encode a release paired with a directly reported key press, retaining
    /// any stable alternate layout identity captured on key-down.
    pub fn handle_reported_key_release_with_metadata(
        &self,
        key: Key,
        modifiers: Modifiers,
        metadata: KeyEventMetadata<'_>,
    ) -> Option<Vec<u8>> {
        let flags = self.screen.keyboard_enhancement_flags();
        if !flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES) {
            return None;
        }

        if let Key::Char(c) = key {
            let disambiguate = flags.intersects(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
            );
            if disambiguate {
                return Some(csi_u_key(
                    c as u32,
                    modifiers,
                    KeyEventKind::Release,
                    flags,
                    metadata,
                ));
            }
        }

        self.handle_key_event_with_metadata(key, modifiers, KeyEventKind::Release, metadata)
    }

    fn handle_non_release_legacy(
        &self,
        key: Key,
        modifiers: Modifiers,
        kind: KeyEventKind,
    ) -> Option<Vec<u8>> {
        if kind == KeyEventKind::Release {
            None
        } else {
            self.handle_legacy_key(key, modifiers)
        }
    }

    fn handle_legacy_key(&self, key: Key, modifiers: Modifiers) -> Option<Vec<u8>> {
        let app_cursor = self.screen.modes.application_cursor;
        let app_keypad = self.screen.modes.application_keypad;

        match key {
            Key::Char(c) => {
                if modifiers.contains(Modifiers::CTRL) {
                    // Control characters
                    legacy_control_char(c).map(|byte| vec![byte])
                } else if modifiers.contains(Modifiers::ALT) {
                    // Alt + char = Escape + char
                    let mut buf = String::from('\x1b');
                    buf.push(c);
                    Some(buf.into_bytes())
                } else {
                    // Regular character
                    let mut buf = [0u8; 4];
                    let s = c.encode_utf8(&mut buf);
                    Some(s.as_bytes().to_vec())
                }
            }
            Key::Enter => {
                if modifiers.contains(Modifiers::ALT) {
                    // Alt+Enter
                    Some(b"\x1b\r".to_vec())
                } else {
                    Some(b"\r".to_vec())
                }
            }
            Key::Tab => {
                if modifiers.contains(Modifiers::SHIFT) {
                    // Shift+Tab sends CSI Z (backtab)
                    Some(b"\x1b[Z".to_vec())
                } else {
                    Some(b"\t".to_vec())
                }
            }
            Key::Backspace => {
                if modifiers.contains(Modifiers::ALT) {
                    // Alt+Backspace
                    Some(b"\x1b\x7f".to_vec())
                } else if modifiers.contains(Modifiers::CTRL) {
                    // Ctrl+Backspace - send Ctrl+W (delete word) or \x08
                    Some(b"\x08".to_vec())
                } else {
                    Some(b"\x7f".to_vec())
                }
            }
            Key::Escape => Some(b"\x1b".to_vec()),
            Key::Up => Some(cursor_key(b'A', modifiers, app_cursor)),
            Key::Down => Some(cursor_key(b'B', modifiers, app_cursor)),
            Key::Right => Some(cursor_key(b'C', modifiers, app_cursor)),
            Key::Left => Some(cursor_key(b'D', modifiers, app_cursor)),
            Key::Home => Some(cursor_key(b'H', modifiers, app_cursor)),
            Key::End => Some(cursor_key(b'F', modifiers, app_cursor)),
            Key::PageUp => Some(tilde_key(5, modifiers)),
            Key::PageDown => Some(tilde_key(6, modifiers)),
            Key::Insert => Some(tilde_key(2, modifiers)),
            Key::Delete => Some(tilde_key(3, modifiers)),
            Key::F(n) => {
                let sequence = function_key(n, modifiers);
                (!sequence.is_empty()).then_some(sequence)
            }
            Key::NumpadDigit(digit) if digit <= 9 => {
                let application_suffix = b'p' + digit;
                let normal = b'0' + digit;
                Some(keypad_key(
                    application_suffix,
                    normal,
                    modifiers,
                    app_keypad,
                ))
            }
            Key::NumpadDecimal => Some(keypad_key(b'n', b'.', modifiers, app_keypad)),
            Key::NumpadDivide => Some(keypad_key(b'o', b'/', modifiers, app_keypad)),
            Key::NumpadMultiply => Some(keypad_key(b'j', b'*', modifiers, app_keypad)),
            Key::NumpadSubtract => Some(keypad_key(b'm', b'-', modifiers, app_keypad)),
            Key::NumpadAdd => Some(keypad_key(b'k', b'+', modifiers, app_keypad)),
            Key::NumpadEnter => Some(keypad_key(b'M', b'\r', modifiers, app_keypad)),
            Key::NumpadDigit(_) => None,
            Key::Named(named) => Some(csi_u_key(
                named.kitty_code(),
                modifiers,
                KeyEventKind::Press,
                KeyboardEnhancementFlags::empty(),
                KeyEventMetadata::default(),
            )),
        }
    }
}

/// Encode a numeric-keypad key.  In application mode this follows the VT220
/// SS3 keypad table, including foot/xterm's modifier parameter spelling.
fn keypad_key(
    application_suffix: u8,
    normal: u8,
    modifiers: Modifiers,
    application: bool,
) -> Vec<u8> {
    if application {
        let modifier = modifier_param(modifiers);
        if modifier == 1 {
            vec![0x1b, b'O', application_suffix]
        } else {
            format!("\x1bO{modifier}{}", application_suffix as char).into_bytes()
        }
    } else if modifiers.contains(Modifiers::ALT) {
        vec![0x1b, normal]
    } else {
        vec![normal]
    }
}

/// Kitty keyboard protocol functional-key code for physical keypad keys.
fn kitty_numpad_code(key: Key) -> Option<u32> {
    Some(match key {
        Key::NumpadDigit(digit @ 0..=9) => 57399 + u32::from(digit),
        Key::NumpadDecimal => 57409,
        Key::NumpadDivide => 57410,
        Key::NumpadMultiply => 57411,
        Key::NumpadSubtract => 57412,
        Key::NumpadAdd => 57413,
        Key::NumpadEnter => 57414,
        _ => return None,
    })
}

fn csi_u_key(
    codepoint: u32,
    modifiers: Modifiers,
    kind: KeyEventKind,
    flags: KeyboardEnhancementFlags,
    metadata: KeyEventMetadata<'_>,
) -> Vec<u8> {
    let mut key_field = codepoint.to_string();
    if flags.contains(KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS) {
        let shifted_key = modifiers
            .contains(Modifiers::SHIFT)
            .then_some(metadata.shifted_key)
            .flatten();
        match (shifted_key, metadata.base_layout_key) {
            (Some(shifted), Some(base)) => {
                key_field.push_str(&format!(":{}:{}", shifted as u32, base as u32));
            }
            (Some(shifted), None) => {
                key_field.push_str(&format!(":{}", shifted as u32));
            }
            (None, Some(base)) => {
                key_field.push_str(&format!("::{}", base as u32));
            }
            (None, None) => {}
        }
    }

    let mut encoded = format!("\x1b[{key_field};{}", kitty_modifier_param(modifiers));
    if flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES) {
        encoded.push_str(&format!(":{}", kind.protocol_value()));
    }

    if kind != KeyEventKind::Release
        && flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES)
        && flags.contains(KeyboardEnhancementFlags::REPORT_ASSOCIATED_TEXT)
    {
        if let Some(codepoints) = metadata.associated_text.and_then(encoded_text_codepoints) {
            encoded.push(';');
            encoded.push_str(&codepoints);
        }
    }
    encoded.push('u');
    encoded.into_bytes()
}

fn encode_pure_text_event(text: &str) -> Option<Vec<u8>> {
    let codepoints = encoded_text_codepoints(text)?;
    Some(format!("\x1b[0;;{codepoints}u").into_bytes())
}

fn encoded_text_codepoints(text: &str) -> Option<String> {
    let codepoints = text
        .chars()
        .filter(|character| !is_c0_or_c1_control(*character))
        .map(|character| u32::from(character).to_string())
        .collect::<Vec<_>>();
    (!codepoints.is_empty()).then(|| codepoints.join(":"))
}

fn is_c0_or_c1_control(character: char) -> bool {
    matches!(u32::from(character), 0x00..=0x1f | 0x7f..=0x9f)
}

/// Encode xterm's unambiguous modified-character form. Level 1 preserves
/// well-known control bytes and only encodes Ctrl combinations which would
/// otherwise disappear; level 2 also encodes Ctrl/Alt and shifted ASCII
/// letters, matching foot's behavior.
fn modify_other_key(c: char, modifiers: Modifiers, level: u8) -> Option<Vec<u8>> {
    if level == 0 || modifiers.contains(Modifiers::SUPER) {
        return None;
    }

    let ctrl = modifiers.contains(Modifiers::CTRL);
    let alt = modifiers.contains(Modifiers::ALT);
    let shifted_ascii_letter = modifiers.contains(Modifiers::SHIFT) && c.is_ascii_uppercase();
    let encode = if level >= 2 {
        ctrl || alt || shifted_ascii_letter
    } else {
        ctrl && legacy_control_char(c).is_none()
    };

    encode.then(|| format!("\x1b[27;{};{}~", modifier_param(modifiers), c as u32).into_bytes())
}

fn legacy_control_char(c: char) -> Option<u8> {
    match c.to_ascii_lowercase() {
        'a'..='z' => Some(c.to_ascii_lowercase() as u8 - b'a' + 1),
        '[' => Some(0x1b),
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' => Some(0x1f),
        _ => None,
    }
}

fn enhanced_cursor_key(
    key: u8,
    modifiers: Modifiers,
    kind: KeyEventKind,
    enabled: bool,
    report_events: bool,
) -> Option<Vec<u8>> {
    enabled.then(|| {
        let modifier = kitty_modifier_param(modifiers);
        if !report_events {
            return if modifier == 1 {
                vec![b'\x1b', b'[', key]
            } else {
                format!("\x1b[1;{modifier}{}", key as char).into_bytes()
            };
        }
        format!(
            "\x1b[1;{}:{}{}",
            modifier,
            kind.protocol_value(),
            key as char
        )
        .into_bytes()
    })
}

fn enhanced_tilde_key(
    code: u8,
    modifiers: Modifiers,
    kind: KeyEventKind,
    report_events: bool,
) -> Option<Vec<u8>> {
    report_events.then(|| {
        format!(
            "\x1b[{code};{}:{}~",
            kitty_modifier_param(modifiers),
            kind.protocol_value()
        )
        .into_bytes()
    })
}

fn enhanced_function_key(
    n: u8,
    modifiers: Modifiers,
    kind: KeyEventKind,
    report_events: bool,
) -> Option<Vec<u8>> {
    if !report_events {
        return None;
    }

    match n {
        1..=4 => {
            let final_char = b"PQRS"[(n - 1) as usize];
            enhanced_cursor_key(final_char, modifiers, kind, true, true)
        }
        5 => enhanced_tilde_key(15, modifiers, kind, true),
        6 => enhanced_tilde_key(17, modifiers, kind, true),
        7 => enhanced_tilde_key(18, modifiers, kind, true),
        8 => enhanced_tilde_key(19, modifiers, kind, true),
        9 => enhanced_tilde_key(20, modifiers, kind, true),
        10 => enhanced_tilde_key(21, modifiers, kind, true),
        11 => enhanced_tilde_key(23, modifiers, kind, true),
        12 => enhanced_tilde_key(24, modifiers, kind, true),
        _ => None,
    }
}

fn cursor_key(key: u8, modifiers: Modifiers, app_cursor: bool) -> Vec<u8> {
    let modifier = modifier_param(modifiers);

    if modifier > 1 {
        format!("\x1b[1;{}{}", modifier, key as char).into_bytes()
    } else if app_cursor {
        vec![0x1b, b'O', key]
    } else {
        vec![0x1b, b'[', key]
    }
}

/// Generate escape sequence for tilde-style keys (PageUp, PageDown, Insert, Delete)
/// Format: CSI code ~ or CSI code ; modifier ~ with modifiers
fn tilde_key(code: u8, modifiers: Modifiers) -> Vec<u8> {
    let modifier = modifier_param(modifiers);

    if modifier > 1 {
        format!("\x1b[{};{}~", code, modifier).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

fn function_key(n: u8, modifiers: Modifiers) -> Vec<u8> {
    let modifier = modifier_param(modifiers);

    let code = match n {
        1 => "11",
        2 => "12",
        3 => "13",
        4 => "14",
        5 => "15",
        6 => "17",
        7 => "18",
        8 => "19",
        9 => "20",
        10 => "21",
        11 => "23",
        12 => "24",
        _ => return Vec::new(),
    };

    if modifier > 1 {
        format!("\x1b[{};{}~", code, modifier).into_bytes()
    } else {
        format!("\x1b[{}~", code).into_bytes()
    }
}

fn modifier_param(modifiers: Modifiers) -> u16 {
    let mut param = 1u16;
    if modifiers.contains(Modifiers::SHIFT) {
        param += 1;
    }
    if modifiers.contains(Modifiers::ALT) {
        param += 2;
    }
    if modifiers.contains(Modifiers::CTRL) {
        param += 4;
    }
    if modifiers.contains(Modifiers::SUPER) {
        param += 8;
    }
    if modifiers.contains(Modifiers::HYPER) {
        param += 16;
    }
    if modifiers.contains(Modifiers::META) {
        param += 32;
    }
    param
}

fn kitty_modifier_param(modifiers: Modifiers) -> u16 {
    let mut param = modifier_param(modifiers);
    if modifiers.contains(Modifiers::CAPS_LOCK) {
        param += 64;
    }
    if modifiers.contains(Modifiers::NUM_LOCK) {
        param += 128;
    }
    param
}

/// Keyboard key
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Insert,
    Delete,
    F(u8),
    NumpadDigit(u8),
    NumpadDecimal,
    NumpadDivide,
    NumpadMultiply,
    NumpadSubtract,
    NumpadAdd,
    NumpadEnter,
    /// Kitty functional keys without an older terminal encoding.
    Named(NamedKey),
}

/// Named keys encoded with Kitty's Unicode private-use key numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NamedKey {
    CapsLock,
    ScrollLock,
    NumLock,
    PrintScreen,
    Pause,
    Menu,
    NumpadEqual,
    NumpadSeparator,
    NumpadLeft,
    NumpadRight,
    NumpadUp,
    NumpadDown,
    NumpadPageUp,
    NumpadPageDown,
    NumpadHome,
    NumpadEnd,
    NumpadInsert,
    NumpadDelete,
    NumpadBegin,
    MediaPlay,
    MediaPause,
    MediaPlayPause,
    MediaReverse,
    MediaStop,
    MediaFastForward,
    MediaRewind,
    MediaTrackNext,
    MediaTrackPrevious,
    MediaRecord,
    LowerVolume,
    RaiseVolume,
    MuteVolume,
    LeftShift,
    LeftControl,
    LeftAlt,
    LeftSuper,
    LeftHyper,
    LeftMeta,
    RightShift,
    RightControl,
    RightAlt,
    RightSuper,
    RightHyper,
    RightMeta,
    IsoLevel3Shift,
    IsoLevel5Shift,
}

impl NamedKey {
    pub const fn is_modifier(self) -> bool {
        matches!(
            self,
            Self::LeftShift
                | Self::LeftControl
                | Self::LeftAlt
                | Self::LeftSuper
                | Self::LeftHyper
                | Self::LeftMeta
                | Self::RightShift
                | Self::RightControl
                | Self::RightAlt
                | Self::RightSuper
                | Self::RightHyper
                | Self::RightMeta
                | Self::IsoLevel3Shift
                | Self::IsoLevel5Shift
        )
    }

    pub const fn kitty_code(self) -> u32 {
        match self {
            Self::CapsLock => 57358,
            Self::ScrollLock => 57359,
            Self::NumLock => 57360,
            Self::PrintScreen => 57361,
            Self::Pause => 57362,
            Self::Menu => 57363,
            Self::NumpadEqual => 57415,
            Self::NumpadSeparator => 57416,
            Self::NumpadLeft => 57417,
            Self::NumpadRight => 57418,
            Self::NumpadUp => 57419,
            Self::NumpadDown => 57420,
            Self::NumpadPageUp => 57421,
            Self::NumpadPageDown => 57422,
            Self::NumpadHome => 57423,
            Self::NumpadEnd => 57424,
            Self::NumpadInsert => 57425,
            Self::NumpadDelete => 57426,
            Self::NumpadBegin => 57427,
            Self::MediaPlay => 57428,
            Self::MediaPause => 57429,
            Self::MediaPlayPause => 57430,
            Self::MediaReverse => 57431,
            Self::MediaStop => 57432,
            Self::MediaFastForward => 57433,
            Self::MediaRewind => 57434,
            Self::MediaTrackNext => 57435,
            Self::MediaTrackPrevious => 57436,
            Self::MediaRecord => 57437,
            Self::LowerVolume => 57438,
            Self::RaiseVolume => 57439,
            Self::MuteVolume => 57440,
            Self::LeftShift => 57441,
            Self::LeftControl => 57442,
            Self::LeftAlt => 57443,
            Self::LeftSuper => 57444,
            Self::LeftHyper => 57445,
            Self::LeftMeta => 57446,
            Self::RightShift => 57447,
            Self::RightControl => 57448,
            Self::RightAlt => 57449,
            Self::RightSuper => 57450,
            Self::RightHyper => 57451,
            Self::RightMeta => 57452,
            Self::IsoLevel3Shift => 57453,
            Self::IsoLevel5Shift => 57454,
        }
    }

    pub const fn from_kitty_code(code: u32) -> Option<Self> {
        Some(match code {
            57358 => Self::CapsLock,
            57359 => Self::ScrollLock,
            57360 => Self::NumLock,
            57361 => Self::PrintScreen,
            57362 => Self::Pause,
            57363 => Self::Menu,
            57415 => Self::NumpadEqual,
            57416 => Self::NumpadSeparator,
            57417 => Self::NumpadLeft,
            57418 => Self::NumpadRight,
            57419 => Self::NumpadUp,
            57420 => Self::NumpadDown,
            57421 => Self::NumpadPageUp,
            57422 => Self::NumpadPageDown,
            57423 => Self::NumpadHome,
            57424 => Self::NumpadEnd,
            57425 => Self::NumpadInsert,
            57426 => Self::NumpadDelete,
            57427 => Self::NumpadBegin,
            57428 => Self::MediaPlay,
            57429 => Self::MediaPause,
            57430 => Self::MediaPlayPause,
            57431 => Self::MediaReverse,
            57432 => Self::MediaStop,
            57433 => Self::MediaFastForward,
            57434 => Self::MediaRewind,
            57435 => Self::MediaTrackNext,
            57436 => Self::MediaTrackPrevious,
            57437 => Self::MediaRecord,
            57438 => Self::LowerVolume,
            57439 => Self::RaiseVolume,
            57440 => Self::MuteVolume,
            57441 => Self::LeftShift,
            57442 => Self::LeftControl,
            57443 => Self::LeftAlt,
            57444 => Self::LeftSuper,
            57445 => Self::LeftHyper,
            57446 => Self::LeftMeta,
            57447 => Self::RightShift,
            57448 => Self::RightControl,
            57449 => Self::RightAlt,
            57450 => Self::RightSuper,
            57451 => Self::RightHyper,
            57452 => Self::RightMeta,
            57453 => Self::IsoLevel3Shift,
            57454 => Self::IsoLevel5Shift,
            _ => return None,
        })
    }
}

bitflags::bitflags! {
    /// Keyboard modifiers
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct Modifiers: u8 {
        const SHIFT = 1 << 0;
        const CTRL = 1 << 1;
        const ALT = 1 << 2;
        const SUPER = 1 << 3;
        const HYPER = 1 << 4;
        const META = 1 << 5;
        const CAPS_LOCK = 1 << 6;
        const NUM_LOCK = 1 << 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_terminal_new() {
        let term = Terminal::new(80, 24, ScreenConfig::default());
        assert_eq!(term.cols(), 80);
        assert_eq!(term.rows(), 24);
    }

    #[test]
    fn test_terminal_process() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        term.process(b"Hello, World!");

        assert_eq!(term.screen().get_cell(0, 0).unwrap().text(), "H");
        assert_eq!(term.screen().get_cell(0, 12).unwrap().text(), "!");
    }

    #[test]
    fn daemon_collection_does_not_drop_a_single_read_transfer_burst() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let count = crate::kitty_file_transfer::MAX_PENDING_FILE_TRANSFER_COMMANDS + 32;
        let input = b"\x1b]5113;ac=cancel;id=x\x1b\\".repeat(count);

        let (_, _, commands) = term.process_collecting_with_file_transfers(&input);

        assert_eq!(commands.len(), count);
        assert!(!term.screen().has_kitty_file_transfer_commands());
    }

    #[test]
    fn daemon_mirror_discards_authoritative_transfer_commands() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        term.process_mirror(b"\x1b]5113;ac=send;id=daemon-owned\x1b\\");

        assert!(!term.screen().has_kitty_file_transfer_commands());
    }

    #[test]
    fn test_process_collecting_returns_responses() {
        // A terminal with no PTY: parser responses (e.g. DSR cursor-position report)
        // must be RETURNED by process_collecting rather than written/swallowed, so the
        // daemon can route them through its off-thread PTY writer.
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        // CSI 6 n = Device Status Report (cursor position). Cursor is at 1;1.
        let (_events, responses) = term.process_collecting(b"\x1b[6n");

        assert_eq!(responses.len(), 1, "expected one DSR response");
        assert_eq!(responses[0], b"\x1b[1;1R");
    }

    #[test]
    fn test_process_collecting_no_response_for_plain_text() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let (_events, responses) = term.process_collecting(b"Hello");

        assert!(
            responses.is_empty(),
            "plain text must not generate PTY responses"
        );
        assert_eq!(term.screen().get_cell(0, 0).unwrap().text(), "H");
    }

    #[test]
    fn process_emits_desktop_notification_actions() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let events = term.process_mirror(b"\x1b]99;i=build:p=body;finished\x1b\\");

        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::DesktopNotification(DesktopNotificationAction::Show(notification))
                if notification.id.as_deref() == Some("build")
                    && notification.title == "finished"
                    && notification.focus
        )));
    }

    #[test]
    fn process_emits_validated_dnd_commands_without_premature_capability_claims() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let (events, responses) = term.process_collecting(
            concat!(
                "\x1b]72;t=q:i=3\x1b\\",
                "\x1b]72;t=o:x=1;1:machine-id\x1b\\"
            )
            .as_bytes(),
        );

        assert!(responses.is_empty());
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::DndCommand(command)
                if command.command_type == crate::dnd::DndCommandType::Query
                    && command.client_id == 3
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::DndCommand(command)
                if command.command_type == crate::dnd::DndCommandType::OfferDrag
                    && command.cell_x == 1
                    && command.payload == b"1:machine-id"
        )));
    }

    #[test]
    fn reports_sixel_and_measured_cell_geometry_in_probe_order() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.resize_with_pixels(80, 24, 800, 480);

        let (_events, responses) = term.process_collecting(b"\x1b[c\x1b[16t\x1b[5n");

        assert_eq!(
            responses,
            [
                b"\x1b[?62;4;22;28;52c".to_vec(),
                b"\x1b[6;20;10t".to_vec(),
                b"\x1b[0n".to_vec(),
            ]
        );
    }

    #[test]
    fn parses_sixel_after_advertising_the_capability() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        term.process(b"\x1bPq~\x1b\\");

        let images = term.screen().visible_images();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].pixel_width, images[0].pixel_height), (1, 12));
    }

    #[test]
    fn daemon_mirror_never_sends_duplicate_query_replies() {
        let writes = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&writes);
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.set_write_fn(Box::new(move |data| {
            observed.lock().unwrap().extend_from_slice(data);
            Ok(())
        }));

        term.process_mirror(b"\x1b[c\x1b[16t\x1b[5n");

        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_mirror_never_answers_theme_color_queries_in_the_frontend() {
        use crate::color::{ColorPalette, Rgb};

        let writes = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&writes);
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let mut palette = ColorPalette::default_dark();
        palette.foreground = Rgb::new(0x12, 0x34, 0x56);
        palette.background = Rgb::new(0x78, 0x9a, 0xbc);
        palette.cursor = Rgb::new(0xde, 0xf0, 0x11);
        term.set_base_palette(palette);
        term.set_write_fn(Box::new(move |data| {
            observed.lock().unwrap().extend_from_slice(data);
            Ok(())
        }));

        term.process_mirror(b"\x1b]10;?\x1b\\\x1b]11;?\x07\x1b]12;?\x1b\\");

        assert!(writes.lock().unwrap().is_empty());
    }

    #[test]
    fn daemon_authority_returns_palette_queries_in_stream_order() {
        use crate::color::{ColorPalette, Rgb};

        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let mut palette = ColorPalette::default_dark();
        palette.ansi[1] = Rgb::new(0x10, 0x20, 0x30);
        term.set_base_palette(palette);

        let (_, responses) = term.process_collecting(
            b"\x1b]4;1;?;200;?\x1b\\\x1b]4;1;#abc;1;?\x1b\\\x1b]104;1\x1b\\\x1b]4;1;?\x1b\\",
        );

        assert_eq!(
            responses.concat().as_slice(),
            b"\x1b]4;1;rgb:1010/2020/3030\x1b\\\
              \x1b]4;200;rgb:ffff/0000/d7d7\x1b\\\
              \x1b]4;1;rgb:aaaa/bbbb/cccc\x1b\\\
              \x1b]4;1;rgb:1010/2020/3030\x1b\\"
        );
    }

    #[test]
    fn daemon_color_query_replies_preserve_stream_order() {
        use crate::color::{ColorPalette, Rgb};

        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let mut palette = ColorPalette::default_dark();
        palette.foreground = Rgb::new(0x12, 0x34, 0x56);
        term.set_base_palette(palette);

        let (_, responses) = term.process_collecting(
            b"\x1b]10;?\x1b\\\x1b]10;#abc\x1b\\\x1b]10;?\x1b\\\x1b]110\x1b\\\x1b]10;?\x1b\\",
        );

        assert_eq!(
            responses.concat().as_slice(),
            b"\x1b]10;rgb:1212/3434/5656\x1b\\\
              \x1b]10;rgb:aaaa/bbbb/cccc\x1b\\\
              \x1b]10;rgb:1212/3434/5656\x1b\\"
        );
    }

    #[test]
    fn daemon_authority_answers_color_queries_without_a_frontend() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let (events, responses) = term.process_collecting(b"\x1b]10;?\x1b\\");

        assert_eq!(responses.len(), 1);
        assert_eq!(
            responses[0],
            Terminal::color_query_response(
                ColorQuery::Foreground,
                ColorPalette::default().foreground
            )
        );
        assert!(events
            .iter()
            .all(|event| matches!(event, TerminalEvent::ContentChanged)));
    }

    #[test]
    fn foot_theme_and_visibility_queries_report_frontend_state() {
        use crate::{FrontendState, ThemeAppearance, WindowVisibility};

        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let (_, responses) = term.process_collecting(b"\x1b[?996n\x1b[?998n");
        assert_eq!(
            responses,
            [b"\x1b[?997;1n".to_vec(), b"\x1b[?999;1n".to_vec()]
        );

        let reports = term.set_frontend_state_collecting(FrontendState {
            appearance: ThemeAppearance::Light,
            visibility: WindowVisibility::Hidden,
        });
        assert!(reports.is_empty());

        let (_, responses) = term.process_collecting(b"\x1b[?996n\x1b[?998n");
        assert_eq!(
            responses,
            [b"\x1b[?997;2n".to_vec(), b"\x1b[?999;2n".to_vec()]
        );
    }

    #[test]
    fn foot_theme_and_visibility_change_modes_report_and_restore() {
        use crate::{FrontendState, ThemeAppearance, WindowVisibility};

        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let (_, responses) =
            term.process_collecting(b"\x1b[?2031h\x1b[?2033h\x1b[?2031$p\x1b[?2033$p");
        assert_eq!(
            responses,
            [
                b"\x1b[?999;1n".to_vec(),
                b"\x1b[?2031;1$y".to_vec(),
                b"\x1b[?2033;1$y".to_vec(),
            ]
        );

        let reports = term.set_frontend_state_collecting(FrontendState {
            appearance: ThemeAppearance::Light,
            visibility: WindowVisibility::Hidden,
        });
        assert_eq!(
            reports,
            [b"\x1b[?997;2n".to_vec(), b"\x1b[?999;2n".to_vec()]
        );

        let (_, responses) = term.process_collecting(
            b"\x1b[?2031s\x1b[?2033s\x1b[?2031l\x1b[?2033l\x1b[?2031r\x1b[?2033r",
        );
        assert_eq!(responses, [b"\x1b[?999;2n".to_vec()]);
        assert!(term.screen().modes.theme_change_reports);
        assert!(term.screen().modes.visibility_change_reports);
    }

    #[test]
    fn foreground_cwd_uses_osc7_without_a_local_pty() {
        let expected = std::env::temp_dir().join("cterm-osc7-cwd");
        let uri = url::Url::from_file_path(&expected).unwrap();
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        term.process_mirror(format!("\x1b]7;{uri}\x1b\\").as_bytes());

        assert_eq!(term.foreground_cwd(), Some(expected));
    }

    #[test]
    fn kitty_keyboard_event_mode_reports_press_repeat_and_release() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        let (_, responses) = term.process_collecting(b"\x1b[>3u\x1b[?u");
        assert_eq!(responses, [b"\x1b[?3u".to_vec()]);

        assert_eq!(
            term.handle_key_event(Key::Up, Modifiers::empty(), KeyEventKind::Press),
            Some(b"\x1b[1;1:1A".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::Up, Modifiers::empty(), KeyEventKind::Repeat),
            Some(b"\x1b[1;1:2A".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::Up, Modifiers::empty(), KeyEventKind::Release),
            Some(b"\x1b[1;1:3A".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::Char('c'), Modifiers::CTRL, KeyEventKind::Release),
            Some(b"\x1b[99;5:3u".to_vec())
        );
        assert_eq!(
            term.handle_reported_key_release(Key::Char('c'), Modifiers::empty()),
            Some(b"\x1b[99;1:3u".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::F(8), Modifiers::SHIFT, KeyEventKind::Repeat),
            Some(b"\x1b[19;2:2~".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::Char('c'), Modifiers::SUPER, KeyEventKind::Press),
            Some(b"\x1b[99;9:1u".to_vec())
        );
    }

    #[test]
    fn kitty_disambiguation_uses_csi_cursor_keys_even_in_application_mode() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process(b"\x1b[?1h\x1b[>1u");

        assert_eq!(
            term.handle_key_event(Key::Up, Modifiers::empty(), KeyEventKind::Press),
            Some(b"\x1b[A".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::Up, Modifiers::CTRL, KeyEventKind::Press),
            Some(b"\x1b[1;5A".to_vec())
        );
    }

    #[test]
    fn application_keypad_uses_vt_sequences_and_modifiers() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        assert_eq!(
            term.handle_key(Key::NumpadDigit(0), Modifiers::empty()),
            Some(b"0".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::NumpadEnter, Modifiers::empty()),
            Some(b"\r".to_vec())
        );

        term.process(b"\x1b=");
        assert_eq!(
            term.handle_key(Key::NumpadDigit(0), Modifiers::empty()),
            Some(b"\x1bOp".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::NumpadAdd, Modifiers::CTRL),
            Some(b"\x1bO5k".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::NumpadEnter, Modifiers::SHIFT | Modifiers::ALT),
            Some(b"\x1bO4M".to_vec())
        );
    }

    #[test]
    fn modify_other_keys_matches_foot_levels() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        assert_eq!(
            term.handle_key(Key::Char('i'), Modifiers::CTRL),
            Some(vec![b'\t'])
        );
        assert_eq!(
            term.handle_key(Key::Char('1'), Modifiers::CTRL),
            Some(b"\x1b[27;5;49~".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::Char('x'), Modifiers::ALT),
            Some(b"\x1bx".to_vec())
        );

        term.process_mirror(b"\x1b[>4;2m");
        assert_eq!(
            term.handle_key(Key::Char('i'), Modifiers::CTRL),
            Some(b"\x1b[27;5;105~".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::Char('x'), Modifiers::ALT),
            Some(b"\x1b[27;3;120~".to_vec())
        );
        assert_eq!(
            term.handle_key(Key::Char('A'), Modifiers::SHIFT),
            Some(b"\x1b[27;2;65~".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_reports_physical_keypad_codes() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process(b"\x1b[>3u");

        assert_eq!(
            term.handle_key_event(Key::NumpadDigit(0), Modifiers::empty(), KeyEventKind::Press,),
            Some(b"\x1b[57399;1:1u".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::NumpadAdd, Modifiers::CTRL, KeyEventKind::Repeat),
            Some(b"\x1b[57413;5:2u".to_vec())
        );
        assert_eq!(
            term.handle_key_event(Key::NumpadEnter, Modifiers::empty(), KeyEventKind::Release,),
            Some(b"\x1b[57414;1:3u".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_supports_all_key_mode_and_separates_screens() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let (_, responses) = term.process_collecting(b"\x1b[>11u\x1b[?u");
        assert_eq!(responses, [b"\x1b[?11u".to_vec()]);

        let (_, responses) = term.process_collecting(b"\x1b[?1049h\x1b[?u");
        assert_eq!(responses, [b"\x1b[?0u".to_vec()]);

        let (_, responses) = term.process_collecting(b"\x1b[>1u\x1b[?1049l\x1b[?u");
        assert_eq!(responses, [b"\x1b[?11u".to_vec()]);
    }

    #[test]
    fn kitty_keyboard_reports_alternate_keys_and_associated_text() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process(b"\x1b[>31u");
        let metadata = KeyEventMetadata::new()
            .with_shifted_key(Some('+'))
            .with_base_layout_key(Some('='))
            .with_associated_text(Some("+"));

        assert_eq!(
            term.handle_key_event_with_metadata(
                Key::Char('='),
                Modifiers::SHIFT | Modifiers::CTRL,
                KeyEventKind::Press,
                metadata,
            ),
            Some(b"\x1b[61:43:61;6:1;43u".to_vec())
        );
        assert_eq!(
            term.handle_reported_key_release_with_metadata(
                Key::Char('='),
                Modifiers::CTRL,
                metadata,
            ),
            Some(b"\x1b[61::61;5:3u".to_vec())
        );
    }

    #[test]
    fn kitty_keyboard_encodes_identified_and_pure_text_without_controls() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process(b"\x1b[>24u");

        assert_eq!(
            term.handle_key_event_with_metadata(
                Key::Char('a'),
                Modifiers::SHIFT,
                KeyEventKind::Press,
                KeyEventMetadata::new().with_associated_text(Some("A")),
            ),
            Some(b"\x1b[97;2;65u".to_vec())
        );
        assert_eq!(
            term.handle_text_input("\u{e5}\u{65e5}\u{7}"),
            b"\x1b[0;;229:26085u".to_vec()
        );

        term.process(b"\x1b[=8u");
        assert!(term.handle_text_input("not duplicated").is_empty());
    }

    #[test]
    fn kitty_keyboard_covers_extended_function_and_modifier_keys() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process(b"\x1b[>10u");

        assert_eq!(
            term.handle_key_event(
                Key::Named(NamedKey::LeftControl),
                Modifiers::CTRL,
                KeyEventKind::Press,
            ),
            Some(b"\x1b[57442;5:1u".to_vec())
        );
        assert_eq!(
            term.handle_key_event(
                Key::F(35),
                Modifiers::CAPS_LOCK | Modifiers::NUM_LOCK,
                KeyEventKind::Repeat,
            ),
            Some(b"\x1b[57398;193:2u".to_vec())
        );

        term.process(b"\x1b[=0u");
        assert_eq!(
            term.handle_key(Key::Named(NamedKey::LeftControl), Modifiers::CTRL),
            None
        );
    }

    #[test]
    fn synchronized_updates_coalesce_content_until_commit() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let events = term.process_mirror(b"\x1b[?2026hfirst");
        assert!(term.screen().modes.application_sync_updates);
        assert!(!events
            .iter()
            .any(|event| matches!(event, TerminalEvent::ContentChanged)));
        assert!(term.synchronized_update_deadline().is_some());

        let events = term.process_mirror(b" second\x1b[?2026l");
        assert!(!term.screen().modes.application_sync_updates);
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::ContentChanged))
                .count(),
            1
        );
    }

    #[test]
    fn synchronized_update_fail_safe_releases_deferred_damage() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.process_mirror(b"\x1b[?2026hpartial");
        term.synchronized_update_deadline = Some(Instant::now());

        assert!(term.expire_synchronized_update());
        assert!(!term.screen().modes.application_sync_updates);
        assert!(term.synchronized_update_deadline().is_none());
        assert!(!term.expire_synchronized_update());
    }

    #[test]
    fn synchronized_update_legacy_dcs_spelling_is_supported() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        let events = term.process_mirror(b"\x1bP=1s\x1b\\frame");
        assert!(term.screen().modes.application_sync_updates);
        assert!(!events
            .iter()
            .any(|event| matches!(event, TerminalEvent::ContentChanged)));

        let events = term.process_mirror(b"\x1bP=2s\x1b\\");
        assert!(!term.screen().modes.application_sync_updates);
        assert!(events
            .iter()
            .any(|event| matches!(event, TerminalEvent::ContentChanged)));
    }

    #[test]
    fn test_terminal_resize() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());

        term.process(b"X");
        term.resize(100, 30);

        assert_eq!(term.cols(), 100);
        assert_eq!(term.rows(), 30);
        assert_eq!(term.screen().get_cell(0, 0).unwrap().text(), "X");
    }

    #[test]
    fn zero_pixel_resize_preserves_measured_cell_size() {
        let mut term = Terminal::new(80, 24, ScreenConfig::default());
        term.resize_with_pixels(80, 24, 720, 480);
        term.resize_with_pixels(100, 30, 0, 0);

        assert_eq!(term.screen().cell_width_hint(), 9.0);
        assert_eq!(term.screen().cell_height_hint(), 20.0);
    }

    #[test]
    fn test_handle_key() {
        let term = Terminal::new(80, 24, ScreenConfig::default());

        // Regular character
        assert_eq!(
            term.handle_key(Key::Char('a'), Modifiers::empty()),
            Some(b"a".to_vec())
        );

        // Enter
        assert_eq!(
            term.handle_key(Key::Enter, Modifiers::empty()),
            Some(b"\r".to_vec())
        );

        // Ctrl+C
        assert_eq!(
            term.handle_key(Key::Char('c'), Modifiers::CTRL),
            Some(vec![0x03])
        );

        // Arrow key
        let up = term.handle_key(Key::Up, Modifiers::empty());
        assert_eq!(up, Some(b"\x1b[A".to_vec()));
    }
}
