//! Cross-platform layout policy for Kitty OSC 66 text blocks.

use cterm_core::{Multicell, Screen, TextSizeAlignment};

/// Renderer-independent geometry of one multicell block, expressed in cells.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MulticellRenderMetrics {
    pub columns: usize,
    pub rows: usize,
    pub font_scale: f64,
    pub horizontal_offset: f64,
    pub vertical_offset: f64,
}

pub fn multicell_render_metrics(multicell: &Multicell) -> MulticellRenderMetrics {
    let fraction = multicell
        .fractional_scale
        .map_or(1.0, |(numerator, denominator)| {
            if numerator == 0 {
                1.0
            } else {
                f64::from(numerator) / f64::from(denominator)
            }
        });
    let remaining = 1.0 - fraction;
    let aligned_offset = |alignment: TextSizeAlignment, extent: f64| match alignment {
        TextSizeAlignment::Start => 0.0,
        TextSizeAlignment::End => extent * remaining,
        TextSizeAlignment::Center => extent * remaining / 2.0,
    };
    let columns = usize::from(multicell.columns);
    let rows = usize::from(multicell.rows);
    MulticellRenderMetrics {
        columns,
        rows,
        font_scale: f64::from(multicell.scale) * fraction,
        horizontal_offset: aligned_offset(multicell.horizontal_alignment, columns as f64),
        vertical_offset: aligned_offset(multicell.vertical_alignment, rows as f64),
    }
}

/// A block is normally painted from its top-left cell. If that row has moved
/// above the visible viewport, its first visible lower-left cell becomes the
/// paint origin and supplies the shared text payload.
pub fn is_multicell_render_anchor(
    multicell: &Multicell,
    absolute_line: usize,
    visible_top: usize,
) -> bool {
    multicell.column_offset == 0
        && (multicell.row_offset == 0
            || usize::from(multicell.row_offset) > absolute_line
            || absolute_line - usize::from(multicell.row_offset) < visible_top)
}

/// Selecting any occupied cell highlights the complete block.
pub fn multicell_is_selected(
    screen: &Screen,
    absolute_line: usize,
    col: usize,
    multicell: &Multicell,
) -> bool {
    let row_offset = usize::from(multicell.row_offset);
    let missing_rows = row_offset.saturating_sub(absolute_line);
    let anchor_line = absolute_line.saturating_sub(row_offset);
    let anchor_col = col.saturating_sub(usize::from(multicell.column_offset));
    (anchor_line..anchor_line + usize::from(multicell.rows).saturating_sub(missing_rows)).any(
        |line| {
            (anchor_col..anchor_col + usize::from(multicell.columns))
                .any(|column| screen.is_selected(line, column))
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn multicell(
        fraction: Option<(u8, u8)>,
        vertical: TextSizeAlignment,
        horizontal: TextSizeAlignment,
    ) -> Multicell {
        Multicell::from_parts(
            "x".to_string(),
            6,
            3,
            0,
            0,
            3,
            fraction,
            vertical,
            horizontal,
            false,
        )
        .unwrap()
    }

    #[test]
    fn fractional_scale_and_alignment_share_one_cross_platform_policy() {
        let metrics = multicell_render_metrics(&multicell(
            Some((1, 2)),
            TextSizeAlignment::Center,
            TextSizeAlignment::End,
        ));
        assert_eq!((metrics.columns, metrics.rows), (6, 3));
        assert_eq!(metrics.font_scale, 1.5);
        assert_eq!(metrics.horizontal_offset, 3.0);
        assert_eq!(metrics.vertical_offset, 0.75);
    }

    #[test]
    fn lower_row_renders_only_after_anchor_scrolls_above_viewport() {
        let mut block = multicell(None, TextSizeAlignment::Start, TextSizeAlignment::Start);
        block.row_offset = 1;
        assert!(!is_multicell_render_anchor(&block, 6, 5));
        assert!(is_multicell_render_anchor(&block, 6, 6));
    }
}
