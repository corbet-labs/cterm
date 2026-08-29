//! cterm-proto: Protobuf definitions and type conversions for the cterm gRPC protocol
//!
//! This crate contains the shared protocol definitions used by both ctermd (daemon)
//! and cterm (UI client) for communication over Unix sockets or SSH.

pub mod convert;

/// Wire protocol implemented by this cterm UI/daemon pair.
pub const PROTOCOL_VERSION: u32 = 1;

/// Generated protobuf and gRPC code
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("cterm.terminal");
}
