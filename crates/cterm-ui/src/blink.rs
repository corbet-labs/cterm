//! Shared blink timing and render policy for native frontends.
//!
//! Cursor source composition follows foot's independent DECSET 12 and
//! DECSCUSR behavior. Slow/rapid cell classification follows Rio's tested
//! SGR 5/6 distinction; native frontends own only their clock and invalidation.

use std::time::Duration;

use cterm_core::{CellAttrs, Screen};

/// Polling cadence used by native event-loop adapters.
pub const BLINK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CURSOR_HALF_PERIOD_MS: u128 = 500;
const SLOW_HALF_PERIOD_MS: u128 = 500;
const RAPID_HALF_PERIOD_MS: u128 = 150;

/// Blink phases sampled from a monotonic native clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlinkPhase {
    pub cursor_visible: bool,
    pub slow_visible: bool,
    pub rapid_visible: bool,
}

impl Default for BlinkPhase {
    fn default() -> Self {
        Self {
            cursor_visible: true,
            slow_visible: true,
            rapid_visible: true,
        }
    }
}

impl BlinkPhase {
    /// Compact representation suitable for native atomic state.
    pub fn bits(self) -> u8 {
        u8::from(self.cursor_visible)
            | (u8::from(self.slow_visible) << 1)
            | (u8::from(self.rapid_visible) << 2)
    }

    /// Restore a phase stored by [`Self::bits`].
    pub fn from_bits(bits: u8) -> Self {
        Self {
            cursor_visible: bits & 1 != 0,
            slow_visible: bits & 2 != 0,
            rapid_visible: bits & 4 != 0,
        }
    }
}

/// Which visual clocks can affect the current viewport.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlinkNeeds {
    pub cursor: bool,
    pub slow_cells: bool,
    pub rapid_cells: bool,
}

impl BlinkNeeds {
    /// Inspect only the visible viewport, mirroring foot's redraw scan.
    pub fn for_screen(screen: &Screen) -> Self {
        let mut needs = Self {
            cursor: (screen.modes.show_cursor || screen.has_extra_cursors())
                && screen.scroll_offset == 0
                && screen.cursor.blink.enabled(),
            ..Self::default()
        };

        for row in 0..screen.height() {
            let line = screen.visible_row_to_absolute_line(row);
            for col in 0..screen.width() {
                let Some(cell) = screen.get_cell_with_scrollback(line, col) else {
                    continue;
                };
                needs.slow_cells |= cell.attrs.contains(CellAttrs::BLINK);
                needs.rapid_cells |= cell.attrs.contains(CellAttrs::RAPID_BLINK);
                if needs.slow_cells && needs.rapid_cells {
                    return needs;
                }
            }
        }
        needs
    }

    fn phase_changed(self, before: BlinkPhase, after: BlinkPhase) -> bool {
        (self.cursor && before.cursor_visible != after.cursor_visible)
            || (self.slow_cells && before.slow_visible != after.slow_visible)
            || (self.rapid_cells && before.rapid_visible != after.rapid_visible)
    }
}

/// Stateful edge detector shared by GTK, Cocoa and Win32 invalidation loops.
/// Each source starts visible when it becomes relevant, as native timer-based
/// terminals do when arming a blink timer for newly visible content.
#[derive(Debug)]
pub struct BlinkClock {
    phase: BlinkPhase,
    active: BlinkNeeds,
    cursor_started: Duration,
    slow_started: Duration,
    rapid_started: Duration,
}

impl Default for BlinkClock {
    fn default() -> Self {
        Self {
            phase: BlinkPhase::default(),
            active: BlinkNeeds::default(),
            cursor_started: Duration::ZERO,
            slow_started: Duration::ZERO,
            rapid_started: Duration::ZERO,
        }
    }
}

impl BlinkClock {
    pub fn phase(&self) -> BlinkPhase {
        self.phase
    }

    /// Keep an active cursor visible and restart its period after terminal
    /// output, matching foot's "prevent blinking while typing" behavior.
    pub fn rearm_cursor(&mut self, elapsed: Duration) -> bool {
        if !self.active.cursor {
            return false;
        }
        let redraw = !self.phase.cursor_visible;
        self.cursor_started = elapsed;
        self.phase.cursor_visible = true;
        redraw
    }

    /// Sample `elapsed` and report whether an applicable phase edge needs redraw.
    pub fn update(&mut self, elapsed: Duration, needs: BlinkNeeds) -> bool {
        let before = self.phase;
        Self::update_source(
            elapsed,
            needs.cursor,
            &mut self.active.cursor,
            &mut self.cursor_started,
            &mut self.phase.cursor_visible,
            CURSOR_HALF_PERIOD_MS,
        );
        Self::update_source(
            elapsed,
            needs.slow_cells,
            &mut self.active.slow_cells,
            &mut self.slow_started,
            &mut self.phase.slow_visible,
            SLOW_HALF_PERIOD_MS,
        );
        Self::update_source(
            elapsed,
            needs.rapid_cells,
            &mut self.active.rapid_cells,
            &mut self.rapid_started,
            &mut self.phase.rapid_visible,
            RAPID_HALF_PERIOD_MS,
        );
        needs.phase_changed(before, self.phase)
    }

    fn update_source(
        elapsed: Duration,
        needed: bool,
        active: &mut bool,
        started: &mut Duration,
        visible: &mut bool,
        half_period_ms: u128,
    ) {
        if !needed {
            *active = false;
            *visible = true;
            return;
        }
        if !*active {
            *active = true;
            *started = elapsed;
            *visible = true;
            return;
        }
        let millis = elapsed.saturating_sub(*started).as_millis();
        *visible = (millis / half_period_ms).is_multiple_of(2);
    }
}

/// Determine the three phases for a monotonic elapsed duration.
pub fn phase_at(elapsed: Duration) -> BlinkPhase {
    let millis = elapsed.as_millis();
    BlinkPhase {
        cursor_visible: (millis / CURSOR_HALF_PERIOD_MS).is_multiple_of(2),
        slow_visible: (millis / SLOW_HALF_PERIOD_MS).is_multiple_of(2),
        rapid_visible: (millis / RAPID_HALF_PERIOD_MS).is_multiple_of(2),
    }
}

/// Whether foreground glyphs/decorations should be emitted for this cell.
pub fn cell_foreground_visible(attrs: CellAttrs, phase: BlinkPhase) -> bool {
    if attrs.contains(CellAttrs::RAPID_BLINK) {
        phase.rapid_visible
    } else if attrs.contains(CellAttrs::BLINK) {
        phase.slow_visible
    } else {
        true
    }
}

/// Whether the cursor should be emitted for this screen and phase.
pub fn cursor_visible(screen: &Screen, phase: BlinkPhase) -> bool {
    screen.modes.show_cursor
        && screen.scroll_offset == 0
        && (!screen.cursor.blink.enabled() || phase.cursor_visible)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_core::{screen::ScreenConfig, Parser};

    #[test]
    fn slow_rapid_and_cursor_phases_have_distinct_edges() {
        assert_eq!(phase_at(Duration::ZERO), BlinkPhase::default());
        assert!(!phase_at(Duration::from_millis(150)).rapid_visible);
        assert!(phase_at(Duration::from_millis(150)).slow_visible);
        assert!(!phase_at(Duration::from_millis(500)).slow_visible);
        assert!(!phase_at(Duration::from_millis(500)).cursor_visible);
        assert!(!phase_at(Duration::from_millis(500)).rapid_visible);
    }

    #[test]
    fn clock_invalidates_only_for_relevant_phase_edges() {
        let mut clock = BlinkClock::default();
        assert!(!clock.update(Duration::from_millis(150), BlinkNeeds::default()));
        assert!(!clock.update(
            Duration::from_millis(300),
            BlinkNeeds {
                slow_cells: true,
                ..BlinkNeeds::default()
            }
        ));
        assert!(clock.update(
            Duration::from_millis(800),
            BlinkNeeds {
                slow_cells: true,
                ..BlinkNeeds::default()
            }
        ));
    }

    #[test]
    fn newly_armed_source_starts_visible_independent_of_global_epoch() {
        let mut clock = BlinkClock::default();
        let rapid = BlinkNeeds {
            rapid_cells: true,
            ..BlinkNeeds::default()
        };

        assert!(!clock.update(Duration::from_millis(975), rapid));
        assert!(clock.phase().rapid_visible);
        assert!(clock.update(Duration::from_millis(1125), rapid));
        assert!(!clock.phase().rapid_visible);
        assert!(!clock.update(Duration::from_secs(2), BlinkNeeds::default()));
        assert!(clock.phase().rapid_visible);
    }

    #[test]
    fn cursor_rearm_restores_visibility_and_restarts_period() {
        let mut clock = BlinkClock::default();
        let cursor = BlinkNeeds {
            cursor: true,
            ..BlinkNeeds::default()
        };
        assert!(!clock.update(Duration::ZERO, cursor));
        assert!(clock.update(Duration::from_millis(500), cursor));
        assert!(clock.rearm_cursor(Duration::from_millis(650)));
        assert!(clock.phase().cursor_visible);
        assert!(!clock.update(Duration::from_millis(1_000), cursor));
        assert!(clock.update(Duration::from_millis(1_150), cursor));
    }

    #[test]
    fn screen_needs_and_visibility_follow_core_attributes() {
        let mut screen = Screen::new(8, 2, ScreenConfig::default());
        screen.configure_cursor(cterm_core::CursorStyle::Block, false);
        let mut parser = Parser::new();
        parser.parse(&mut screen, b"\x1b[5mA\x1b[6mB");

        assert_eq!(
            BlinkNeeds::for_screen(&screen),
            BlinkNeeds {
                cursor: false,
                slow_cells: true,
                rapid_cells: true,
            }
        );
        let off = BlinkPhase {
            cursor_visible: false,
            slow_visible: false,
            rapid_visible: false,
        };
        assert!(!cell_foreground_visible(
            screen.get_cell(0, 0).unwrap().attrs,
            off
        ));
        assert!(!cell_foreground_visible(
            screen.get_cell(0, 1).unwrap().attrs,
            off
        ));
        assert!(cursor_visible(&screen, off));
    }
}
