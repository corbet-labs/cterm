//! Keycode conversion for macOS
//!
//! Maps macOS virtual key codes to cterm-ui KeyCode enum.

use cterm_core::term::{Key, NamedKey};
use cterm_core::{KeyEventKind, KeyEventMetadata};
use cterm_ui::events::{KeyCode, Modifiers};
use objc2_app_kit::{NSEvent, NSEventModifierFlags};

pub(crate) fn key_event_kind(is_repeat: bool) -> KeyEventKind {
    if is_repeat {
        KeyEventKind::Repeat
    } else {
        KeyEventKind::Press
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReportedKey {
    pub(crate) key: Key,
    pub(crate) shifted_key: Option<char>,
    pub(crate) base_layout_key: Option<char>,
}

impl ReportedKey {
    pub(crate) fn metadata(self) -> KeyEventMetadata<'static> {
        KeyEventMetadata::new()
            .with_shifted_key(self.shifted_key)
            .with_base_layout_key(self.base_layout_key)
    }
}

pub(crate) fn terminal_key_for_keycode(keycode: u16) -> Option<Key> {
    Some(match keycode {
        0x7E => Key::Up,
        0x7D => Key::Down,
        0x7B => Key::Left,
        0x7C => Key::Right,
        0x73 => Key::Home,
        0x77 => Key::End,
        0x74 => Key::PageUp,
        0x79 => Key::PageDown,
        0x72 => Key::Insert,
        0x75 => Key::Delete,
        0x33 => Key::Backspace,
        0x24 => Key::Enter,
        0x30 => Key::Tab,
        0x35 => Key::Escape,
        0x52 => Key::NumpadDigit(0),
        0x53 => Key::NumpadDigit(1),
        0x54 => Key::NumpadDigit(2),
        0x55 => Key::NumpadDigit(3),
        0x56 => Key::NumpadDigit(4),
        0x57 => Key::NumpadDigit(5),
        0x58 => Key::NumpadDigit(6),
        0x59 => Key::NumpadDigit(7),
        0x5B => Key::NumpadDigit(8),
        0x5C => Key::NumpadDigit(9),
        0x41 => Key::NumpadDecimal,
        0x4B => Key::NumpadDivide,
        0x43 => Key::NumpadMultiply,
        0x4E => Key::NumpadSubtract,
        0x45 => Key::NumpadAdd,
        0x4C => Key::NumpadEnter,
        0x51 => Key::Named(NamedKey::NumpadEqual),
        0x7A => Key::F(1),
        0x78 => Key::F(2),
        0x63 => Key::F(3),
        0x76 => Key::F(4),
        0x60 => Key::F(5),
        0x61 => Key::F(6),
        0x62 => Key::F(7),
        0x64 => Key::F(8),
        0x65 => Key::F(9),
        0x6D => Key::F(10),
        0x67 => Key::F(11),
        0x6F => Key::F(12),
        0x69 => Key::F(13),
        0x6B => Key::F(14),
        0x71 => Key::F(15),
        0x6A => Key::F(16),
        0x40 => Key::F(17),
        0x4F => Key::F(18),
        0x50 => Key::F(19),
        0x5A => Key::F(20),
        _ => return None,
    })
}

pub(crate) fn modifier_key_for_keycode(keycode: u16) -> Option<(NamedKey, Modifiers)> {
    Some(match keycode {
        0x38 => (NamedKey::LeftShift, Modifiers::SHIFT),
        0x3C => (NamedKey::RightShift, Modifiers::SHIFT),
        0x3B => (NamedKey::LeftControl, Modifiers::CTRL),
        0x3E => (NamedKey::RightControl, Modifiers::CTRL),
        0x3A => (NamedKey::LeftAlt, Modifiers::ALT),
        0x3D => (NamedKey::RightAlt, Modifiers::ALT),
        0x37 => (NamedKey::LeftSuper, Modifiers::SUPER),
        0x36 => (NamedKey::RightSuper, Modifiers::SUPER),
        0x39 => (NamedKey::CapsLock, Modifiers::CAPS_LOCK),
        _ => return None,
    })
}

pub(crate) fn exactly_one_char(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let character = chars.next()?;
    chars.next().is_none().then_some(character)
}

pub(crate) fn unmodified_key_char(text: &str) -> Option<char> {
    let character = exactly_one_char(text)?;
    let mut lowercase = character.to_lowercase();
    let character = lowercase.next()?;
    lowercase.next().is_none().then_some(character)
}

pub(crate) fn direct_modified_key_char(
    event_text: &str,
    base_text: &str,
    alt_may_produce_text: bool,
) -> Option<char> {
    let event = exactly_one_char(event_text)?;
    let base = unmodified_key_char(base_text)?;
    if alt_may_produce_text && !event.is_control() {
        let mut lowercase = event.to_lowercase();
        if lowercase.next() != Some(base) || lowercase.next().is_some() {
            return None;
        }
    }
    Some(base)
}

/// Convert NSEvent modifier flags to our Modifiers
pub fn modifiers_from_event(event: &NSEvent) -> Modifiers {
    let flags = event.modifierFlags();
    let mut modifiers = Modifiers::empty();

    if flags.contains(NSEventModifierFlags::Shift) {
        modifiers.insert(Modifiers::SHIFT);
    }
    if flags.contains(NSEventModifierFlags::Control) {
        modifiers.insert(Modifiers::CTRL);
    }
    if flags.contains(NSEventModifierFlags::Option) {
        modifiers.insert(Modifiers::ALT);
    }
    if flags.contains(NSEventModifierFlags::Command) {
        modifiers.insert(Modifiers::SUPER);
    }
    if flags.contains(NSEventModifierFlags::CapsLock) {
        modifiers.insert(Modifiers::CAPS_LOCK);
    }

    modifiers
}

/// Convert macOS virtual key code to our KeyCode
pub fn keycode_from_event(event: &NSEvent) -> Option<KeyCode> {
    let keycode = event.keyCode();

    // macOS virtual key codes (from Carbon HIToolbox/Events.h)
    Some(match keycode {
        // Letters (QWERTY layout key positions)
        0x00 => KeyCode::A,
        0x0B => KeyCode::B,
        0x08 => KeyCode::C,
        0x02 => KeyCode::D,
        0x0E => KeyCode::E,
        0x03 => KeyCode::F,
        0x05 => KeyCode::G,
        0x04 => KeyCode::H,
        0x22 => KeyCode::I,
        0x26 => KeyCode::J,
        0x28 => KeyCode::K,
        0x25 => KeyCode::L,
        0x2E => KeyCode::M,
        0x2D => KeyCode::N,
        0x1F => KeyCode::O,
        0x23 => KeyCode::P,
        0x0C => KeyCode::Q,
        0x0F => KeyCode::R,
        0x01 => KeyCode::S,
        0x11 => KeyCode::T,
        0x20 => KeyCode::U,
        0x09 => KeyCode::V,
        0x0D => KeyCode::W,
        0x07 => KeyCode::X,
        0x10 => KeyCode::Y,
        0x06 => KeyCode::Z,

        // Numbers
        0x1D => KeyCode::Key0,
        0x12 => KeyCode::Key1,
        0x13 => KeyCode::Key2,
        0x14 => KeyCode::Key3,
        0x15 => KeyCode::Key4,
        0x17 => KeyCode::Key5,
        0x16 => KeyCode::Key6,
        0x1A => KeyCode::Key7,
        0x1C => KeyCode::Key8,
        0x19 => KeyCode::Key9,

        // Function keys
        0x7A => KeyCode::F1,
        0x78 => KeyCode::F2,
        0x63 => KeyCode::F3,
        0x76 => KeyCode::F4,
        0x60 => KeyCode::F5,
        0x61 => KeyCode::F6,
        0x62 => KeyCode::F7,
        0x64 => KeyCode::F8,
        0x65 => KeyCode::F9,
        0x6D => KeyCode::F10,
        0x67 => KeyCode::F11,
        0x6F => KeyCode::F12,

        // Navigation
        0x7E => KeyCode::Up,
        0x7D => KeyCode::Down,
        0x7B => KeyCode::Left,
        0x7C => KeyCode::Right,
        0x73 => KeyCode::Home,
        0x77 => KeyCode::End,
        0x74 => KeyCode::PageUp,
        0x79 => KeyCode::PageDown,

        // Editing
        0x72 => KeyCode::Insert, // Help key on Mac, often mapped to Insert
        0x75 => KeyCode::Delete, // Forward delete
        0x33 => KeyCode::Backspace,
        0x24 => KeyCode::Enter,
        0x30 => KeyCode::Tab,

        // Special
        0x35 => KeyCode::Escape,
        0x31 => KeyCode::Space,

        // Punctuation
        0x1B => KeyCode::Minus,
        0x18 => KeyCode::Equals,
        0x21 => KeyCode::LeftBracket,
        0x1E => KeyCode::RightBracket,
        0x29 => KeyCode::Semicolon,
        0x27 => KeyCode::Quote,
        0x32 => KeyCode::Backquote,
        0x2A => KeyCode::Backslash,
        0x2B => KeyCode::Comma,
        0x2F => KeyCode::Period,
        0x2C => KeyCode::Slash,

        // Numpad
        0x52 => KeyCode::Numpad0,
        0x53 => KeyCode::Numpad1,
        0x54 => KeyCode::Numpad2,
        0x55 => KeyCode::Numpad3,
        0x56 => KeyCode::Numpad4,
        0x57 => KeyCode::Numpad5,
        0x58 => KeyCode::Numpad6,
        0x59 => KeyCode::Numpad7,
        0x5B => KeyCode::Numpad8,
        0x5C => KeyCode::Numpad9,
        0x45 => KeyCode::NumpadAdd,
        0x4E => KeyCode::NumpadSubtract,
        0x43 => KeyCode::NumpadMultiply,
        0x4B => KeyCode::NumpadDivide,
        0x41 => KeyCode::NumpadDecimal,
        0x4C => KeyCode::NumpadEnter,

        _ => return None,
    })
}

/// Get the character string from an NSEvent
pub fn characters_from_event(event: &NSEvent) -> Option<String> {
    event.characters().map(|s| s.to_string())
}

/// Get the character string ignoring modifiers (useful for Ctrl+key combinations)
pub fn characters_ignoring_modifiers(event: &NSEvent) -> Option<String> {
    event.charactersIgnoringModifiers().map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_press_and_repeat_event_kinds() {
        assert_eq!(key_event_kind(false), KeyEventKind::Press);
        assert_eq!(key_event_kind(true), KeyEventKind::Repeat);
    }

    #[test]
    fn maps_functional_terminal_keycodes() {
        assert_eq!(terminal_key_for_keycode(0x7E), Some(Key::Up));
        assert_eq!(terminal_key_for_keycode(0x75), Some(Key::Delete));
        assert_eq!(terminal_key_for_keycode(0x7A), Some(Key::F(1)));
        assert_eq!(terminal_key_for_keycode(0x6F), Some(Key::F(12)));
        assert_eq!(terminal_key_for_keycode(0x57), Some(Key::NumpadDigit(5)));
        assert_eq!(terminal_key_for_keycode(0x4C), Some(Key::NumpadEnter));
        assert_eq!(terminal_key_for_keycode(0x5A), Some(Key::F(20)));
        assert_eq!(
            terminal_key_for_keycode(0x51),
            Some(Key::Named(NamedKey::NumpadEqual))
        );
        assert_eq!(terminal_key_for_keycode(0x00), None);
    }

    #[test]
    fn modifier_keycodes_preserve_left_and_right_kitty_identity() {
        assert_eq!(
            modifier_key_for_keycode(0x38),
            Some((NamedKey::LeftShift, Modifiers::SHIFT))
        );
        assert_eq!(
            modifier_key_for_keycode(0x36),
            Some((NamedKey::RightSuper, Modifiers::SUPER))
        );
        assert_eq!(
            modifier_key_for_keycode(0x39),
            Some((NamedKey::CapsLock, Modifiers::CAPS_LOCK))
        );
    }

    #[test]
    fn accepts_only_one_unicode_scalar_for_direct_reporting() {
        assert_eq!(exactly_one_char("a"), Some('a'));
        assert_eq!(exactly_one_char("é"), Some('é'));
        assert_eq!(exactly_one_char(""), None);
        assert_eq!(exactly_one_char("ab"), None);
        assert_eq!(exactly_one_char("e\u{301}"), None);
        assert_eq!(unmodified_key_char("A"), Some('a'));
        assert_eq!(unmodified_key_char("İ"), None);
        assert_eq!(direct_modified_key_char("a", "a", true), Some('a'));
        assert_eq!(direct_modified_key_char("A", "A", true), Some('a'));
        assert_eq!(direct_modified_key_char("å", "a", true), None);
        assert_eq!(direct_modified_key_char("\u{1}", "A", false), Some('a'));
        assert_eq!(direct_modified_key_char("\u{1}", "A", true), Some('a'));
    }
}
