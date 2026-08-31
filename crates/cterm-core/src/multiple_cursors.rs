//! Kitty multiple-cursors protocol state.
//!
//! Extra cursors are viewport overlays. They deliberately do not move with
//! terminal contents when the grid scrolls.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Color, CursorStyle, Rgb};

/// Shape of an application-defined extra cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtraCursorShape {
    Block,
    Bar,
    Underline,
    FollowMain,
}

impl ExtraCursorShape {
    pub(crate) fn from_protocol(value: u16) -> Option<Self> {
        match value {
            1 => Some(Self::Block),
            2 => Some(Self::Bar),
            3 => Some(Self::Underline),
            29 => Some(Self::FollowMain),
            _ => None,
        }
    }

    pub(crate) const fn protocol_code(self) -> u16 {
        match self {
            Self::Block => 1,
            Self::Bar => 2,
            Self::Underline => 3,
            Self::FollowMain => 29,
        }
    }

    /// Resolve the protocol's follow-main shape at render time.
    pub const fn resolve(self, main: CursorStyle) -> CursorStyle {
        match self {
            Self::Block => CursorStyle::Block,
            Self::Bar => CursorStyle::Bar,
            Self::Underline => CursorStyle::Underline,
            Self::FollowMain => main,
        }
    }
}

/// One viewport-relative extra cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraCursor {
    pub row: usize,
    pub col: usize,
    pub shape: ExtraCursorShape,
}

/// Color selection used by every extra cursor.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExtraCursorColor {
    /// Use the corresponding main-cursor color.
    #[default]
    Main,
    /// Derive a contrasting color from the cell under the cursor.
    Reverse,
    /// Use an indexed or RGB terminal color.
    Color(Color),
}

impl ExtraCursorColor {
    pub(crate) fn parse(params: &[u16]) -> Option<Self> {
        match params {
            [0] => Some(Self::Main),
            [1] => Some(Self::Reverse),
            [2, red, green, blue, ..] => Some(Self::Color(Color::Rgb(Rgb::new(
                u8::try_from(*red).ok()?,
                u8::try_from(*green).ok()?,
                u8::try_from(*blue).ok()?,
            )))),
            [5, index, ..] => Some(Self::Color(Color::Indexed(u8::try_from(*index).ok()?))),
            _ => None,
        }
    }

    pub(crate) fn protocol_value(self) -> String {
        match self {
            Self::Main => "0".to_string(),
            Self::Reverse => "1".to_string(),
            Self::Color(Color::Indexed(index)) => format!("5:{index}"),
            Self::Color(Color::Rgb(rgb)) => format!("2:{}:{}:{}", rgb.r, rgb.g, rgb.b),
            // The protocol only accepts indexed and RGB explicit colors.
            Self::Color(Color::Default | Color::Ansi(_)) => "0".to_string(),
        }
    }
}

/// Shared color pair for all extra cursors.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraCursorColors {
    pub text: ExtraCursorColor,
    pub cursor: ExtraCursorColor,
}

/// Which member of the shared color pair an operation changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExtraCursorColorTarget {
    Text,
    Cursor,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct MultipleCursors {
    cells: BTreeMap<(usize, usize), ExtraCursorShape>,
    colors: ExtraCursorColors,
}

impl MultipleCursors {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = ExtraCursor> + '_ {
        self.cells
            .iter()
            .map(|(&(row, col), &shape)| ExtraCursor { row, col, shape })
    }

    pub fn colors(&self) -> ExtraCursorColors {
        self.colors
    }

    pub fn clear_positions(&mut self) -> bool {
        if self.cells.is_empty() {
            false
        } else {
            self.cells.clear();
            true
        }
    }

    pub fn reset(&mut self) -> bool {
        let changed = !self.cells.is_empty() || self.colors != ExtraCursorColors::default();
        self.cells.clear();
        self.colors = ExtraCursorColors::default();
        changed
    }

    pub fn retain_within(&mut self, height: usize, width: usize) -> bool {
        let old_len = self.cells.len();
        self.cells
            .retain(|&(row, col), _| row < height && col < width);
        old_len != self.cells.len()
    }

    pub fn set_points(
        &mut self,
        shape: Option<ExtraCursorShape>,
        points: impl IntoIterator<Item = (usize, usize)>,
        height: usize,
        width: usize,
    ) -> bool {
        let mut changed = false;
        for (row, col) in points {
            if row >= height || col >= width {
                continue;
            }
            match shape {
                Some(shape) => changed |= self.cells.insert((row, col), shape) != Some(shape),
                None => changed |= self.cells.remove(&(row, col)).is_some(),
            }
        }
        changed
    }

    pub fn set_rectangles(
        &mut self,
        shape: Option<ExtraCursorShape>,
        rectangles: &[(usize, usize, usize, usize)],
        height: usize,
        width: usize,
        full_screen: bool,
    ) -> bool {
        if full_screen {
            if shape.is_none() {
                return self.clear_positions();
            }
            let shape = shape.expect("checked above");
            let replacement = (0..height)
                .flat_map(|row| (0..width).map(move |col| ((row, col), shape)))
                .collect::<BTreeMap<_, _>>();
            if self.cells == replacement {
                return false;
            }
            self.cells = replacement;
            return true;
        }

        let Some(shape) = shape else {
            let old_len = self.cells.len();
            self.cells.retain(|&(row, col), _| {
                !rectangles.iter().any(|&(top, left, bottom, right)| {
                    top <= row && row <= bottom && left <= col && col <= right
                })
            });
            return self.cells.len() != old_len;
        };

        let mut changed = false;
        for &(top, left, bottom, right) in rectangles {
            if top >= height || left >= width || bottom < top || right < left {
                continue;
            }
            for row in top..=bottom.min(height.saturating_sub(1)) {
                for col in left..=right.min(width.saturating_sub(1)) {
                    changed |= self.cells.insert((row, col), shape) != Some(shape);
                }
            }
        }
        changed
    }

    pub fn set_color(&mut self, target: ExtraCursorColorTarget, color: ExtraCursorColor) -> bool {
        let slot = match target {
            ExtraCursorColorTarget::Text => &mut self.colors.text,
            ExtraCursorColorTarget::Cursor => &mut self.colors.cursor,
        };
        if *slot == color {
            false
        } else {
            *slot = color;
            true
        }
    }

    pub fn replace(
        &mut self,
        cursors: impl IntoIterator<Item = ExtraCursor>,
        colors: ExtraCursorColors,
    ) {
        self.cells = cursors
            .into_iter()
            .map(|cursor| ((cursor.row, cursor.col), cursor.shape))
            .collect();
        self.colors = colors;
    }

    pub fn state_response(&self) -> Vec<u8> {
        let mut response = String::from("\x1b[>100");
        for cursor in self.iter() {
            response.push_str(&format!(
                ";{}:2:{}:{}",
                cursor.shape.protocol_code(),
                cursor.row + 1,
                cursor.col + 1
            ));
        }
        response.push_str(" q");
        response.into_bytes()
    }

    pub fn color_response(&self) -> Vec<u8> {
        format!(
            "\x1b[>101;30:{};40:{} q",
            self.colors.text.protocol_value(),
            self.colors.cursor.protocol_value()
        )
        .into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rectangle_updates_replace_only_the_selected_cells() {
        let mut cursors = MultipleCursors::default();
        assert!(cursors.set_rectangles(Some(ExtraCursorShape::FollowMain), &[], 4, 5, true));
        assert_eq!(cursors.iter().count(), 20);
        assert!(cursors.set_rectangles(Some(ExtraCursorShape::Bar), &[(1, 1, 2, 2)], 4, 5, false));
        assert_eq!(cursors.iter().count(), 20);
        assert_eq!(cursors.cells.get(&(1, 1)), Some(&ExtraCursorShape::Bar));
        assert_eq!(
            cursors.cells.get(&(0, 0)),
            Some(&ExtraCursorShape::FollowMain)
        );
    }

    #[test]
    fn query_responses_are_stable_and_row_major() {
        let mut cursors = MultipleCursors::default();
        cursors.set_points(Some(ExtraCursorShape::Underline), [(2, 3), (0, 1)], 4, 5);
        assert_eq!(cursors.state_response(), b"\x1b[>100;3:2:1:2;3:2:3:4 q");
        assert_eq!(cursors.color_response(), b"\x1b[>101;30:0;40:0 q");
    }
}
