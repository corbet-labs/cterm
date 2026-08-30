//! Deterministic terminal workloads shared by the cterm-core benchmarks.
//!
//! The workload categories follow the comparison methodology published by the
//! MIT-licensed `rio-vt-benchmark` project. The byte streams and implementation
//! in this module are original to cterm; no upstream source code was copied.

// Each benchmark binary imports a different subset of this shared fixture.
#![allow(dead_code)]

use cterm_core::screen::ScreenConfig;
use cterm_core::{Parser, Screen};

pub const COLS: usize = 80;
pub const ROWS: usize = 24;

pub fn terminal(scrollback_lines: usize) -> (Screen, Parser) {
    (
        Screen::new(COLS, ROWS, ScreenConfig { scrollback_lines }),
        Parser::new(),
    )
}

pub mod corpus {
    /// A representative shell session combining text, colors, cursor motion,
    /// and occasional screen clears.
    pub fn mixed_session() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(320_000);

        for line in 0..3_000_u32 {
            bytes.extend_from_slice(b"\x1b[1;34mproject\x1b[0m  \x1b[32msrc/module-");
            bytes.extend_from_slice(line.to_string().as_bytes());
            bytes.extend_from_slice(b".rs\x1b[0m  \x1b[90mregular shell output\x1b[0m\r\n");

            if line % 64 == 0 {
                bytes.extend_from_slice(b"\x1b[2J\x1b[H\x1b[1;33mstatus refresh\x1b[0m\r\n");
            }
        }

        bytes
    }

    /// Plain printable ASCII with line endings and no escape sequences.
    pub fn ascii_plain() -> Vec<u8> {
        const LINE: &[u8] =
            b"the quick brown fox jumps over the lazy dog 0123456789 terminal workload";
        let mut bytes = Vec::with_capacity((LINE.len() + 2) * 4_000);

        for _ in 0..4_000 {
            bytes.extend_from_slice(LINE);
            bytes.extend_from_slice(b"\r\n");
        }

        bytes
    }

    /// A foreground-color change before every printable byte.
    pub fn sgr_churn() -> Vec<u8> {
        const TEXT: &[u8] = b"style changes stress terminal cells";
        let mut bytes = Vec::with_capacity(1_200_000);

        for line in 0..2_500_u32 {
            for (column, byte) in TEXT.iter().copied().enumerate() {
                let color = (line + column as u32) % 256;
                bytes.extend_from_slice(b"\x1b[38;5;");
                bytes.extend_from_slice(color.to_string().as_bytes());
                bytes.push(b'm');
                bytes.push(byte);
            }
            bytes.extend_from_slice(b"\x1b[0m\r\n");
        }

        bytes
    }

    /// Short lines which spend most of their time scrolling the viewport.
    pub fn scroll_storm() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(320_000);

        for line in 0..24_000_u32 {
            bytes.extend_from_slice(b"line ");
            bytes.extend_from_slice(line.to_string().as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }

        bytes
    }

    /// Repeated alternate-screen repaints shaped like a full-screen TUI.
    pub fn alternate_screen_redraw() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(720_000);
        bytes.extend_from_slice(b"\x1b[?1049h");

        for frame in 0..400_u32 {
            bytes.extend_from_slice(b"\x1b[2J\x1b[H");
            for row in 1..=24_u32 {
                bytes.extend_from_slice(b"\x1b[");
                bytes.extend_from_slice(row.to_string().as_bytes());
                bytes.extend_from_slice(b";1H\x1b[7m row ");
                bytes.extend_from_slice(row.to_string().as_bytes());
                bytes.extend_from_slice(b" frame ");
                bytes.extend_from_slice(frame.to_string().as_bytes());
                bytes.extend_from_slice(b" | TUI content and status fields       \x1b[0m");
            }
        }

        bytes.extend_from_slice(b"\x1b[?1049l");
        bytes
    }

    /// CJK, combining marks, emoji modifiers, ZWJ sequences, and flags.
    pub fn unicode_wide_and_graphemes() -> Vec<u8> {
        const LINE: &str =
            "日本語 中文 한글 | cafe\u{301} | 👩🏽\u{200d}💻 | 🇨🇭 | 👨\u{200d}👩\u{200d}👧\u{200d}👦 | क्\u{200d}ष ";
        let mut bytes = Vec::with_capacity(LINE.len() * 3_000);

        for _ in 0..3_000 {
            bytes.extend_from_slice(LINE.as_bytes());
            bytes.extend_from_slice(b"\r\n");
        }

        bytes
    }

    /// Long logical ASCII lines which become several soft-wrapped grid rows.
    pub fn wrapped_ascii() -> Vec<u8> {
        const CHUNK: &[u8] = b"lorem ipsum ";
        let mut bytes = Vec::with_capacity(420_000);

        for line in 0..1_500_u32 {
            bytes.extend_from_slice(b"logical-line-");
            bytes.extend_from_slice(line.to_string().as_bytes());
            bytes.push(b' ');
            for _ in 0..20 {
                bytes.extend_from_slice(CHUNK);
            }
            bytes.extend_from_slice(b"\r\n");
        }

        bytes
    }

    /// Long logical lines built from wide and multi-codepoint graphemes.
    pub fn wrapped_unicode() -> Vec<u8> {
        const CHUNK: &str = "界e\u{301}👩🏽\u{200d}💻🇨🇭 ";
        let mut bytes = Vec::with_capacity(480_000);

        for line in 0..1_200_u32 {
            bytes.extend_from_slice(b"unicode-line-");
            bytes.extend_from_slice(line.to_string().as_bytes());
            bytes.push(b' ');
            for _ in 0..18 {
                bytes.extend_from_slice(CHUNK.as_bytes());
            }
            bytes.extend_from_slice(b"\r\n");
        }

        bytes
    }

    /// Exactly one visible screen addressed row by row, without scrollback.
    pub fn filled_viewport() -> Vec<u8> {
        let mut bytes = Vec::with_capacity(2_400);

        for row in 1..=24_u32 {
            bytes.extend_from_slice(b"\x1b[");
            bytes.extend_from_slice(row.to_string().as_bytes());
            bytes.extend_from_slice(b";1H");
            for _ in 0..7 {
                bytes.extend_from_slice(b"0123456789");
            }
        }

        bytes
    }
}
