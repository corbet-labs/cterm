//! Authorization state machine for Kitty OSC 5113 file transfers.
//!
//! Filesystem work is deliberately represented as an action. Native frontends
//! must obtain user approval before passing an authorized command to a future
//! filesystem executor.

use std::collections::HashMap;

use cterm_core::{FileTransferAction, FileTransferCommand, MAX_FILE_TRANSFER_PATH_BYTES};

const MAX_ACTIVE_SESSIONS: usize = 16;
const MAX_RECEIVE_PATHS: usize = 256;

/// Direction from the perspective of the process using the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtyTransferDirection {
    /// The process sends files to the computer running cterm.
    Send,
    /// The process requests files from the computer running cterm.
    Receive,
}

/// A native consent prompt which has not granted any filesystem access yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TtyTransferApprovalRequest {
    /// Monotonic token; frontends must return this, not only the session id.
    pub request_id: u64,
    pub session_id: String,
    pub direction: TtyTransferDirection,
    /// Requested local paths for receive sessions; send sessions have none yet.
    pub paths: Vec<String>,
}

/// A validated command from a session that received explicit user consent.
///
/// The fields are private so raw parser output cannot be passed to the
/// filesystem executor without crossing the authorization state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedTtyTransferCommand {
    direction: TtyTransferDirection,
    quiet: u8,
    command: FileTransferCommand,
}

impl AuthorizedTtyTransferCommand {
    pub fn direction(&self) -> TtyTransferDirection {
        self.direction
    }

    pub fn quiet(&self) -> u8 {
        self.quiet
    }

    pub fn command(&self) -> &FileTransferCommand {
        &self.command
    }

    pub(crate) fn into_parts(self) -> (TtyTransferDirection, u8, FileTransferCommand) {
        (self.direction, self.quiet, self.command)
    }
}

/// Work emitted by the UI-neutral authorization state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TtyTransferAction {
    RequestApproval(TtyTransferApprovalRequest),
    /// A protocol response to write to the PTY.
    Write(Vec<u8>),
    /// A command whose session has received explicit user consent.
    Execute(AuthorizedTtyTransferCommand),
    /// Finish the metadata listing for an approved receive session. The
    /// filesystem executor supplies the local home path in the terminating
    /// `OK` response after every queued path has been inspected.
    CompleteReceiveListing {
        session_id: String,
        quiet: u8,
    },
    /// Drop any filesystem staging owned by this session.
    Abort {
        session_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionPhase {
    CollectReceivePaths,
    AwaitApproval,
    Approved,
}

#[derive(Debug)]
struct TransferSession {
    direction: TtyTransferDirection,
    phase: SessionPhase,
    quiet: u8,
    request_id: u64,
    expected_paths: usize,
    receive_requests: Vec<FileTransferCommand>,
}

/// Bounded OSC 5113 session authorization shared by all native frontends.
#[derive(Debug)]
pub struct TtyTransferManager {
    sessions: HashMap<String, TransferSession>,
    next_request_id: u64,
    receive_supported: bool,
}

impl Default for TtyTransferManager {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            next_request_id: 1,
            receive_supported: true,
        }
    }
}

impl TtyTransferManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Construct a manager for an executor that does not yet implement local
    /// filesystem reads. Unsupported receive requests fail before a consent
    /// prompt or an `OK` response can imply that data will follow.
    pub fn send_only() -> Self {
        Self {
            receive_supported: false,
            ..Self::default()
        }
    }

    /// Consume one decoded command without performing filesystem I/O.
    pub fn handle(&mut self, command: FileTransferCommand) -> Vec<TtyTransferAction> {
        match command.action {
            FileTransferAction::Send => self.start(command, TtyTransferDirection::Send),
            FileTransferAction::Receive => self.start(command, TtyTransferDirection::Receive),
            FileTransferAction::Cancel => self.cancel(command),
            _ => self.continue_session(command),
        }
    }

    /// Grant the exact still-pending request represented by `request_id`.
    pub fn approve(&mut self, request_id: u64) -> Vec<TtyTransferAction> {
        let Some(session_id) = self.session_id_for_request(request_id) else {
            return Vec::new();
        };
        let Some(session) = self.sessions.get_mut(&session_id) else {
            return Vec::new();
        };
        if session.phase != SessionPhase::AwaitApproval {
            return Vec::new();
        }

        session.phase = SessionPhase::Approved;
        let mut actions = Vec::new();
        if let Some(response) = status_response(&session_id, None, "OK", session.quiet, false) {
            actions.push(TtyTransferAction::Write(response));
        }
        if session.direction == TtyTransferDirection::Receive {
            let direction = session.direction;
            let quiet = session.quiet;
            actions.extend(
                std::mem::take(&mut session.receive_requests)
                    .into_iter()
                    .map(move |command| {
                        TtyTransferAction::Execute(AuthorizedTtyTransferCommand {
                            direction,
                            quiet,
                            command,
                        })
                    }),
            );
            actions.push(TtyTransferAction::CompleteReceiveListing { session_id, quiet });
        }
        actions
    }

    /// Refuse and discard the exact still-pending approval request.
    pub fn deny(&mut self, request_id: u64) -> Vec<TtyTransferAction> {
        self.reject(request_id, "EPERM:User refused the transfer")
    }

    /// Refuse a pending request for a daemon policy reason such as expiry.
    pub fn reject(&mut self, request_id: u64, status: &str) -> Vec<TtyTransferAction> {
        let Some(session_id) = self.session_id_for_request(request_id) else {
            return Vec::new();
        };
        self.reject_session(&session_id, status)
    }

    /// End one pending or approved session for a daemon lifecycle reason.
    /// Approved sessions emit `Abort` before their terminal-visible error so
    /// filesystem staging is discarded before the peer can retry.
    pub fn reject_session(&mut self, session_id: &str, status: &str) -> Vec<TtyTransferAction> {
        let Some(session) = self.sessions.remove(session_id) else {
            return Vec::new();
        };
        let mut actions = Vec::new();
        if session.phase == SessionPhase::Approved {
            actions.push(TtyTransferAction::Abort {
                session_id: session_id.to_string(),
            });
        }
        if let Some(response) = status_response(session_id, None, status, session.quiet, true) {
            actions.push(TtyTransferAction::Write(response));
        }
        actions
    }

    /// End every live session, used for PTY EOF and daemon shutdown cleanup.
    pub fn reject_all(&mut self, status: &str) -> Vec<TtyTransferAction> {
        let session_ids: Vec<_> = self.sessions.keys().cloned().collect();
        session_ids
            .into_iter()
            .flat_map(|session_id| self.reject_session(&session_id, status))
            .collect()
    }

    pub fn active_sessions(&self) -> usize {
        self.sessions.len()
    }

    pub fn has_session(&self, session_id: &str) -> bool {
        self.sessions.contains_key(session_id)
    }

    /// Whether this exact opaque token still names a session awaiting consent.
    ///
    /// Executors use this after processing protocol input so duplicate or
    /// otherwise rejected commands cannot accidentally dismiss a live prompt.
    pub fn is_approval_pending(&self, request_id: u64) -> bool {
        self.session_id_for_request(request_id).is_some()
    }

    fn start(
        &mut self,
        command: FileTransferCommand,
        direction: TtyTransferDirection,
    ) -> Vec<TtyTransferAction> {
        if direction == TtyTransferDirection::Receive && !self.receive_supported {
            return error_response(&command, "ENOTSUP:Receive sessions are not implemented");
        }
        if self.sessions.contains_key(&command.id) {
            return error_response(&command, "EEXIST:Transfer session already exists");
        }
        if self.sessions.len() >= MAX_ACTIVE_SESSIONS {
            return error_response(&command, "ENOSPC:Too many transfer sessions");
        }

        let expected_paths = if direction == TtyTransferDirection::Receive {
            let Some(expected) = command.size.and_then(|size| usize::try_from(size).ok()) else {
                return error_response(&command, "EINVAL:Missing receive path count");
            };
            if expected > MAX_RECEIVE_PATHS {
                return error_response(&command, "E2BIG:Too many receive paths");
            }
            expected
        } else {
            0
        };
        let request_id = self.allocate_request_id();
        let session_id = command.id.clone();
        let phase = if direction == TtyTransferDirection::Receive && expected_paths != 0 {
            SessionPhase::CollectReceivePaths
        } else {
            SessionPhase::AwaitApproval
        };
        self.sessions.insert(
            session_id.clone(),
            TransferSession {
                direction,
                phase,
                quiet: command.quiet,
                request_id,
                expected_paths,
                receive_requests: Vec::with_capacity(expected_paths),
            },
        );

        if phase == SessionPhase::AwaitApproval {
            vec![TtyTransferAction::RequestApproval(
                TtyTransferApprovalRequest {
                    request_id,
                    session_id,
                    direction,
                    paths: Vec::new(),
                },
            )]
        } else {
            Vec::new()
        }
    }

    fn cancel(&mut self, command: FileTransferCommand) -> Vec<TtyTransferAction> {
        let Some(session) = self.sessions.remove(&command.id) else {
            return error_response(&command, "ENOENT:Unknown transfer session");
        };
        let mut actions = Vec::new();
        if session.phase == SessionPhase::Approved {
            actions.push(TtyTransferAction::Abort {
                session_id: command.id.clone(),
            });
        }
        if let Some(response) = status_response(&command.id, None, "CANCELED", session.quiet, false)
        {
            actions.push(TtyTransferAction::Write(response));
        }
        actions
    }

    fn continue_session(&mut self, command: FileTransferCommand) -> Vec<TtyTransferAction> {
        let session_id = command.id.clone();
        let Some(phase) = self.sessions.get(&session_id).map(|session| session.phase) else {
            return error_response(&command, "ENOENT:Unknown transfer session");
        };

        match phase {
            SessionPhase::CollectReceivePaths => self.collect_receive_path(command),
            SessionPhase::AwaitApproval => {
                let quiet = self
                    .sessions
                    .remove(&session_id)
                    .map_or(command.quiet, |session| session.quiet);
                status_response(
                    &session_id,
                    command.file_id.as_deref(),
                    "EINVAL:Command received before authorization",
                    quiet,
                    true,
                )
                .map(TtyTransferAction::Write)
                .into_iter()
                .collect()
            }
            SessionPhase::Approved => self.authorized_command(command),
        }
    }

    fn collect_receive_path(&mut self, command: FileTransferCommand) -> Vec<TtyTransferAction> {
        let session_id = command.id.clone();
        let is_valid_file = command.action == FileTransferAction::File
            && command.file_id.is_some()
            && command.name.as_deref().is_some_and(valid_protocol_path);
        if !is_valid_file {
            let quiet = self
                .sessions
                .remove(&session_id)
                .map_or(command.quiet, |session| session.quiet);
            return status_response(
                &session_id,
                command.file_id.as_deref(),
                "EINVAL:Invalid receive path request",
                quiet,
                true,
            )
            .map(TtyTransferAction::Write)
            .into_iter()
            .collect();
        }

        let session = self.sessions.get_mut(&session_id).expect("session exists");
        let file_id = command.file_id.as_deref().expect("validated file id");
        if session
            .receive_requests
            .iter()
            .any(|request| request.file_id.as_deref() == Some(file_id))
        {
            let quiet = self
                .sessions
                .remove(&session_id)
                .map_or(command.quiet, |session| session.quiet);
            return status_response(
                &session_id,
                Some(file_id),
                "EEXIST:Duplicate file id",
                quiet,
                true,
            )
            .map(TtyTransferAction::Write)
            .into_iter()
            .collect();
        }
        session.receive_requests.push(command);
        if session.receive_requests.len() != session.expected_paths {
            return Vec::new();
        }
        session.phase = SessionPhase::AwaitApproval;
        vec![TtyTransferAction::RequestApproval(
            TtyTransferApprovalRequest {
                request_id: session.request_id,
                session_id,
                direction: session.direction,
                paths: session
                    .receive_requests
                    .iter()
                    .filter_map(|command| command.name.clone())
                    .collect(),
            },
        )]
    }

    fn authorized_command(&mut self, command: FileTransferCommand) -> Vec<TtyTransferAction> {
        let direction = self.sessions[&command.id].direction;
        let closes = matches!(
            (direction, command.action),
            (TtyTransferDirection::Send, FileTransferAction::Finish)
                | (TtyTransferDirection::Receive, FileTransferAction::Finished)
        );
        let permitted = command_shape_is_valid(&command)
            && match direction {
                TtyTransferDirection::Send => matches!(
                    command.action,
                    FileTransferAction::File
                        | FileTransferAction::Data
                        | FileTransferAction::EndData
                        | FileTransferAction::Finish
                ),
                TtyTransferDirection::Receive => matches!(
                    command.action,
                    FileTransferAction::File
                        | FileTransferAction::Data
                        | FileTransferAction::EndData
                        | FileTransferAction::Finished
                ),
            };
        if !permitted {
            let quiet = self
                .sessions
                .remove(&command.id)
                .map_or(command.quiet, |session| session.quiet);
            let mut actions = vec![TtyTransferAction::Abort {
                session_id: command.id.clone(),
            }];
            if let Some(response) = status_response(
                &command.id,
                command.file_id.as_deref(),
                "EINVAL:Action is invalid for transfer direction",
                quiet,
                true,
            ) {
                actions.push(TtyTransferAction::Write(response));
            }
            return actions;
        }
        let authorized = AuthorizedTtyTransferCommand {
            direction,
            quiet: self.sessions[&command.id].quiet,
            command,
        };
        if closes {
            self.sessions.remove(&authorized.command.id);
        }
        vec![TtyTransferAction::Execute(authorized)]
    }

    fn session_id_for_request(&self, request_id: u64) -> Option<String> {
        self.sessions.iter().find_map(|(id, session)| {
            (session.request_id == request_id && session.phase == SessionPhase::AwaitApproval)
                .then(|| id.clone())
        })
    }

    fn allocate_request_id(&mut self) -> u64 {
        let request_id = self.next_request_id;
        self.next_request_id = self.next_request_id.wrapping_add(1).max(1);
        request_id
    }
}

fn command_shape_is_valid(command: &FileTransferCommand) -> bool {
    match command.action {
        FileTransferAction::File => {
            command.file_id.is_some() && command.name.as_deref().is_some_and(valid_protocol_path)
        }
        FileTransferAction::Data | FileTransferAction::EndData => command.file_id.is_some(),
        FileTransferAction::Finish | FileTransferAction::Finished => true,
        _ => false,
    }
}

fn valid_protocol_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_FILE_TRANSFER_PATH_BYTES
        && !bytes.contains(&0)
        && bytes
            .split(|byte| *byte == b'/')
            .all(|part| part.len() <= 255 && part != b"." && part != b"..")
}

fn error_response(command: &FileTransferCommand, status: &str) -> Vec<TtyTransferAction> {
    status_response(
        &command.id,
        command.file_id.as_deref(),
        status,
        command.quiet,
        true,
    )
    .map(TtyTransferAction::Write)
    .into_iter()
    .collect()
}

fn status_response(
    session_id: &str,
    file_id: Option<&str>,
    status: &str,
    quiet: u8,
    is_error: bool,
) -> Option<Vec<u8>> {
    if quiet >= 2 || quiet == 1 && !is_error {
        return None;
    }
    FileTransferCommand {
        action: FileTransferAction::Status,
        id: session_id.to_string(),
        file_id: file_id.map(str::to_string),
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
    }
    .encode()
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(action: FileTransferAction, id: &str) -> FileTransferCommand {
        FileTransferCommand {
            action,
            id: id.into(),
            file_id: None,
            bypass: None,
            quiet: 0,
            mtime: None,
            permissions: None,
            size: None,
            name: None,
            status: None,
            parent: None,
            data: Vec::new(),
            compression: None,
            file_type: None,
            transmission_type: None,
        }
    }

    fn approval(actions: &[TtyTransferAction]) -> &TtyTransferApprovalRequest {
        let [TtyTransferAction::RequestApproval(request)] = actions else {
            panic!("expected one approval request, got {actions:?}");
        };
        request
    }

    #[test]
    fn send_waits_for_exact_approval_before_emitting_ok_or_commands() {
        let mut manager = TtyTransferManager::new();
        let request_actions = manager.handle(command(FileTransferAction::Send, "send-1"));
        let request_id = approval(&request_actions).request_id;

        let mut early = command(FileTransferAction::File, "send-1");
        early.file_id = Some("f1".into());
        assert!(matches!(
            manager.handle(early).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
        assert_eq!(manager.active_sessions(), 0);
        assert!(manager.approve(request_id).is_empty());

        let request_actions = manager.handle(command(FileTransferAction::Send, "send-1"));
        let request_id = approval(&request_actions).request_id;
        assert!(matches!(
            manager.approve(request_id).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
        assert_eq!(manager.active_sessions(), 1);
    }

    #[test]
    fn duplicate_start_does_not_invalidate_original_approval() {
        let mut manager = TtyTransferManager::new();
        let request_id =
            approval(&manager.handle(command(FileTransferAction::Send, "duplicate"))).request_id;

        assert!(matches!(
            manager
                .handle(command(FileTransferAction::Send, "duplicate"))
                .as_slice(),
            [TtyTransferAction::Write(_)]
        ));
        assert!(manager.is_approval_pending(request_id));
        assert!(matches!(
            manager.approve(request_id).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
    }

    #[test]
    fn lifecycle_rejection_aborts_approved_staging_and_clears_state() {
        let mut manager = TtyTransferManager::new();
        let request_id =
            approval(&manager.handle(command(FileTransferAction::Send, "idle"))).request_id;
        manager.approve(request_id);

        let actions = manager.reject_session("idle", "ETIMEDOUT:Transfer session idle");

        assert!(matches!(
            actions.as_slice(),
            [TtyTransferAction::Abort { session_id }, TtyTransferAction::Write(_)]
                if session_id == "idle"
        ));
        assert_eq!(manager.active_sessions(), 0);
    }

    #[test]
    fn receive_collects_bounded_paths_before_requesting_consent() {
        let mut manager = TtyTransferManager::new();
        let mut start = command(FileTransferAction::Receive, "receive-1");
        start.size = Some(2);
        assert!(manager.handle(start).is_empty());

        let mut first = command(FileTransferAction::File, "receive-1");
        first.file_id = Some("f1".into());
        first.name = Some("~/one".into());
        assert!(manager.handle(first).is_empty());

        let mut second = command(FileTransferAction::File, "receive-1");
        second.file_id = Some("f2".into());
        second.name = Some("/tmp/two".into());
        let request_actions = manager.handle(second);
        let request = approval(&request_actions);
        assert_eq!(request.direction, TtyTransferDirection::Receive);
        assert_eq!(request.paths, ["~/one", "/tmp/two"]);

        let approved = manager.approve(request.request_id);
        assert!(matches!(
            approved.first(),
            Some(TtyTransferAction::Write(_))
        ));
        assert_eq!(
            approved
                .iter()
                .filter(|action| matches!(action, TtyTransferAction::Execute(_)))
                .count(),
            2
        );
        assert!(matches!(
            approved.last(),
            Some(TtyTransferAction::CompleteReceiveListing { session_id, quiet: 0 })
                if session_id == "receive-1"
        ));
    }

    #[test]
    fn receive_rejects_invalid_paths_and_oversized_request_sets() {
        let mut manager = TtyTransferManager::new();
        let mut start = command(FileTransferAction::Receive, "bad-path");
        start.size = Some(1);
        manager.handle(start);
        let mut path = command(FileTransferAction::File, "bad-path");
        path.file_id = Some("f1".into());
        path.name = Some("../outside".into());
        assert!(matches!(
            manager.handle(path).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
        assert_eq!(manager.active_sessions(), 0);

        let mut oversized = command(FileTransferAction::Receive, "too-many");
        oversized.size = Some((MAX_RECEIVE_PATHS + 1) as u64);
        assert!(matches!(
            manager.handle(oversized).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
    }

    #[test]
    fn ordinary_relative_paths_are_valid_home_relative_protocol_names() {
        let mut manager = TtyTransferManager::new();
        let request_id =
            approval(&manager.handle(command(FileTransferAction::Send, "relative"))).request_id;
        manager.approve(request_id);
        let mut file = command(FileTransferAction::File, "relative");
        file.file_id = Some("f1".into());
        file.name = Some("downloads/report.txt".into());

        assert!(matches!(
            manager.handle(file).as_slice(),
            [TtyTransferAction::Execute(_)]
        ));
    }

    #[test]
    fn send_only_manager_rejects_receive_before_requesting_consent() {
        let mut manager = TtyTransferManager::send_only();
        let mut start = command(FileTransferAction::Receive, "receive-disabled");
        start.size = Some(1);

        assert!(matches!(
            manager.handle(start).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
        assert_eq!(manager.active_sessions(), 0);
    }

    #[test]
    fn denial_and_cancel_honor_quiet_levels() {
        let mut manager = TtyTransferManager::new();
        let mut start = command(FileTransferAction::Send, "quiet-1");
        start.quiet = 1;
        let request_id = approval(&manager.handle(start)).request_id;
        assert!(matches!(
            manager.deny(request_id).as_slice(),
            [TtyTransferAction::Write(_)]
        ));

        let mut silent = command(FileTransferAction::Send, "quiet-2");
        silent.quiet = 2;
        let request_id = approval(&manager.handle(silent)).request_id;
        assert!(manager.deny(request_id).is_empty());

        let mut start = command(FileTransferAction::Send, "cancel");
        start.quiet = 1;
        manager.handle(start);
        assert!(manager
            .handle(command(FileTransferAction::Cancel, "cancel"))
            .is_empty());
    }

    #[test]
    fn stale_approval_token_cannot_authorize_reused_session_id() {
        let mut manager = TtyTransferManager::new();
        let stale = approval(&manager.handle(command(FileTransferAction::Send, "same"))).request_id;
        manager.deny(stale);
        let current =
            approval(&manager.handle(command(FileTransferAction::Send, "same"))).request_id;
        assert_ne!(stale, current);
        assert!(manager.approve(stale).is_empty());
        assert!(matches!(
            manager.approve(current).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
    }

    #[test]
    fn approved_sessions_forward_only_direction_valid_actions() {
        let mut manager = TtyTransferManager::new();
        let request_id =
            approval(&manager.handle(command(FileTransferAction::Send, "send"))).request_id;
        manager.approve(request_id);
        let mut data = command(FileTransferAction::Data, "send");
        data.file_id = Some("f1".into());
        assert!(matches!(
            manager.handle(data).as_slice(),
            [TtyTransferAction::Execute(_)]
        ));
        assert!(matches!(
            manager
                .handle(command(FileTransferAction::Finished, "send"))
                .as_slice(),
            [TtyTransferAction::Abort { .. }, TtyTransferAction::Write(_)]
        ));
        assert_eq!(manager.active_sessions(), 0);
    }

    #[test]
    fn parser_to_authorization_pipeline_never_executes_before_consent() {
        let mut screen = cterm_core::Screen::new(80, 24, Default::default());
        let mut parser = cterm_core::Parser::new();
        parser.parse(&mut screen, b"\x1b]5113;ac=send;id=pipeline\x1b\\");

        let commands = screen.take_kitty_file_transfer_commands();
        let [command] = commands.as_slice() else {
            panic!("expected one decoded command");
        };
        let mut manager = TtyTransferManager::new();
        let actions = manager.handle(command.clone());
        assert!(!actions
            .iter()
            .any(|action| matches!(action, TtyTransferAction::Execute(_))));

        let request_id = approval(&actions).request_id;
        assert!(matches!(
            manager.approve(request_id).as_slice(),
            [TtyTransferAction::Write(_)]
        ));
    }
}
