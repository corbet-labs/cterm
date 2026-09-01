//! Nonblocking daemon execution for consent-gated Kitty OSC 5113 sessions.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use cterm_app::{
    TtyTransferAction, TtyTransferLimits, TtyTransferManager, TtyTransferSendFilesystem,
};
use cterm_core::FileTransferCommand;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::SessionState;

const COMMAND_QUEUE_CAPACITY: usize = 512;
pub(super) const APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);
const APPROVED_IDLE_TIMEOUT: Duration = Duration::from_secs(10 * 60);
pub(super) const DEFAULT_MAX_FILES_PER_SESSION: usize = 256;
pub(super) const DEFAULT_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
pub(super) const DEFAULT_MAX_SESSION_BYTES: u64 = 16 * 1024 * 1024 * 1024;

enum WorkerMessage {
    Protocol(FileTransferCommand),
    Approval {
        request_id: u64,
        approve: bool,
        accepted_tx: oneshot::Sender<bool>,
    },
    Shutdown {
        done_tx: oneshot::Sender<()>,
    },
}

/// Bounded ingress for one session's asynchronous transfer worker.
pub(super) struct TtyTransferController {
    tx: mpsc::Sender<WorkerMessage>,
    accepting: Arc<AtomicBool>,
    active: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
}

impl TtyTransferController {
    pub(super) fn spawn(session: &Arc<SessionState>) -> Result<Self, &'static str> {
        let home = local_home_directory().ok_or("local home directory is unavailable")?;
        let limits = TtyTransferLimits::new(
            DEFAULT_MAX_FILES_PER_SESSION,
            DEFAULT_MAX_FILE_BYTES,
            DEFAULT_MAX_SESSION_BYTES,
        )
        .map_err(|_| "default transfer limits are invalid")?;
        let filesystem = TtyTransferSendFilesystem::new(home, limits)
            .map_err(|_| "local transfer filesystem is unavailable")?;
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| "Tokio runtime is unavailable")?;
        let (tx, rx) = mpsc::channel(COMMAND_QUEUE_CAPACITY);
        let accepting = Arc::new(AtomicBool::new(true));
        let active = Arc::new(AtomicBool::new(false));
        let queued = Arc::new(AtomicUsize::new(0));
        runtime.spawn(run_worker(
            Arc::downgrade(session),
            rx,
            filesystem,
            Arc::clone(&active),
            Arc::clone(&queued),
        ));
        Ok(Self {
            tx,
            accepting,
            active,
            queued,
        })
    }

    pub(super) async fn submit(
        &self,
        command: FileTransferCommand,
    ) -> Result<(), FileTransferCommand> {
        if !self.accepting.load(Ordering::SeqCst) {
            return Err(command);
        }
        self.queued.fetch_add(1, Ordering::SeqCst);
        if !self.accepting.load(Ordering::SeqCst) {
            self.queued.fetch_sub(1, Ordering::SeqCst);
            return Err(command);
        }
        let result = self
            .tx
            .send(WorkerMessage::Protocol(command))
            .await
            .map_err(|error| match error.0 {
                WorkerMessage::Protocol(command) => command,
                WorkerMessage::Approval { .. } => {
                    unreachable!("submit only sends protocol messages")
                }
                WorkerMessage::Shutdown { .. } => {
                    unreachable!("submit only sends protocol messages")
                }
            });
        if result.is_err() {
            decrement_queued(&self.queued);
        }
        result
    }

    pub(super) async fn respond(&self, request_id: u64, approve: bool) -> bool {
        let (accepted_tx, accepted_rx) = oneshot::channel();
        if self
            .tx
            .send(WorkerMessage::Approval {
                request_id,
                approve,
                accepted_tx,
            })
            .await
            .is_err()
        {
            return false;
        }
        accepted_rx.await.unwrap_or(false)
    }

    pub(super) async fn shutdown(&self) {
        self.accepting.store(false, Ordering::SeqCst);
        let (done_tx, done_rx) = oneshot::channel();
        if self
            .tx
            .send(WorkerMessage::Shutdown { done_tx })
            .await
            .is_ok()
        {
            let _ = done_rx.await;
        }
    }

    pub(super) fn quiesce(&self) {
        self.accepting.store(false, Ordering::SeqCst);
    }

    pub(super) fn resume(&self) {
        self.accepting.store(true, Ordering::SeqCst);
    }

    pub(super) fn has_work(&self) -> bool {
        self.active.load(Ordering::SeqCst) || self.queued.load(Ordering::SeqCst) != 0
    }
}

fn decrement_queued(queued: &AtomicUsize) {
    let _ = queued.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
        Some(count.saturating_sub(1))
    });
}

fn local_home_directory() -> Option<PathBuf> {
    let home = directories::BaseDirs::new()?.home_dir().to_path_buf();
    home.is_absolute().then_some(home)
}

async fn run_worker(
    session: Weak<SessionState>,
    mut rx: mpsc::Receiver<WorkerMessage>,
    filesystem: TtyTransferSendFilesystem,
    active: Arc<AtomicBool>,
    queued: Arc<AtomicUsize>,
) {
    struct ActivityReset {
        active: Arc<AtomicBool>,
        queued: Arc<AtomicUsize>,
    }
    impl Drop for ActivityReset {
        fn drop(&mut self) {
            self.active.store(false, Ordering::SeqCst);
            self.queued.store(0, Ordering::SeqCst);
        }
    }
    let _activity_reset = ActivityReset {
        active: Arc::clone(&active),
        queued: Arc::clone(&queued),
    };
    let mut manager = TtyTransferManager::send_only();
    let mut filesystem = filesystem;
    let mut pending: HashMap<u64, (String, Instant)> = HashMap::new();
    let mut approved_idle: HashMap<String, Instant> = HashMap::new();
    let mut expiry = tokio::time::interval(Duration::from_secs(1));
    expiry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            message = rx.recv() => {
                let Some(message) = message else { break };
                let (actions, protocol_id, newly_approved) = match message {
                    WorkerMessage::Protocol(command) => {
                        let transfer_id = command.id.clone();
                        let was_pending = pending
                            .iter()
                            .find_map(|(request_id, (id, _))| (id == &transfer_id).then_some(*request_id));
                        let actions = manager.handle(command);
                        // Publish manager-owned work before any approval event
                        // becomes observable. Relaunch quiescence must never
                        // see an idle controller after the UI has seen a live
                        // consent request.
                        active.store(manager.active_sessions() != 0, Ordering::SeqCst);
                        if let Some(request_id) = was_pending {
                            if !manager.is_approval_pending(request_id) {
                                pending.remove(&request_id);
                                if let Some(session) = session.upgrade() {
                                    session.clear_tty_transfer_prompt(request_id);
                                }
                            }
                        }
                        (actions, Some(transfer_id), None)
                    }
                    WorkerMessage::Approval { request_id, approve, accepted_tx } => {
                        let Some((transfer_id, deadline)) = pending.remove(&request_id) else {
                            let _ = accepted_tx.send(false);
                            continue;
                        };
                        if let Some(session) = session.upgrade() {
                            session.clear_tty_transfer_prompt(request_id);
                        }
                        if deadline <= Instant::now() {
                            let _ = accepted_tx.send(false);
                            let actions = manager.reject(
                                request_id,
                                "ETIMEDOUT:Transfer approval expired",
                            );
                            let Some(next) = dispatch_actions(
                                &session,
                                &mut pending,
                                filesystem,
                                actions,
                            )
                            .await else {
                                return;
                            };
                            filesystem = next;
                            active.store(manager.active_sessions() != 0, Ordering::SeqCst);
                            continue;
                        }
                        let actions = if approve {
                            manager.approve(request_id)
                        } else {
                            manager.deny(request_id)
                        };
                        let _ = accepted_tx.send(true);
                        let newly_approved = approve.then_some(transfer_id);
                        (actions, None, newly_approved)
                    }
                    WorkerMessage::Shutdown { done_tx } => {
                        if let Some(session) = session.upgrade() {
                            for request_id in pending.keys() {
                                session.clear_tty_transfer_prompt(*request_id);
                            }
                        }
                        pending.clear();
                        approved_idle.clear();
                        let actions = manager.reject_all("ECANCELED:Terminal session closed");
                        let _ = dispatch_actions(
                            &session,
                            &mut pending,
                            filesystem,
                            actions,
                        )
                        .await;
                        let _ = done_tx.send(());
                        return;
                    }
                };
                let refresh_idle = actions
                    .iter()
                    .any(|action| matches!(action, TtyTransferAction::Execute(_)));
                let Some(next) = dispatch_actions(
                    &session,
                    &mut pending,
                    filesystem,
                    actions,
                )
                .await else {
                    return;
                };
                filesystem = next;
                if let Some(transfer_id) = newly_approved {
                    approved_idle.insert(
                        transfer_id,
                        Instant::now() + APPROVED_IDLE_TIMEOUT,
                    );
                }
                if let Some(transfer_id) = protocol_id {
                    if !manager.has_session(&transfer_id) {
                        approved_idle.remove(&transfer_id);
                    } else if refresh_idle && approved_idle.contains_key(&transfer_id) {
                        approved_idle.insert(
                            transfer_id,
                            Instant::now() + APPROVED_IDLE_TIMEOUT,
                        );
                    }
                    decrement_queued(&queued);
                }
                active.store(manager.active_sessions() != 0, Ordering::SeqCst);
            }
            _ = expiry.tick() => {
                let now = Instant::now();
                let expired: Vec<_> = pending
                    .iter()
                    .filter_map(|(request_id, (_, deadline))| (*deadline <= now).then_some(*request_id))
                    .collect();
                for request_id in expired {
                    pending.remove(&request_id);
                    if let Some(session) = session.upgrade() {
                        session.clear_tty_transfer_prompt(request_id);
                    }
                    let actions = manager.reject(request_id, "ETIMEDOUT:Transfer approval expired");
                    let Some(next) = dispatch_actions(
                        &session,
                        &mut pending,
                        filesystem,
                        actions,
                    )
                    .await else {
                        return;
                    };
                    filesystem = next;
                }
                let expired: Vec<_> = approved_idle
                    .iter()
                    .filter(|(_, deadline)| **deadline <= now)
                    .map(|(session_id, _)| session_id.clone())
                    .collect();
                for session_id in expired {
                    approved_idle.remove(&session_id);
                    let actions = manager.reject_session(
                        &session_id,
                        "ETIMEDOUT:Transfer session idle",
                    );
                    let Some(next) = dispatch_actions(
                        &session,
                        &mut pending,
                        filesystem,
                        actions,
                    )
                    .await else {
                        return;
                    };
                    filesystem = next;
                }
                active.store(manager.active_sessions() != 0, Ordering::SeqCst);
            }
        }
    }
}

async fn dispatch_actions(
    session: &Weak<SessionState>,
    pending: &mut HashMap<u64, (String, Instant)>,
    mut filesystem: TtyTransferSendFilesystem,
    actions: Vec<TtyTransferAction>,
) -> Option<TtyTransferSendFilesystem> {
    let mut actions: VecDeque<_> = actions.into();
    while let Some(action) = actions.pop_front() {
        match action {
            TtyTransferAction::Write(bytes) => {
                let session = session.upgrade()?;
                session.send_tty_transfer_response(&bytes);
            }
            TtyTransferAction::RequestApproval(request) => {
                let expires_at = Instant::now() + APPROVAL_TIMEOUT;
                pending.insert(request.request_id, (request.session_id.clone(), expires_at));
                let session = session.upgrade()?;
                session.register_tty_transfer_prompt(request, expires_at);
            }
            executable @ (TtyTransferAction::Execute(_) | TtyTransferAction::Abort { .. }) => {
                let mut current = filesystem;
                match tokio::task::spawn_blocking(move || {
                    let next = current.handle_action(executable);
                    (current, next)
                })
                .await
                {
                    Ok((current, next)) => {
                        filesystem = current;
                        actions.extend(next);
                    }
                    Err(error) => {
                        log::error!("OSC 5113 filesystem worker panicked: {error}");
                        return None;
                    }
                }
            }
        }
    }
    Some(filesystem)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_core::FileTransferAction;

    #[test]
    fn transfer_defaults_are_valid_and_bounded() {
        let limits = TtyTransferLimits::new(
            DEFAULT_MAX_FILES_PER_SESSION,
            DEFAULT_MAX_FILE_BYTES,
            DEFAULT_MAX_SESSION_BYTES,
        )
        .unwrap();
        assert_eq!(limits.max_files_per_session, 256);
        assert!(limits.max_file_bytes < limits.max_session_bytes);
        assert_eq!(COMMAND_QUEUE_CAPACITY, 2 * 256);
    }

    #[test]
    fn queued_work_counter_never_wraps_during_worker_shutdown() {
        let queued = AtomicUsize::new(0);
        decrement_queued(&queued);
        assert_eq!(queued.load(Ordering::SeqCst), 0);

        queued.store(1, Ordering::SeqCst);
        decrement_queued(&queued);
        assert_eq!(queued.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn only_start_actions_can_enter_a_fresh_manager() {
        let mut manager = TtyTransferManager::send_only();
        let command = FileTransferCommand {
            action: FileTransferAction::Receive,
            id: "unsupported".into(),
            file_id: None,
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: Some(0),
            name: None,
            status: None,
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        };
        assert!(matches!(
            manager.handle(command).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
    }
}
