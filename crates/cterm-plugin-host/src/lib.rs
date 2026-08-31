//! One-shot process boundary for cterm command plugins.
//!
//! This crate intentionally has no dependency on cterm's daemon, UI, or native
//! frontends. The application broker is expected to launch the sibling
//! `cterm-plugin-host` executable once per invocation and enforce a wall-clock
//! timeout around that child process.

mod framing;
mod runtime;

pub use framing::{read_bounded, BoundedReadError};
pub use runtime::{invoke, InvocationLimits, InvocationOutput, RunnerError};

/// Package-relative executable name reserved for the application broker.
#[cfg(windows)]
pub const HOST_EXECUTABLE_NAME: &str = "cterm-plugin-host.exe";

/// Package-relative executable name reserved for the application broker.
#[cfg(not(windows))]
pub const HOST_EXECUTABLE_NAME: &str = "cterm-plugin-host";
