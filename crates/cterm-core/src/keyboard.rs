//! Kitty keyboard progressive-enhancement state.

use bitflags::bitflags;

bitflags! {
    /// Kitty keyboard progressive-enhancement flags.
    ///
    /// The bit values are part of the wire protocol and must remain stable.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct KeyboardEnhancementFlags: u8 {
        const DISAMBIGUATE_ESCAPE_CODES = 1 << 0;
        const REPORT_EVENT_TYPES = 1 << 1;
        const REPORT_ALTERNATE_KEYS = 1 << 2;
        const REPORT_ALL_KEYS_AS_ESCAPE_CODES = 1 << 3;
        const REPORT_ASSOCIATED_TEXT = 1 << 4;
    }
}

impl KeyboardEnhancementFlags {
    /// Flags that may be enabled by an application.
    pub const SUPPORTED: Self = Self::from_bits_retain(0b1_1111);
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

/// Optional layout and text data attached to a physical key event.
///
/// Native frontends fill in only the information their input system exposes.
/// In particular, `base_layout_key` is optional: omitting it is preferable to
/// inventing a US-layout identity for an unknown physical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct KeyEventMetadata<'a> {
    /// Shifted value in the active layout. It is emitted only while Shift is on.
    pub shifted_key: Option<char>,
    /// Value at this physical position in the standard PC-101 layout.
    pub base_layout_key: Option<char>,
    /// Text produced by the event after layout/IME processing.
    pub associated_text: Option<&'a str>,
}

impl<'a> KeyEventMetadata<'a> {
    pub const fn new() -> Self {
        Self {
            shifted_key: None,
            base_layout_key: None,
            associated_text: None,
        }
    }

    pub const fn with_shifted_key(mut self, shifted_key: Option<char>) -> Self {
        self.shifted_key = shifted_key;
        self
    }

    pub const fn with_base_layout_key(mut self, base_layout_key: Option<char>) -> Self {
        self.base_layout_key = base_layout_key;
        self
    }

    pub const fn with_associated_text(mut self, associated_text: Option<&'a str>) -> Self {
        self.associated_text = associated_text;
        self
    }
}
