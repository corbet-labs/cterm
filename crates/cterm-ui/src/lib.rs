//! cterm-ui: UI abstraction layer
//!
//! This crate defines traits and types for the UI layer, allowing
//! different UI backends (GTK4, Qt, etc.) to implement the terminal
//! interface.

pub mod blink;
pub mod events;
pub mod pane;
pub mod sprite;
pub mod theme;
pub mod traits;
pub mod utils;

pub use blink::*;
pub use events::*;
pub use pane::*;
pub use theme::*;
pub use traits::*;
pub use utils::*;
