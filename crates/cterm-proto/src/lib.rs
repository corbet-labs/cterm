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
/// Managed-daemon authentication is an additive, domain-versioned extension
/// so a generic client can still connect to an already-running v1 daemon
/// during an upgrade. Managed clients independently require and verify the
/// authentication proof, and therefore still fail closed against old daemons.
pub const PROTOCOL_VERSION: u32 = 1;

/// Generated protobuf and gRPC code
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("cterm.terminal");
}
