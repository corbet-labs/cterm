//! Mouse reporting utilities for terminal applications
//!
//! Implements xterm-style mouse reporting escape sequences for applications
//! that request mouse events (vim, tmux, htop, etc.)

use crate::screen::{MouseEncoding, MouseMode};

/// Mouse button types
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    /// Scroll up
    WheelUp,
    /// Scroll down
    WheelDown,
}

/// Kind of pointer event delivered by a native frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseEvent {
    Press(MouseButton),
    Release(MouseButton),
    /// Motion with the held button, or `None` for hover motion.
    Motion(Option<MouseButton>),
    /// Pointer left the native terminal view (Kitty SGR-pixel extension).
    Leave,
}

/// Cell and pixel coordinates for one pointer event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MousePosition {
    pub col: usize,
    pub row: usize,
    pub pixel_x: i32,
    pub pixel_y: i32,
}

impl MousePosition {
    pub const fn new(col: usize, row: usize, pixel_x: i32, pixel_y: i32) -> Self {
        Self {
            col,
            row,
            pixel_x,
            pixel_y,
        }
    }
}

/// Modifier keys held during mouse event
#[derive(Debug, Clone, Copy, Default)]
pub struct MouseModifiers {
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
}

/// Generate mouse event escape sequence
///
/// Returns the escape sequence to send to the PTY, or None if mouse reporting
/// is not active for this event type.
pub fn encode_mouse_event(
    mode: MouseMode,
    encoding: MouseEncoding,
    event: MouseEvent,
    position: MousePosition,
    modifiers: MouseModifiers,
) -> Option<Vec<u8>> {
    let (button, release, motion) = match event {
        MouseEvent::Press(button) => (Some(button), false, false),
        MouseEvent::Release(button) => (Some(button), true, false),
        MouseEvent::Motion(button) => (button, false, true),
        MouseEvent::Leave => {
            if mode == MouseMode::None || encoding != MouseEncoding::SgrPixels {
                return None;
            }
            let mut code = 1 << 8 | 1 << 5;
            if modifiers.shift {
                code |= 4;
            }
            if modifiers.alt {
                code |= 8;
            }
            if modifiers.ctrl {
                code |= 16;
            }
            let x = i64::from(position.pixel_x.max(0)) + 1;
            let y = i64::from(position.pixel_y.max(0)) + 1;
            return Some(format!("\x1b[<{code};{x};{y}M").into_bytes());
        }
    };

    // Check if this event type should be reported based on mode
    match mode {
        MouseMode::None => return None,
        MouseMode::X10 => {
            // X10 only reports button presses (not releases, drags, or wheel)
            if release || motion {
                return None;
            }
            if matches!(button, Some(MouseButton::WheelUp | MouseButton::WheelDown)) {
                return None;
            }
        }
        MouseMode::Normal => {
            // Normal reports presses and releases, but not motion
            if motion {
                return None;
            }
        }
        MouseMode::ButtonEvent => {
            // Button event reports presses, releases, and dragging with button held
            // Motion without button is not reported
            if motion && button.is_none() {
                return None;
            }
        }
        MouseMode::AnyEvent => {
            // Any event reports everything including motion
        }
    }

    // Calculate button code
    if release && matches!(button, Some(MouseButton::WheelUp | MouseButton::WheelDown)) {
        return None;
    }

    let button_code = match button {
        Some(MouseButton::Left) => 0,
        Some(MouseButton::Middle) => 1,
        Some(MouseButton::Right) => 2,
        Some(MouseButton::WheelUp) => 64,
        Some(MouseButton::WheelDown) => 65,
        None => 3,
    };

    // Add modifier bits
    let mut code = button_code;
    if modifiers.shift {
        code |= 4;
    }
    if modifiers.alt {
        code |= 8;
    }
    if modifiers.ctrl {
        code |= 16;
    }
    // Drag bit (motion with button held)
    if motion {
        code |= 32;
    }

    match encoding {
        MouseEncoding::Normal => {
            // Coordinates are encoded as value + 33. Values that cannot fit
            // in one byte are not reported; foot does not clamp them.
            let col_byte = position
                .col
                .checked_add(33)
                .and_then(|v| u8::try_from(v).ok())?;
            let row_byte = position
                .row
                .checked_add(33)
                .and_then(|v| u8::try_from(v).ok())?;
            let reported_code = if release { 3 } else { code };
            let button_byte = u8::try_from(reported_code + 32).ok()?;
            Some(vec![0x1b, b'[', b'M', button_byte, col_byte, row_byte])
        }
        MouseEncoding::Sgr | MouseEncoding::SgrPixels => {
            let (x, y) = if encoding == MouseEncoding::SgrPixels {
                (
                    i64::from(position.pixel_x.max(0)) + 1,
                    i64::from(position.pixel_y.max(0)) + 1,
                )
            } else {
                (
                    i64::try_from(position.col).ok()?.checked_add(1)?,
                    i64::try_from(position.row).ok()?.checked_add(1)?,
                )
            };
            let suffix = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{suffix}").into_bytes())
        }
        MouseEncoding::Urxvt => {
            let reported_code = if release { 3 } else { code };
            Some(
                format!(
                    "\x1b[{};{};{}M",
                    reported_code + 32,
                    position.col.checked_add(1)?,
                    position.row.checked_add(1)?
                )
                .into_bytes(),
            )
        }
    }
}

/// Check if mouse events should be captured (not used for selection)
pub fn should_capture_mouse(mode: MouseMode) -> bool {
    !matches!(mode, MouseMode::None)
}

#[cfg(test)]
mod tests {
    use super::*;

    const POS: MousePosition = MousePosition::new(10, 5, 87, 46);

    #[test]
    fn sgr_reports_real_button_on_release() {
        let seq = encode_mouse_event(
            MouseMode::Normal,
            MouseEncoding::Sgr,
            MouseEvent::Release(MouseButton::Right),
            POS,
            MouseModifiers::default(),
        );
        assert_eq!(seq, Some(b"\x1b[<2;11;6m".to_vec()));
    }

    #[test]
    fn sgr_pixels_uses_pixel_coordinates_and_clamps_negative_values() {
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::SgrPixels,
                MouseEvent::Press(MouseButton::Left),
                POS,
                MouseModifiers::default(),
            ),
            Some(b"\x1b[<0;88;47M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::SgrPixels,
                MouseEvent::Press(MouseButton::Left),
                MousePosition::new(10, 5, -8, -2),
                MouseModifiers::default(),
            ),
            Some(b"\x1b[<0;1;1M".to_vec())
        );
    }

    #[test]
    fn urxvt_reports_decimal_coordinates_and_legacy_release() {
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Urxvt,
                MouseEvent::Press(MouseButton::Middle),
                POS,
                MouseModifiers::default(),
            ),
            Some(b"\x1b[33;11;6M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Urxvt,
                MouseEvent::Release(MouseButton::Right),
                POS,
                MouseModifiers {
                    shift: true,
                    ..Default::default()
                },
            ),
            Some(b"\x1b[35;11;6M".to_vec())
        );
    }

    #[test]
    fn normal_encoding_drops_coordinates_that_do_not_fit() {
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Normal,
                MouseEvent::Press(MouseButton::Left),
                POS,
                MouseModifiers::default(),
            ),
            Some(vec![0x1b, b'[', b'M', 32, 43, 38])
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Normal,
                MouseEvent::Press(MouseButton::Left),
                MousePosition::new(223, 5, 0, 0),
                MouseModifiers::default(),
            ),
            None
        );
    }

    #[test]
    fn tracking_modes_filter_motion_like_foot() {
        let hover = MouseEvent::Motion(None);
        let drag = MouseEvent::Motion(Some(MouseButton::Left));
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Sgr,
                drag,
                POS,
                MouseModifiers::default(),
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::ButtonEvent,
                MouseEncoding::Sgr,
                hover,
                POS,
                MouseModifiers::default(),
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::ButtonEvent,
                MouseEncoding::Sgr,
                drag,
                POS,
                MouseModifiers::default(),
            ),
            Some(b"\x1b[<32;11;6M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::AnyEvent,
                MouseEncoding::Sgr,
                hover,
                POS,
                MouseModifiers::default(),
            ),
            Some(b"\x1b[<35;11;6M".to_vec())
        );
    }

    #[test]
    fn wheel_releases_and_x10_non_press_events_are_not_reported() {
        assert_eq!(
            encode_mouse_event(
                MouseMode::Normal,
                MouseEncoding::Sgr,
                MouseEvent::Release(MouseButton::WheelUp),
                POS,
                MouseModifiers::default(),
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::X10,
                MouseEncoding::Normal,
                MouseEvent::Release(MouseButton::Left),
                POS,
                MouseModifiers::default(),
            ),
            None
        );
    }

    #[test]
    fn kitty_mouse_leave_is_reported_only_for_sgr_pixels() {
        assert_eq!(
            encode_mouse_event(
                MouseMode::AnyEvent,
                MouseEncoding::SgrPixels,
                MouseEvent::Leave,
                POS,
                MouseModifiers::default(),
            ),
            Some(b"\x1b[<288;88;47M".to_vec())
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::AnyEvent,
                MouseEncoding::Sgr,
                MouseEvent::Leave,
                POS,
                MouseModifiers::default(),
            ),
            None
        );
        assert_eq!(
            encode_mouse_event(
                MouseMode::None,
                MouseEncoding::SgrPixels,
                MouseEvent::Leave,
                POS,
                MouseModifiers::default(),
            ),
            None
        );
    }

    #[test]
    fn modifiers_are_combined_with_motion_and_button_codes() {
        let seq = encode_mouse_event(
            MouseMode::AnyEvent,
            MouseEncoding::Sgr,
            MouseEvent::Motion(Some(MouseButton::Right)),
            POS,
            MouseModifiers {
                shift: true,
                alt: true,
                ctrl: true,
            },
        );
        assert_eq!(seq, Some(b"\x1b[<62;11;6M".to_vec()));
    }

    #[test]
    fn inactive_tracking_drops_events() {
        let seq = encode_mouse_event(
            MouseMode::None,
            MouseEncoding::Sgr,
            MouseEvent::Press(MouseButton::Left),
            POS,
            MouseModifiers::default(),
        );
        assert_eq!(seq, None);
    }

    #[test]
    fn sgr_press_uses_one_based_cell_coordinates() {
        let seq = encode_mouse_event(
            MouseMode::Normal,
            MouseEncoding::Sgr,
            MouseEvent::Press(MouseButton::Left),
            POS,
            MouseModifiers::default(),
        );
        assert_eq!(seq, Some(b"\x1b[<0;11;6M".to_vec()));
    }
}
