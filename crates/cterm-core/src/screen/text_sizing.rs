//! Screen mutation for Kitty OSC 66 multicell text.

use super::Screen;
use crate::cell::CellAttrs;
use crate::text_sizing::{
    Multicell, TextSizeAlignment, TextSizeRequest, MAX_TEXT_SIZE_PAYLOAD_BYTES,
};
use std::collections::HashSet;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

impl Screen {
    /// Render an OSC 66 request as one or more independently placed blocks.
    pub(crate) fn put_text_size(&mut self, request: TextSizeRequest) {
        let scale = request.scale;
        let fractional_scale = request.fractional_scale;
        let vertical_alignment = request.vertical_alignment;
        let horizontal_alignment = request.horizontal_alignment;
        for chunk in request.chunks() {
            self.put_text_size_block(
                chunk.text,
                chunk.width,
                scale,
                fractional_scale,
                vertical_alignment,
                horizontal_alignment,
                chunk.natural_width,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn put_text_size_block(
        &mut self,
        text: &str,
        unscaled_columns: u8,
        scale: u8,
        fractional_scale: Option<(u8, u8)>,
        vertical_alignment: TextSizeAlignment,
        horizontal_alignment: TextSizeAlignment,
        natural_width: bool,
    ) {
        let columns = usize::from(unscaled_columns) * usize::from(scale);
        let rows = usize::from(scale);
        let screen_width = self.width();
        let margin_height = self
            .scroll_region
            .bottom
            .saturating_sub(self.scroll_region.top);
        if columns == 0 || columns > screen_width || rows == 0 || rows > margin_height {
            return;
        }

        if !self.move_cursor_past_multicell(columns) {
            return;
        }

        if self.scroll_region.contains(self.cursor.row) {
            let available_rows = self.scroll_region.bottom.saturating_sub(self.cursor.row);
            if rows > available_rows {
                let scroll_count = rows - available_rows;
                self.scroll_up(scroll_count);
                self.cursor.row = self.cursor.row.saturating_sub(scroll_count);
            }
        } else if self.cursor.row + rows > self.height() {
            return;
        }

        if self.modes.insert_mode {
            for row in self.cursor.row..self.cursor.row + rows {
                self.insert_cells_at(row, self.cursor.col, columns);
            }
        }
        self.clear_selection_if_rows_selected(self.cursor.row, self.cursor.row + rows - 1);

        for row in self.cursor.row..self.cursor.row + rows {
            for col in self.cursor.col..self.cursor.col + columns {
                self.clear_multicell_span_at(row, col);
                self.clear_wide_cell_at(row, col);
            }
        }

        let text: Arc<str> = Arc::from(text);
        for row_offset in 0..rows {
            for column_offset in 0..columns {
                let Some(cell) = self.grid.get_mut(
                    self.cursor.row + row_offset,
                    self.cursor.col + column_offset,
                ) else {
                    continue;
                };
                cell.reset();
                self.style.apply_to(cell);
                if row_offset == 0 && column_offset == 0 {
                    cell.set_text_size_payload(&text);
                }
                cell.attrs.remove(CellAttrs::WIDE | CellAttrs::WIDE_SPACER);
                cell.multicell = Some(Multicell::new_block(
                    Arc::clone(&text),
                    columns as u8,
                    rows as u8,
                    column_offset as u8,
                    row_offset as u8,
                    scale,
                    fractional_scale,
                    vertical_alignment,
                    horizontal_alignment,
                    natural_width,
                ));
            }
        }

        self.cursor.col += columns;
        self.dirty = true;
    }

    /// Find a writable run, skipping the lower rows of existing multicell
    /// blocks even when auto-wrap is disabled.
    pub(super) fn move_cursor_past_multicell(&mut self, required_width: usize) -> bool {
        let screen_width = self.width();
        if required_width == 0 || required_width > screen_width {
            return false;
        }

        loop {
            while self.cursor.col + required_width <= screen_width {
                let intersects_lower_row =
                    (self.cursor.col..self.cursor.col + required_width).any(|col| {
                        self.grid
                            .get(self.cursor.row, col)
                            .and_then(|cell| cell.multicell.as_ref())
                            .is_some_and(|multicell| multicell.row_offset > 0)
                    });
                if !intersects_lower_row {
                    return true;
                }
                self.cursor.col += 1;
            }

            let tail_start = screen_width - required_width;
            let tail_intersects_lower_row = (tail_start..screen_width).any(|col| {
                self.grid
                    .get(self.cursor.row, col)
                    .and_then(|cell| cell.multicell.as_ref())
                    .is_some_and(|multicell| multicell.row_offset > 0)
            });
            if self.modes.auto_wrap || tail_intersects_lower_row {
                self.carriage_return();
                self.line_feed();
                if let Some(row) = self.grid.row_mut(self.cursor.row) {
                    row.wrapped = true;
                }
            } else {
                self.cursor.col = tail_start;
                return true;
            }
        }
    }

    pub(super) fn try_extend_previous_multicell(&mut self, c: char) -> bool {
        if self.cursor.col == 0 || self.cursor.row >= self.height() {
            return false;
        }
        let col = self.cursor.col.min(self.width()).saturating_sub(1);
        let Some(multicell) = self
            .grid
            .get(self.cursor.row, col)
            .and_then(|cell| cell.multicell.clone())
        else {
            return false;
        };
        if multicell.row_offset != 0 {
            return false;
        }

        let mut candidate = String::with_capacity(multicell.text().len() + c.len_utf8());
        candidate.push_str(multicell.text());
        candidate.push(c);
        if candidate.len() > MAX_TEXT_SIZE_PAYLOAD_BYTES
            || UnicodeSegmentation::graphemes(candidate.as_str(), true).count()
                != UnicodeSegmentation::graphemes(multicell.text(), true).count()
        {
            return false;
        }

        let anchor_col = col.saturating_sub(usize::from(multicell.column_offset));
        let anchor_row = self
            .scrollback
            .len()
            .saturating_add(self.cursor.row)
            .saturating_sub(usize::from(multicell.row_offset));
        let shared: Arc<str> = Arc::from(candidate.as_str());
        for row_offset in 0..usize::from(multicell.rows) {
            for column_offset in 0..usize::from(multicell.columns) {
                let Some(cell) = self.get_cell_by_absolute_line_mut(
                    anchor_row + row_offset,
                    anchor_col + column_offset,
                ) else {
                    continue;
                };
                let Some(metadata) = cell.multicell.as_mut() else {
                    continue;
                };
                if metadata.same_span(&multicell)
                    && usize::from(metadata.column_offset) == column_offset
                    && usize::from(metadata.row_offset) == row_offset
                {
                    metadata.set_text(Arc::clone(&shared));
                    if row_offset == 0 && column_offset == 0 {
                        cell.set_text_size_payload(&shared);
                    }
                }
            }
        }
        self.dirty = true;
        true
    }

    pub(super) fn clear_multicell_span_at(&mut self, row: usize, col: usize) {
        let Some(multicell) = self
            .grid
            .get(row, col)
            .and_then(|cell| cell.multicell.clone())
        else {
            return;
        };
        let anchor_col = col.saturating_sub(usize::from(multicell.column_offset));
        let absolute_row = self.scrollback.len().saturating_add(row);
        let anchor_row = absolute_row as isize - isize::from(multicell.row_offset);
        for row_offset in 0..usize::from(multicell.rows) {
            let absolute_row = anchor_row + row_offset as isize;
            if absolute_row < 0 {
                continue;
            }
            for column_offset in 0..usize::from(multicell.columns) {
                let Some(cell) = self.get_cell_by_absolute_line_mut(
                    absolute_row as usize,
                    anchor_col + column_offset,
                ) else {
                    continue;
                };
                if cell.multicell.as_ref().is_some_and(|metadata| {
                    metadata.same_span(&multicell)
                        && usize::from(metadata.column_offset) == column_offset
                        && usize::from(metadata.row_offset) == row_offset
                }) {
                    cell.reset();
                }
            }
        }
    }

    fn get_cell_by_absolute_line_mut(
        &mut self,
        absolute_line: usize,
        col: usize,
    ) -> Option<&mut crate::cell::Cell> {
        let scrollback_len = self.scrollback.len();
        if absolute_line < scrollback_len {
            self.scrollback.get_mut(absolute_line)?.get_mut(col)
        } else {
            self.grid.get_mut(absolute_line - scrollback_len, col)
        }
    }

    pub(super) fn clear_multicells_intersecting_row_range(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
    ) {
        let cells = (start..end.min(self.width()))
            .filter_map(|col| {
                self.grid.get(row, col).and_then(|cell| {
                    cell.multicell.as_ref().map(|metadata| {
                        (
                            row.saturating_sub(usize::from(metadata.row_offset)),
                            col.saturating_sub(usize::from(metadata.column_offset)),
                            col,
                        )
                    })
                })
            })
            .collect::<Vec<_>>();
        let mut cleared = HashSet::new();
        for (anchor_row, anchor_col, col) in cells {
            if cleared.insert((anchor_row, anchor_col)) {
                self.clear_multicell_span_at(row, col);
            }
        }
    }

    pub(super) fn clear_multiline_multicells_intersecting_row_range(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
    ) {
        let cells = (start..end.min(self.width()))
            .filter(|&col| {
                self.grid
                    .get(row, col)
                    .and_then(|cell| cell.multicell.as_ref())
                    .is_some_and(|multicell| multicell.rows > 1)
            })
            .collect::<Vec<_>>();
        for col in cells {
            self.clear_multicell_span_at(row, col);
        }
    }

    pub(super) fn clear_multicells_intersecting_rows(&mut self, start: usize, end: usize) {
        let mut cells = Vec::new();
        for row in start..end.min(self.height()) {
            for col in 0..self.width() {
                if self
                    .grid
                    .get(row, col)
                    .is_some_and(|cell| cell.multicell.is_some())
                {
                    cells.push((row, col));
                }
            }
        }
        for (row, col) in cells {
            self.clear_multicell_span_at(row, col);
        }
    }

    pub(super) fn clear_multicells_crossing_row_boundary(&mut self, boundary: usize) {
        if boundary == 0 || boundary >= self.height() {
            return;
        }
        let cells = (0..self.width())
            .filter(|&col| {
                self.grid
                    .get(boundary, col)
                    .and_then(|cell| cell.multicell.as_ref())
                    .is_some_and(|multicell| multicell.row_offset > 0)
            })
            .collect::<Vec<_>>();
        for col in cells {
            self.clear_multicell_span_at(boundary, col);
        }
    }

    pub(super) fn repair_multicell_spans_in_row(&mut self, row_index: usize) {
        let width = self.width();
        let Some(row) = self.grid.row(row_index) else {
            return;
        };
        let mut invalid_anchors = HashSet::new();
        for (col, cell) in row.iter().enumerate() {
            let Some(multicell) = cell.multicell.as_ref() else {
                continue;
            };
            let Some(anchor) = col.checked_sub(usize::from(multicell.column_offset)) else {
                invalid_anchors.insert(0);
                continue;
            };
            let columns = usize::from(multicell.columns);
            let complete = anchor + columns <= width
                && (0..columns).all(|offset| {
                    row.get(anchor + offset)
                        .and_then(|candidate| candidate.multicell.as_ref())
                        .is_some_and(|candidate| {
                            candidate.same_span(multicell)
                                && usize::from(candidate.column_offset) == offset
                        })
                });
            if !complete {
                invalid_anchors.insert(anchor);
            }
        }

        if let Some(row) = self.grid.row_mut(row_index) {
            for (col, cell) in row.iter_mut().enumerate() {
                let invalid = cell.multicell.as_ref().is_some_and(|multicell| {
                    invalid_anchors
                        .contains(&col.saturating_sub(usize::from(multicell.column_offset)))
                });
                if invalid {
                    cell.reset();
                }
            }
        }
    }
}
