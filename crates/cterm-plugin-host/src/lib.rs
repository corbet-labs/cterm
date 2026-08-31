//! One-shot process boundary for cterm command plugins.
//!
//! This crate intentionally has no dependency on cterm's daemon, UI, or native
//! frontends. The application broker is expected to launch the sibling
//! `cterm-plugin-host` executable once per invocation and enforce a wall-clock
//! timeout around that child process.

mod framing;
mod runtime;

pub use cterm_plugin_api::PLUGIN_HOST_EXECUTABLE_NAME as HOST_EXECUTABLE_NAME;
pub use framing::{read_bounded, BoundedReadError};
pub use runtime::{invoke, InvocationLimits, InvocationOutput, RunnerError};
