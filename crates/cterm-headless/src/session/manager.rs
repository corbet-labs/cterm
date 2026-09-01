//! Thread-safe session manager

use crate::error::{HeadlessError, Result};
use crate::session::{generate_session_id, SessionState};
use parking_lot::{Mutex, MutexGuard, RwLock};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[cfg(any(unix, test))]
fn apply_relaunch_terminal_state(
    terminal: &mut cterm_core::Terminal,
    cursor_style: cterm_core::CursorStyle,
    cursor_blink: bool,
    screen_snapshot: Option<&cterm_proto::proto::GetScreenResponse>,
) {
    terminal
        .screen_mut()
        .configure_cursor(cursor_style, cursor_blink);
    if let Some(screen_snapshot) = screen_snapshot {
        cterm_proto::convert::screen::apply_screen_snapshot(terminal, screen_snapshot);
    }
}

/// Thread-safe manager for terminal sessions
pub struct SessionManager {
    sessions: RwLock<HashMap<String, Arc<SessionState>>>,
    /// Human-readable name → session ID index (for latch named sessions)
    named_sessions: RwLock<HashMap<String, String>>,
    /// Default scrollback lines for new sessions
    scrollback_lines: usize,
    /// Whether at least one session has ever been created
    had_sessions: AtomicBool,
    /// Serializes session creation/destruction with exec-in-place relaunch.
    lifecycle_gate: Mutex<()>,
    /// Sessions removed from the public map whose non-serializable transfer
    /// actors are still draining. Relaunch treats every entry as active.
    draining_tty_transfers: Arc<Mutex<HashSet<String>>>,
}

/// Keeps new OSC 5113 work and session-map mutations quiesced until relaunch
/// either execs successfully or returns an error.
pub struct RelaunchTransferGuard<'a> {
    manager: &'a SessionManager,
    _lifecycle: MutexGuard<'a, ()>,
}

impl Drop for RelaunchTransferGuard<'_> {
    fn drop(&mut self) {
        for session in self.manager.list_sessions() {
            session.resume_tty_transfers();
        }
    }
}

impl SessionManager {
    /// Create a new session manager with default scrollback (10000 lines)
    pub fn new() -> Self {
        Self::with_scrollback(10000)
    }

    /// Create a new session manager with custom scrollback
    pub fn with_scrollback(scrollback_lines: usize) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            named_sessions: RwLock::new(HashMap::new()),
            scrollback_lines,
            had_sessions: AtomicBool::new(false),
            lifecycle_gate: Mutex::new(()),
            draining_tty_transfers: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Create a new terminal session
    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        cols: usize,
        rows: usize,
        shell: Option<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
        term: Option<String>,
    ) -> Result<Arc<SessionState>> {
        self.create_session_with_size(
            cterm_core::PtySize {
                cols: cols.clamp(1, u16::MAX as usize) as u16,
                rows: rows.clamp(1, u16::MAX as usize) as u16,
                ..Default::default()
            },
            shell,
            args,
            cwd,
            env,
            term,
        )
    }

    /// Create a terminal session with complete cell and pixel dimensions.
    #[allow(clippy::too_many_arguments)]
    pub fn create_session_with_size(
        &self,
        size: cterm_core::PtySize,
        shell: Option<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
        term: Option<String>,
    ) -> Result<Arc<SessionState>> {
        self.create_session_with_size_and_palette(
            size,
            shell,
            args,
            cwd,
            env,
            term,
            None,
            cterm_core::FrontendState::default(),
            cterm_core::CursorStyle::Block,
            true,
        )
    }

    /// Create a terminal session with complete geometry and an authoritative
    /// frontend palette installed before the reader processes child output.
    #[allow(clippy::too_many_arguments)]
    pub fn create_session_with_size_and_palette(
        &self,
        size: cterm_core::PtySize,
        shell: Option<String>,
        args: Vec<String>,
        cwd: Option<PathBuf>,
        env: Vec<(String, String)>,
        term: Option<String>,
        base_palette: Option<cterm_core::ColorPalette>,
        frontend_state: cterm_core::FrontendState,
        cursor_style: cterm_core::CursorStyle,
        cursor_blink: bool,
    ) -> Result<Arc<SessionState>> {
        let _lifecycle = self.lifecycle_gate.lock();
        let size = size.normalized();
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let id = generate_session_id();

        // Check for collision (extremely unlikely with UUID v4)
        if self.sessions.read().contains_key(&id) {
            return Err(HeadlessError::SessionAlreadyExists(id));
        }

        let state = SessionState::new(
            id.clone(),
            size,
            shell,
            args,
            cwd,
            env,
            term,
            self.scrollback_lines,
        )?;

        if let Some(palette) = base_palette {
            state.set_base_palette(palette);
        }
        state.set_frontend_state(frontend_state);
        state.configure_cursor(cursor_style, cursor_blink);

        // Start the PTY reader task
        let state = state.start_reader()?;

        // Store the session
        self.had_sessions.store(true, Ordering::Relaxed);
        self.sessions.write().insert(id, Arc::clone(&state));

        log::info!("Created session {} ({}x{})", state.id, cols, rows);

        Ok(state)
    }

    /// Create a new session backed by a native SSH connection (puressh).
    ///
    /// Returns immediately with a "connecting" session; the SSH connection is
    /// established on a background task, surfacing any interactive prompts via
    /// the session's event stream. Must be called from within the Tokio runtime.
    pub fn create_ssh_session(
        &self,
        cols: usize,
        rows: usize,
        ssh_config: cterm_core::SshConfig,
    ) -> Result<Arc<SessionState>> {
        self.create_ssh_session_with_size(
            cterm_core::PtySize {
                cols: cols.clamp(1, u16::MAX as usize) as u16,
                rows: rows.clamp(1, u16::MAX as usize) as u16,
                ..Default::default()
            },
            ssh_config,
        )
    }

    /// Create a native SSH session with complete cell and pixel dimensions.
    pub fn create_ssh_session_with_size(
        &self,
        size: cterm_core::PtySize,
        ssh_config: cterm_core::SshConfig,
    ) -> Result<Arc<SessionState>> {
        self.create_ssh_session_with_size_and_palette(
            size,
            ssh_config,
            None,
            cterm_core::FrontendState::default(),
            cterm_core::CursorStyle::Block,
            true,
        )
    }

    /// Create a native SSH session with complete geometry and frontend palette.
    pub fn create_ssh_session_with_size_and_palette(
        &self,
        size: cterm_core::PtySize,
        ssh_config: cterm_core::SshConfig,
        base_palette: Option<cterm_core::ColorPalette>,
        frontend_state: cterm_core::FrontendState,
        cursor_style: cterm_core::CursorStyle,
        cursor_blink: bool,
    ) -> Result<Arc<SessionState>> {
        let _lifecycle = self.lifecycle_gate.lock();
        let size = size.normalized();
        let cols = size.cols as usize;
        let rows = size.rows as usize;
        let id = generate_session_id();

        if self.sessions.read().contains_key(&id) {
            return Err(HeadlessError::SessionAlreadyExists(id));
        }

        let host = ssh_config.host.clone();
        let state = SessionState::new_ssh_connecting(id.clone(), size, self.scrollback_lines);
        if let Some(palette) = base_palette {
            state.set_base_palette(palette);
        }
        state.set_frontend_state(frontend_state);
        state.configure_cursor(cursor_style, cursor_blink);

        // Store the session before driving the connection so prompt/event
        // subscribers can attach immediately.
        self.had_sessions.store(true, Ordering::Relaxed);
        self.sessions.write().insert(id, Arc::clone(&state));

        // Drive the SSH connection (and prompts) on a background task.
        state.spawn_ssh_connect(ssh_config, size);

        log::info!(
            "Created SSH session {} to {} ({}x{}, connecting)",
            state.id,
            host,
            cols,
            rows
        );

        Ok(state)
    }

    /// Get a session by ID
    pub fn get_session(&self, id: &str) -> Result<Arc<SessionState>> {
        self.sessions
            .read()
            .get(id)
            .cloned()
            .ok_or_else(|| HeadlessError::SessionNotFound(id.to_string()))
    }

    /// List all sessions
    pub fn list_sessions(&self) -> Vec<Arc<SessionState>> {
        self.sessions.read().values().cloned().collect()
    }

    /// Destroy a session
    pub async fn destroy_session(&self, id: &str, signal: Option<i32>) -> Result<()> {
        let session = {
            let _lifecycle = self.lifecycle_gate.lock();
            let session = self
                .sessions
                .write()
                .remove(id)
                .ok_or_else(|| HeadlessError::SessionNotFound(id.to_string()))?;

            // Reject new filesystem commands before releasing the lifecycle
            // gate. A concurrent relaunch can no longer observe this session,
            // while its actor is drained explicitly below.
            session.quiesce_tty_transfers();
            self.draining_tty_transfers.lock().insert(id.to_string());
            self.named_sessions.write().retain(|_, v| v != id);
            session
        };

        // Send signal to terminate the process
        #[cfg(unix)]
        let sig = signal.unwrap_or(libc::SIGHUP);
        #[cfg(not(unix))]
        let sig = signal.unwrap_or(15); // SIGTERM

        let _ = session.send_signal(sig);
        // A spawned cleanup is not cancelled when the requesting gRPC future
        // disconnects. The draining marker is removed only after staging and
        // the actor queue are fully gone.
        let draining = Arc::clone(&self.draining_tty_transfers);
        let draining_id = id.to_string();
        tokio::spawn(async move {
            session.shutdown_tty_transfers().await;
            draining.lock().remove(&draining_id);
        })
        .await
        .map_err(|error| {
            HeadlessError::Internal(format!("OSC 5113 session cleanup failed: {error}"))
        })?;

        log::info!("Destroyed session {}", id);

        Ok(())
    }

    /// Restore a session from a raw FD (used during relaunch).
    ///
    /// # Safety
    /// The caller must ensure `fd` is a valid PTY master FD and `child_pid` is correct.
    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn restore_session(
        &self,
        id: String,
        fd: i32,
        child_pid: i32,
        cols: usize,
        rows: usize,
        custom_title: String,
        tab_color: String,
        template_name: String,
        cursor_style: cterm_core::CursorStyle,
        cursor_blink: bool,
        scrollback_lines: usize,
        screen_snapshot: Option<&cterm_proto::proto::GetScreenResponse>,
    ) -> Result<Arc<SessionState>> {
        let _lifecycle = self.lifecycle_gate.lock();
        let state = SessionState::from_raw_fd(
            id.clone(),
            fd,
            child_pid,
            cols,
            rows,
            custom_title,
            tab_color,
            template_name,
            scrollback_lines,
        )?;

        // Install native defaults and the saved screen atomically before the
        // reader can process new PTY output.
        state.with_terminal_mut(|term| {
            apply_relaunch_terminal_state(term, cursor_style, cursor_blink, screen_snapshot);
        });

        // The saved screen must be authoritative before the reader can consume
        // bytes written while ctermd was exec'ing; otherwise applying it later
        // can overwrite newly parsed output.
        let state = state.start_reader()?;

        self.had_sessions.store(true, Ordering::Relaxed);
        self.sessions.write().insert(id.clone(), Arc::clone(&state));

        log::info!(
            "Restored session {} (fd={}, pid={}, {}x{})",
            id,
            fd,
            child_pid,
            cols,
            rows
        );

        Ok(state)
    }

    /// Get or create a session by human-readable name.
    ///
    /// If a running session with this name exists, returns it.
    /// Otherwise creates a new session and registers the name mapping.
    #[allow(clippy::too_many_arguments)]
    pub fn get_or_create_named_session(
        &self,
        name: &str,
        cols: usize,
        rows: usize,
        shell: Option<String>,
        env: Vec<(String, String)>,
        term: Option<String>,
    ) -> Result<Arc<SessionState>> {
        // Check for existing named session
        {
            let named = self.named_sessions.read();
            if let Some(id) = named.get(name) {
                let sessions = self.sessions.read();
                if let Some(session) = sessions.get(id) {
                    if session.is_running() {
                        log::info!("Attaching to existing session '{}' ({})", name, id);
                        return Ok(Arc::clone(session));
                    }
                }
            }
        }

        // Create new session
        let session = self.create_session(cols, rows, shell, Vec::new(), None, env, term)?;

        // Register the name
        session.set_session_name(Some(name.to_string()));
        self.named_sessions
            .write()
            .insert(name.to_string(), session.id.clone());

        log::info!("Created named session '{}' ({})", name, session.id);
        Ok(session)
    }

    /// Look up a session by human-readable name.
    pub fn get_session_by_name(&self, name: &str) -> Option<Arc<SessionState>> {
        let named = self.named_sessions.read();
        let id = named.get(name)?;
        let sessions = self.sessions.read();
        sessions.get(id).cloned()
    }

    /// Get the number of active sessions
    pub fn session_count(&self) -> usize {
        self.sessions.read().len()
    }

    /// Whether at least one session has ever been created
    pub fn had_sessions(&self) -> bool {
        self.had_sessions.load(Ordering::Relaxed)
    }

    /// Freeze session-map mutation and reject new OSC 5113 starts, then prove
    /// that no actor queue, approval, or approved filesystem session remains.
    pub fn begin_relaunch_transfer_quiesce(
        &self,
    ) -> std::result::Result<RelaunchTransferGuard<'_>, Vec<String>> {
        let lifecycle = self.lifecycle_gate.lock();
        let sessions = self.list_sessions();
        for session in &sessions {
            session.quiesce_tty_transfers();
        }
        let mut active: Vec<_> = self.draining_tty_transfers.lock().iter().cloned().collect();
        active.extend(
            sessions
                .iter()
                .filter(|session| session.has_active_tty_transfers())
                .map(|session| session.id.clone()),
        );
        active.sort();
        active.dedup();
        if !active.is_empty() {
            for session in sessions {
                session.resume_tty_transfers();
            }
            return Err(active);
        }
        Ok(RelaunchTransferGuard {
            manager: self,
            _lifecycle: lifecycle,
        })
    }

    /// Clean up dead sessions, returns the number of sessions removed
    pub fn cleanup_dead_sessions(&self) -> usize {
        let _lifecycle = self.lifecycle_gate.lock();
        // Snapshot (id, Arc) pairs under a SHORT read lock, then drop the lock.
        // Crucially, we do NOT call `is_running()` (which acquires a session's
        // terminal lock) while holding the global `sessions` lock — otherwise a
        // single contended terminal lock would freeze the whole map and block every
        // concurrent `get_session()`, cascading into a daemon-wide deadlock.
        let snapshot: Vec<(String, Arc<SessionState>)> = {
            let sessions = self.sessions.read();
            sessions
                .iter()
                .map(|(id, s)| (id.clone(), Arc::clone(s)))
                .collect()
        };

        // Check liveness without holding the map lock.
        let mut dead_ids = Vec::new();
        for (id, session) in &snapshot {
            if session.is_running() {
                continue;
            }
            // Close admission before checking the actor. The PTY reader may
            // have parsed its final chunk but not yet submitted the contained
            // command; the controller's double-check then rejects that race.
            session.quiesce_tty_transfers();
            if !session.has_active_tty_transfers() {
                dead_ids.push(id.clone());
            }
            // Active dead sessions stay quiesced until the EOF reader drains
            // them, then the next cleanup pass can remove them safely.
        }

        // Take the write lock only to remove the dead ids.
        let count = dead_ids.len();
        if count > 0 {
            let mut sessions = self.sessions.write();
            let mut named = self.named_sessions.write();
            for id in &dead_ids {
                sessions.remove(id);
                // Clean up named session mapping
                named.retain(|_, v| v != id);
                log::info!("Cleaned up dead session {}", id);
            }
        }
        count
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_session_manager_new() {
        let manager = SessionManager::new();
        assert_eq!(manager.session_count(), 0);
    }

    #[test]
    fn test_session_not_found() {
        let manager = SessionManager::new();
        let result = manager.get_session("nonexistent");
        assert!(matches!(result, Err(HeadlessError::SessionNotFound(_))));
    }

    #[test]
    fn relaunch_snapshot_precedes_new_output_and_keeps_cursor_defaults() {
        let mut source =
            cterm_core::Terminal::new(32, 2, cterm_core::screen::ScreenConfig::default());
        source.process(b"before");
        let snapshot = cterm_proto::convert::screen::screen_to_proto(source.screen(), true);

        let mut restored =
            cterm_core::Terminal::new(32, 2, cterm_core::screen::ScreenConfig::default());
        apply_relaunch_terminal_state(
            &mut restored,
            cterm_core::CursorStyle::Bar,
            false,
            Some(&snapshot),
        );
        // Models bytes buffered in the PTY while ctermd was exec'ing. The
        // reader starts only after the helper above has returned.
        restored.process(b"after");

        assert!(restored.screen().grid().text().contains("beforeafter"));
        assert_eq!(restored.screen().cursor.style, cterm_core::CursorStyle::Bar);
        assert!(!restored.screen().cursor.blink.enabled());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn relaunch_quiescence_rejects_new_transfers_and_refuses_active_sessions() {
        let manager = SessionManager::new();
        let session = manager
            .create_session(
                80,
                24,
                Some("/bin/sh".to_string()),
                Vec::new(),
                None,
                Vec::new(),
                None,
            )
            .unwrap();
        let (_, mut approvals) = session.subscribe_tty_transfer_prompts();

        let guard = manager.begin_relaunch_transfer_quiesce().unwrap();
        session
            .process_output(b"\x1b]5113;ac=send;id=blocked-during-relaunch\x1b\\")
            .await;
        assert!(matches!(
            approvals.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        drop(guard);

        session
            .process_output(b"\x1b]5113;ac=send;id=active-transfer\x1b\\")
            .await;
        let approval = tokio::time::timeout(std::time::Duration::from_secs(1), approvals.recv())
            .await
            .expect("approval event timed out")
            .expect("approval channel closed");
        assert_eq!(approval.transfer_id, "active-transfer");
        assert!(session.has_active_tty_transfers());

        let active_sessions = manager
            .begin_relaunch_transfer_quiesce()
            .err()
            .expect("active transfer must refuse relaunch");
        assert_eq!(
            active_sessions.as_slice(),
            std::slice::from_ref(&session.id)
        );
        assert!(
            session
                .respond_tty_transfer_approval(approval.request_id, true)
                .await
        );

        manager.destroy_session(&session.id, None).await.unwrap();
        assert!(!session.has_active_tty_transfers());
        assert!(session.subscribe_tty_transfer_prompts().0.is_empty());
    }
}
