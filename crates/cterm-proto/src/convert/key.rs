//! Key conversion between proto and cterm-core

use crate::proto;
use cterm_core::term::{Key, Modifiers, NamedKey};

/// Convert proto Key to cterm_core Key
pub fn proto_to_key(key: &proto::Key) -> Option<Key> {
    use proto::key::KeyType;
    use proto::SpecialKey;

    match &key.key_type {
        Some(KeyType::Char(s)) => s.chars().next().map(Key::Char),
        Some(KeyType::Special(special)) => {
            let special = SpecialKey::try_from(*special).ok()?;
            match special {
                SpecialKey::Unspecified => None,
                SpecialKey::Enter => Some(Key::Enter),
                SpecialKey::Tab => Some(Key::Tab),
                SpecialKey::Backspace => Some(Key::Backspace),
                SpecialKey::Escape => Some(Key::Escape),
                SpecialKey::Up => Some(Key::Up),
                SpecialKey::Down => Some(Key::Down),
                SpecialKey::Left => Some(Key::Left),
                SpecialKey::Right => Some(Key::Right),
                SpecialKey::Home => Some(Key::Home),
                SpecialKey::End => Some(Key::End),
                SpecialKey::PageUp => Some(Key::PageUp),
                SpecialKey::PageDown => Some(Key::PageDown),
                SpecialKey::Insert => Some(Key::Insert),
                SpecialKey::Delete => Some(Key::Delete),
                SpecialKey::Numpad0 => Some(Key::NumpadDigit(0)),
                SpecialKey::Numpad1 => Some(Key::NumpadDigit(1)),
                SpecialKey::Numpad2 => Some(Key::NumpadDigit(2)),
                SpecialKey::Numpad3 => Some(Key::NumpadDigit(3)),
                SpecialKey::Numpad4 => Some(Key::NumpadDigit(4)),
                SpecialKey::Numpad5 => Some(Key::NumpadDigit(5)),
                SpecialKey::Numpad6 => Some(Key::NumpadDigit(6)),
                SpecialKey::Numpad7 => Some(Key::NumpadDigit(7)),
                SpecialKey::Numpad8 => Some(Key::NumpadDigit(8)),
                SpecialKey::Numpad9 => Some(Key::NumpadDigit(9)),
                SpecialKey::NumpadDecimal => Some(Key::NumpadDecimal),
                SpecialKey::NumpadDivide => Some(Key::NumpadDivide),
                SpecialKey::NumpadMultiply => Some(Key::NumpadMultiply),
                SpecialKey::NumpadSubtract => Some(Key::NumpadSubtract),
                SpecialKey::NumpadAdd => Some(Key::NumpadAdd),
                SpecialKey::NumpadEnter => Some(Key::NumpadEnter),
            }
        }
        Some(KeyType::Function(n)) => {
            if *n >= 1 && *n <= 35 {
                Some(Key::F(*n as u8))
            } else {
                None
            }
        }
        Some(KeyType::KittyNamed(code)) => NamedKey::from_kitty_code(*code).map(Key::Named),
        None => None,
    }
}

/// Convert proto Modifiers to cterm_core Modifiers
pub fn proto_to_modifiers(modifiers: &proto::Modifiers) -> Modifiers {
    let mut result = Modifiers::empty();
    if modifiers.shift {
        result |= Modifiers::SHIFT;
    }
    if modifiers.ctrl {
        result |= Modifiers::CTRL;
    }
    if modifiers.alt {
        result |= Modifiers::ALT;
    }
    if modifiers.super_ {
        result |= Modifiers::SUPER;
    }
    if modifiers.hyper {
        result |= Modifiers::HYPER;
    }
    if modifiers.meta {
        result |= Modifiers::META;
    }
    if modifiers.caps_lock {
        result |= Modifiers::CAPS_LOCK;
    }
    if modifiers.num_lock {
        result |= Modifiers::NUM_LOCK;
    }
    result
}

/// Convert cterm_core Key to proto Key
pub fn key_to_proto(key: Key) -> proto::Key {
    use proto::key::KeyType;
    use proto::SpecialKey;

    let key_type = match key {
        Key::Char(c) => Some(KeyType::Char(c.to_string())),
        Key::Enter => Some(KeyType::Special(SpecialKey::Enter as i32)),
        Key::Tab => Some(KeyType::Special(SpecialKey::Tab as i32)),
        Key::Backspace => Some(KeyType::Special(SpecialKey::Backspace as i32)),
        Key::Escape => Some(KeyType::Special(SpecialKey::Escape as i32)),
        Key::Up => Some(KeyType::Special(SpecialKey::Up as i32)),
        Key::Down => Some(KeyType::Special(SpecialKey::Down as i32)),
        Key::Left => Some(KeyType::Special(SpecialKey::Left as i32)),
        Key::Right => Some(KeyType::Special(SpecialKey::Right as i32)),
        Key::Home => Some(KeyType::Special(SpecialKey::Home as i32)),
        Key::End => Some(KeyType::Special(SpecialKey::End as i32)),
        Key::PageUp => Some(KeyType::Special(SpecialKey::PageUp as i32)),
        Key::PageDown => Some(KeyType::Special(SpecialKey::PageDown as i32)),
        Key::Insert => Some(KeyType::Special(SpecialKey::Insert as i32)),
        Key::Delete => Some(KeyType::Special(SpecialKey::Delete as i32)),
        Key::F(n) => Some(KeyType::Function(n as u32)),
        Key::NumpadDigit(digit) => {
            let special = match digit {
                0 => SpecialKey::Numpad0,
                1 => SpecialKey::Numpad1,
                2 => SpecialKey::Numpad2,
                3 => SpecialKey::Numpad3,
                4 => SpecialKey::Numpad4,
                5 => SpecialKey::Numpad5,
                6 => SpecialKey::Numpad6,
                7 => SpecialKey::Numpad7,
                8 => SpecialKey::Numpad8,
                9 => SpecialKey::Numpad9,
                _ => SpecialKey::Unspecified,
            };
            Some(KeyType::Special(special as i32))
        }
        Key::NumpadDecimal => Some(KeyType::Special(SpecialKey::NumpadDecimal as i32)),
        Key::NumpadDivide => Some(KeyType::Special(SpecialKey::NumpadDivide as i32)),
        Key::NumpadMultiply => Some(KeyType::Special(SpecialKey::NumpadMultiply as i32)),
        Key::NumpadSubtract => Some(KeyType::Special(SpecialKey::NumpadSubtract as i32)),
        Key::NumpadAdd => Some(KeyType::Special(SpecialKey::NumpadAdd as i32)),
        Key::NumpadEnter => Some(KeyType::Special(SpecialKey::NumpadEnter as i32)),
        Key::Named(named) => Some(KeyType::KittyNamed(named.kitty_code())),
    };

    proto::Key { key_type }
}

/// Convert cterm_core Modifiers to proto Modifiers
pub fn modifiers_to_proto(modifiers: Modifiers) -> proto::Modifiers {
    proto::Modifiers {
        shift: modifiers.contains(Modifiers::SHIFT),
        ctrl: modifiers.contains(Modifiers::CTRL),
        alt: modifiers.contains(Modifiers::ALT),
        super_: modifiers.contains(Modifiers::SUPER),
        hyper: modifiers.contains(Modifiers::HYPER),
        meta: modifiers.contains(Modifiers::META),
        caps_lock: modifiers.contains(Modifiers::CAPS_LOCK),
        num_lock: modifiers.contains(Modifiers::NUM_LOCK),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_key_roundtrip() {
        let key = Key::Char('a');
        let proto = key_to_proto(key);
        let back = proto_to_key(&proto);
        assert_eq!(back, Some(key));
    }

    #[test]
    fn test_special_key_roundtrip() {
        let key = Key::Enter;
        let proto = key_to_proto(key);
        let back = proto_to_key(&proto);
        assert_eq!(back, Some(key));
    }

    #[test]
    fn test_numeric_keypad_roundtrip() {
        for key in [
            Key::NumpadDigit(0),
            Key::NumpadDigit(9),
            Key::NumpadDecimal,
            Key::NumpadDivide,
            Key::NumpadMultiply,
            Key::NumpadSubtract,
            Key::NumpadAdd,
            Key::NumpadEnter,
        ] {
            let proto = key_to_proto(key);
            assert_eq!(proto_to_key(&proto), Some(key));
        }
    }

    #[test]
    fn test_modifiers_roundtrip() {
        let mods = Modifiers::SHIFT
            | Modifiers::CTRL
            | Modifiers::HYPER
            | Modifiers::META
            | Modifiers::CAPS_LOCK
            | Modifiers::NUM_LOCK;
        let proto = modifiers_to_proto(mods);
        let back = proto_to_modifiers(&proto);
        assert_eq!(back, mods);
    }

    #[test]
    fn test_kitty_named_and_extended_function_roundtrip() {
        for key in [Key::Named(NamedKey::MediaPlay), Key::F(35)] {
            let proto = key_to_proto(key);
            assert_eq!(proto_to_key(&proto), Some(key));
        }
    }
}
