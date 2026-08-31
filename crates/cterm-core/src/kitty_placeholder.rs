//! Kitty Unicode image-placeholder decoding.
//!
//! The pure row scanner and the 297-entry diacritic table are adapted from
//! Noa commit 8d843ce352e2f10ef1c130bcf7f94198f1ccaca6 (MIT). Inference is
//! deliberately stricter than Noa's implementation so it follows Kitty's
//! foreground/underline-color conditions and its reference implementation.

use crate::color::Color;

/// The 297 combining diacritics Kitty assigns to row, column, and high-byte
/// values. The index is the encoded value and the code points are sorted.
static ROWCOLUMN_DIACRITICS: [u32; 297] = [
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

fn diacritic_value(c: char) -> Option<u32> {
    ROWCOLUMN_DIACRITICS
        .binary_search(&(c as u32))
        .ok()
        .map(|index| index as u32)
}

fn color_id(color: Color) -> Option<u32> {
    match color {
        Color::Rgb(rgb) => {
            Some((u32::from(rgb.r) << 16) | (u32::from(rgb.g) << 8) | u32::from(rgb.b))
        }
        Color::Indexed(index) => Some(u32::from(index)),
        Color::Ansi(index) => Some(index as u32),
        Color::Default => None,
    }
}

/// A maximal same-image horizontal strip decoded from placeholder cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlaceholderRun {
    pub image_id: u32,
    pub placement_id: u32,
    pub image_row: u32,
    pub image_col: u32,
    pub screen_col: usize,
    pub columns: usize,
}

#[derive(Clone, Copy)]
struct PreviousCell {
    foreground: Color,
    underline: Option<Color>,
    row: u32,
    column: u32,
    high_byte: u32,
}

/// Decode and fuse the placeholder cells in one physical terminal row.
pub(crate) fn scan_cells<'a>(
    cells: impl IntoIterator<Item = &'a crate::cell::Cell>,
) -> Vec<PlaceholderRun> {
    let mut runs = Vec::new();
    let mut current: Option<PlaceholderRun> = None;
    let mut previous: Option<PreviousCell> = None;

    for (screen_col, cell) in cells.into_iter().enumerate() {
        let Some(low_bits) = cell
            .is_kitty_image_placeholder()
            .then(|| color_id(cell.fg))
            .flatten()
        else {
            runs.extend(current.take());
            previous = None;
            continue;
        };

        let mut diacritics = cell.text().chars().skip(1);
        let explicit_row = diacritics.next().and_then(diacritic_value);
        let explicit_column = diacritics.next().and_then(diacritic_value);
        let explicit_high_byte = diacritics.next().and_then(diacritic_value);
        let same_colors = previous.is_some_and(|previous| {
            previous.foreground == cell.fg && previous.underline == cell.underline_color
        });
        let inferred = previous.filter(|previous| {
            same_colors
                && explicit_row.is_none_or(|row| row == previous.row)
                && explicit_column.is_none_or(|column| column == previous.column + 1)
                && explicit_high_byte.is_none_or(|high| high == previous.high_byte)
        });
        let image_row = explicit_row
            .or_else(|| inferred.map(|previous| previous.row))
            .unwrap_or(0);
        let image_col = explicit_column
            .or_else(|| inferred.map(|previous| previous.column + 1))
            .unwrap_or(0);
        let high_byte = explicit_high_byte
            .or_else(|| inferred.map(|previous| previous.high_byte))
            .unwrap_or(0);
        let image_id = (high_byte << 24) | (low_bits & 0x00ff_ffff);
        let placement_id = cell.underline_color.and_then(color_id).unwrap_or(0);

        let extends = current.as_ref().is_some_and(|run| {
            run.image_id == image_id
                && run.placement_id == placement_id
                && run.image_row == image_row
                && image_col == run.image_col + run.columns as u32
                && screen_col == run.screen_col + run.columns
        });
        if extends {
            current.as_mut().expect("checked current run").columns += 1;
        } else {
            runs.extend(current.take());
            current = Some(PlaceholderRun {
                image_id,
                placement_id,
                image_row,
                image_col,
                screen_col,
                columns: 1,
            });
        }
        previous = Some(PreviousCell {
            foreground: cell.fg,
            underline: cell.underline_color,
            row: image_row,
            column: image_col,
            high_byte,
        });
    }
    runs.extend(current);
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Cell, KITTY_IMAGE_PLACEHOLDER};
    use crate::color::Rgb;
    use crate::grid::Row;

    fn placeholder(foreground: Color, underline: Option<Color>, suffix: &str) -> Cell {
        let mut cell = Cell::new(KITTY_IMAGE_PLACEHOLDER);
        cell.set_text(&format!("{KITTY_IMAGE_PLACEHOLDER}{suffix}"));
        cell.fg = foreground;
        cell.underline_color = underline;
        cell
    }

    fn row(cells: Vec<Cell>) -> Row {
        let mut row = Row::new(cells.len());
        for (index, cell) in cells.into_iter().enumerate() {
            *row.get_mut(index).expect("row has requested cell") = cell;
        }
        row
    }

    #[test]
    fn diacritic_table_is_sorted_and_reversible() {
        assert!(ROWCOLUMN_DIACRITICS
            .windows(2)
            .all(|pair| pair[0] < pair[1]));
        for (value, codepoint) in ROWCOLUMN_DIACRITICS.into_iter().enumerate() {
            assert_eq!(
                diacritic_value(char::from_u32(codepoint).expect("valid scalar")),
                Some(value as u32)
            );
        }
    }

    #[test]
    fn explicit_coordinates_colors_and_high_byte_decode() {
        let cells = row(vec![placeholder(
            Color::Rgb(Rgb::new(0, 0, 7)),
            Some(Color::Rgb(Rgb::new(0, 1, 0))),
            "\u{030D}\u{030E}\u{030E}",
        )]);
        let runs = scan_cells(cells.iter());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].image_id, (2 << 24) | 7);
        assert_eq!(runs[0].placement_id, 256);
        assert_eq!((runs[0].image_row, runs[0].image_col), (1, 2));
    }

    #[test]
    fn omission_infers_only_when_colors_and_coordinates_allow_it() {
        let id = Color::Indexed(42);
        let placement = Some(Color::Indexed(3));
        let cells = row(vec![
            placeholder(id, placement, "\u{0305}\u{0305}\u{030D}"),
            placeholder(id, placement, ""),
            placeholder(id, placement, "\u{0305}"),
            placeholder(id, placement, "\u{0305}\u{0310}"),
        ]);
        let runs = scan_cells(cells.iter());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].image_id, (1 << 24) | 42);
        assert_eq!(runs[0].columns, 4);
        assert_eq!(runs[0].image_col, 0);
    }

    #[test]
    fn color_change_prevents_noa_style_cross_image_inference() {
        let cells = row(vec![
            placeholder(Color::Indexed(1), None, "\u{030D}\u{030E}\u{030D}"),
            placeholder(Color::Indexed(2), None, ""),
        ]);
        let runs = scan_cells(cells.iter());
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].image_id, (1 << 24) | 1);
        assert_eq!(runs[1].image_id, 2);
        assert_eq!((runs[1].image_row, runs[1].image_col), (0, 0));
    }

    #[test]
    fn plain_cells_break_runs_and_default_foreground_is_ignored() {
        let cells = row(vec![
            placeholder(Color::Indexed(7), None, ""),
            Cell::new('x'),
            placeholder(Color::Default, None, "\u{0305}\u{0305}"),
            placeholder(Color::Indexed(7), None, ""),
        ]);
        let runs = scan_cells(cells.iter());
        assert_eq!(runs.len(), 2);
        assert_eq!((runs[0].screen_col, runs[0].image_col), (0, 0));
        assert_eq!((runs[1].screen_col, runs[1].image_col), (3, 0));
    }
}
