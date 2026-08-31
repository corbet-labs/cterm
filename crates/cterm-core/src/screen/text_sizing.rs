//! Screen mutation for Kitty OSC 66 width-controlled text.

use super::Screen;
use crate::cell::CellAttrs;
use crate::text_sizing::{Multicell, TextSizeRequest, MAX_TEXT_SIZE_PAYLOAD_BYTES};
use std::collections::HashSet;
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;

impl Screen {
    /// Render an OSC 66 request using the protocol's independently detectable
    /// width capability.
    ///
    /// Integer and fractional scaling are activated in the subsequent
    /// multicell-layout stage. Until then, treating `s` as one is the explicit
    /// width-only implementation allowed by the protocol and produces an
    /// honest CPR capability result.
    pub(crate) fn put_text_size(&mut self, request: TextSizeRequest) {
        for chunk in request.chunks() {
            self.put_text_width_span(chunk.text, chunk.width, chunk.natural_width);
        }
    }

    fn put_text_width_span(&mut self, text: &str, columns: u8, natural_width: bool) {
        let columns = usize::from(columns);
        let screen_width = self.width();
        if columns == 0 || columns > screen_width {
            return;
        }

        if self.cursor.col >= screen_width || self.cursor.col + columns > screen_width {
            if self.modes.auto_wrap {
                self.carriage_return();
                self.line_feed();
                if let Some(row) = self.grid.row_mut(self.cursor.row) {
                    row.wrapped = true;
                }
            } else {
                self.cursor.col = screen_width - columns;
            }
        }

        if self.modes.insert_mode {
            self.insert_cells(columns);
        }
        self.clear_selection_if_row_selected(self.cursor.row);

        for col in self.cursor.col..self.cursor.col + columns {
            self.clear_multicell_span_at(self.cursor.row, col);
            self.clear_wide_cell_at(self.cursor.row, col);
        }

        let text: Arc<str> = Arc::from(text);
        for offset in 0..columns {
            let Some(cell) = self.grid.get_mut(self.cursor.row, self.cursor.col + offset) else {
                continue;
            };
            cell.reset();
            self.style.apply_to(cell);
            if offset == 0 {
                cell.set_text_size_payload(&text);
            }
            cell.attrs.remove(CellAttrs::WIDE | CellAttrs::WIDE_SPACER);
            cell.multicell = Some(Multicell::new_width_span(
                Arc::clone(&text),
                columns as u8,
                offset as u8,
                natural_width,
            ));
        }

        self.cursor.col += columns;
        self.dirty = true;
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

        let anchor = col.saturating_sub(usize::from(multicell.column_offset));
        let shared: Arc<str> = Arc::from(candidate.as_str());
        for offset in 0..usize::from(multicell.columns) {
            let Some(cell) = self.grid.get_mut(self.cursor.row, anchor + offset) else {
                continue;
            };
            let Some(metadata) = cell.multicell.as_mut() else {
                continue;
            };
            if metadata.same_span(&multicell) && usize::from(metadata.column_offset) == offset {
                metadata.set_text(Arc::clone(&shared));
                if offset == 0 {
                    cell.set_text_size_payload(&shared);
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
        let anchor = col.saturating_sub(usize::from(multicell.column_offset));
        for offset in 0..usize::from(multicell.columns) {
            let Some(cell) = self.grid.get_mut(row, anchor + offset) else {
                continue;
            };
            if cell.multicell.as_ref().is_some_and(|metadata| {
                metadata.same_span(&multicell) && usize::from(metadata.column_offset) == offset
            }) {
                cell.reset();
            }
        }
    }

    pub(super) fn clear_multicells_intersecting_row_range(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
    ) {
        let anchors = (start..end.min(self.width()))
            .filter_map(|col| {
                self.grid.get(row, col).and_then(|cell| {
                    cell.multicell
                        .as_ref()
                        .map(|metadata| col.saturating_sub(usize::from(metadata.column_offset)))
                })
            })
            .collect::<HashSet<_>>();
        for anchor in anchors {
            self.clear_multicell_span_at(row, anchor);
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
