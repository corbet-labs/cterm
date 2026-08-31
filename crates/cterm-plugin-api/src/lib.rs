//! Stable contracts shared by cterm, command plugins, and the isolated runner.
//!
//! The ABI is intentionally independent of cterm's Rust UI enums. The local
//! application broker performs explicit conversions and remains the authority
//! for managed-mode and native action policy.

mod bundle;
mod grants;
mod wire;

pub use bundle::{
    ActionScope, BundleDigest, BundleLimits, CommandId, PluginBundle, PluginCommand, PluginId,
    PluginManifest, PluginPackageError, ABI_MAJOR, ABI_MINOR, MANIFEST_FILE, MANIFEST_VERSION,
    MODULE_FILE,
};
pub use grants::{GrantDecision, GrantError, GrantStore};
pub use wire::{
    decode_request_frame, decode_response_frame, encode_request_frame, encode_response_frame,
    validate_request, validate_response, WireError, MAX_ACTIONS, MAX_DIAGNOSTICS,
    MAX_DIAGNOSTIC_BYTES, MAX_FRAME_BYTES,
};

/// Generated, backwards-compatible protobuf ABI.
pub mod proto {
    include!(concat!(env!("OUT_DIR"), "/cterm.plugin.v1.rs"));
}

/// Package-relative executable name used by the application broker.
#[cfg(windows)]
pub const PLUGIN_HOST_EXECUTABLE_NAME: &str = "cterm-plugin-host.exe";

/// Package-relative executable name used by the application broker.
#[cfg(not(windows))]
pub const PLUGIN_HOST_EXECUTABLE_NAME: &str = "cterm-plugin-host";
