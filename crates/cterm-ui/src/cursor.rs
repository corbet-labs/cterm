//! Shared cursor rendering policy for native frontends.

use cterm_core::{
    CellAttrs, Color, ColorPalette, ExtraCursorColor, ExtraCursorColors, Rgb, Screen,
};

use crate::BlinkPhase;

/// Colors resolved for an extra cursor at one cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedExtraCursorColors {
    pub cursor: Rgb,
    pub text: Rgb,
}

/// Cell block covered by a cursor positioned anywhere inside OSC 66 text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorFootprint {
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub columns: usize,
}

pub fn cursor_footprint(screen: &Screen, row: usize, col: usize) -> CursorFootprint {
    let Some(cell) = screen.get_cell(row, col) else {
        return CursorFootprint {
            row,
            col,
            rows: 1,
            columns: 1,
        };
    };
    cell.multicell.as_ref().map_or_else(
        || CursorFootprint {
            row,
            col: if cell.is_wide_spacer() {
                col.saturating_sub(1)
            } else {
                col
            },
            rows: 1,
            columns: usize::from(cell.is_wide() || cell.is_wide_spacer()) + 1,
        },
        |multicell| {
            let row_offset = usize::from(multicell.row_offset);
            let missing_rows = row_offset.saturating_sub(row);
            CursorFootprint {
                row: row.saturating_sub(row_offset),
                col: col.saturating_sub(usize::from(multicell.column_offset)),
                rows: usize::from(multicell.rows).saturating_sub(missing_rows),
                columns: usize::from(multicell.columns),
            }
        },
    )
}

/// Extra cursors ignore DECTCEM but share the main cursor's blink phase.
pub fn extra_cursors_visible(screen: &Screen, phase: BlinkPhase) -> bool {
    screen.has_extra_cursors()
        && screen.scroll_offset == 0
        && (!screen.cursor.blink.enabled() || phase.cursor_visible)
}

/// Resolve Kitty's shared extra-cursor color pair against the underlying cell.
pub fn resolve_extra_cursor_colors(
    screen: &Screen,
    palette: &ColorPalette,
    main_cursor_text: Rgb,
    row: usize,
    col: usize,
) -> ResolvedExtraCursorColors {
    let (cell_foreground, cell_background) = screen
        .get_cell(row, col)
        .map(|cell| {
            let mut foreground = screen.resolve_color(cell.fg, palette);
            let mut background = if cell.bg == Color::Default {
                palette.background
            } else {
                screen.resolve_color(cell.bg, palette)
            };
            let inverted = cell.attrs.contains(CellAttrs::INVERSE) ^ screen.modes.reverse_video;
            if inverted {
                std::mem::swap(&mut foreground, &mut background);
            } else if cell.hyperlink.is_some() && cell.fg == Color::Default {
                foreground = Rgb::new(100, 149, 237);
            }
            if cell.attrs.contains(CellAttrs::DIM) {
                foreground = Rgb::new(foreground.r / 2, foreground.g / 2, foreground.b / 2);
            }
            (foreground, background)
        })
        .unwrap_or((palette.foreground, palette.background));

    let ExtraCursorColors { text, cursor } = screen.extra_cursor_colors();
    let resolve_explicit = |color| screen.resolve_color(color, palette);
    let cursor_color = match cursor {
        ExtraCursorColor::Main => palette.cursor,
        ExtraCursorColor::Reverse => cell_foreground,
        ExtraCursorColor::Color(color) => resolve_explicit(color),
    };
    let text_color = if cursor == ExtraCursorColor::Reverse {
        // Full reverse mode overrides the separately configured text color.
        cell_background
    } else {
        match text {
            ExtraCursorColor::Main => main_cursor_text,
            ExtraCursorColor::Reverse => cell_background,
            ExtraCursorColor::Color(color) => resolve_explicit(color),
        }
    };

    ResolvedExtraCursorColors {
        cursor: cursor_color,
        text: text_color,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_core::{screen::ScreenConfig, ExtraCursorShape, Parser};

    #[test]
    fn reverse_cursor_uses_underlying_foreground_and_background() {
        let mut screen = Screen::new(2, 1, ScreenConfig::default());
        let mut parser = Parser::new();
        parser.parse(
            &mut screen,
            b"\x1b[38;2;10;20;30;48;2;40;50;60mX\x1b[>1;2:1:1 q\x1b[>40;1 q",
        );
        let colors =
            resolve_extra_cursor_colors(&screen, &ColorPalette::default(), Rgb::new(1, 2, 3), 0, 0);
        assert_eq!(colors.cursor, Rgb::new(10, 20, 30));
        assert_eq!(colors.text, Rgb::new(40, 50, 60));
        assert!(screen.has_extra_cursors());
        assert_eq!(
            screen.extra_cursors().next().unwrap().shape,
            ExtraCursorShape::Block
        );
    }

    #[test]
    fn cursor_covers_an_entire_text_width_span() {
        let mut screen = Screen::new(5, 1, ScreenConfig::default());
        let mut parser = Parser::new();
        parser.parse(&mut screen, b"\x1b]66;w=3;x\x07");

        assert_eq!(
            cursor_footprint(&screen, 0, 2),
            CursorFootprint {
                row: 0,
                col: 0,
                rows: 1,
                columns: 3,
            }
        );
    }

    #[test]
    fn cursor_clips_a_scaled_block_after_its_anchor_is_evicted() {
        let mut screen = Screen::new(
            5,
            2,
            ScreenConfig {
                scrollback_lines: 0,
            },
        );
        let mut parser = Parser::new();
        parser.parse(&mut screen, b"\x1b]66;s=2:w=1;A\x07\x1b[2;1H\n");

        assert_eq!(
            cursor_footprint(&screen, 0, 0),
            CursorFootprint {
                row: 0,
                col: 0,
                rows: 1,
                columns: 2,
            }
        );
    }
}
