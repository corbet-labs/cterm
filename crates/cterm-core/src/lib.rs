//! cterm-core: Core terminal emulation library
//!
//! This crate provides the fundamental building blocks for terminal emulation:
//! - Color and cell attribute types
//! - Screen buffer management (grid, scrollback)
//! - ANSI/VT sequence parsing
//! - Cross-platform PTY handling

pub mod cell;
pub mod color;
pub mod drcs;
pub mod grid;
pub mod image_decode;
pub mod iterm2;
pub mod keyboard;
mod kitty_graphics;
mod kitty_placeholder;
pub mod mouse;
mod multiple_cursors;
mod osc1337;
pub mod parser;
pub mod pty;
pub mod screen;
pub mod sixel;
pub mod ssh;
pub mod streaming_file;
pub mod term;

pub use cell::{Cell, CellAttrs};
pub use color::{AnsiColor, Color, ColorPalette, Rgb};
pub use drcs::{DecdldDecoder, DrcsFont, DrcsGlyph};
pub use grid::Grid;
pub use image_decode::{decode_image, DecodedImage, ImageDecodeError};
pub use iterm2::{Iterm2Dimension, Iterm2FileParams};
pub use keyboard::{KeyEventKind, KeyEventMetadata, KeyboardEnhancementFlags};
pub use kitty_graphics::GraphicsAnimationTick;
pub use multiple_cursors::{ExtraCursor, ExtraCursorColor, ExtraCursorColors, ExtraCursorShape};
pub use parser::Parser;
#[cfg(unix)]
pub use pty::save_original_nofile_limit;
pub use pty::{Pty, PtyConfig, PtyError, PtySize};
pub use screen::{
    ClipboardOperation, ClipboardSelection, ColorQuery, CursorStyle, DesktopNotification,
    DesktopNotificationAction, FileTransferOperation, FrontendState, ImageLayer, MouseEncoding,
    MouseMode, NotificationUrgency, Screen, SearchResult, Selection, SelectionMode, SelectionPoint,
    TerminalImage, ThemeAppearance, WindowVisibility,
};
pub use sixel::{SixelDecoder, SixelImage};
pub use ssh::{
    HostKeyPrompt, HostKeyRequest, LocalForward, PassphrasePrompt, PasswordPrompt, SshConfig,
    SshPrompts,
};
#[cfg(unix)]
pub use ssh::{SshChannelOpener, SshChannelReader, SshChannelWriter, SshTunnel};
pub use streaming_file::{StreamingFileData, StreamingFileReceiver, StreamingFileResult};
pub use term::{Terminal, WriteFn};
