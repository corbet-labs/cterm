//! Terminal cell types
//!
//! A cell represents one display position in the terminal grid, including its
//! extended grapheme cluster, colors, and attributes.

use crate::color::Color;
use bitflags::bitflags;
use serde::{Deserialize, Serialize};
use smol_str::SmolStr;
use std::sync::Arc;

/// Kitty's private-use scalar for image placements that move with text.
pub const KITTY_IMAGE_PLACEHOLDER: char = '\u{10EEEE}';

/// Maximum UTF-8 payload retained in one terminal cell.
///
/// Real-world extended grapheme clusters are generally small. Bounding the
/// exceptional case prevents an untrusted PTY from growing a single cell
/// without limit by streaming combining characters.
pub const MAX_GRAPHEME_BYTES: usize = 64;

bitflags! {
    /// Cell rendering attributes
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
    pub struct CellAttrs: u16 {
        /// Bold/bright text
        const BOLD = 1 << 0;
        /// Italic text
        const ITALIC = 1 << 1;
        /// Underlined text
        const UNDERLINE = 1 << 2;
        /// Double underline
        const DOUBLE_UNDERLINE = 1 << 3;
        /// Curly underline (undercurl)
        const CURLY_UNDERLINE = 1 << 4;
        /// Dotted underline
        const DOTTED_UNDERLINE = 1 << 5;
        /// Dashed underline
        const DASHED_UNDERLINE = 1 << 6;
        /// Slowly blinking text (SGR 5)
        const BLINK = 1 << 7;
        /// Reverse video (swap fg/bg)
        const INVERSE = 1 << 8;
        /// Hidden/invisible text
        const HIDDEN = 1 << 9;
        /// Strikethrough text
        const STRIKETHROUGH = 1 << 10;
        /// Dim/faint text
        const DIM = 1 << 11;
        /// Overline
        const OVERLINE = 1 << 12;
        /// Wide character (takes 2 cells)
        const WIDE = 1 << 13;
        /// Placeholder for second cell of wide char
        const WIDE_SPACER = 1 << 14;
        /// Rapidly blinking text (SGR 6)
        const RAPID_BLINK = 1 << 15;
    }
}

impl CellAttrs {
    /// Check if either slow or rapid blinking is selected.
    pub fn has_blink(&self) -> bool {
        self.intersects(Self::BLINK | Self::RAPID_BLINK)
    }

    /// Clear both mutually exclusive blink speeds.
    pub fn clear_blink(&mut self) {
        self.remove(Self::BLINK | Self::RAPID_BLINK);
    }

    /// Check if any underline style is set
    pub fn has_underline(&self) -> bool {
        self.intersects(
            Self::UNDERLINE
                | Self::DOUBLE_UNDERLINE
                | Self::CURLY_UNDERLINE
                | Self::DOTTED_UNDERLINE
                | Self::DASHED_UNDERLINE,
        )
    }

    /// Clear all underline styles
    pub fn clear_underline(&mut self) {
        self.remove(
            Self::UNDERLINE
                | Self::DOUBLE_UNDERLINE
                | Self::CURLY_UNDERLINE
                | Self::DOTTED_UNDERLINE
                | Self::DASHED_UNDERLINE,
        );
    }
}

/// Hyperlink information (OSC 8)
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Hyperlink {
    /// Unique ID for the hyperlink (optional)
    pub id: Option<String>,
    /// The URI target
    pub uri: String,
}

impl Hyperlink {
    pub fn new(uri: String) -> Self {
        Self { id: None, uri }
    }

    pub fn with_id(id: String, uri: String) -> Self {
        Self { id: Some(id), uri }
    }
}

/// A single terminal cell containing one extended grapheme cluster
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cell {
    /// The extended grapheme cluster displayed in this cell.
    text: SmolStr,
    /// Foreground color
    pub fg: Color,
    /// Background color
    pub bg: Color,
    /// Underline color (if different from fg)
    pub underline_color: Option<Color>,
    /// Cell attributes (bold, italic, etc.)
    pub attrs: CellAttrs,
    /// Hyperlink if present (shared via Arc for efficiency)
    pub hyperlink: Option<Arc<Hyperlink>>,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            text: SmolStr::new_static(" "),
            fg: Color::Default,
            bg: Color::Default,
            underline_color: None,
            attrs: CellAttrs::empty(),
            hyperlink: None,
        }
    }
}

impl Cell {
    /// Create a new cell with the given character
    pub fn new(c: char) -> Self {
        Self {
            text: SmolStr::new(c.to_string()),
            ..Default::default()
        }
    }

    /// Return the full extended grapheme cluster in this cell.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Return the first scalar value, primarily for character classification.
    pub fn first_char(&self) -> char {
        self.text.chars().next().unwrap_or(' ')
    }

    /// Whether this cell carries Kitty's Unicode image-placeholder scalar.
    pub fn is_kitty_image_placeholder(&self) -> bool {
        self.first_char() == KITTY_IMAGE_PLACEHOLDER
    }

    /// Return the cell content when it consists of exactly one scalar value.
    pub fn single_char(&self) -> Option<char> {
        let mut chars = self.text.chars();
        let first = chars.next()?;
        chars.next().is_none().then_some(first)
    }

    /// Replace the cell content with one scalar value.
    pub fn set_char(&mut self, c: char) {
        self.text = SmolStr::new(c.to_string());
    }

    /// Replace the cell content with a bounded UTF-8 string.
    pub fn set_text(&mut self, text: &str) {
        if text.is_empty() {
            self.text = SmolStr::new_static(" ");
            return;
        }

        let mut end = text.len().min(MAX_GRAPHEME_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.text = SmolStr::new(&text[..end]);
    }

    /// Append a scalar value while enforcing the per-cell memory bound.
    pub(crate) fn append_char(&mut self, c: char) -> bool {
        if self.text.len() + c.len_utf8() > MAX_GRAPHEME_BYTES {
            return false;
        }

        let mut text = String::with_capacity(self.text.len() + c.len_utf8());
        text.push_str(&self.text);
        text.push(c);
        self.text = SmolStr::new(text);
        true
    }

    /// Create an empty (space) cell
    pub fn empty() -> Self {
        Self::default()
    }

    /// Check if this cell is empty (space with default colors and no attrs)
    pub fn is_empty(&self) -> bool {
        self.text == " "
            && self.fg == Color::Default
            && self.bg == Color::Default
            && self.attrs.is_empty()
            && self.hyperlink.is_none()
    }

    /// Check if this cell is a wide character
    pub fn is_wide(&self) -> bool {
        self.attrs.contains(CellAttrs::WIDE)
    }

    /// Check if this cell is a spacer for a wide character
    pub fn is_wide_spacer(&self) -> bool {
        self.attrs.contains(CellAttrs::WIDE_SPACER)
    }

    /// Reset cell to empty state
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Copy attributes from another cell (colors and attrs, not character)
    pub fn copy_style_from(&mut self, other: &Cell) {
        self.fg = other.fg;
        self.bg = other.bg;
        self.underline_color = other.underline_color;
        self.attrs = other.attrs;
        self.hyperlink = other.hyperlink.clone();
    }
}

/// Current terminal styling state (used when writing new characters)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CellStyle {
    pub fg: Color,
    pub bg: Color,
    pub underline_color: Option<Color>,
    pub attrs: CellAttrs,
    pub hyperlink: Option<Arc<Hyperlink>>,
}

impl CellStyle {
    /// Apply this style to a cell
    pub fn apply_to(&self, cell: &mut Cell) {
        cell.fg = self.fg;
        cell.bg = self.bg;
        cell.underline_color = self.underline_color;
        cell.attrs = self.attrs;
        cell.hyperlink = self.hyperlink.clone();
    }

    /// Create a cell with this style and the given character
    pub fn create_cell(&self, c: char) -> Cell {
        Cell {
            text: SmolStr::new(c.to_string()),
            fg: self.fg,
            bg: self.bg,
            underline_color: self.underline_color,
            attrs: self.attrs,
            hyperlink: self.hyperlink.clone(),
        }
    }

    /// Reset to default style
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cell_default() {
        let cell = Cell::default();
        assert_eq!(cell.text(), " ");
        assert!(cell.is_empty());
    }

    #[test]
    fn test_cell_not_empty() {
        let mut cell = Cell::new('A');
        assert!(!cell.is_empty());

        cell = Cell::default();
        cell.fg = Color::Ansi(crate::color::AnsiColor::Red);
        assert!(!cell.is_empty());
    }

    #[test]
    fn test_cell_attrs() {
        let mut attrs = CellAttrs::BOLD | CellAttrs::UNDERLINE;
        assert!(attrs.contains(CellAttrs::BOLD));
        assert!(attrs.has_underline());

        attrs.clear_underline();
        assert!(!attrs.has_underline());
        assert!(attrs.contains(CellAttrs::BOLD));
    }

    #[test]
    fn test_cell_style_apply() {
        let style = CellStyle {
            fg: Color::Ansi(crate::color::AnsiColor::Red),
            attrs: CellAttrs::BOLD,
            ..Default::default()
        };

        let cell = style.create_cell('X');
        assert_eq!(cell.text(), "X");
        assert_eq!(cell.fg, Color::Ansi(crate::color::AnsiColor::Red));
        assert!(cell.attrs.contains(CellAttrs::BOLD));
    }
}
