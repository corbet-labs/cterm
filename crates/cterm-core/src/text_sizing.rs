//! Parser and value types for Kitty's OSC 66 text-sizing protocol.
//!
//! This module deliberately contains no screen mutation. Keeping the wire
//! grammar separate makes malformed input handling testable before multicell
//! layout, editing, reflow, and rendering are layered on top.

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Maximum text payload allowed by the Kitty text-sizing protocol.
pub(crate) const MAX_TEXT_SIZE_PAYLOAD_BYTES: usize = 4096;

/// Alignment of fractionally scaled text inside its cell block.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextSizeAlignment {
    #[default]
    Start,
    End,
    Center,
}

/// Layout metadata shared by every cell occupied by one OSC 66 text block.
///
/// The payload is reference counted so a fixed-width span does not duplicate
/// up to 4096 bytes in every grid cell. Offsets make each cell self-describing
/// across dirty-row protocol updates and scrollback boundaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Multicell {
    text: Arc<str>,
    pub columns: u8,
    pub rows: u8,
    pub column_offset: u8,
    pub row_offset: u8,
    pub scale: u8,
    pub fractional_scale: Option<(u8, u8)>,
    pub vertical_alignment: TextSizeAlignment,
    pub horizontal_alignment: TextSizeAlignment,
    pub natural_width: bool,
}

impl Multicell {
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        text: String,
        columns: u8,
        rows: u8,
        column_offset: u8,
        row_offset: u8,
        scale: u8,
        fractional_scale: Option<(u8, u8)>,
        vertical_alignment: TextSizeAlignment,
        horizontal_alignment: TextSizeAlignment,
        natural_width: bool,
    ) -> Option<Self> {
        let valid_fraction = fractional_scale
            .is_none_or(|(numerator, denominator)| denominator > numerator && denominator <= 15);
        if text.is_empty()
            || text.len() > MAX_TEXT_SIZE_PAYLOAD_BYTES
            || columns == 0
            || columns > 49
            || rows == 0
            || rows > 7
            || column_offset >= columns
            || row_offset >= rows
            || !(1..=7).contains(&scale)
            || !valid_fraction
        {
            return None;
        }
        Some(Self {
            text: Arc::from(text),
            columns,
            rows,
            column_offset,
            row_offset,
            scale,
            fractional_scale,
            vertical_alignment,
            horizontal_alignment,
            natural_width,
        })
    }

    pub(crate) fn new_width_span(
        text: Arc<str>,
        columns: u8,
        column_offset: u8,
        natural_width: bool,
    ) -> Self {
        Self {
            text,
            columns,
            rows: 1,
            column_offset,
            row_offset: 0,
            scale: 1,
            fractional_scale: None,
            vertical_alignment: TextSizeAlignment::Start,
            horizontal_alignment: TextSizeAlignment::Start,
            natural_width,
        }
    }

    /// Complete text rendered by this block, available from every occupied
    /// cell so a dirty row remains independently renderable.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn is_anchor(&self) -> bool {
        self.column_offset == 0 && self.row_offset == 0
    }

    pub(crate) fn set_text(&mut self, text: Arc<str>) {
        self.text = text;
    }

    pub(crate) fn same_span(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.rows == other.rows
            && self.scale == other.scale
            && self.fractional_scale == other.fractional_scale
            && self.vertical_alignment == other.vertical_alignment
            && self.horizontal_alignment == other.horizontal_alignment
            && self.natural_width == other.natural_width
            && self.text == other.text
    }
}

impl TextSizeAlignment {
    fn parse(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Start),
            1 => Some(Self::End),
            2 => Some(Self::Center),
            _ => None,
        }
    }
}

/// Validated metadata and text from one OSC 66 command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TextSizeRequest {
    /// Overall integer scale, in the protocol range 1 through 7.
    pub scale: u8,
    /// Requested width in unscaled cells. Zero selects natural-width chunks.
    pub width: u8,
    /// Optional fractional scale numerator and denominator.
    pub fractional_scale: Option<(u8, u8)>,
    pub vertical_alignment: TextSizeAlignment,
    pub horizontal_alignment: TextSizeAlignment,
    pub text: String,
}

impl TextSizeRequest {
    /// Split natural-width payloads into independently placeable graphemes.
    ///
    /// Fixed-width requests keep the complete payload in one chunk. Empty and
    /// zero-width graphemes are omitted because they cannot create a cell on
    /// their own; combining marks remain attached by Unicode segmentation.
    pub(crate) fn chunks(&self) -> Vec<TextSizeChunk<'_>> {
        if self.width != 0 {
            return vec![TextSizeChunk {
                text: &self.text,
                width: self.width,
                natural_width: false,
            }];
        }

        UnicodeSegmentation::graphemes(self.text.as_str(), true)
            .filter_map(|text| {
                let width = UnicodeWidthStr::width(text);
                if width == 0 {
                    return None;
                }
                let width = u8::try_from(width).ok()?.min(2);
                Some(TextSizeChunk {
                    text,
                    width,
                    natural_width: true,
                })
            })
            .collect()
    }
}

/// One independently laid-out text block derived from an OSC 66 request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TextSizeChunk<'a> {
    pub text: &'a str,
    pub width: u8,
    pub natural_width: bool,
}

/// Parse VTE's semicolon-split OSC parameters for Kitty OSC 66.
pub(crate) fn parse_text_size_request(params: &[&[u8]]) -> Option<TextSizeRequest> {
    let metadata = params.get(1).copied()?;
    let payload_parts = params.get(2..)?;

    // VTE splits every semicolon into a separate parameter. The first one
    // separates metadata from text; subsequent semicolons belong to the text.
    let payload_len = payload_parts
        .iter()
        .map(|part| part.len())
        .sum::<usize>()
        .saturating_add(payload_parts.len().saturating_sub(1));
    if payload_len == 0 || payload_len > MAX_TEXT_SIZE_PAYLOAD_BYTES {
        return None;
    }

    let mut scale = 1;
    let mut width = 0;
    let mut numerator = 0;
    let mut denominator = 0;
    let mut vertical_alignment = TextSizeAlignment::Start;
    let mut horizontal_alignment = TextSizeAlignment::Start;

    if !metadata.is_empty() {
        for field in metadata.split(|byte| *byte == b':') {
            let (&key, value) = field.split_first()?;
            let value = value.strip_prefix(b"=")?;
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let value = std::str::from_utf8(value).ok()?.parse::<u32>().ok()?;

            match key {
                b's' => scale = value.clamp(1, 7) as u8,
                b'w' => width = value.min(7) as u8,
                b'n' => numerator = value.min(15) as u8,
                b'd' => denominator = value.min(15) as u8,
                b'v' => {
                    vertical_alignment = TextSizeAlignment::parse(u8::try_from(value).ok()?)?;
                }
                b'h' => {
                    horizontal_alignment = TextSizeAlignment::parse(u8::try_from(value).ok()?)?;
                }
                _ => return None,
            }
        }
    }

    let fractional_scale =
        (denominator > numerator && denominator != 0).then_some((numerator, denominator));
    let mut bytes = Vec::with_capacity(payload_len);
    for (index, part) in payload_parts.iter().enumerate() {
        if index != 0 {
            bytes.push(b';');
        }
        bytes.extend_from_slice(part);
    }
    let text: String = String::from_utf8_lossy(&bytes)
        .chars()
        .filter(|character| {
            let codepoint = *character as u32;
            codepoint >= 0x20 && !(0x7f..=0x9f).contains(&codepoint)
        })
        .collect();
    if text.is_empty() {
        return None;
    }

    Some(TextSizeRequest {
        scale,
        width,
        fractional_scale,
        vertical_alignment,
        horizontal_alignment,
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_defaults_and_natural_width_graphemes() {
        let request =
            parse_text_size_request(&[b"66", b"", "a\u{301}猫".as_bytes()]).expect("valid request");

        assert_eq!(request.scale, 1);
        assert_eq!(request.width, 0);
        assert_eq!(request.fractional_scale, None);
        assert_eq!(
            request.chunks(),
            vec![
                TextSizeChunk {
                    text: "a\u{301}",
                    width: 1,
                    natural_width: true,
                },
                TextSizeChunk {
                    text: "猫",
                    width: 2,
                    natural_width: true,
                },
            ]
        );
    }

    #[test]
    fn parses_fixed_width_scale_fraction_and_alignment() {
        let request = parse_text_size_request(&[b"66", b"w=2:s=3:n=1:d=2:v=1:h=2", b"ab", b"cd"])
            .expect("valid request");

        assert_eq!(request.scale, 3);
        assert_eq!(request.width, 2);
        assert_eq!(request.fractional_scale, Some((1, 2)));
        assert_eq!(request.vertical_alignment, TextSizeAlignment::End);
        assert_eq!(request.horizontal_alignment, TextSizeAlignment::Center);
        assert_eq!(request.text, "ab;cd");
        assert_eq!(
            request.chunks(),
            vec![TextSizeChunk {
                text: "ab;cd",
                width: 2,
                natural_width: false,
            }]
        );
    }

    #[test]
    fn clamps_numeric_ranges_and_ignores_invalid_fraction() {
        let request =
            parse_text_size_request(&[b"66", b"w=99:s=0:n=9:d=4", b"x"]).expect("valid request");

        assert_eq!(request.scale, 1);
        assert_eq!(request.width, 7);
        assert_eq!(request.fractional_scale, None);
    }

    #[test]
    fn rejects_malformed_unknown_and_oversized_requests() {
        assert!(parse_text_size_request(&[b"66", b"w=x", b"x"]).is_none());
        assert!(parse_text_size_request(&[b"66", b"q=1", b"x"]).is_none());
        assert!(parse_text_size_request(&[b"66", b"v=3", b"x"]).is_none());
        assert!(parse_text_size_request(&[b"66", b"v=256", b"x"]).is_none());
        assert!(parse_text_size_request(&[b"66", b"w=1", b""]).is_none());

        let oversized = vec![b'x'; MAX_TEXT_SIZE_PAYLOAD_BYTES + 1];
        assert!(parse_text_size_request(&[b"66", b"", &oversized]).is_none());
    }

    #[test]
    fn replaces_invalid_utf8_and_removes_control_characters() {
        let request =
            parse_text_size_request(&[b"66", b"w=1", b"a\x01\xffb"]).expect("valid request");
        assert_eq!(request.text, "a\u{fffd}b");
    }

    #[test]
    fn omits_standalone_zero_width_natural_chunks() {
        let request =
            parse_text_size_request(&[b"66", b"", "\u{200b}x".as_bytes()]).expect("valid request");
        assert_eq!(request.chunks().len(), 1);
        assert_eq!(request.chunks()[0].text, "x");
    }
}
