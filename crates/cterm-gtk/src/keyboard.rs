//! Wayland/GDK keyboard adaptation for the shared terminal input model.

use cterm_core::term::{Key, Modifiers, NamedKey, Terminal};
use cterm_core::{KeyEventMetadata, KeyboardEnhancementFlags};
use gtk4::prelude::*;
use gtk4::{gdk, EventControllerKey};

#[derive(Debug, Clone, Copy)]
pub(crate) struct ReportedKey {
    pub(crate) key: Key,
    shifted_key: Option<char>,
    base_layout_key: Option<char>,
}

impl ReportedKey {
    pub(crate) fn metadata(self) -> KeyEventMetadata<'static> {
        KeyEventMetadata::new()
            .with_shifted_key(self.shifted_key)
            .with_base_layout_key(self.base_layout_key)
    }
}

/// Convert GTK modifier state to the shared terminal modifiers.
pub(crate) fn gtk_state_to_modifiers(state: gdk::ModifierType) -> Modifiers {
    let mut modifiers = Modifiers::empty();

    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        modifiers.insert(Modifiers::CTRL);
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        modifiers.insert(Modifiers::SHIFT);
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        modifiers.insert(Modifiers::ALT);
    }
    if state.contains(gdk::ModifierType::SUPER_MASK) {
        modifiers.insert(Modifiers::SUPER);
    }
    if state.contains(gdk::ModifierType::HYPER_MASK) {
        modifiers.insert(Modifiers::HYPER);
    }
    if state.contains(gdk::ModifierType::META_MASK) {
        modifiers.insert(Modifiers::META);
    }
    if state.contains(gdk::ModifierType::LOCK_MASK) {
        modifiers.insert(Modifiers::CAPS_LOCK);
    }

    modifiers
}

pub(crate) fn should_route_enhanced_key(term: &Terminal, key: Key, modifiers: Modifiers) -> bool {
    let flags = term.screen().keyboard_enhancement_flags();
    let events = flags.contains(KeyboardEnhancementFlags::REPORT_EVENT_TYPES);
    let all_keys = flags.contains(KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES);
    let disambiguate = flags.contains(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES);

    if all_keys {
        return true;
    }

    match key {
        Key::Char(_) => {
            disambiguate
                && modifiers.intersects(
                    Modifiers::CTRL
                        | Modifiers::ALT
                        | Modifiers::SUPER
                        | Modifiers::HYPER
                        | Modifiers::META,
                )
        }
        Key::Escape => disambiguate || events,
        Key::Enter | Key::Tab | Key::Backspace => false,
        Key::Named(named) if named.is_modifier() => false,
        _ => events,
    }
}

pub(crate) fn reported_key_from_gdk(
    controller: &EventControllerKey,
    keyval: gdk::Key,
    keycode: u32,
) -> Option<ReportedKey> {
    if let Some(key) = keyval_to_key(keyval) {
        return Some(ReportedKey {
            key,
            shifted_key: None,
            base_layout_key: None,
        });
    }

    let layout = controller
        .current_event()
        .as_ref()
        .and_then(|event| event.downcast_ref::<gdk::KeyEvent>())
        .map_or(0, |event| event.layout() as i32);
    let display = controller.widget()?.display();
    let unshifted = display
        .translate_key(keycode, gdk::ModifierType::empty(), layout)
        .and_then(|(translated, _, _, _)| translated.to_unicode())
        .and_then(single_lowercase)
        .or_else(|| keyval.to_lower().to_unicode());
    let shifted_key = display
        .translate_key(keycode, gdk::ModifierType::SHIFT_MASK, layout)
        .and_then(|(translated, _, _, _)| translated.to_unicode());

    Some(ReportedKey {
        key: Key::Char(unshifted?),
        shifted_key,
        base_layout_key: pc101_key_for_gdk_keycode(keycode),
    })
}

fn single_lowercase(character: char) -> Option<char> {
    let mut lowercase = character.to_lowercase();
    let character = lowercase.next()?;
    lowercase.next().is_none().then_some(character)
}

pub(crate) fn associated_text_for_gdk_key(
    key: Key,
    keyval: gdk::Key,
    modifiers: Modifiers,
) -> Option<String> {
    if !matches!(key, Key::Char(_))
        || modifiers
            .intersects(Modifiers::CTRL | Modifiers::SUPER | Modifiers::HYPER | Modifiers::META)
    {
        return None;
    }
    keyval
        .to_unicode()
        .filter(|character| !character.is_control())
        .map(|character| character.to_string())
}

/// Standard PC-101 identity for the XKB keycodes delivered by GDK/Wayland.
fn pc101_key_for_gdk_keycode(keycode: u32) -> Option<char> {
    Some(match keycode {
        10..=19 => "1234567890".chars().nth((keycode - 10) as usize)?,
        20 => '-',
        21 => '=',
        24..=35 => "qwertyuiop[]".chars().nth((keycode - 24) as usize)?,
        38..=49 => "asdfghjkl;'`".chars().nth((keycode - 38) as usize)?,
        51 => '\\',
        52..=61 => "zxcvbnm,./".chars().nth((keycode - 52) as usize)?,
        65 => ' ',
        _ => return None,
    })
}

/// Convert a GDK key value to the shared terminal key model.
pub(crate) fn keyval_to_key(keyval: gdk::Key) -> Option<Key> {
    use gdk::Key as GK;

    Some(match keyval {
        GK::Up => Key::Up,
        GK::Down => Key::Down,
        GK::Left => Key::Left,
        GK::Right => Key::Right,
        GK::Home => Key::Home,
        GK::End => Key::End,
        GK::Page_Up => Key::PageUp,
        GK::Page_Down => Key::PageDown,
        GK::Insert => Key::Insert,
        GK::Delete => Key::Delete,
        GK::BackSpace => Key::Backspace,
        GK::Return => Key::Enter,
        GK::KP_0 => Key::NumpadDigit(0),
        GK::KP_1 => Key::NumpadDigit(1),
        GK::KP_2 => Key::NumpadDigit(2),
        GK::KP_3 => Key::NumpadDigit(3),
        GK::KP_4 => Key::NumpadDigit(4),
        GK::KP_5 => Key::NumpadDigit(5),
        GK::KP_6 => Key::NumpadDigit(6),
        GK::KP_7 => Key::NumpadDigit(7),
        GK::KP_8 => Key::NumpadDigit(8),
        GK::KP_9 => Key::NumpadDigit(9),
        GK::KP_Decimal => Key::NumpadDecimal,
        GK::KP_Divide => Key::NumpadDivide,
        GK::KP_Multiply => Key::NumpadMultiply,
        GK::KP_Subtract => Key::NumpadSubtract,
        GK::KP_Add => Key::NumpadAdd,
        GK::KP_Enter => Key::NumpadEnter,
        GK::KP_Equal => Key::Named(NamedKey::NumpadEqual),
        GK::KP_Separator => Key::Named(NamedKey::NumpadSeparator),
        GK::KP_Left => Key::Named(NamedKey::NumpadLeft),
        GK::KP_Right => Key::Named(NamedKey::NumpadRight),
        GK::KP_Up => Key::Named(NamedKey::NumpadUp),
        GK::KP_Down => Key::Named(NamedKey::NumpadDown),
        GK::KP_Page_Up => Key::Named(NamedKey::NumpadPageUp),
        GK::KP_Page_Down => Key::Named(NamedKey::NumpadPageDown),
        GK::KP_Home => Key::Named(NamedKey::NumpadHome),
        GK::KP_End => Key::Named(NamedKey::NumpadEnd),
        GK::KP_Insert => Key::Named(NamedKey::NumpadInsert),
        GK::KP_Delete => Key::Named(NamedKey::NumpadDelete),
        GK::KP_Begin => Key::Named(NamedKey::NumpadBegin),
        GK::Tab | GK::ISO_Left_Tab => Key::Tab,
        GK::Escape => Key::Escape,
        GK::F1 => Key::F(1),
        GK::F2 => Key::F(2),
        GK::F3 => Key::F(3),
        GK::F4 => Key::F(4),
        GK::F5 => Key::F(5),
        GK::F6 => Key::F(6),
        GK::F7 => Key::F(7),
        GK::F8 => Key::F(8),
        GK::F9 => Key::F(9),
        GK::F10 => Key::F(10),
        GK::F11 => Key::F(11),
        GK::F12 => Key::F(12),
        GK::F13 => Key::F(13),
        GK::F14 => Key::F(14),
        GK::F15 => Key::F(15),
        GK::F16 => Key::F(16),
        GK::F17 => Key::F(17),
        GK::F18 => Key::F(18),
        GK::F19 => Key::F(19),
        GK::F20 => Key::F(20),
        GK::F21 => Key::F(21),
        GK::F22 => Key::F(22),
        GK::F23 => Key::F(23),
        GK::F24 => Key::F(24),
        GK::F25 => Key::F(25),
        GK::F26 => Key::F(26),
        GK::F27 => Key::F(27),
        GK::F28 => Key::F(28),
        GK::F29 => Key::F(29),
        GK::F30 => Key::F(30),
        GK::F31 => Key::F(31),
        GK::F32 => Key::F(32),
        GK::F33 => Key::F(33),
        GK::F34 => Key::F(34),
        GK::F35 => Key::F(35),
        GK::Caps_Lock => Key::Named(NamedKey::CapsLock),
        GK::Scroll_Lock => Key::Named(NamedKey::ScrollLock),
        GK::Num_Lock => Key::Named(NamedKey::NumLock),
        GK::Print => Key::Named(NamedKey::PrintScreen),
        GK::Pause => Key::Named(NamedKey::Pause),
        GK::Menu => Key::Named(NamedKey::Menu),
        GK::AudioPlay => Key::Named(NamedKey::MediaPlay),
        GK::AudioPause => Key::Named(NamedKey::MediaPause),
        GK::AudioStop => Key::Named(NamedKey::MediaStop),
        GK::AudioForward => Key::Named(NamedKey::MediaFastForward),
        GK::AudioRewind => Key::Named(NamedKey::MediaRewind),
        GK::AudioNext => Key::Named(NamedKey::MediaTrackNext),
        GK::AudioPrev => Key::Named(NamedKey::MediaTrackPrevious),
        GK::AudioRecord => Key::Named(NamedKey::MediaRecord),
        GK::AudioLowerVolume => Key::Named(NamedKey::LowerVolume),
        GK::AudioRaiseVolume => Key::Named(NamedKey::RaiseVolume),
        GK::AudioMute => Key::Named(NamedKey::MuteVolume),
        GK::Shift_L => Key::Named(NamedKey::LeftShift),
        GK::Control_L => Key::Named(NamedKey::LeftControl),
        GK::Alt_L => Key::Named(NamedKey::LeftAlt),
        GK::Super_L => Key::Named(NamedKey::LeftSuper),
        GK::Hyper_L => Key::Named(NamedKey::LeftHyper),
        GK::Meta_L => Key::Named(NamedKey::LeftMeta),
        GK::Shift_R => Key::Named(NamedKey::RightShift),
        GK::Control_R => Key::Named(NamedKey::RightControl),
        GK::Alt_R => Key::Named(NamedKey::RightAlt),
        GK::Super_R => Key::Named(NamedKey::RightSuper),
        GK::Hyper_R => Key::Named(NamedKey::RightHyper),
        GK::Meta_R => Key::Named(NamedKey::RightMeta),
        GK::ISO_Level3_Shift => Key::Named(NamedKey::IsoLevel3Shift),
        GK::ISO_Level5_Shift => Key::Named(NamedKey::IsoLevel5Shift),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_core::screen::ScreenConfig;

    #[test]
    fn named_keys_cover_wayland_modifiers_media_and_extended_functions() {
        assert_eq!(
            keyval_to_key(gdk::Key::Control_R),
            Some(Key::Named(NamedKey::RightControl))
        );
        assert_eq!(
            keyval_to_key(gdk::Key::AudioPlay),
            Some(Key::Named(NamedKey::MediaPlay))
        );
        assert_eq!(keyval_to_key(gdk::Key::F35), Some(Key::F(35)));
        assert_eq!(
            keyval_to_key(gdk::Key::KP_Page_Up),
            Some(Key::Named(NamedKey::NumpadPageUp))
        );
    }

    #[test]
    fn pc101_identity_uses_physical_wayland_keycodes() {
        assert_eq!(pc101_key_for_gdk_keycode(10), Some('1'));
        assert_eq!(pc101_key_for_gdk_keycode(38), Some('a'));
        assert_eq!(pc101_key_for_gdk_keycode(51), Some('\\'));
        assert_eq!(pc101_key_for_gdk_keycode(65), Some(' '));
        assert_eq!(pc101_key_for_gdk_keycode(999), None);
    }

    #[test]
    fn all_key_mode_routes_plain_and_modifier_keys() {
        let mut terminal = Terminal::new(8, 2, ScreenConfig::default());
        terminal.process(b"\x1b[>8u");

        assert!(should_route_enhanced_key(
            &terminal,
            Key::Char('a'),
            Modifiers::empty(),
        ));
        assert!(should_route_enhanced_key(
            &terminal,
            Key::Named(NamedKey::LeftShift),
            Modifiers::SHIFT,
        ));
    }
}
