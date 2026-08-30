//! Screen and cell conversion between cterm-core and proto

use crate::convert::color::{color_to_proto, proto_to_color};
use crate::proto;
use cterm_core::cell::Hyperlink;
use cterm_core::drcs::{DrcsFont, DrcsGlyph};
use cterm_core::term::Terminal;
use cterm_core::{Cell, CellAttrs, Color, ColorQuery, Screen, TerminalImage};
use std::sync::Arc;

/// Convert cell attributes to proto
pub fn attrs_to_proto(attrs: CellAttrs) -> proto::CellAttributes {
    proto::CellAttributes {
        bold: attrs.contains(CellAttrs::BOLD),
        italic: attrs.contains(CellAttrs::ITALIC),
        underline: attrs.contains(CellAttrs::UNDERLINE),
        double_underline: attrs.contains(CellAttrs::DOUBLE_UNDERLINE),
        curly_underline: attrs.contains(CellAttrs::CURLY_UNDERLINE),
        dotted_underline: attrs.contains(CellAttrs::DOTTED_UNDERLINE),
        dashed_underline: attrs.contains(CellAttrs::DASHED_UNDERLINE),
        blink: attrs.contains(CellAttrs::BLINK),
        inverse: attrs.contains(CellAttrs::INVERSE),
        hidden: attrs.contains(CellAttrs::HIDDEN),
        strikethrough: attrs.contains(CellAttrs::STRIKETHROUGH),
        dim: attrs.contains(CellAttrs::DIM),
        overline: attrs.contains(CellAttrs::OVERLINE),
        wide: attrs.contains(CellAttrs::WIDE),
        wide_spacer: attrs.contains(CellAttrs::WIDE_SPACER),
    }
}

/// Convert proto attributes to cell attributes
pub fn proto_to_attrs(attrs: &proto::CellAttributes) -> CellAttrs {
    let mut result = CellAttrs::empty();
    if attrs.bold {
        result |= CellAttrs::BOLD;
    }
    if attrs.italic {
        result |= CellAttrs::ITALIC;
    }
    if attrs.underline {
        result |= CellAttrs::UNDERLINE;
    }
    if attrs.double_underline {
        result |= CellAttrs::DOUBLE_UNDERLINE;
    }
    if attrs.curly_underline {
        result |= CellAttrs::CURLY_UNDERLINE;
    }
    if attrs.dotted_underline {
        result |= CellAttrs::DOTTED_UNDERLINE;
    }
    if attrs.dashed_underline {
        result |= CellAttrs::DASHED_UNDERLINE;
    }
    if attrs.blink {
        result |= CellAttrs::BLINK;
    }
    if attrs.inverse {
        result |= CellAttrs::INVERSE;
    }
    if attrs.hidden {
        result |= CellAttrs::HIDDEN;
    }
    if attrs.strikethrough {
        result |= CellAttrs::STRIKETHROUGH;
    }
    if attrs.dim {
        result |= CellAttrs::DIM;
    }
    if attrs.overline {
        result |= CellAttrs::OVERLINE;
    }
    if attrs.wide {
        result |= CellAttrs::WIDE;
    }
    if attrs.wide_spacer {
        result |= CellAttrs::WIDE_SPACER;
    }
    result
}

/// Convert a cell to proto
pub fn cell_to_proto(cell: &Cell) -> proto::Cell {
    proto::Cell {
        char: cell.text().to_owned(),
        fg: Some(color_to_proto(&cell.fg)),
        bg: Some(color_to_proto(&cell.bg)),
        attrs: Some(attrs_to_proto(cell.attrs)),
        underline_color: cell.underline_color.as_ref().map(color_to_proto),
        hyperlink: cell.hyperlink.as_ref().map(|h| proto::Hyperlink {
            id: h.id.clone(),
            uri: h.uri.clone(),
        }),
    }
}

/// Convert a row of cells to proto
pub fn row_to_proto(cells: &[Cell]) -> proto::Row {
    proto::Row {
        cells: cells.iter().map(cell_to_proto).collect(),
    }
}

/// Convert a DRCS glyph to proto
pub fn drcs_glyph_to_proto(char_position: u8, glyph: &DrcsGlyph) -> proto::DrcsGlyph {
    proto::DrcsGlyph {
        char_position: char_position as u32,
        data: glyph.data.clone(),
        width: glyph.width as u32,
        height: glyph.height as u32,
    }
}

/// Convert a DRCS font to proto
pub fn drcs_font_to_proto(font: &DrcsFont) -> proto::DrcsFont {
    proto::DrcsFont {
        designator: font.designator.clone(),
        font_number: font.font_number as u32,
        cell_width: font.cell_width as u32,
        cell_height: font.cell_height as u32,
        is_96_char: font.is_96_char,
        full_cell: font.full_cell,
        glyphs: font
            .glyphs
            .iter()
            .map(|(&pos, glyph)| drcs_glyph_to_proto(pos, glyph))
            .collect(),
    }
}

/// Convert proto DRCS font back to core type
pub fn proto_to_drcs_font(proto_font: &proto::DrcsFont) -> DrcsFont {
    let mut font = DrcsFont::new(
        proto_font.font_number as u8,
        proto_font.designator.clone(),
        proto_font.cell_width as usize,
        proto_font.cell_height as usize,
        proto_font.is_96_char,
        proto_font.full_cell,
    );
    for proto_glyph in &proto_font.glyphs {
        let glyph = DrcsGlyph {
            data: proto_glyph.data.clone(),
            width: proto_glyph.width as usize,
            height: proto_glyph.height as usize,
        };
        font.glyphs.insert(proto_glyph.char_position as u8, glyph);
    }
    font
}

/// Convert all DRCS fonts from screen to proto
pub fn drcs_fonts_to_proto(screen: &Screen) -> Vec<proto::DrcsFont> {
    screen
        .drcs_fonts()
        .values()
        .map(drcs_font_to_proto)
        .collect()
}

/// Convert a terminal image to its portable RGBA wire representation.
pub fn terminal_image_to_proto(image: &TerminalImage) -> proto::TerminalImage {
    proto::TerminalImage {
        id: image.id,
        col: image.col as u32,
        line: image.line as u64,
        cell_width: image.cell_width as u32,
        cell_height: image.cell_height as u32,
        rgba: image.data.as_ref().clone(),
        pixel_width: image.pixel_width as u32,
        pixel_height: image.pixel_height as u32,
    }
}

/// Convert all stored images in deterministic paint order.
pub fn terminal_images_to_proto(screen: &Screen) -> Vec<proto::TerminalImage> {
    screen
        .images()
        .into_iter()
        .map(terminal_image_to_proto)
        .collect()
}

/// Validate and convert a wire image back to the shared core representation.
pub fn proto_to_terminal_image(image: &proto::TerminalImage) -> Option<TerminalImage> {
    let pixel_width = usize::try_from(image.pixel_width).ok()?;
    let pixel_height = usize::try_from(image.pixel_height).ok()?;
    let expected_len = pixel_width.checked_mul(pixel_height)?.checked_mul(4)?;
    if pixel_width == 0 || pixel_height == 0 || image.rgba.len() != expected_len {
        return None;
    }

    Some(TerminalImage {
        id: image.id,
        col: usize::try_from(image.col).ok()?,
        line: usize::try_from(image.line).ok()?,
        cell_width: usize::try_from(image.cell_width).ok()?.max(1),
        cell_height: usize::try_from(image.cell_height).ok()?.max(1),
        data: Arc::new(image.rgba.clone()),
        pixel_width,
        pixel_height,
    })
}

/// Convert screen to proto representation
pub fn screen_to_proto(screen: &Screen, include_scrollback: bool) -> proto::GetScreenResponse {
    let cursor = proto::CursorPosition {
        row: screen.cursor.row as u32,
        col: screen.cursor.col as u32,
        visible: screen.modes.show_cursor,
        style: proto::CursorStyle::Block as i32,
    };

    // Get visible rows
    let visible_rows: Vec<proto::Row> = (0..screen.height())
        .map(|row_idx| {
            let cells: Vec<Cell> = (0..screen.width())
                .map(|col| screen.get_cell(row_idx, col).cloned().unwrap_or_default())
                .collect();
            row_to_proto(&cells)
        })
        .collect();

    // Get scrollback if requested
    let scrollback = if include_scrollback {
        screen
            .scrollback()
            .iter()
            .map(|row| {
                let cells: Vec<Cell> = row.iter().cloned().collect();
                row_to_proto(&cells)
            })
            .collect()
    } else {
        Vec::new()
    };

    proto::GetScreenResponse {
        cols: screen.width() as u32,
        rows: screen.height() as u32,
        cursor: Some(cursor),
        visible_rows,
        scrollback,
        title: screen.title.clone(),
        modes: Some(modes_to_proto(screen)),
        drcs_fonts: drcs_fonts_to_proto(screen),
        images: terminal_images_to_proto(screen),
    }
}

/// Convert a single visible row from the screen to proto
pub fn visible_row_to_proto(screen: &Screen, row_idx: usize) -> proto::Row {
    let cells: Vec<Cell> = (0..screen.width())
        .map(|col| screen.get_cell(row_idx, col).cloned().unwrap_or_default())
        .collect();
    row_to_proto(&cells)
}

/// Convert all visible rows to proto (no scrollback)
pub fn visible_rows_to_proto(screen: &Screen) -> Vec<proto::Row> {
    (0..screen.height())
        .map(|row_idx| visible_row_to_proto(screen, row_idx))
        .collect()
}

/// Build a cursor position proto from the screen state
pub fn cursor_to_proto(screen: &Screen) -> proto::CursorPosition {
    proto::CursorPosition {
        row: screen.cursor.row as u32,
        col: screen.cursor.col as u32,
        visible: screen.modes.show_cursor,
        style: proto::CursorStyle::Block as i32,
    }
}

/// Build terminal modes proto from the screen state
pub fn modes_to_proto(screen: &Screen) -> proto::TerminalModes {
    proto::TerminalModes {
        application_cursor: screen.modes.application_cursor,
        application_keypad: screen.modes.application_keypad,
        bracketed_paste: screen.modes.bracketed_paste,
        focus_events: screen.modes.focus_events,
        charset_g0: screen.modes.charset_g0.clone(),
        charset_g1: screen.modes.charset_g1.clone(),
        charset_g1_active: screen.modes.charset_g1_active,
        keyboard_enhancement_flags: u32::from(screen.keyboard_enhancement_flags().bits()),
        reverse_video: screen.modes.reverse_video,
        reverse_wrap: screen.modes.reverse_wrap,
        modify_other_keys: u32::from(screen.modes.modify_other_keys),
        dynamic_colors: Some(proto::DynamicColors {
            foreground: screen
                .dynamic_color(ColorQuery::Foreground)
                .map(|color| color_to_proto(&Color::Rgb(color))),
            background: screen
                .dynamic_color(ColorQuery::Background)
                .map(|color| color_to_proto(&Color::Rgb(color))),
            cursor: screen
                .dynamic_color(ColorQuery::Cursor)
                .map(|color| color_to_proto(&Color::Rgb(color))),
            palette: screen
                .dynamic_palette_colors()
                .map(|(index, color)| proto::DynamicPaletteColor {
                    index: u32::from(index),
                    color: Some(color_to_proto(&Color::Rgb(color))),
                })
                .collect(),
        }),
        current_working_directory: screen
            .current_working_directory()
            .map(|path| path.to_string_lossy().into_owned()),
    }
}

/// Get screen text as lines
pub fn screen_to_text(
    screen: &Screen,
    include_scrollback: bool,
    start_row: Option<u32>,
    end_row: Option<u32>,
) -> Vec<String> {
    let mut lines = Vec::new();

    // Add scrollback if requested
    if include_scrollback {
        for row in screen.scrollback().iter() {
            lines.push(row.text());
        }
    }

    // Add visible rows
    let start = start_row.unwrap_or(0) as usize;
    let end = end_row.map(|e| e as usize + 1).unwrap_or(screen.height());
    let end = end.min(screen.height());

    for row_idx in start..end {
        lines.push(
            screen
                .grid()
                .row(row_idx)
                .map(|row| row.text())
                .unwrap_or_default(),
        );
    }

    lines
}

/// Apply a proto screen snapshot to a local terminal.
///
/// Restores full screen content including visible rows, scrollback,
/// cursor position, title, and terminal modes from the proto snapshot.
pub fn apply_screen_snapshot(terminal: &mut Terminal, screen_data: &proto::GetScreenResponse) {
    let screen = terminal.screen_mut();

    // Resize if needed
    if screen_data.cols > 0 && screen_data.rows > 0 {
        screen.resize(screen_data.cols as usize, screen_data.rows as usize);
    }

    // Restore visible rows
    for (row_idx, row) in screen_data.visible_rows.iter().enumerate() {
        for (col_idx, cell) in row.cells.iter().enumerate() {
            if let Some(grid_cell) = screen.grid_mut().get_mut(row_idx, col_idx) {
                apply_proto_cell(grid_cell, cell);
            }
        }
    }

    // Restore scrollback
    if !screen_data.scrollback.is_empty() {
        use cterm_core::grid::Row;
        for proto_row in &screen_data.scrollback {
            let mut row = Row::new(screen_data.cols as usize);
            for (col_idx, cell) in proto_row.cells.iter().enumerate() {
                if let Some(grid_cell) = row.get_mut(col_idx) {
                    apply_proto_cell(grid_cell, cell);
                }
            }
            screen.scrollback_mut().push_back(row);
        }
    }

    // Restore cursor
    if let Some(cursor) = &screen_data.cursor {
        screen.cursor.row = cursor.row as usize;
        screen.cursor.col = cursor.col as usize;
        screen.modes.show_cursor = cursor.visible;
    }

    // Restore title
    if !screen_data.title.is_empty() {
        screen.title = screen_data.title.clone();
    }

    // Restore terminal modes
    screen.set_current_working_directory(None);
    if let Some(modes) = &screen_data.modes {
        screen.modes.application_cursor = modes.application_cursor;
        screen.modes.application_keypad = modes.application_keypad;
        screen.modes.bracketed_paste = modes.bracketed_paste;
        screen.modes.focus_events = modes.focus_events;
        screen.modes.charset_g0 = modes.charset_g0.clone();
        screen.modes.charset_g1 = modes.charset_g1.clone();
        screen.modes.charset_g1_active = modes.charset_g1_active;
        screen.modes.reverse_video = modes.reverse_video;
        screen.modes.reverse_wrap = modes.reverse_wrap;
        screen.modes.modify_other_keys = u8::try_from(modes.modify_other_keys)
            .unwrap_or(1)
            .clamp(1, 2);
        screen.set_current_working_directory(
            modes
                .current_working_directory
                .as_deref()
                .filter(|path| !path.is_empty())
                .map(std::path::PathBuf::from),
        );
        let dynamic_rgb = |color: Option<&proto::Color>| {
            color.map(proto_to_color).and_then(|color| {
                if let Color::Rgb(rgb) = color {
                    Some(rgb)
                } else {
                    None
                }
            })
        };
        let dynamic_colors = modes.dynamic_colors.as_ref();
        screen.set_dynamic_color(
            ColorQuery::Foreground,
            dynamic_rgb(dynamic_colors.and_then(|colors| colors.foreground.as_ref())),
        );
        screen.set_dynamic_color(
            ColorQuery::Background,
            dynamic_rgb(dynamic_colors.and_then(|colors| colors.background.as_ref())),
        );
        screen.set_dynamic_color(
            ColorQuery::Cursor,
            dynamic_rgb(dynamic_colors.and_then(|colors| colors.cursor.as_ref())),
        );
        screen.reset_dynamic_palette();
        if let Some(colors) = dynamic_colors {
            for entry in &colors.palette {
                let Ok(index) = u8::try_from(entry.index) else {
                    continue;
                };
                if let Some(color) = dynamic_rgb(entry.color.as_ref()) {
                    screen.set_dynamic_color(ColorQuery::Palette(index), Some(color));
                }
            }
        }
        screen.set_keyboard_enhancement_flags(
            cterm_core::KeyboardEnhancementFlags::from_bits_retain(
                modes.keyboard_enhancement_flags as u8,
            ),
        );
    }

    // Restore DRCS soft fonts
    if !screen_data.drcs_fonts.is_empty() {
        screen.clear_drcs_fonts();
        for proto_font in &screen_data.drcs_fonts {
            let font = proto_to_drcs_font(proto_font);
            // Use erase_control=0 (replace) since we're restoring from snapshot
            let font_number = font.font_number;
            screen.add_drcs_font(font, 0, font_number);
        }
    }

    screen.replace_images(
        screen_data
            .images
            .iter()
            .filter_map(proto_to_terminal_image),
    );
}

fn apply_proto_cell(cell: &mut Cell, proto_cell: &proto::Cell) {
    cell.set_text(&proto_cell.char);
    cell.fg = proto_cell
        .fg
        .as_ref()
        .map(proto_to_color)
        .unwrap_or(Color::Default);
    cell.bg = proto_cell
        .bg
        .as_ref()
        .map(proto_to_color)
        .unwrap_or(Color::Default);
    cell.attrs = proto_cell
        .attrs
        .as_ref()
        .map(proto_to_attrs)
        .unwrap_or_default();
    cell.underline_color = proto_cell.underline_color.as_ref().map(proto_to_color);
    cell.hyperlink = proto_cell.hyperlink.as_ref().map(|link| {
        Arc::new(Hyperlink {
            id: link.id.clone(),
            uri: link.uri.clone(),
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cterm_core::screen::ScreenConfig;
    use cterm_core::SixelImage;

    #[test]
    fn test_attrs_roundtrip() {
        let attrs = CellAttrs::BOLD | CellAttrs::ITALIC | CellAttrs::UNDERLINE;
        let proto = attrs_to_proto(attrs);
        let back = proto_to_attrs(&proto);
        assert_eq!(attrs, back);
    }

    #[test]
    fn test_cell_to_proto() {
        let cell = Cell::new('A');
        let proto = cell_to_proto(&cell);
        assert_eq!(proto.char, "A");
    }

    #[test]
    fn screen_snapshot_roundtrips_images_and_cell_metadata() {
        let mut source = Terminal::new(4, 2, ScreenConfig::default());
        source.screen_mut().add_image_with_size(
            1,
            0,
            2,
            1,
            SixelImage {
                data: vec![255, 0, 0, 128, 0, 255, 0, 255],
                width: 2,
                height: 1,
            },
        );
        let expected_link = Arc::new(Hyperlink::with_id(
            "link-1".into(),
            "https://example.test".into(),
        ));
        let source_cell = source.screen_mut().grid_mut().get_mut(1, 0).unwrap();
        source_cell.set_text("x\u{301}");
        source_cell.underline_color = Some(Color::rgb(1, 2, 3));
        source_cell.hyperlink = Some(expected_link.clone());
        source.screen_mut().modes.reverse_video = true;
        source.screen_mut().modes.reverse_wrap = false;
        source.screen_mut().modes.modify_other_keys = 2;
        source
            .screen_mut()
            .set_dynamic_color(ColorQuery::Foreground, Some(cterm_core::Rgb::new(4, 5, 6)));
        source
            .screen_mut()
            .set_dynamic_color(ColorQuery::Background, Some(cterm_core::Rgb::new(7, 8, 9)));
        source
            .screen_mut()
            .set_dynamic_color(ColorQuery::Cursor, Some(cterm_core::Rgb::new(10, 11, 12)));
        source.screen_mut().set_dynamic_color(
            ColorQuery::Palette(200),
            Some(cterm_core::Rgb::new(13, 14, 15)),
        );
        source
            .screen_mut()
            .set_current_working_directory(Some(std::path::PathBuf::from("/tmp/cterm cwd")));

        let snapshot = screen_to_proto(source.screen(), true);
        let mut restored = Terminal::new(1, 1, ScreenConfig::default());
        apply_screen_snapshot(&mut restored, &snapshot);

        assert_eq!(restored.screen().images(), source.screen().images());
        let restored_cell = restored.screen().get_cell(1, 0).unwrap();
        assert_eq!(restored_cell.text(), "x\u{301}");
        assert_eq!(restored_cell.underline_color, Some(Color::rgb(1, 2, 3)));
        assert_eq!(restored_cell.hyperlink, Some(expected_link));
        assert!(restored.screen().modes.reverse_video);
        assert!(!restored.screen().modes.reverse_wrap);
        assert_eq!(restored.screen().modes.modify_other_keys, 2);
        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Foreground),
            Some(cterm_core::Rgb::new(4, 5, 6))
        );
        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Background),
            Some(cterm_core::Rgb::new(7, 8, 9))
        );
        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Cursor),
            Some(cterm_core::Rgb::new(10, 11, 12))
        );
        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Palette(200)),
            Some(cterm_core::Rgb::new(13, 14, 15))
        );
        assert_eq!(
            restored.screen().current_working_directory(),
            Some(std::path::Path::new("/tmp/cterm cwd"))
        );
    }

    #[test]
    fn screen_snapshot_clears_absent_dynamic_colors() {
        let source = Terminal::new(80, 24, ScreenConfig::default());
        let snapshot = screen_to_proto(source.screen(), true);
        let mut restored = Terminal::new(80, 24, ScreenConfig::default());
        restored
            .screen_mut()
            .set_dynamic_color(ColorQuery::Foreground, Some(cterm_core::Rgb::new(1, 2, 3)));
        restored.screen_mut().set_dynamic_color(
            ColorQuery::Palette(200),
            Some(cterm_core::Rgb::new(4, 5, 6)),
        );
        restored
            .screen_mut()
            .set_current_working_directory(Some(std::path::PathBuf::from("/tmp/stale")));

        apply_screen_snapshot(&mut restored, &snapshot);

        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Foreground),
            None
        );
        assert_eq!(
            restored.screen().dynamic_color(ColorQuery::Palette(200)),
            None
        );
        assert_eq!(restored.screen().current_working_directory(), None);
    }

    #[test]
    fn rejects_invalid_terminal_image_payloads() {
        let image = proto::TerminalImage {
            id: 1,
            col: 0,
            line: 0,
            cell_width: 1,
            cell_height: 1,
            rgba: vec![1, 2, 3],
            pixel_width: 1,
            pixel_height: 1,
        };

        assert!(proto_to_terminal_image(&image).is_none());
    }
}
