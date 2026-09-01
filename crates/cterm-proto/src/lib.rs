//! cterm-proto: Protobuf definitions and type conversions for the cterm gRPC protocol
//!
//! This crate contains the shared protocol definitions used by both ctermd (daemon)
//! and cterm (UI client) for communication over Unix sockets or SSH.

pub mod convert;

mod daemon_auth;

#[cfg(windows)]
pub use daemon_auth::set_private_daemon_auth_file_acl;
pub use daemon_auth::{
    load_daemon_auth_secret, managed_daemon_auth_proof, verify_managed_daemon_auth_proof,
    DaemonAuthSecret, DAEMON_AUTH_CHALLENGE_BYTES, DAEMON_AUTH_SECRET_BYTES,
};
#[cfg(unix)]
pub use daemon_auth::{validate_private_daemon_directory, validate_private_daemon_socket};

/// Wire protocol implemented by this cterm UI/daemon pair.
///
/// OSC 5113 consent is an additive protobuf extension: old frontends ignore its
/// unknown event and the daemon denies on timeout, while new frontends attached
/// to an old daemon simply do not receive transfer requests. Keeping this at v1
/// preserves existing sessions during rolling UI/daemon upgrades. Increment the
/// version only for a wire- or semantics-incompatible change. Managed clients
/// independently require an exact package version and authenticated identity.
pub const PROTOCOL_VERSION: u32 = 1;

/// Generated protobuf and gRPC code
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("cterm.terminal");
}

#[cfg(test)]
mod tests {
    use prost::Message;

    /// Minimal stand-in for the v1 `TerminalEvent` schema before OSC 5113 was
    /// added. Protobuf readers must discard the new oneof tag instead of
    /// rejecting the stream used by an already-running frontend.
    #[derive(Clone, PartialEq, Message)]
    struct LegacyTerminalEvent {
        #[prost(bytes = "vec", tag = "1")]
        process_exited: Vec<u8>,
    }

    #[test]
    fn osc_5113_event_remains_wire_compatible_with_v1_frontends() {
        assert_eq!(super::PROTOCOL_VERSION, 1);

        let current = super::proto::TerminalEvent {
            event: Some(
                super::proto::terminal_event::Event::TtyFileTransferApproval(
                    super::proto::TtyFileTransferApprovalEvent {
                        request_id: 7,
                        transfer_id: "transfer".to_string(),
                        direction: super::proto::TtyFileTransferDirection::Send as i32,
                        paths: Vec::new(),
                        expires_in_ms: 60_000,
                        max_files: 16,
                        max_file_bytes: 1_024,
                        max_session_bytes: 4_096,
                    },
                ),
            ),
        };

        let legacy = LegacyTerminalEvent::decode(current.encode_to_vec().as_slice()).unwrap();
        assert!(legacy.process_exited.is_empty());
    }
}
