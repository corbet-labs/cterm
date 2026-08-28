//! Kitty keyboard progressive-enhancement state.

use bitflags::bitflags;

bitflags! {
    /// Progressive keyboard enhancements implemented by cterm.
    ///
    /// The values are defined by the kitty keyboard protocol. All-key,
    /// alternate-key, and associated-text reporting are deliberately not
    /// advertised until every UI backend can provide layout/IME key identity
    /// consistently.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct KeyboardEnhancementFlags: u8 {
        const DISAMBIGUATE_ESCAPE_CODES = 1 << 0;
        const REPORT_EVENT_TYPES = 1 << 1;
        const REPORT_ALL_KEYS_AS_ESCAPE_CODES = 1 << 3;
    }
}

impl KeyboardEnhancementFlags {
    /// Flags that may be enabled by an application.
    pub const SUPPORTED: Self = Self::from_bits_retain(
        Self::DISAMBIGUATE_ESCAPE_CODES.bits() | Self::REPORT_EVENT_TYPES.bits(),
    );
}

/// Physical key event kind used by the enhanced keyboard protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyEventKind {
    Press,
    Repeat,
    Release,
}

impl KeyEventKind {
    pub(crate) fn protocol_value(self) -> u8 {
        match self {
            Self::Press => 1,
            Self::Repeat => 2,
            Self::Release => 3,
        }
    }
}
