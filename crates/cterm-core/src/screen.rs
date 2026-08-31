//! Screen - Terminal screen with scrollback buffer
//!
//! Manages the visible grid and scrollback history, handling resize
//! and scroll operations.

use crate::cell::{Cell, CellAttrs, CellStyle, MAX_GRAPHEME_BYTES};
use crate::color::{Color, ColorPalette, Rgb};
use crate::drcs::{DrcsFont, DrcsGlyph};
use crate::grid::{Grid, Row};
use crate::keyboard::KeyboardEnhancementFlags;
use crate::sixel::SixelImage;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Configuration for the screen
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenConfig {
    /// Maximum scrollback lines (0 = no scrollback)
    pub scrollback_lines: usize,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            scrollback_lines: 10000,
        }
    }
}

/// Independent sources which can request cursor blinking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorBlink {
    configured: bool,
    dec_mode_12: bool,
    decscusr: Option<bool>,
}

impl Default for CursorBlink {
    fn default() -> Self {
        Self {
            configured: true,
            dec_mode_12: false,
            decscusr: None,
        }
    }
}

impl CursorBlink {
    /// Effective blink state. DEC mode 12 and the style source are additive,
    /// matching foot's independent `decset` and `deccsusr` sources.
    pub fn enabled(self) -> bool {
        self.dec_mode_12 || self.decscusr.unwrap_or(self.configured)
    }

    /// Blink state selected by DECSCUSR, falling back to configuration.
    pub fn style_enabled(self) -> bool {
        self.decscusr.unwrap_or(self.configured)
    }

    /// DEC private mode 12 state, independent from DECSCUSR.
    pub fn dec_mode_12(self) -> bool {
        self.dec_mode_12
    }

    /// Explicit DECSCUSR blink selection, if an application supplied one.
    pub fn decscusr(self) -> Option<bool> {
        self.decscusr
    }

    /// Native frontend default used when DECSCUSR has no explicit override.
    pub fn configured(self) -> bool {
        self.configured
    }

    pub(crate) fn set_dec_mode_12(&mut self, enabled: bool) {
        self.dec_mode_12 = enabled;
    }

    pub(crate) fn set_decscusr(&mut self, enabled: Option<bool>) {
        self.decscusr = enabled;
    }
}

/// Cursor position and state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cursor {
    /// Column position (0-indexed)
    pub col: usize,
    /// Row position (0-indexed)
    pub row: usize,
    /// Cursor style
    pub style: CursorStyle,
    /// Independent configuration/DECSCUSR and DEC mode 12 blink sources.
    pub blink: CursorBlink,
    /// Configured shape restored by DECSCUSR 0 and terminal reset.
    configured_style: CursorStyle,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            col: 0,
            row: 0,
            style: CursorStyle::Block,
            blink: CursorBlink::default(),
            configured_style: CursorStyle::Block,
        }
    }
}

impl Cursor {
    /// Apply native configuration as the DECSCUSR default source.
    pub fn configure(&mut self, style: CursorStyle, blink: bool) {
        self.configured_style = style;
        self.style = style;
        self.blink.configured = blink;
        self.blink.decscusr = None;
    }

    /// Native frontend shape restored by DECSCUSR 0 and terminal resets.
    pub fn configured_style(&self) -> CursorStyle {
        self.configured_style
    }

    /// Restore configured style and blink while preserving DEC mode 12.
    pub fn reset_style_to_config(&mut self) {
        self.style = self.configured_style;
        self.blink.set_decscusr(None);
    }

    /// Restore application-controlled cursor state from a remote snapshot.
    /// An absent DECSCUSR override deliberately leaves the native configured
    /// shape and blink source intact.
    pub fn restore_protocol_snapshot(
        &mut self,
        style: Option<CursorStyle>,
        decscusr: Option<bool>,
        dec_mode_12: Option<bool>,
    ) {
        match decscusr {
            Some(decscusr) => {
                if let Some(style) = style {
                    self.style = style;
                }
                self.blink.set_decscusr(Some(decscusr));
            }
            None => {
                // A snapshot is authoritative: absence means the application
                // has no DECSCUSR override. Clear any value left by an older
                // snapshot while retaining this frontend's native defaults.
                self.reset_style_to_config();
            }
        }
        if let Some(dec_mode_12) = dec_mode_12 {
            self.blink.set_dec_mode_12(dec_mode_12);
        }
    }

    /// Reset protocol sources while preserving native configuration.
    pub fn reset_protocol_state(&mut self) {
        self.col = 0;
        self.row = 0;
        self.style = self.configured_style;
        self.blink.dec_mode_12 = false;
        self.blink.decscusr = None;
    }
}

/// Cursor shape style
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum CursorStyle {
    #[default]
    Block,
    Underline,
    Bar,
}

/// Scroll region bounds
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ScrollRegion {
    pub top: usize,
    pub bottom: usize,
}

impl ScrollRegion {
    pub fn contains(&self, row: usize) -> bool {
        row >= self.top && row < self.bottom
    }
}

const fn default_enabled() -> bool {
    true
}

const fn default_modify_other_keys() -> u8 {
    1
}

/// Visual theme class reported through foot's CSI ? 996 n extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum ThemeAppearance {
    #[default]
    Dark,
    Light,
}

/// Native window visibility reported through foot's CSI ? 998 n extension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowVisibility {
    #[default]
    Visible,
    Hidden,
}

/// State owned by the native frontend but needed for terminal protocol replies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrontendState {
    pub appearance: ThemeAppearance,
    pub visibility: WindowVisibility,
}

/// Terminal modes that affect behavior
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TerminalModes {
    /// Application cursor keys mode (DECCKM)
    pub application_cursor: bool,
    /// Application keypad mode (DECKPAM)
    pub application_keypad: bool,
    /// Auto-wrap mode (DECAWM)
    pub auto_wrap: bool,
    /// Reverse screen mode (DECSCNM)
    #[serde(default)]
    pub reverse_video: bool,
    /// Reverse-wrap mode (DEC private mode 45). When enabled together with
    /// auto-wrap, backspace at the left edge moves to the previous line.
    #[serde(default = "default_enabled")]
    pub reverse_wrap: bool,
    /// Origin mode (DECOM)
    pub origin_mode: bool,
    /// Insert mode (IRM)
    pub insert_mode: bool,
    /// Line feed/new line mode (LNM)
    pub line_feed_mode: bool,
    /// Show cursor (DECTCEM)
    pub show_cursor: bool,
    /// Mouse reporting mode
    pub mouse_mode: MouseMode,
    /// Mouse-coordinate encoding selected by DEC private modes 1006, 1015,
    /// and 1016.
    #[serde(default)]
    pub mouse_encoding: MouseEncoding,
    /// Alternate scroll mode (mode 1007): on the alternate screen, translate the
    /// scroll wheel into cursor-key input when the application isn't tracking the
    /// mouse. Enabled by default so pagers (less/man) scroll out of the box.
    pub alternate_scroll: bool,
    /// Bracketed paste mode
    pub bracketed_paste: bool,
    /// xterm modifyOtherKeys level. foot supports level 1 (default) and 2.
    #[serde(default = "default_modify_other_keys")]
    pub modify_other_keys: u8,
    /// Application synchronized updates (DEC private mode 2026)
    #[serde(default)]
    pub application_sync_updates: bool,
    /// Report native theme changes (DEC private mode 2031).
    #[serde(default)]
    pub theme_change_reports: bool,
    /// Report native window visibility changes (DEC private mode 2033).
    #[serde(default)]
    pub visibility_change_reports: bool,
    /// Focus events reporting
    pub focus_events: bool,
    /// Alternate screen buffer active
    pub alternate_screen: bool,
    /// Active charset (true = G1, false = G0) - controlled by SO/SI
    pub charset_g1_active: bool,
    /// Sixel scrolling mode (DECSDM, mode 80)
    /// When true (default), sixel images start at cursor and can scroll
    /// When false, sixel images start at top-left and don't scroll
    pub sixel_scrolling: bool,
    /// Position the cursor to the right of newly drawn sixels (mode 8452)
    /// instead of at the image's left edge on the following text row.
    #[serde(default)]
    pub sixel_cursor_right: bool,
    /// Give every Sixel image a fresh palette (DEC private mode 1070).
    /// Resetting the mode shares palette definitions between images.
    #[serde(default = "default_enabled")]
    pub sixel_private_palette: bool,
    /// G0 character set designator (None = standard ASCII)
    pub charset_g0: Option<String>,
    /// G1 character set designator (None = standard)
    pub charset_g1: Option<String>,
}

/// Character set designations
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Charset {
    /// ASCII (USASCII)
    #[default]
    Ascii,
    /// DEC Special Graphics (line drawing)
    DecSpecialGraphics,
    /// UK character set
    Uk,
}

/// Mouse reporting modes
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseMode {
    #[default]
    None,
    /// X10 mouse reporting
    X10,
    /// Normal tracking mode
    Normal,
    /// Button event tracking
    ButtonEvent,
    /// Any event tracking
    AnyEvent,
}

/// Encoding used for mouse reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseEncoding {
    /// Legacy X11-compatible `CSI M` byte encoding.
    #[default]
    Normal,
    /// SGR cell coordinates (DEC private mode 1006).
    Sgr,
    /// URXVT decimal cell coordinates (DEC private mode 1015).
    Urxvt,
    /// SGR pixel coordinates (DEC private mode 1016).
    SgrPixels,
}

/// Clipboard selection type for OSC 52
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardSelection {
    /// System clipboard (c)
    Clipboard,
    /// Primary selection (p)
    Primary,
    /// Both clipboard and primary (s)
    Select,
}

/// Clipboard operation from OSC 52
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ClipboardOperation {
    /// Set clipboard content (base64 decoded data)
    Set {
        selection: ClipboardSelection,
        data: Vec<u8>,
    },
    /// Query clipboard content
    Query { selection: ClipboardSelection },
}

/// Color query type (OSC 4 and OSC 10-12)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColorQuery {
    /// Query one entry in the 256-color palette (OSC 4)
    Palette(u8),
    /// Query foreground color (OSC 10)
    Foreground,
    /// Query background color (OSC 11)
    Background,
    /// Query cursor color (OSC 12)
    Cursor,
}

impl ColorQuery {
    pub const fn from_osc_code(code: u32) -> Option<Self> {
        match code {
            10 => Some(Self::Foreground),
            11 => Some(Self::Background),
            12 => Some(Self::Cursor),
            _ => None,
        }
    }

    pub const fn osc_code(self) -> u8 {
        match self {
            Self::Palette(_) => 4,
            Self::Foreground => 10,
            Self::Background => 11,
            Self::Cursor => 12,
        }
    }
}

/// Native urgency requested by Kitty OSC 99.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NotificationUrgency {
    Low,
    #[default]
    Normal,
    Critical,
}

/// A desktop notification requested by a terminal application.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesktopNotification {
    /// Stable Kitty notification identifier, when supplied.
    pub id: Option<String>,
    /// Short heading shown by the native notification service.
    pub title: String,
    /// Optional detail text shown below the heading.
    pub body: String,
    /// Requested native urgency.
    pub urgency: NotificationUrgency,
    /// Requested expiry in milliseconds; negative/absent means native default.
    pub expire_time: Option<i32>,
    /// Suppress notification sound.
    pub muted: bool,
    /// Focus the terminal when the notification is activated.
    pub focus: bool,
}

/// A native notification operation emitted by the terminal parser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopNotificationAction {
    Show(DesktopNotification),
    Close(String),
}

/// File transfer operation for iTerm2 OSC 1337 protocol
///
/// When inline=0, the protocol sends files that should be offered
/// to the user for saving rather than displayed inline.
#[derive(Debug)]
pub enum FileTransferOperation {
    /// A file was received and should be offered for saving (legacy, small files)
    FileReceived {
        /// Unique ID for this transfer
        id: u64,
        /// Filename (if provided)
        name: Option<String>,
        /// File data
        data: Vec<u8>,
    },
    /// A file was received via streaming (supports large files)
    StreamingFileReceived {
        /// Unique ID for this transfer
        id: u64,
        /// The streaming result containing params and data
        result: crate::streaming_file::StreamingFileResult,
    },
}

/// A point in the terminal buffer (absolute line index + column)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectionPoint {
    /// Absolute line index (0 = oldest scrollback line)
    pub line: usize,
    /// Column position
    pub col: usize,
}

impl SelectionPoint {
    pub fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }

    /// Returns true if self comes before other in reading order
    pub fn is_before(&self, other: &SelectionPoint) -> bool {
        self.line < other.line || (self.line == other.line && self.col < other.col)
    }
}

impl PartialOrd for SelectionPoint {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SelectionPoint {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.line.cmp(&other.line) {
            std::cmp::Ordering::Equal => self.col.cmp(&other.col),
            ord => ord,
        }
    }
}

/// Text selection state
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selection {
    /// Starting point of selection (where mouse was pressed)
    pub anchor: SelectionPoint,
    /// End of original anchor region (for word/line mode, the originally selected word/line end)
    /// This ensures the original word/line stays selected when extending in either direction
    pub anchor_end: Option<SelectionPoint>,
    /// Current end point of selection (where mouse is now)
    pub end: SelectionPoint,
    /// Selection type (char, word, line)
    pub mode: SelectionMode,
}

/// Selection granularity mode
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SelectionMode {
    /// Character-by-character selection (single click drag)
    #[default]
    Char,
    /// Word selection (double-click)
    Word,
    /// Line selection (triple-click)
    Line,
    /// Block/rectangular selection (Option+drag on macOS)
    Block,
}

impl Selection {
    /// Create a new selection starting at a point
    pub fn new(point: SelectionPoint, mode: SelectionMode) -> Self {
        Self {
            anchor: point,
            anchor_end: None,
            end: point,
            mode,
        }
    }

    /// Create a new selection with an anchor range (for word/line modes)
    pub fn new_with_range(
        anchor_start: SelectionPoint,
        anchor_end: SelectionPoint,
        mode: SelectionMode,
    ) -> Self {
        Self {
            anchor: anchor_start,
            anchor_end: Some(anchor_end),
            end: anchor_end,
            mode,
        }
    }

    /// Get the start and end points in reading order (start <= end)
    pub fn ordered(&self) -> (SelectionPoint, SelectionPoint) {
        match self.anchor_end {
            Some(anchor_end) => {
                // Word/line mode: anchor..anchor_end defines the original region
                if self.end.is_before(&self.anchor) {
                    // Dragging before anchor region: end..anchor_end
                    (self.end, anchor_end)
                } else if anchor_end.is_before(&self.end) {
                    // Dragging after anchor region: anchor..end
                    (self.anchor, self.end)
                } else {
                    // Within anchor region: anchor..anchor_end
                    (self.anchor, anchor_end)
                }
            }
            None => {
                if self.anchor.is_before(&self.end) {
                    (self.anchor, self.end)
                } else {
                    (self.end, self.anchor)
                }
            }
        }
    }

    /// Check if a cell at (line, col) is within the selection
    pub fn contains(&self, line: usize, col: usize) -> bool {
        let (start, end) = self.ordered();

        if line < start.line || line > end.line {
            return false;
        }

        // Block/rectangular selection: check if col is within column range
        if self.mode == SelectionMode::Block {
            let (min_col, max_col) = if self.anchor.col <= self.end.col {
                (self.anchor.col, self.end.col)
            } else {
                (self.end.col, self.anchor.col)
            };
            return col >= min_col && col <= max_col;
        }

        // Normal selection modes
        if start.line == end.line {
            // Single line selection
            col >= start.col && col <= end.col
        } else if line == start.line {
            // First line of multi-line selection
            col >= start.col
        } else if line == end.line {
            // Last line of multi-line selection
            col <= end.col
        } else {
            // Middle lines are fully selected
            true
        }
    }

    /// Update the end point of the selection
    pub fn extend_to(&mut self, point: SelectionPoint) {
        self.end = point;
    }
}

/// A terminal image (from Sixel or other protocols)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalImage {
    /// Unique image ID
    pub id: u64,
    /// Column position (cell coordinates)
    pub col: usize,
    /// Absolute line number (scrollback.len() + row at time of creation)
    pub line: usize,
    /// Width in cells
    pub cell_width: usize,
    /// Height in cells
    pub cell_height: usize,
    /// RGBA pixel data
    pub data: Arc<Vec<u8>>,
    /// Pixel width
    pub pixel_width: usize,
    /// Pixel height
    pub pixel_height: usize,
}

/// Sentinel column value meaning "end of row" for line selection mode.
/// Used in `SelectionPoint::col` to indicate the selection extends to the end of the line.
const COL_END_OF_ROW: usize = usize::MAX;

/// Terminal screen state
#[derive(Debug)]
pub struct Screen {
    /// Active display grid
    grid: Grid,
    /// Scrollback buffer (oldest lines first)
    scrollback: VecDeque<Row>,
    /// Alternate screen buffer (for vim, less, etc.)
    alternate_grid: Option<Grid>,
    /// Screen configuration
    config: ScreenConfig,
    /// Cursor state
    pub cursor: Cursor,
    /// Saved cursor state (for save/restore)
    saved_cursor: Option<Cursor>,
    /// Alternate saved cursor (for alternate screen)
    alt_saved_cursor: Option<Cursor>,
    /// Scroll region
    scroll_region: ScrollRegion,
    /// Current cell styling
    pub style: CellStyle,
    /// Terminal modes
    pub modes: TerminalModes,
    /// Window title
    pub title: String,
    /// Icon name
    pub icon_name: String,
    /// Whether content has changed since last render
    pub dirty: bool,
    /// Current scroll offset (for viewing scrollback)
    pub scroll_offset: usize,
    /// Bell was triggered (should be cleared after notification)
    pub bell: bool,
    /// Tab stop positions (columns where tabs stop)
    tab_stops: Vec<bool>,
    /// Pending responses to send back to the PTY (for DSR etc)
    pending_responses: Vec<Vec<u8>>,
    /// Pending clipboard operations from OSC 52
    pending_clipboard_ops: Vec<ClipboardOperation>,
    /// Pending desktop notifications from OSC 9/777/99.
    pending_notifications: Vec<DesktopNotificationAction>,
    /// Pending color queries (OSC 4 and OSC 10-12)
    pending_color_queries: Vec<(ColorQuery, Option<Rgb>)>,
    /// Frontend theme used for authoritative OSC color-query replies.
    base_palette: ColorPalette,
    /// Native frontend state used for theme and visibility protocol replies.
    frontend_state: FrontendState,
    /// Application-provided overrides from OSC 10-12.
    dynamic_foreground: Option<Rgb>,
    dynamic_background: Option<Rgb>,
    dynamic_cursor: Option<Rgb>,
    /// Application-provided overrides from OSC 4, indexed by palette entry.
    dynamic_palette: [Option<Rgb>; 256],
    /// xterm color-palette stack. The index is one-based, matching the wire
    /// protocol; zero means no current entry.
    color_stack: Vec<DynamicColorState>,
    color_stack_index: usize,
    /// Shell-reported working directory from OSC 7.
    current_working_directory: Option<PathBuf>,
    /// Current text selection (if any)
    pub selection: Option<Selection>,
    /// Terminal images (Sixel, etc.)
    images: HashMap<u64, TerminalImage>,
    /// Next image ID
    next_image_id: u64,
    /// Pending file transfer operations (iTerm2 OSC 1337 with inline=0)
    pending_file_transfers: Vec<FileTransferOperation>,
    /// Next file transfer ID
    next_file_transfer_id: u64,
    /// Cell height hint in pixels (set by UI layer for image row calculations)
    cell_height_hint: f64,
    /// Cell width hint in pixels (set by UI layer for image column calculations)
    cell_width_hint: f64,
    /// Keyboard enhancement stack for the primary screen.
    keyboard_main_stack: Vec<KeyboardEnhancementFlags>,
    /// Keyboard enhancement stack for the alternate screen.
    keyboard_alt_stack: Vec<KeyboardEnhancementFlags>,
    /// DRCS fonts (soft fonts) keyed by designator
    drcs_fonts: HashMap<String, DrcsFont>,
    /// Incremented whenever an application starts (or restarts) a synchronized
    /// update, allowing Terminal to re-arm its fail-safe deadline.
    sync_update_generation: u64,
}

#[derive(Debug, Clone)]
struct DynamicColorState {
    foreground: Option<Rgb>,
    background: Option<Rgb>,
    cursor: Option<Rgb>,
    palette: Box<[Option<Rgb>; 256]>,
}

/// Reflowed physical rows plus a mapping from old cell coordinates to their
/// new location. The mapping is shared by the cursor, selection, viewport and
/// image anchors so resize cannot silently detach metadata from its text.
struct ReflowedRows {
    rows: Vec<Row>,
    old_row_origins: Vec<(usize, usize)>,
    logical_boundaries: Vec<Vec<(usize, usize)>>,
    new_width: usize,
}

impl ReflowedRows {
    fn map_position(&self, row: usize, col: usize) -> (usize, usize) {
        let Some(&(logical_line, row_offset)) = self
            .old_row_origins
            .get(row.min(self.old_row_origins.len().saturating_sub(1)))
        else {
            return (0, 0);
        };
        let boundaries = &self.logical_boundaries[logical_line];
        let offset = row_offset.saturating_add(col);

        if let Some(&position) = boundaries.get(offset) {
            return position;
        }

        // A cursor or selection may sit in the unoccupied tail of a row. The
        // tail is not stored as text, so extend from the final mapped boundary.
        let &(row, col) = boundaries.last().unwrap_or(&(0, 0));
        let extra = offset.saturating_sub(boundaries.len().saturating_sub(1));
        let columns = col.saturating_add(extra);
        (row + columns / self.new_width, columns % self.new_width)
    }
}

fn reflow_rows(rows: &[Row], old_width: usize, new_width: usize) -> ReflowedRows {
    let new_width = new_width.max(1);
    let mut output = Vec::new();
    let mut old_row_origins = vec![(0, 0); rows.len()];
    let mut logical_boundaries = Vec::new();
    let mut group_start = 0;

    while group_start < rows.len() {
        let logical_line = logical_boundaries.len();
        let mut group_end = group_start;
        while group_end + 1 < rows.len() && rows[group_end + 1].wrapped {
            group_end += 1;
        }

        let mut cells = Vec::new();
        for row_index in group_start..=group_end {
            old_row_origins[row_index] = (logical_line, (row_index - group_start) * old_width);
            let continues = row_index < group_end;
            let used = if continues {
                rows[row_index].len()
            } else {
                let text_end = rows[row_index]
                    .iter()
                    .rposition(|cell| !cell.is_empty())
                    .map_or(0, |index| index + 1);
                let marker_end = rows[row_index]
                    .shell_integration
                    .command_start
                    .into_iter()
                    .chain(rows[row_index].shell_integration.command_end)
                    .max()
                    .unwrap_or(0);
                text_end.max(marker_end).min(rows[row_index].len())
            };

            cells.extend(
                rows[row_index]
                    .iter()
                    .take(used)
                    .filter(|cell| !cell.is_wide_spacer())
                    .cloned(),
            );
        }

        let first_row_index = output.len();
        let mut row = Row::new(new_width);
        row.wrapped = rows[group_start].wrapped;
        let mut col = 0;
        let mut boundaries = vec![(first_row_index, 0)];

        for mut cell in cells {
            let cell_width = if cell.is_wide() && new_width > 1 {
                2
            } else {
                1
            };

            if col > 0 && col + cell_width > new_width || col == new_width {
                output.push(row);
                row = Row::new(new_width);
                row.wrapped = true;
                col = 0;
                if let Some(boundary) = boundaries.last_mut() {
                    *boundary = (output.len(), 0);
                }
            }

            cell.attrs.remove(CellAttrs::WIDE_SPACER);
            row[col] = cell.clone();

            if cell_width == 2 {
                let mut spacer = cell;
                spacer.set_char(' ');
                spacer.attrs.remove(CellAttrs::WIDE);
                spacer.attrs.insert(CellAttrs::WIDE_SPACER);
                row[col + 1] = spacer;
            }

            for step in 1..=cell_width {
                boundaries.push((output.len(), col + step));
            }
            col += cell_width;
        }

        output.push(row);
        logical_boundaries.push(boundaries);
        group_start = group_end + 1;
    }

    if output.is_empty() {
        output.push(Row::new(new_width));
    }

    let mut reflowed = ReflowedRows {
        rows: output,
        old_row_origins,
        logical_boundaries,
        new_width,
    };

    // Shell markers refer to positions in the old grid. Remap them through
    // the same boundary table as cursors, selections and image anchors.
    let shell_markers: Vec<_> = rows
        .iter()
        .enumerate()
        .map(|(row, source)| {
            let prompt_row = source
                .shell_integration
                .prompt_marker
                .then(|| reflowed.map_position(row, 0).0);
            let command_start = source
                .shell_integration
                .command_start
                .map(|col| reflowed.map_position(row, col));
            let command_end = source
                .shell_integration
                .command_end
                .map(|col| reflowed.map_position(row, col));
            (prompt_row, command_start, command_end)
        })
        .collect();

    for (prompt_row, command_start, command_end) in shell_markers {
        if let Some(row) = prompt_row.and_then(|row| reflowed.rows.get_mut(row)) {
            row.shell_integration.prompt_marker = true;
        }
        if let Some((row, col)) = command_start {
            if let Some(row) = reflowed.rows.get_mut(row) {
                row.shell_integration.command_start = Some(col.min(new_width));
            }
        }
        if let Some((row, col)) = command_end {
            if let Some(row) = reflowed.rows.get_mut(row) {
                row.shell_integration.command_end = Some(col.min(new_width));
            }
        }
    }

    reflowed
}

impl Screen {
    /// Create a new screen with the given dimensions
    pub fn new(width: usize, height: usize, config: ScreenConfig) -> Self {
        let modes = TerminalModes {
            auto_wrap: true,
            reverse_wrap: true,
            show_cursor: true,
            sixel_scrolling: true, // Sixel scrolling enabled by default
            sixel_private_palette: true,
            alternate_scroll: true, // Alternate-screen wheel-to-arrows enabled by default
            modify_other_keys: 1,
            ..Default::default()
        };

        Self {
            grid: Grid::new(width, height),
            scrollback: VecDeque::with_capacity(config.scrollback_lines.min(1000)),
            alternate_grid: None,
            config,
            cursor: Cursor::default(),
            saved_cursor: None,
            alt_saved_cursor: None,
            scroll_region: ScrollRegion {
                top: 0,
                bottom: height,
            },
            style: CellStyle::default(),
            modes,
            title: String::new(),
            icon_name: String::new(),
            dirty: true,
            scroll_offset: 0,
            bell: false,
            tab_stops: Self::default_tab_stops(width),
            pending_responses: Vec::new(),
            pending_clipboard_ops: Vec::new(),
            pending_notifications: Vec::new(),
            pending_color_queries: Vec::new(),
            base_palette: ColorPalette::default(),
            frontend_state: FrontendState::default(),
            dynamic_foreground: None,
            dynamic_background: None,
            dynamic_cursor: None,
            dynamic_palette: [None; 256],
            color_stack: Vec::new(),
            color_stack_index: 0,
            current_working_directory: None,
            selection: None,
            images: HashMap::new(),
            // Zero is reserved as an invalid/sentinel identifier by native UI
            // code, so real terminal images always start at one.
            next_image_id: 1,
            pending_file_transfers: Vec::new(),
            next_file_transfer_id: 0,
            cell_height_hint: 16.0, // Default assumption
            cell_width_hint: 8.0,   // Default assumption
            keyboard_main_stack: Vec::new(),
            keyboard_alt_stack: Vec::new(),
            drcs_fonts: HashMap::new(),
            sync_update_generation: 0,
        }
    }

    /// Change application synchronized-update mode.  Repeated enable requests
    /// deliberately advance the generation so the one-second fail-safe is
    /// restarted, matching foot's behavior.
    pub(crate) fn set_application_sync_updates(&mut self, enabled: bool) {
        self.modes.application_sync_updates = enabled;
        if enabled {
            self.sync_update_generation = self.sync_update_generation.wrapping_add(1);
        }
    }

    pub(crate) fn sync_update_generation(&self) -> u64 {
        self.sync_update_generation
    }

    /// Queue a response to be sent back through the PTY
    pub fn queue_response(&mut self, response: Vec<u8>) {
        self.pending_responses.push(response);
    }

    /// Active kitty keyboard progressive-enhancement flags.
    pub fn keyboard_enhancement_flags(&self) -> KeyboardEnhancementFlags {
        self.active_keyboard_stack()
            .last()
            .copied()
            .unwrap_or_default()
    }

    /// Replace the current keyboard mode, applying only supported flags.
    pub fn set_keyboard_enhancement_flags(&mut self, flags: KeyboardEnhancementFlags) {
        let flags = flags & KeyboardEnhancementFlags::SUPPORTED;
        let stack = self.active_keyboard_stack_mut();
        if let Some(current) = stack.last_mut() {
            *current = flags;
        } else {
            stack.push(flags);
        }
    }

    /// Push a keyboard mode. The stack is bounded to avoid untrusted terminal
    /// output growing memory without limit.
    pub fn push_keyboard_enhancement_flags(&mut self, flags: KeyboardEnhancementFlags) {
        const MAX_DEPTH: usize = 16;
        let flags = flags & KeyboardEnhancementFlags::SUPPORTED;
        let stack = self.active_keyboard_stack_mut();
        if stack.len() == MAX_DEPTH {
            stack.remove(0);
        }
        stack.push(flags);
    }

    /// Pop one or more keyboard modes. An empty stack means legacy mode.
    pub fn pop_keyboard_enhancement_flags(&mut self, count: usize) {
        let stack = self.active_keyboard_stack_mut();
        let new_len = stack.len().saturating_sub(count);
        stack.truncate(new_len);
    }

    fn active_keyboard_stack(&self) -> &[KeyboardEnhancementFlags] {
        if self.modes.alternate_screen {
            &self.keyboard_alt_stack
        } else {
            &self.keyboard_main_stack
        }
    }

    fn active_keyboard_stack_mut(&mut self) -> &mut Vec<KeyboardEnhancementFlags> {
        if self.modes.alternate_screen {
            &mut self.keyboard_alt_stack
        } else {
            &mut self.keyboard_main_stack
        }
    }

    /// Queue a clipboard operation (from OSC 52)
    pub fn queue_clipboard_op(&mut self, op: ClipboardOperation) {
        self.pending_clipboard_ops.push(op);
    }

    /// Take all pending clipboard operations (drains the queue)
    pub fn take_clipboard_ops(&mut self) -> Vec<ClipboardOperation> {
        std::mem::take(&mut self.pending_clipboard_ops)
    }

    /// Check if there are pending clipboard operations
    pub fn has_clipboard_ops(&self) -> bool {
        !self.pending_clipboard_ops.is_empty()
    }

    /// Queue a native desktop notification requested by terminal output.
    pub fn queue_notification(&mut self, notification: DesktopNotification) {
        self.pending_notifications
            .push(DesktopNotificationAction::Show(notification));
    }

    /// Queue removal of a previously identified native notification.
    pub fn queue_notification_close(&mut self, id: String) {
        self.pending_notifications
            .push(DesktopNotificationAction::Close(id));
    }

    /// Drain all pending desktop notification requests.
    pub fn take_notifications(&mut self) -> Vec<DesktopNotificationAction> {
        std::mem::take(&mut self.pending_notifications)
    }

    /// Check whether terminal output queued desktop notifications.
    pub fn has_notifications(&self) -> bool {
        !self.pending_notifications.is_empty()
    }

    /// Queue a default-color query (from OSC 10-12)
    pub fn queue_color_query(&mut self, osc_code: u8) {
        let query = match osc_code {
            10 => ColorQuery::Foreground,
            11 => ColorQuery::Background,
            12 => ColorQuery::Cursor,
            _ => return,
        };
        let dynamic_color = self.dynamic_color(query);
        self.pending_color_queries.push((query, dynamic_color));
    }

    /// Queue one 256-color palette query (OSC 4).
    pub fn queue_palette_query(&mut self, index: u8) {
        let query = ColorQuery::Palette(index);
        self.pending_color_queries
            .push((query, self.dynamic_color(query)));
    }

    /// Take all pending color queries (drains the queue)
    pub fn take_color_queries(&mut self) -> Vec<(ColorQuery, Option<Rgb>)> {
        std::mem::take(&mut self.pending_color_queries)
    }

    /// Check if there are pending color queries
    pub fn has_color_queries(&self) -> bool {
        !self.pending_color_queries.is_empty()
    }

    /// Set the frontend's configured palette for OSC query replies.
    pub fn set_base_palette(&mut self, palette: ColorPalette) {
        self.base_palette = palette;
    }

    /// Return the frontend-owned state used by terminal protocol reports.
    pub fn frontend_state(&self) -> FrontendState {
        self.frontend_state
    }

    /// Apply the native cursor defaults used by DECSCUSR 0 and terminal reset.
    pub fn configure_cursor(&mut self, style: CursorStyle, blink: bool) {
        self.cursor.configure(style, blink);
        self.dirty = true;
    }

    /// Update the native theme class and report a change when requested.
    pub fn set_theme_appearance(&mut self, appearance: ThemeAppearance) {
        if self.frontend_state.appearance == appearance {
            return;
        }
        self.frontend_state.appearance = appearance;
        if self.modes.theme_change_reports {
            self.queue_theme_report();
        }
    }

    /// Update native visibility and report a change when requested.
    pub fn set_window_visibility(&mut self, visibility: WindowVisibility) {
        if self.frontend_state.visibility == visibility {
            return;
        }
        self.frontend_state.visibility = visibility;
        if self.modes.visibility_change_reports {
            self.queue_visibility_report();
        }
    }

    /// Queue foot's current-theme report (1 = dark, 2 = light).
    pub fn queue_theme_report(&mut self) {
        let value = match self.frontend_state.appearance {
            ThemeAppearance::Dark => 1,
            ThemeAppearance::Light => 2,
        };
        self.queue_response(format!("\x1b[?997;{value}n").into_bytes());
    }

    /// Queue foot's current-visibility report (1 = visible, 2 = hidden).
    pub fn queue_visibility_report(&mut self) {
        let value = match self.frontend_state.visibility {
            WindowVisibility::Visible => 1,
            WindowVisibility::Hidden => 2,
        };
        self.queue_response(format!("\x1b[?999;{value}n").into_bytes());
    }

    /// Set or reset an application-provided default color.
    pub fn set_dynamic_color(&mut self, target: ColorQuery, color: Option<Rgb>) {
        match target {
            ColorQuery::Palette(index) => self.dynamic_palette[index as usize] = color,
            ColorQuery::Foreground => self.dynamic_foreground = color,
            ColorQuery::Background => self.dynamic_background = color,
            ColorQuery::Cursor => self.dynamic_cursor = color,
        }
        self.dirty = true;
    }

    /// Return an application-provided default color override.
    pub fn dynamic_color(&self, target: ColorQuery) -> Option<Rgb> {
        match target {
            ColorQuery::Palette(index) => self.dynamic_palette[index as usize],
            ColorQuery::Foreground => self.dynamic_foreground,
            ColorQuery::Background => self.dynamic_background,
            ColorQuery::Cursor => self.dynamic_cursor,
        }
    }

    /// Resolve dynamic terminal defaults over a frontend theme palette.
    pub fn resolved_palette(&self, base: &ColorPalette) -> ColorPalette {
        let mut palette = base.clone();
        for (index, color) in self.dynamic_palette[..16].iter().enumerate() {
            if let Some(color) = color {
                palette.ansi[index] = *color;
            }
        }
        if let Some(color) = self.dynamic_foreground {
            palette.foreground = color;
        }
        if let Some(color) = self.dynamic_background {
            palette.background = color;
        }
        if let Some(color) = self.dynamic_cursor {
            palette.cursor = color;
        }
        palette
    }

    /// Resolve a cell color, including all OSC 4 palette overrides.
    pub fn resolve_color(&self, color: Color, palette: &ColorPalette) -> Rgb {
        match color {
            Color::Ansi(ansi) => {
                self.dynamic_palette[ansi as usize].unwrap_or_else(|| color.to_rgb(palette))
            }
            Color::Indexed(index) => {
                self.dynamic_palette[index as usize].unwrap_or_else(|| color.to_rgb(palette))
            }
            _ => color.to_rgb(palette),
        }
    }

    /// Reset every application-provided OSC 4 palette override.
    pub fn reset_dynamic_palette(&mut self) {
        self.dynamic_palette.fill(None);
        self.dirty = true;
    }

    /// Iterate over the active OSC 4 palette overrides.
    pub fn dynamic_palette_colors(&self) -> impl Iterator<Item = (u8, Rgb)> + '_ {
        self.dynamic_palette
            .iter()
            .enumerate()
            .filter_map(|(index, color)| color.map(|color| (index as u8, color)))
    }

    fn dynamic_color_state(&self) -> DynamicColorState {
        DynamicColorState {
            foreground: self.dynamic_foreground,
            background: self.dynamic_background,
            cursor: self.dynamic_cursor,
            palette: Box::new(self.dynamic_palette),
        }
    }

    /// Save the active application palette in an xterm color-stack slot.
    /// Slot zero means the entry after the current one; explicit slots are
    /// one-based and capped at xterm/foot's 128-entry limit.
    pub(crate) fn push_color_palette(&mut self, slot: usize) {
        const MAX_COLOR_STACK_DEPTH: usize = 128;

        let slot = if slot == 0 {
            self.color_stack_index.saturating_add(1)
        } else {
            slot
        }
        .min(MAX_COLOR_STACK_DEPTH);
        let state = self.dynamic_color_state();

        if self.color_stack.len() < slot {
            self.color_stack.resize(slot, state.clone());
        }
        self.color_stack_index = slot;
        self.color_stack[slot - 1] = state;
    }

    /// Restore an xterm color-stack slot. Slot zero pops the current entry.
    pub(crate) fn pop_color_palette(&mut self, slot: usize) {
        let slot = if slot == 0 {
            self.color_stack_index
        } else {
            slot
        };
        let Some(state) = slot
            .checked_sub(1)
            .and_then(|index| self.color_stack.get(index))
            .cloned()
        else {
            return;
        };

        self.color_stack_index = slot - 1;
        self.dynamic_foreground = state.foreground;
        self.dynamic_background = state.background;
        self.dynamic_cursor = state.cursor;
        self.dynamic_palette = *state.palette;
        self.dirty = true;
    }

    /// Return the current one-based xterm color-stack entry and allocated size.
    pub(crate) fn color_palette_stack_status(&self) -> (usize, usize) {
        (self.color_stack_index, self.color_stack.len())
    }

    /// Return one queried color from the frontend's configured palette.
    pub fn base_query_color(&self, target: ColorQuery) -> Rgb {
        match target {
            ColorQuery::Palette(index) => Color::Indexed(index).to_rgb(&self.base_palette),
            ColorQuery::Foreground => self.base_palette.foreground,
            ColorQuery::Background => self.base_palette.background,
            ColorQuery::Cursor => self.base_palette.cursor,
        }
    }

    /// Queue a file transfer operation (from OSC 1337 with inline=0)
    pub fn queue_file_transfer(&mut self, name: Option<String>, data: Vec<u8>) {
        let id = self.next_file_transfer_id;
        self.next_file_transfer_id += 1;
        self.pending_file_transfers
            .push(FileTransferOperation::FileReceived { id, name, data });
    }

    /// Queue a streaming file transfer operation
    pub fn queue_streaming_file_transfer(
        &mut self,
        result: crate::streaming_file::StreamingFileResult,
    ) {
        let id = self.next_file_transfer_id;
        self.next_file_transfer_id += 1;
        self.pending_file_transfers
            .push(FileTransferOperation::StreamingFileReceived { id, result });
    }

    /// Take all pending file transfer operations (drains the queue)
    pub fn take_file_transfers(&mut self) -> Vec<FileTransferOperation> {
        std::mem::take(&mut self.pending_file_transfers)
    }

    /// Check if there are pending file transfer operations
    pub fn has_file_transfers(&self) -> bool {
        !self.pending_file_transfers.is_empty()
    }

    /// Get the next file transfer ID (for pre-allocation)
    pub fn next_file_transfer_id(&self) -> u64 {
        self.next_file_transfer_id
    }

    /// Take all pending responses (drains the queue)
    pub fn take_pending_responses(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.pending_responses)
    }

    /// Check if there are pending responses
    pub fn has_pending_responses(&self) -> bool {
        !self.pending_responses.is_empty()
    }

    /// Create default tab stops (every 8 columns)
    fn default_tab_stops(width: usize) -> Vec<bool> {
        (0..width).map(|i| i % 8 == 0 && i > 0).collect()
    }

    /// Set a tab stop at the current cursor position
    pub fn set_tab_stop(&mut self) {
        let col = self.cursor.col;
        if col < self.tab_stops.len() {
            self.tab_stops[col] = true;
        }
    }

    /// Clear tab stop at current cursor position
    pub fn clear_tab_stop(&mut self) {
        let col = self.cursor.col;
        if col < self.tab_stops.len() {
            self.tab_stops[col] = false;
        }
    }

    /// Clear all tab stops
    pub fn clear_all_tab_stops(&mut self) {
        self.tab_stops.fill(false);
    }

    /// Move cursor to the next tab stop
    pub fn tab_forward(&mut self, count: usize) {
        let width = self.width();
        for _ in 0..count {
            // Find next tab stop
            let mut next_col = self.cursor.col + 1;
            while next_col < width && !self.tab_stops.get(next_col).copied().unwrap_or(false) {
                next_col += 1;
            }
            // If no tab stop found, go to the last column
            self.cursor.col = next_col.min(width.saturating_sub(1));
        }
        self.dirty = true;
    }

    /// Move cursor to the previous tab stop
    pub fn tab_backward(&mut self, count: usize) {
        for _ in 0..count {
            // Find previous tab stop
            if self.cursor.col == 0 {
                break;
            }
            let mut prev_col = self.cursor.col - 1;
            while prev_col > 0 && !self.tab_stops.get(prev_col).copied().unwrap_or(false) {
                prev_col -= 1;
            }
            // If no tab stop found, go to column 0
            self.cursor.col = prev_col;
        }
        self.dirty = true;
    }

    /// Get screen width
    pub fn width(&self) -> usize {
        self.grid.width()
    }

    /// Get screen height
    pub fn height(&self) -> usize {
        self.grid.height()
    }

    /// Get the active grid
    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    /// Get a mutable reference to the active grid
    pub fn grid_mut(&mut self) -> &mut Grid {
        &mut self.grid
    }

    /// Get scroll region
    pub fn scroll_region(&self) -> &ScrollRegion {
        &self.scroll_region
    }

    /// Set scroll region
    pub fn set_scroll_region(&mut self, top: usize, bottom: usize) {
        let top = top.min(self.height().saturating_sub(1));
        let bottom = bottom.min(self.height()).max(top + 1);
        self.scroll_region = ScrollRegion { top, bottom };
    }

    /// Reset scroll region to full screen
    pub fn reset_scroll_region(&mut self) {
        self.scroll_region = ScrollRegion {
            top: 0,
            bottom: self.height(),
        };
    }

    /// Get scrollback buffer
    pub fn scrollback(&self) -> &VecDeque<Row> {
        &self.scrollback
    }

    /// Get mutable scrollback buffer
    pub fn scrollback_mut(&mut self) -> &mut VecDeque<Row> {
        &mut self.scrollback
    }

    /// Get alternate grid if active
    pub fn alternate_grid(&self) -> Option<&Grid> {
        self.alternate_grid.as_ref()
    }

    /// Get saved cursor
    pub fn saved_cursor(&self) -> Option<&Cursor> {
        self.saved_cursor.as_ref()
    }

    /// Get alternate saved cursor
    pub fn alt_saved_cursor(&self) -> Option<&Cursor> {
        self.alt_saved_cursor.as_ref()
    }

    /// Get tab stops
    pub fn tab_stops(&self) -> &[bool] {
        &self.tab_stops
    }

    /// Total lines (scrollback + visible)
    pub fn total_lines(&self) -> usize {
        self.scrollback.len() + self.height()
    }

    /// Resize the screen
    pub fn resize(&mut self, width: usize, height: usize) {
        let width = width.max(1);
        let height = height.max(1);
        if width == self.width() && height == self.height() {
            return;
        }

        // Save old dimensions BEFORE resizing grid, for scroll region adjustment
        let old_height = self.height();
        let old_scroll_bottom = self.scroll_region.bottom;
        let old_width = self.width();

        if self.modes.alternate_screen {
            // Alternate-screen applications own their layout and generally
            // repaint after SIGWINCH. Keep their active coordinates stable,
            // but reflow the hidden primary buffer immediately so returning
            // from the application cannot reveal truncated history.
            let primary = self
                .alternate_grid
                .take()
                .unwrap_or_else(|| Grid::new(old_width, old_height));
            let alt_grid = std::mem::replace(&mut self.grid, primary);
            let primary_cursor = self.alt_saved_cursor.take().unwrap_or_default();
            let alt_cursor = std::mem::replace(&mut self.cursor, primary_cursor);

            self.resize_primary_grid(old_width, width, height);

            let resized_primary = std::mem::replace(&mut self.grid, alt_grid);
            let resized_primary_cursor = std::mem::replace(&mut self.cursor, alt_cursor);
            self.alternate_grid = Some(resized_primary);
            self.alt_saved_cursor = Some(resized_primary_cursor);

            self.grid.resize(width, height);
            self.cursor.col = self.cursor.col.min(width.saturating_sub(1));
            self.cursor.row = self.cursor.row.min(height.saturating_sub(1));
        } else {
            self.resize_primary_grid(old_width, width, height);
        }

        // Update scroll region
        // If scroll region was at full screen height, extend it to new height
        if old_scroll_bottom == old_height {
            self.scroll_region.bottom = height;
        } else {
            self.scroll_region.bottom = self.scroll_region.bottom.min(height);
        }
        self.scroll_region.top = self.scroll_region.top.min(height.saturating_sub(1));

        // Resize tab stops array to match new width
        self.tab_stops.resize(width, false);
        // Set default tab stops (every 8 columns) for new columns
        for i in old_width..width {
            self.tab_stops[i] = i % 8 == 0;
        }

        self.dirty = true;
    }

    fn resize_primary_grid(&mut self, old_width: usize, width: usize, height: usize) {
        let old_scrollback_len = self.scrollback.len();
        let old_scroll_offset = self.scroll_offset;
        let old_visible_top = old_scrollback_len.saturating_sub(old_scroll_offset);
        let old_rows: Vec<Row> = self
            .scrollback
            .iter()
            .chain(self.grid.iter())
            .cloned()
            .collect();
        let mut reflowed = reflow_rows(&old_rows, old_width, width);

        let old_cursor_line = old_scrollback_len + self.cursor.row;
        let (cursor_line, cursor_col) = reflowed.map_position(old_cursor_line, self.cursor.col);
        let viewport_start = cursor_line.saturating_sub(height.saturating_sub(1));
        let viewport_end = viewport_start + height;
        if reflowed.rows.len() < viewport_end {
            reflowed.rows.resize_with(viewport_end, || Row::new(width));
        }

        let front_drop = viewport_start.saturating_sub(self.config.scrollback_lines);
        let new_scrollback_len = viewport_start - front_drop;

        let remap_point = |point: SelectionPoint| -> Option<SelectionPoint> {
            let source_col = if point.col == COL_END_OF_ROW {
                old_width.saturating_sub(1)
            } else {
                point.col.min(old_width)
            };
            let (line, col) = reflowed.map_position(point.line, source_col);
            if line < front_drop || line >= viewport_end {
                return None;
            }
            Some(SelectionPoint::new(
                line - front_drop,
                if point.col == COL_END_OF_ROW {
                    COL_END_OF_ROW
                } else {
                    col.min(width.saturating_sub(1))
                },
            ))
        };

        self.selection = self.selection.take().and_then(|selection| {
            Some(Selection {
                anchor: remap_point(selection.anchor)?,
                end: remap_point(selection.end)?,
                mode: selection.mode,
                anchor_end: selection.anchor_end.and_then(remap_point),
            })
        });

        self.images.retain(|_, image| {
            let (line, col) = reflowed.map_position(image.line, image.col);
            if line < front_drop || line >= viewport_end {
                return false;
            }
            image.line = line - front_drop;
            image.col = col.min(width.saturating_sub(1));
            true
        });

        if let Some(saved) = self.saved_cursor.as_mut() {
            let (line, col) = reflowed.map_position(old_scrollback_len + saved.row, saved.col);
            saved.row = line.saturating_sub(viewport_start).min(height - 1);
            saved.col = col.min(width - 1);
        }

        let mapped_visible_top = reflowed.map_position(old_visible_top, 0).0;
        self.scroll_offset = if old_scroll_offset == 0 {
            0
        } else {
            let relative_top = mapped_visible_top.saturating_sub(front_drop);
            new_scrollback_len.saturating_sub(relative_top.min(new_scrollback_len))
        };

        self.scrollback = reflowed.rows[front_drop..viewport_start]
            .iter()
            .cloned()
            .collect();
        let mut grid = Grid::new(width, height);
        for (index, row) in reflowed.rows[viewport_start..viewport_end]
            .iter()
            .cloned()
            .enumerate()
        {
            grid[index] = row;
        }
        self.grid = grid;

        self.cursor.row = cursor_line.saturating_sub(viewport_start).min(height - 1);
        self.cursor.col = cursor_col.min(width - 1);
    }

    /// Get a cell at the given position
    pub fn get_cell(&self, row: usize, col: usize) -> Option<&Cell> {
        self.grid.get(row, col)
    }

    /// Get a cell from scrollback + visible area
    pub fn get_cell_with_scrollback(&self, line: usize, col: usize) -> Option<&Cell> {
        if line < self.scrollback.len() {
            self.scrollback.get(line)?.get(col)
        } else {
            let row = line - self.scrollback.len();
            self.grid.get(row, col)
        }
    }

    /// Move the cursor left for a C0 BS control, including DEC reverse-wrap.
    pub(crate) fn backspace(&mut self) {
        if self.cursor.col > 0 {
            self.cursor.col -= 1;
        } else if self.modes.reverse_wrap
            && self.modes.auto_wrap
            && self.cursor.row > self.scroll_region.top
        {
            self.cursor.row -= 1;
            self.cursor.col = self.width().saturating_sub(1);
        }
    }

    /// Put a character at the current cursor position
    pub fn put_char(&mut self, c: char) {
        let c = self.map_active_charset_char(c);

        if self.try_extend_previous_grapheme(c) {
            return;
        }

        let width = UnicodeWidthChar::width(c).unwrap_or(0).min(2);
        if width == 0 {
            return;
        }

        // Handle auto-wrap
        if self.cursor.col >= self.width() || (width > 1 && self.cursor.col + width > self.width())
        {
            if self.modes.auto_wrap {
                self.carriage_return();
                self.line_feed();
                if let Some(row) = self.grid.row_mut(self.cursor.row) {
                    row.wrapped = true;
                }
            } else {
                self.cursor.col = self.width() - 1;
            }
        }

        // Insert mode: shift characters right
        if self.modes.insert_mode && self.cursor.col < self.width() {
            self.insert_cells(width);
        }

        // Clear selection if writing to a selected row
        self.clear_selection_if_row_selected(self.cursor.row);

        self.clear_wide_cell_at(self.cursor.row, self.cursor.col);

        // Write the character
        if let Some(cell) = self.grid.get_mut(self.cursor.row, self.cursor.col) {
            cell.set_char(c);
            self.style.apply_to(cell);

            if width > 1 {
                cell.attrs.insert(CellAttrs::WIDE);
            }
        }

        // Handle wide characters (write spacer in next cell)
        if width > 1 && self.cursor.col + 1 < self.width() {
            if let Some(cell) = self.grid.get_mut(self.cursor.row, self.cursor.col + 1) {
                cell.set_char(' ');
                self.style.apply_to(cell);
                cell.attrs.remove(CellAttrs::WIDE);
                cell.attrs.insert(CellAttrs::WIDE_SPACER);
            }
        }

        // Advance cursor
        self.cursor.col += width;
        self.dirty = true;
    }

    /// Extend the preceding cell when the next scalar belongs to the same
    /// Unicode extended grapheme cluster.
    fn try_extend_previous_grapheme(&mut self, c: char) -> bool {
        let screen_width = self.width();
        if screen_width == 0 {
            return false;
        }

        let cursor_follows_cell = self.cursor.col > 0;
        let mut col = self.cursor.col.min(screen_width).saturating_sub(1);
        if self
            .grid
            .get(self.cursor.row, col)
            .is_some_and(Cell::is_wide_spacer)
        {
            col = col.saturating_sub(1);
        }

        let Some(cell) = self.grid.get(self.cursor.row, col) else {
            return false;
        };
        if cell.text().len() + c.len_utf8() > MAX_GRAPHEME_BYTES {
            return UnicodeWidthChar::width(c).unwrap_or(0) == 0;
        }

        let old_width = if cell.is_wide() { 2 } else { 1 };
        let mut candidate = String::with_capacity(cell.text().len() + c.len_utf8());
        candidate.push_str(cell.text());
        candidate.push(c);
        if UnicodeSegmentation::graphemes(candidate.as_str(), true).count() != 1 {
            return false;
        }

        let new_width = UnicodeWidthStr::width(candidate.as_str()).clamp(1, 2);
        let Some(cell) = self.grid.get_mut(self.cursor.row, col) else {
            return false;
        };
        if !cell.append_char(c) {
            return true;
        }
        cell.attrs.remove(CellAttrs::WIDE | CellAttrs::WIDE_SPACER);
        if new_width == 2 {
            cell.attrs.insert(CellAttrs::WIDE);
        }

        if new_width == 2 && old_width == 1 && col + 1 < screen_width {
            if let Some(spacer) = self.grid.get_mut(self.cursor.row, col + 1) {
                spacer.reset();
                self.style.apply_to(spacer);
                spacer.attrs.remove(CellAttrs::WIDE);
                spacer.attrs.insert(CellAttrs::WIDE_SPACER);
            }
        } else if new_width == 1 && old_width == 2 && col + 1 < screen_width {
            if let Some(spacer) = self.grid.get_mut(self.cursor.row, col + 1) {
                spacer.reset();
            }
        }

        if cursor_follows_cell {
            self.cursor.col = if new_width > old_width {
                (self.cursor.col + new_width - old_width).min(screen_width)
            } else {
                self.cursor.col.saturating_sub(old_width - new_width)
            };
        }
        self.dirty = true;
        true
    }

    /// Remove both halves of a wide cell before overwriting either half.
    fn clear_wide_cell_at(&mut self, row: usize, col: usize) {
        let attrs = match self.grid.get(row, col) {
            Some(cell) => cell.attrs,
            None => return,
        };

        if attrs.contains(CellAttrs::WIDE) {
            if let Some(spacer) = self.grid.get_mut(row, col + 1) {
                spacer.reset();
            }
        } else if attrs.contains(CellAttrs::WIDE_SPACER) && col > 0 {
            if let Some(wide) = self.grid.get_mut(row, col - 1) {
                wide.reset();
            }
        }
    }

    /// Insert blank cells at cursor, shifting existing cells right
    fn insert_cells(&mut self, count: usize) {
        let cursor_row = self.cursor.row;
        let cursor_col = self.cursor.col;
        let width = self.width();

        if let Some(row) = self.grid.row_mut(cursor_row) {
            for i in (cursor_col + count..width).rev() {
                let src_col = i - count;
                let src_cell = row[src_col].clone();
                row[i] = src_cell;
            }
            for i in cursor_col..cursor_col + count {
                if i < width {
                    row[i].reset();
                }
            }
        }
    }

    /// Move cursor to start of line
    pub fn carriage_return(&mut self) {
        self.cursor.col = 0;
    }

    /// Move cursor down, scrolling if needed
    pub fn line_feed(&mut self) {
        if self.cursor.row + 1 >= self.scroll_region.bottom {
            self.scroll_up(1);
        } else {
            self.cursor.row += 1;
        }
        self.dirty = true;
    }

    /// Scroll up within scroll region
    pub fn scroll_up(&mut self, count: usize) {
        let scrolled =
            self.grid
                .scroll_up(count, self.scroll_region.top, self.scroll_region.bottom);

        // Add to scrollback if not in alternate screen and scrolling from top
        if !self.modes.alternate_screen && self.scroll_region.top == 0 {
            let lines_added = scrolled.len();
            let mut lines_removed = 0;
            for row in scrolled {
                if self.scrollback.len() >= self.config.scrollback_lines {
                    self.scrollback.pop_front();
                    lines_removed += 1;
                }
                if self.config.scrollback_lines > 0 {
                    self.scrollback.push_back(row);
                } else {
                    lines_removed += 1;
                }
            }

            // If user is viewing scrollback (not at bottom), adjust scroll_offset
            // to keep the same content visible. Adding lines pushes content "up"
            // (increasing offset needed), while removing from front pushes content
            // "down" (decreasing offset needed).
            if self.scroll_offset > 0 {
                let net_change = lines_added.saturating_sub(lines_removed);
                self.scroll_offset += net_change;
                // Cap at scrollback length (in case viewed content was removed)
                self.scroll_offset = self.scroll_offset.min(self.scrollback.len());
            }

            // Handle selection when lines are removed from scrollback
            if lines_removed > 0 {
                if let Some(ref mut selection) = self.selection {
                    let (start, _end) = selection.ordered();
                    // If any part of the selection is in the removed lines, clear it
                    if start.line < lines_removed {
                        self.selection = None;
                    } else {
                        // Adjust selection indices to account for removed lines.
                        // All three of anchor, end, and anchor_end (when set for
                        // word/line modes) must be shifted together — otherwise
                        // the ordered range expands as new output arrives.
                        selection.anchor.line -= lines_removed;
                        selection.end.line -= lines_removed;
                        if let Some(ref mut anchor_end) = selection.anchor_end {
                            anchor_end.line -= lines_removed;
                        }
                    }
                }

                // Image anchors use the same buffer-relative line coordinates
                // as selections. Drop images whose anchor was evicted and
                // shift the surviving anchors with the text.
                self.images.retain(|_, image| {
                    if image.line < lines_removed {
                        false
                    } else {
                        image.line -= lines_removed;
                        true
                    }
                });
            }
        }

        self.dirty = true;
    }

    /// Scroll down within scroll region
    pub fn scroll_down(&mut self, count: usize) {
        self.grid
            .scroll_down(count, self.scroll_region.top, self.scroll_region.bottom);
        self.dirty = true;
    }

    /// Move cursor to position
    pub fn move_cursor(&mut self, row: usize, col: usize) {
        let (base_row, max_row) = if self.modes.origin_mode {
            (self.scroll_region.top, self.scroll_region.bottom)
        } else {
            (0, self.height())
        };

        self.cursor.row = (base_row + row).min(max_row.saturating_sub(1));
        self.cursor.col = col.min(self.width().saturating_sub(1));
    }

    /// Move cursor relative to current position
    pub fn move_cursor_relative(&mut self, row_delta: i32, col_delta: i32) {
        let new_row = (self.cursor.row as i32 + row_delta)
            .max(0)
            .min(self.height() as i32 - 1) as usize;
        let new_col = (self.cursor.col as i32 + col_delta)
            .max(0)
            .min(self.width() as i32 - 1) as usize;

        self.cursor.row = new_row;
        self.cursor.col = new_col;
    }

    /// Save cursor state
    pub fn save_cursor(&mut self) {
        self.saved_cursor = Some(self.cursor.clone());
    }

    /// Restore cursor state
    pub fn restore_cursor(&mut self) {
        if let Some(saved) = self.saved_cursor.take() {
            self.cursor = saved;
        }
    }

    /// Switch to alternate screen buffer
    pub fn enter_alternate_screen(&mut self) {
        if self.modes.alternate_screen {
            return;
        }

        self.modes.alternate_screen = true;
        self.alt_saved_cursor = Some(self.cursor.clone());

        let alt = Grid::new(self.width(), self.height());
        self.alternate_grid = Some(std::mem::replace(&mut self.grid, alt));

        self.cursor.reset_protocol_state();
        self.dirty = true;
    }

    /// Switch back to primary screen buffer
    pub fn exit_alternate_screen(&mut self) {
        if !self.modes.alternate_screen {
            return;
        }

        self.modes.alternate_screen = false;

        if let Some(primary) = self.alternate_grid.take() {
            self.grid = primary;
        }

        if let Some(saved) = self.alt_saved_cursor.take() {
            self.cursor = saved;
        }

        self.dirty = true;
    }

    /// Clear screen (or parts of it)
    pub fn clear(&mut self, mode: ClearMode) {
        let cursor_row = self.cursor.row;
        let cursor_col = self.cursor.col;
        let width = self.width();
        let height = self.height();

        // Clear selection if it overlaps with the cleared area
        match mode {
            ClearMode::Below => {
                self.clear_selection_if_rows_selected(cursor_row, height.saturating_sub(1));
            }
            ClearMode::Above => {
                self.clear_selection_if_rows_selected(0, cursor_row);
            }
            ClearMode::All => {
                self.clear_selection_if_rows_selected(0, height.saturating_sub(1));
            }
            ClearMode::Scrollback => {
                // Clearing scrollback invalidates all absolute line indices in the selection
                self.selection = None;
            }
        }

        match mode {
            ClearMode::Below => {
                // Clear from cursor to end of line
                if let Some(row) = self.grid.row_mut(cursor_row) {
                    for col in cursor_col..width {
                        row[col].reset();
                    }
                }
                // Clear all lines below
                for row_idx in cursor_row + 1..height {
                    if let Some(row) = self.grid.row_mut(row_idx) {
                        row.clear();
                    }
                }
            }
            ClearMode::Above => {
                // Clear all lines above
                for row_idx in 0..cursor_row {
                    if let Some(row) = self.grid.row_mut(row_idx) {
                        row.clear();
                    }
                }
                // Clear from start of line to cursor
                if let Some(row) = self.grid.row_mut(cursor_row) {
                    for col in 0..=cursor_col.min(width.saturating_sub(1)) {
                        row[col].reset();
                    }
                }
            }
            ClearMode::All => {
                self.grid.clear();
            }
            ClearMode::Scrollback => {
                self.scrollback.clear();
            }
        }
        self.dirty = true;
    }

    /// Clear line (or parts of it)
    pub fn clear_line(&mut self, mode: LineClearMode) {
        let cursor_row = self.cursor.row;
        let cursor_col = self.cursor.col;
        let width = self.width();

        // Clear selection if it overlaps with the cleared line
        self.clear_selection_if_row_selected(cursor_row);

        let (start, end) = match mode {
            LineClearMode::Right => (cursor_col, width),
            LineClearMode::Left => (0, cursor_col + 1),
            LineClearMode::All => (0, width),
        };

        if let Some(row) = self.grid.row_mut(cursor_row) {
            if matches!(mode, LineClearMode::All) {
                row.clear();
            } else {
                for col in start..end.min(width) {
                    row[col].reset();
                }
            }
        }
        self.dirty = true;
    }

    /// Apply the DEC-supported SGR attribute subset to an inclusive rectangle.
    ///
    /// DECCARA deliberately affects only bold, underline, blink and inverse;
    /// colors and all other attributes remain untouched. This matches VT400
    /// behavior and foot's implementation.
    pub(crate) fn change_rectangular_attributes(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        params: &[usize],
    ) {
        if top > bottom || left > right {
            return;
        }

        self.clear_selection_if_rows_selected(top, bottom);
        for row_index in top..=bottom.min(self.height().saturating_sub(1)) {
            let Some(row) = self.grid.row_mut(row_index) else {
                continue;
            };
            for col in left..=right.min(row.len().saturating_sub(1)) {
                let attrs = &mut row[col].attrs;
                for &param in params {
                    match param {
                        0 => {
                            attrs.remove(
                                CellAttrs::BOLD
                                    | CellAttrs::BLINK
                                    | CellAttrs::RAPID_BLINK
                                    | CellAttrs::INVERSE,
                            );
                            attrs.clear_underline();
                        }
                        1 => attrs.insert(CellAttrs::BOLD),
                        4 => {
                            attrs.clear_underline();
                            attrs.insert(CellAttrs::UNDERLINE);
                        }
                        5 => {
                            attrs.clear_blink();
                            attrs.insert(CellAttrs::BLINK);
                        }
                        7 => attrs.insert(CellAttrs::INVERSE),
                        22 => attrs.remove(CellAttrs::BOLD),
                        24 => attrs.clear_underline(),
                        25 => attrs.clear_blink(),
                        27 => attrs.remove(CellAttrs::INVERSE),
                        _ => {}
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Invert the DEC-supported SGR attribute subset in an inclusive rectangle.
    pub(crate) fn reverse_rectangular_attributes(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        params: &[usize],
    ) {
        if top > bottom || left > right {
            return;
        }

        self.clear_selection_if_rows_selected(top, bottom);
        for row_index in top..=bottom.min(self.height().saturating_sub(1)) {
            let Some(row) = self.grid.row_mut(row_index) else {
                continue;
            };
            for col in left..=right.min(row.len().saturating_sub(1)) {
                let attrs = &mut row[col].attrs;
                for &param in params {
                    match param {
                        0 => {
                            attrs.toggle(CellAttrs::BOLD | CellAttrs::BLINK | CellAttrs::INVERSE);
                            attrs.remove(CellAttrs::RAPID_BLINK);
                            Self::toggle_basic_underline(attrs);
                        }
                        1 => attrs.toggle(CellAttrs::BOLD),
                        4 => Self::toggle_basic_underline(attrs),
                        5 => {
                            attrs.toggle(CellAttrs::BLINK);
                            attrs.remove(CellAttrs::RAPID_BLINK);
                        }
                        7 => attrs.toggle(CellAttrs::INVERSE),
                        _ => {}
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn toggle_basic_underline(attrs: &mut CellAttrs) {
        if attrs.has_underline() {
            attrs.clear_underline();
        } else {
            attrs.insert(CellAttrs::UNDERLINE);
        }
    }

    /// Copy a rectangular cell area using snapshot semantics, so overlapping
    /// source and destination rectangles behave like `memmove`.
    pub(crate) fn copy_rectangular_area(
        &mut self,
        src_top: usize,
        src_left: usize,
        src_bottom: usize,
        src_right: usize,
        dst_top: usize,
        dst_left: usize,
    ) {
        if src_top > src_bottom || src_left > src_right {
            return;
        }

        let row_count = (src_bottom - src_top + 1).min(self.height().saturating_sub(dst_top));
        let col_count = (src_right - src_left + 1).min(self.width().saturating_sub(dst_left));
        if row_count == 0 || col_count == 0 {
            return;
        }

        let mut copy = Vec::with_capacity(row_count);
        for row_index in src_top..src_top + row_count {
            let Some(row) = self.grid.row(row_index) else {
                return;
            };
            copy.push(
                (src_left..src_left + col_count)
                    .map(|col| {
                        let mut cell = row[col].clone();
                        // Foot copies cell contents and SGR attributes, but
                        // deliberately does not copy OSC 8 URI ranges.
                        cell.hyperlink = None;
                        cell
                    })
                    .collect::<Vec<_>>(),
            );
        }

        let dst_bottom = dst_top + row_count - 1;
        let dst_right = dst_left + col_count - 1;
        self.clear_selection_if_rows_selected(dst_top, dst_bottom);
        self.remove_images_overlapping_rectangle(dst_top, dst_left, dst_bottom, dst_right);

        for row_offset in 0..row_count {
            for col_offset in 0..col_count {
                self.clear_wide_cell_at(dst_top + row_offset, dst_left + col_offset);
            }
        }
        for (row_offset, cells) in copy.into_iter().enumerate() {
            if let Some(row) = self.grid.row_mut(dst_top + row_offset) {
                for (col_offset, cell) in cells.into_iter().enumerate() {
                    row[dst_left + col_offset] = cell;
                }
            }
            self.repair_wide_cells_in_row(dst_top + row_offset);
        }
        self.dirty = true;
    }

    /// Fill an inclusive rectangle with a single-byte DEC character and the
    /// current SGR style (DECFRA).
    pub(crate) fn fill_rectangular_area(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
        c: char,
    ) {
        if top > bottom || left > right {
            return;
        }

        self.clear_selection_if_rows_selected(top, bottom);
        self.remove_images_overlapping_rectangle(top, left, bottom, right);
        let mut fill = Cell::new(c);
        self.style.apply_to(&mut fill);
        // OSC 8 is not an SGR attribute and is not applied by DECFRA.
        fill.hyperlink = None;

        for row_index in top..=bottom.min(self.height().saturating_sub(1)) {
            for col in left..=right.min(self.width().saturating_sub(1)) {
                self.clear_wide_cell_at(row_index, col);
                if let Some(cell) = self.grid.get_mut(row_index, col) {
                    *cell = fill.clone();
                }
            }
        }
        self.dirty = true;
    }

    /// Erase an inclusive rectangle using the current SGR background (DECERA).
    pub(crate) fn erase_rectangular_area(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    ) {
        if top > bottom || left > right {
            return;
        }

        self.clear_selection_if_rows_selected(top, bottom);
        self.remove_images_overlapping_rectangle(top, left, bottom, right);
        let mut blank = Cell::default();
        blank.bg = self.style.bg;

        for row_index in top..=bottom.min(self.height().saturating_sub(1)) {
            for col in left..=right.min(self.width().saturating_sub(1)) {
                self.clear_wide_cell_at(row_index, col);
                if let Some(cell) = self.grid.get_mut(row_index, col) {
                    *cell = blank.clone();
                }
            }
        }
        self.dirty = true;
    }

    fn repair_wide_cells_in_row(&mut self, row_index: usize) {
        let width = self.width();
        let Some(row) = self.grid.row_mut(row_index) else {
            return;
        };
        for col in 0..width {
            if row[col].attrs.contains(CellAttrs::WIDE) {
                if col + 1 >= width || !row[col + 1].attrs.contains(CellAttrs::WIDE_SPACER) {
                    row[col].reset();
                }
            } else if row[col].attrs.contains(CellAttrs::WIDE_SPACER)
                && (col == 0 || !row[col - 1].attrs.contains(CellAttrs::WIDE))
            {
                row[col].reset();
            }
        }
    }

    fn remove_images_overlapping_rectangle(
        &mut self,
        top: usize,
        left: usize,
        bottom: usize,
        right: usize,
    ) {
        let absolute_top = self.scrollback.len().saturating_add(top);
        let absolute_bottom = self
            .scrollback
            .len()
            .saturating_add(bottom)
            .saturating_add(1);
        self.images.retain(|_, image| {
            let image_bottom = image.line.saturating_add(image.cell_height.max(1));
            let image_right = image.col.saturating_add(image.cell_width.max(1));
            image_bottom <= absolute_top
                || image.line >= absolute_bottom
                || image_right <= left
                || image.col > right
        });
    }

    /// Delete characters at cursor position
    pub fn delete_chars(&mut self, count: usize) {
        let cursor_row = self.cursor.row;
        let cursor_col = self.cursor.col;
        let width = self.width();
        let count = count.min(width.saturating_sub(cursor_col));

        // Clear selection if it overlaps with the modified row
        self.clear_selection_if_row_selected(cursor_row);

        if let Some(row) = self.grid.row_mut(cursor_row) {
            // Shift characters left
            for col in cursor_col..width.saturating_sub(count) {
                row[col] = row[col + count].clone();
            }

            // Clear the rightmost cells
            for col in width.saturating_sub(count)..width {
                row[col].reset();
            }
        }
        self.dirty = true;
    }

    /// Insert blank lines at cursor position
    pub fn insert_lines(&mut self, count: usize) {
        if !self.scroll_region.contains(self.cursor.row) {
            return;
        }

        // Clear selection if it overlaps with the affected region
        self.clear_selection_if_rows_selected(self.cursor.row, self.scroll_region.bottom);

        // Scroll the region below cursor down
        let region_bottom = self.scroll_region.bottom;
        self.grid.scroll_down(count, self.cursor.row, region_bottom);
        self.cursor.col = 0;
        self.dirty = true;
    }

    /// Delete lines at cursor position
    pub fn delete_lines(&mut self, count: usize) {
        if !self.scroll_region.contains(self.cursor.row) {
            return;
        }

        // Clear selection if it overlaps with the affected region
        self.clear_selection_if_rows_selected(self.cursor.row, self.scroll_region.bottom);

        // Scroll the region from cursor up
        let region_bottom = self.scroll_region.bottom;
        self.grid.scroll_up(count, self.cursor.row, region_bottom);
        self.cursor.col = 0;
        self.dirty = true;
    }

    /// Reset terminal state
    pub fn reset(&mut self) {
        self.grid.clear();
        self.scrollback.clear();
        self.alternate_grid = None;
        self.cursor.reset_protocol_state();
        self.saved_cursor = None;
        self.alt_saved_cursor = None;
        self.scroll_region = ScrollRegion {
            top: 0,
            bottom: self.height(),
        };
        self.style = CellStyle::default();
        self.modes = TerminalModes {
            auto_wrap: true,
            reverse_wrap: true,
            show_cursor: true,
            sixel_scrolling: true,
            sixel_private_palette: true,
            alternate_scroll: true,
            modify_other_keys: 1,
            ..Default::default()
        };
        self.title.clear();
        self.icon_name.clear();
        self.dirty = true;
        self.scroll_offset = 0;
        self.images.clear();
        self.dynamic_foreground = None;
        self.dynamic_background = None;
        self.dynamic_cursor = None;
        self.dynamic_palette.fill(None);
        self.color_stack.clear();
        self.color_stack_index = 0;
        self.drcs_fonts.clear();
        self.keyboard_main_stack.clear();
        self.keyboard_alt_stack.clear();
    }

    /// Search for text in scrollback and visible buffer
    ///
    /// Returns all matches found, starting from the oldest scrollback line.
    /// Line index 0 is the oldest scrollback line, and increases toward
    /// the most recent visible line.
    pub fn find(&self, pattern: &str, case_sensitive: bool, regex: bool) -> Vec<SearchResult> {
        let mut results = Vec::new();

        if pattern.is_empty() {
            return results;
        }

        // Build the regex or prepare for simple search
        let regex_pattern = if regex {
            match regex::RegexBuilder::new(pattern)
                .case_insensitive(!case_sensitive)
                .build()
            {
                Ok(re) => Some(re),
                Err(_) => return results, // Invalid regex
            }
        } else {
            None
        };

        let search_pattern = if !case_sensitive && !regex {
            std::borrow::Cow::Owned(pattern.to_lowercase())
        } else {
            std::borrow::Cow::Borrowed(pattern)
        };

        // Reuse a single text buffer across all rows to avoid per-row allocation
        let mut text_buf = String::new();
        // Reuse a lowercase buffer for case-insensitive search
        let mut lower_buf = String::new();

        // Search scrollback
        for (line_idx, row) in self.scrollback.iter().enumerate() {
            row.write_text_to(&mut text_buf);
            Self::search_in_text(
                &text_buf,
                &mut lower_buf,
                line_idx,
                &search_pattern,
                case_sensitive,
                &regex_pattern,
                &mut results,
            );
        }

        // Search visible grid
        let scrollback_len = self.scrollback.len();
        for row_idx in 0..self.grid.height() {
            if let Some(row) = self.grid.row(row_idx) {
                row.write_text_to(&mut text_buf);
                Self::search_in_text(
                    &text_buf,
                    &mut lower_buf,
                    scrollback_len + row_idx,
                    &search_pattern,
                    case_sensitive,
                    &regex_pattern,
                    &mut results,
                );
            }
        }

        results
    }

    /// Search for pattern matches within a single row's text
    fn search_in_text(
        line_text: &str,
        lower_buf: &mut String,
        line_idx: usize,
        pattern: &str,
        case_sensitive: bool,
        regex_pattern: &Option<regex::Regex>,
        results: &mut Vec<SearchResult>,
    ) {
        if let Some(re) = regex_pattern {
            for m in re.find_iter(line_text) {
                results.push(SearchResult {
                    line: line_idx,
                    col: m.start(),
                    len: m.len(),
                });
            }
        } else {
            // Simple string search - reuse lower_buf for case-insensitive
            let search_text = if case_sensitive {
                line_text
            } else {
                lower_buf.clear();
                lower_buf.push_str(&line_text.to_lowercase());
                lower_buf.as_str()
            };

            let mut start = 0;
            while let Some(pos) = search_text[start..].find(pattern) {
                let col = start + pos;
                results.push(SearchResult {
                    line: line_idx,
                    col,
                    len: pattern.len(),
                });
                start = col + 1;
            }
        }
    }

    /// Convert a line index from find() to scroll offset
    ///
    /// Returns the scroll offset needed to show the given line at the top of the visible area.
    pub fn line_to_scroll_offset(&self, line_idx: usize) -> usize {
        let scrollback_len = self.scrollback.len();
        // If line is in scrollback, return offset; otherwise 0 for visible area
        scrollback_len.saturating_sub(line_idx)
    }

    /// Mark the current row as the beginning of a shell prompt (OSC 133 A).
    pub fn mark_shell_prompt(&mut self) {
        if let Some(row) = self.grid.row_mut(self.cursor.row) {
            row.shell_integration.prompt_marker = true;
        }
    }

    /// Mark the current cursor boundary as the start of command output
    /// (OSC 133 C).
    pub fn mark_command_start(&mut self) {
        if let Some(row) = self.grid.row_mut(self.cursor.row) {
            row.shell_integration.command_start = Some(self.cursor.col.min(row.len()));
        }
    }

    /// Mark the current cursor boundary as the end of command output
    /// (OSC 133 D).
    pub fn mark_command_end(&mut self) {
        if let Some(row) = self.grid.row_mut(self.cursor.row) {
            row.shell_integration.command_end = Some(self.cursor.col.min(row.len()));
        }
    }

    /// Move the viewport to the closest prompt before its current top edge.
    ///
    /// This intentionally ignores the alternate screen, matching Foot: prompt
    /// navigation operates on the normal shell's scrollback only.
    pub fn scroll_to_previous_prompt(&mut self) -> bool {
        if self.modes.alternate_screen {
            return false;
        }

        let first_visible = self.scrollback.len().saturating_sub(self.scroll_offset);
        let prompt = (0..first_visible).rev().find(|&line| {
            self.get_row_by_absolute_line(line)
                .is_some_and(|row| row.shell_integration.prompt_marker)
        });

        if let Some(line) = prompt {
            self.scroll_offset = self.line_to_scroll_offset(line);
            true
        } else {
            false
        }
    }

    /// Move the viewport to the next prompt after its current top edge.
    pub fn scroll_to_next_prompt(&mut self) -> bool {
        if self.modes.alternate_screen || self.scroll_offset == 0 {
            return false;
        }

        let first_visible = self.scrollback.len().saturating_sub(self.scroll_offset);
        let prompt = (first_visible + 1..self.total_lines()).find(|&line| {
            self.get_row_by_absolute_line(line)
                .is_some_and(|row| row.shell_integration.prompt_marker)
        });

        if let Some(line) = prompt {
            self.scroll_offset = self.line_to_scroll_offset(line);
            true
        } else {
            false
        }
    }

    /// Extract the output of the most recently completed shell command.
    ///
    /// OSC 133 C and D delimit cell boundaries. Wrapped physical rows are
    /// joined, while hard line breaks are retained.
    pub fn last_command_output(&self) -> Option<String> {
        let mut end = None;
        let mut start = None;

        for line in (0..self.total_lines()).rev() {
            let row = self.get_row_by_absolute_line(line)?;
            if let Some(col) = row.shell_integration.command_end {
                end = Some((line, col.min(row.len())));
            }
            if end.is_some() {
                if let Some(col) = row.shell_integration.command_start {
                    start = Some((line, col.min(row.len())));
                    break;
                }
            }
        }

        let ((start_line, start_col), (end_line, end_col)) = (start?, end?);
        if start_line > end_line || (start_line == end_line && start_col > end_col) {
            return None;
        }

        let mut output = String::new();
        for line in start_line..=end_line {
            let row = self.get_row_by_absolute_line(line)?;
            let from = if line == start_line { start_col } else { 0 };
            let to = if line == end_line { end_col } else { row.len() };

            for cell in row.iter().take(to).skip(from) {
                if !cell.is_wide_spacer() {
                    output.push_str(cell.text());
                }
            }

            if line < end_line {
                let next_is_wrapped = self
                    .get_row_by_absolute_line(line + 1)
                    .is_some_and(|next| next.wrapped);
                if !next_is_wrapped {
                    while output.ends_with(' ') {
                        output.pop();
                    }
                    output.push('\n');
                }
            }
        }

        Some(output)
    }

    // ========== Image Methods ==========

    /// Add an image at the specified position (legacy method)
    ///
    /// The image is stored with an absolute line number that includes scrollback,
    /// so it will scroll with the content.
    pub fn add_image(&mut self, col: usize, row: usize, sixel_image: SixelImage) {
        let cols = self.image_cols_for_width(sixel_image.width);
        let rows = self.image_rows_for_height(sixel_image.height);
        self.add_image_with_size(col, row, cols, rows, sixel_image);
    }

    /// Add an image at the specified position with known cell dimensions
    ///
    /// This also clears the grid cells underneath the image (xterm behavior).
    pub fn add_image_with_size(
        &mut self,
        col: usize,
        row: usize,
        cell_cols: usize,
        cell_rows: usize,
        sixel_image: SixelImage,
    ) {
        let id = self.next_image_id;
        self.next_image_id += 1;

        // Calculate absolute line (scrollback + visible row)
        let absolute_line = self.scrollback.len() + row;

        let image = TerminalImage {
            id,
            col,
            line: absolute_line,
            cell_width: cell_cols,
            cell_height: cell_rows,
            data: Arc::new(sixel_image.data),
            pixel_width: sixel_image.width,
            pixel_height: sixel_image.height,
        };

        // Clear grid cells underneath the image (xterm behavior)
        // This ensures text doesn't show through the image
        self.clear_cells_for_image(col, row, cell_cols, cell_rows);

        self.images.insert(id, image);
        self.dirty = true;
    }

    /// Clear grid cells that will be covered by an image
    fn clear_cells_for_image(&mut self, col: usize, row: usize, cols: usize, rows: usize) {
        let width = self.width();
        let height = self.height();

        for r in row..row + rows {
            if r >= height {
                break;
            }
            if let Some(grid_row) = self.grid.row_mut(r) {
                for c in col..col + cols {
                    if c >= width {
                        break;
                    }
                    // Clear the cell but keep it as a space (not truly empty)
                    grid_row[c].set_char(' ');
                    grid_row[c].attrs = CellAttrs::empty();
                }
            }
        }
    }

    /// Get images visible in the current viewport
    ///
    /// Returns images that overlap with the currently visible portion of the screen.
    pub fn visible_images(&self) -> Vec<&TerminalImage> {
        let scrollback_len = self.scrollback.len();
        let height = self.height();

        // Calculate the range of absolute lines currently visible
        let first_visible_line = scrollback_len.saturating_sub(self.scroll_offset);
        let last_visible_line = first_visible_line + height;

        let mut images: Vec<_> = self
            .images
            .values()
            .filter(|img| {
                // Image is visible if any part of it overlaps with the viewport
                let img_top = img.line;
                let img_rows = img.cell_height.max(1);
                let img_bottom = img.line + img_rows;

                img_bottom > first_visible_line && img_top < last_visible_line
            })
            .collect();

        // HashMap iteration order is deliberately unspecified. Images are
        // painted from oldest to newest so overlapping graphics are stable and
        // a later image consistently appears on top on every backend.
        images.sort_unstable_by_key(|image| image.id);
        images
    }

    /// Calculate the visible row for an image (relative to current viewport)
    ///
    /// Returns None if the image is not in the visible area.
    pub fn image_visible_row(&self, image: &TerminalImage) -> Option<isize> {
        let scrollback_len = self.scrollback.len();
        let first_visible_line = scrollback_len.saturating_sub(self.scroll_offset);
        let last_visible_line = first_visible_line + self.height();
        let image_bottom = image.line + image.cell_height.max(1);

        if image_bottom > first_visible_line && image.line < last_visible_line {
            Some(image.line as isize - first_visible_line as isize)
        } else {
            None
        }
    }

    /// Get all stored images in deterministic paint order.
    pub fn images(&self) -> Vec<&TerminalImage> {
        let mut images: Vec<_> = self.images.values().collect();
        images.sort_unstable_by_key(|image| image.id);
        images
    }

    /// Replace the stored image set from a daemon or relaunch snapshot.
    pub fn replace_images<I>(&mut self, images: I)
    where
        I: IntoIterator<Item = TerminalImage>,
    {
        self.images.clear();
        let mut largest_id = 0;
        for image in images {
            largest_id = largest_id.max(image.id);
            self.images.insert(image.id, image);
        }
        self.next_image_id = largest_id.saturating_add(1).max(1);
        self.dirty = true;
    }

    /// Get the image at a given visible row and column position
    ///
    /// Returns the image if one exists at that position, or None otherwise.
    /// Used for right-click context menu on images.
    pub fn image_at_position(&self, row: usize, col: usize) -> Option<&TerminalImage> {
        let scrollback_len = self.scrollback.len();
        let first_visible_line = scrollback_len.saturating_sub(self.scroll_offset);
        let absolute_line = first_visible_line + row;

        self.images.values().find(|img| {
            // Check if the click position is within the image bounds
            let img_top = img.line;
            let img_bottom = img.line + img.cell_height;
            let img_left = img.col;
            let img_right = img.col + img.cell_width;

            absolute_line >= img_top
                && absolute_line < img_bottom
                && col >= img_left
                && col < img_right
        })
    }

    /// Set the shell-reported working directory from OSC 7.
    pub fn set_current_working_directory(&mut self, path: Option<PathBuf>) {
        self.current_working_directory = path;
    }

    /// Return the shell-reported working directory, if available.
    pub fn current_working_directory(&self) -> Option<&Path> {
        self.current_working_directory.as_deref()
    }

    /// Get an image by its ID
    pub fn image_by_id(&self, id: u64) -> Option<&TerminalImage> {
        self.images.get(&id)
    }

    /// Clear all images (called on screen clear)
    pub fn clear_images(&mut self) {
        self.images.clear();
    }

    /// Set the cell height hint (call from UI layer when font metrics are known)
    pub fn set_cell_height_hint(&mut self, height: f64) {
        self.cell_height_hint = height;
    }

    /// Get the cell height hint
    pub fn cell_height_hint(&self) -> f64 {
        self.cell_height_hint
    }

    /// Calculate how many terminal rows an image of given pixel height will span
    pub fn image_rows_for_height(&self, pixel_height: usize) -> usize {
        if self.cell_height_hint <= 0.0 {
            // Fallback: assume roughly 1 row per 6 pixels (one sixel band)
            pixel_height.div_ceil(6)
        } else {
            ((pixel_height as f64) / self.cell_height_hint).ceil() as usize
        }
    }

    /// Set the cell width hint (call from UI layer when font metrics are known)
    pub fn set_cell_width_hint(&mut self, width: f64) {
        self.cell_width_hint = width;
    }

    /// Get the cell width hint
    pub fn cell_width_hint(&self) -> f64 {
        self.cell_width_hint
    }

    /// Calculate how many terminal columns an image of given pixel width will span
    pub fn image_cols_for_width(&self, pixel_width: usize) -> usize {
        if self.cell_width_hint <= 0.0 {
            // Fallback: assume roughly 1 col per pixel (very conservative)
            pixel_width
        } else {
            ((pixel_width as f64) / self.cell_width_hint).ceil() as usize
        }
    }

    // ========== DRCS (Soft Font) Methods ==========

    /// Add or replace a DRCS font
    ///
    /// The erase_control parameter determines what to erase:
    /// - 0: Erase all characters in DRCS buffer with matching width/rendition
    /// - 1: Erase only locations being reloaded
    /// - 2: Erase all renditions
    pub fn add_drcs_font(&mut self, font: DrcsFont, erase_control: u8, _font_number: u8) {
        let designator = font.designator.clone();

        match erase_control {
            0 | 2 => {
                // Erase all existing fonts with same designator
                self.drcs_fonts.remove(&designator);
            }
            1 => {
                // Only erase/replace specific glyphs being loaded
                // (handled by HashMap insert below)
            }
            _ => {}
        }

        // Insert the new font (or merge glyphs if erase_control == 1)
        if erase_control == 1 {
            if let Some(existing) = self.drcs_fonts.get_mut(&designator) {
                // Merge glyphs into existing font
                for (pos, glyph) in font.glyphs {
                    existing.glyphs.insert(pos, glyph);
                }
                return;
            }
        }

        self.drcs_fonts.insert(designator, font);
        self.dirty = true;
    }

    /// Get a DRCS glyph by designator and character position
    pub fn get_drcs_glyph(&self, designator: &str, char_pos: u8) -> Option<&DrcsGlyph> {
        self.drcs_fonts
            .get(designator)
            .and_then(|font| font.get_glyph(char_pos))
    }

    /// Get a DRCS font by designator
    pub fn get_drcs_font(&self, designator: &str) -> Option<&DrcsFont> {
        self.drcs_fonts.get(designator)
    }

    /// Get all DRCS fonts
    pub fn drcs_fonts(&self) -> &HashMap<String, DrcsFont> {
        &self.drcs_fonts
    }

    /// Clear all DRCS fonts
    pub fn clear_drcs_fonts(&mut self) {
        self.drcs_fonts.clear();
    }

    /// Designate a character set to G0 or G1
    ///
    /// The designator is the DRCS designator string (e.g., " @" for user-defined).
    /// Pass None to reset to standard ASCII.
    pub fn designate_charset(&mut self, g_set: u8, designator: Option<String>) {
        match g_set {
            0 => self.modes.charset_g0 = designator,
            1 => self.modes.charset_g1 = designator,
            _ => {}
        }
    }

    /// Get the currently active character set designator
    pub fn active_charset_designator(&self) -> Option<&str> {
        if self.modes.charset_g1_active {
            self.modes.charset_g1.as_deref()
        } else {
            self.modes.charset_g0.as_deref()
        }
    }

    /// Translate the DEC Special Graphics set to Unicode. This table is
    /// adapted from Rio's tested Rust implementation and matches foot's VT100
    /// behavior for G0/G1 line drawing.
    pub(crate) fn map_active_charset_char(&self, c: char) -> char {
        if self.active_charset_designator() != Some("0") {
            return c;
        }

        match c {
            '_' => ' ',
            '`' => '◆',
            'a' => '▒',
            'b' => '\u{2409}',
            'c' => '\u{240c}',
            'd' => '\u{240d}',
            'e' => '\u{240a}',
            'f' => '°',
            'g' => '±',
            'h' => '\u{2424}',
            'i' => '\u{240b}',
            'j' => '┘',
            'k' => '┐',
            'l' => '┌',
            'm' => '└',
            'n' => '┼',
            'o' => '⎺',
            'p' => '⎻',
            'q' => '─',
            'r' => '⎼',
            's' => '⎽',
            't' => '├',
            'u' => '┤',
            'v' => '┴',
            'w' => '┬',
            'x' => '│',
            'y' => '≤',
            'z' => '≥',
            '{' => 'π',
            '|' => '≠',
            '}' => '£',
            '~' => '·',
            _ => c,
        }
    }

    /// Check if a character should be rendered as DRCS and get its glyph
    pub fn get_drcs_for_char(&self, c: char) -> Option<&DrcsGlyph> {
        // DRCS characters are typically in the range 0x21-0x7E (33-126)
        // mapped to positions 0-93 (or 0-95 for 96-char sets)
        if let Some(designator) = self.active_charset_designator() {
            let char_code = c as u32;
            if (0x21..=0x7E).contains(&char_code) {
                let pos = (char_code - 0x21) as u8;
                return self.get_drcs_glyph(designator, pos);
            }
        }
        None
    }

    // ========== Selection Methods ==========

    /// Check if a character is a word character (for word selection)
    fn is_word_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '.'
    }

    /// Find word boundaries around a column position in a row
    fn find_word_bounds(&self, line: usize, col: usize) -> (SelectionPoint, SelectionPoint) {
        let row = match self.get_row_by_absolute_line(line) {
            Some(r) => r,
            None => {
                return (
                    SelectionPoint::new(line, col),
                    SelectionPoint::new(line, col),
                )
            }
        };

        let row_len = row.len();
        if row_len == 0 || col >= row_len {
            return (
                SelectionPoint::new(line, col),
                SelectionPoint::new(line, col),
            );
        }

        let center_char = row.get(col).map(Cell::first_char).unwrap_or(' ');

        // If we clicked on a non-word character, just select that character
        if !Self::is_word_char(center_char) {
            return (
                SelectionPoint::new(line, col),
                SelectionPoint::new(line, col),
            );
        }

        // Find start of word — walk backward, crossing wrapped line boundaries
        let mut start_line = line;
        let mut start_col = col;
        loop {
            if start_col > 0 {
                let r = self.get_row_by_absolute_line(start_line).unwrap();
                if let Some(cell) = r.get(start_col - 1) {
                    if Self::is_word_char(cell.first_char()) {
                        start_col -= 1;
                        continue;
                    }
                }
                break;
            }
            // At column 0 — check if this row is a continuation of the previous line
            if start_line == 0 {
                break;
            }
            let current_row = self.get_row_by_absolute_line(start_line);
            if current_row.is_some_and(|r| r.wrapped) {
                // This row is a wrapped continuation; move to end of previous line
                if let Some(prev_row) = self.get_row_by_absolute_line(start_line - 1) {
                    let prev_len = prev_row.len();
                    if prev_len > 0 {
                        if let Some(cell) = prev_row.get(prev_len - 1) {
                            if Self::is_word_char(cell.first_char()) {
                                start_line -= 1;
                                start_col = prev_len - 1;
                                continue;
                            }
                        }
                    }
                }
            }
            break;
        }

        // Find end of word — walk forward, crossing into wrapped continuation lines
        let mut end_line = line;
        let mut end_col = col;
        loop {
            let r = self.get_row_by_absolute_line(end_line).unwrap();
            let r_len = r.len();
            if end_col < r_len - 1 {
                if let Some(cell) = r.get(end_col + 1) {
                    if Self::is_word_char(cell.first_char()) {
                        end_col += 1;
                        continue;
                    }
                }
                break;
            }
            // At end of row — check if next row is a wrapped continuation
            if let Some(next_row) = self.get_row_by_absolute_line(end_line + 1) {
                if next_row.wrapped {
                    if let Some(cell) = next_row.get(0) {
                        if Self::is_word_char(cell.first_char()) {
                            end_line += 1;
                            end_col = 0;
                            continue;
                        }
                    }
                }
            }
            break;
        }

        (
            SelectionPoint::new(start_line, start_col),
            SelectionPoint::new(end_line, end_col),
        )
    }

    /// Start a new selection at the given absolute line and column
    pub fn start_selection(&mut self, line: usize, col: usize, mode: SelectionMode) {
        match mode {
            SelectionMode::Char | SelectionMode::Block => {
                let point = SelectionPoint::new(line, col);
                self.selection = Some(Selection::new(point, mode));
            }
            SelectionMode::Word => {
                let (anchor_start, anchor_end) = self.find_word_bounds(line, col);
                self.selection = Some(Selection::new_with_range(anchor_start, anchor_end, mode));
            }
            SelectionMode::Line => {
                // Select entire line (use large end column to select to end of line)
                let anchor_start = SelectionPoint::new(line, 0);
                let anchor_end = SelectionPoint::new(line, COL_END_OF_ROW);
                self.selection = Some(Selection::new_with_range(anchor_start, anchor_end, mode));
            }
        }
        self.dirty = true;
    }

    /// Extend the current selection to the given absolute line and column
    pub fn extend_selection(&mut self, line: usize, col: usize) {
        // Extract mode and anchor info before mutating
        let (mode, anchor_start, anchor_end_opt) = match &self.selection {
            Some(s) => (s.mode, s.anchor, s.anchor_end),
            None => return,
        };

        // Get the effective anchor end (same as anchor start for char/block modes)
        let anchor_end = anchor_end_opt.unwrap_or(anchor_start);

        match mode {
            SelectionMode::Char | SelectionMode::Block => {
                if let Some(ref mut selection) = self.selection {
                    selection.extend_to(SelectionPoint::new(line, col));
                }
            }
            SelectionMode::Word => {
                // Find word bounds at current position
                let (word_start, word_end) = self.find_word_bounds(line, col);
                let current = SelectionPoint::new(line, col);

                if let Some(ref mut selection) = self.selection {
                    if current.is_before(&anchor_start) {
                        // Extending before the original word
                        selection.end = word_start;
                    } else if anchor_end.is_before(&current)
                        || (line == anchor_end.line && col > anchor_end.col)
                    {
                        // Extending after the original word
                        selection.end = word_end;
                    } else {
                        // Within the original word - keep original selection
                        selection.end = anchor_end;
                    }
                }
            }
            SelectionMode::Line => {
                if let Some(ref mut selection) = self.selection {
                    if line < anchor_start.line {
                        // Extending upward
                        selection.end = SelectionPoint::new(line, 0);
                    } else if line > anchor_end.line {
                        // Extending downward
                        selection.end = SelectionPoint::new(line, COL_END_OF_ROW);
                    } else {
                        // Within the original line - keep original selection
                        selection.end = anchor_end;
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Clear the current selection
    pub fn clear_selection(&mut self) {
        if self.selection.is_some() {
            self.selection = None;
            self.dirty = true;
        }
    }

    /// Clear selection if the given grid row is within the selection
    /// Used when content is modified to invalidate affected selections
    fn clear_selection_if_row_selected(&mut self, grid_row: usize) {
        if let Some(ref selection) = self.selection {
            let abs_line = self.scrollback.len() + grid_row;
            let (start, end) = selection.ordered();
            if abs_line >= start.line && abs_line <= end.line {
                self.selection = None;
            }
        }
    }

    /// Clear selection if any row in the given grid row range is selected
    fn clear_selection_if_rows_selected(&mut self, start_row: usize, end_row: usize) {
        if let Some(ref selection) = self.selection {
            let scrollback_len = self.scrollback.len();
            let abs_start = scrollback_len + start_row;
            let abs_end = scrollback_len + end_row;
            let (sel_start, sel_end) = selection.ordered();
            // Check if ranges overlap
            if abs_start <= sel_end.line && abs_end >= sel_start.line {
                self.selection = None;
            }
        }
    }

    /// Check if a cell at the given absolute line and column is selected
    pub fn is_selected(&self, line: usize, col: usize) -> bool {
        self.selection
            .as_ref()
            .map(|s| s.contains(line, col))
            .unwrap_or(false)
    }

    /// Convert visible row (accounting for scroll offset) to absolute line index
    pub fn visible_row_to_absolute_line(&self, visible_row: usize) -> usize {
        let scrollback_len = self.scrollback.len();
        // When scroll_offset is 0, we see the most recent scrollback + current grid
        // visible_row 0 = oldest visible line
        // scrollback_len - scroll_offset = first visible scrollback line index
        // After scrollback, grid rows start
        scrollback_len.saturating_sub(self.scroll_offset) + visible_row
    }

    /// Get the selected text as a string
    ///
    /// Returns None if there's no selection or it's empty
    pub fn get_selected_text(&self) -> Option<String> {
        let selection = self.selection.as_ref()?;
        let (start, end) = selection.ordered();

        // Clamp to valid range
        let total = self.total_lines();
        if start.line >= total {
            return None;
        }

        let mut result = String::new();
        let end_line = end.line.min(total - 1);

        // For block selection, use consistent column range across all lines
        let is_block = selection.mode == SelectionMode::Block;
        let (block_start_col, block_end_col) = if is_block {
            let (min_col, max_col) = if selection.anchor.col <= selection.end.col {
                (selection.anchor.col, selection.end.col)
            } else {
                (selection.end.col, selection.anchor.col)
            };
            (min_col, max_col)
        } else {
            (0, 0) // Not used for non-block selection
        };

        for line_idx in start.line..=end_line {
            let row = self.get_row_by_absolute_line(line_idx)?;

            let (start_col, end_col) = if is_block {
                // Block selection: same columns for all lines
                (
                    block_start_col,
                    block_end_col.min(row.len().saturating_sub(1)),
                )
            } else {
                // Normal selection: varies by line
                let sc = if line_idx == start.line { start.col } else { 0 };
                let ec = if line_idx == end.line {
                    end.col.min(row.len().saturating_sub(1))
                } else {
                    row.len().saturating_sub(1)
                };
                (sc, ec)
            };

            // Extract characters from this row
            for col in start_col..=end_col {
                if let Some(cell) = row.get(col) {
                    // Skip wide character spacers
                    if !cell.attrs.contains(crate::cell::CellAttrs::WIDE_SPACER) {
                        result.push_str(cell.text());
                    }
                }
            }

            // A row marks itself as wrapped when it continues the preceding
            // row, so the following row decides whether this boundary emits
            // a newline. Block selection always preserves physical rows.
            let next_is_wrapped = self
                .get_row_by_absolute_line(line_idx + 1)
                .is_some_and(|next| next.wrapped);
            if line_idx < end_line && (is_block || !next_is_wrapped) {
                result.push('\n');
            }
        }

        // Trim trailing whitespace from each line but keep newlines
        let trimmed: String = result
            .lines()
            .map(|l| l.trim_end())
            .collect::<Vec<_>>()
            .join("\n");

        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    }

    /// Get the selected text as HTML with styling
    ///
    /// Returns HTML with inline styles for colors and attributes.
    /// The color palette is used to convert ANSI colors to RGB.
    pub fn get_selected_html(&self, palette: &crate::color::ColorPalette) -> Option<String> {
        use crate::cell::CellAttrs;
        use crate::color::Color;

        let selection = self.selection.as_ref()?;
        let (start, end) = selection.ordered();

        // Clamp to valid range
        let total = self.total_lines();
        if start.line >= total {
            return None;
        }

        let mut result = String::new();
        result.push_str("<pre style=\"font-family: monospace; background-color: ");
        result.push_str(&format!(
            "#{:02X}{:02X}{:02X}",
            palette.background.r, palette.background.g, palette.background.b
        ));
        result.push_str("; color: ");
        result.push_str(&format!(
            "#{:02X}{:02X}{:02X}",
            palette.foreground.r, palette.foreground.g, palette.foreground.b
        ));
        result.push_str("; padding: 8px;\">");

        let end_line = end.line.min(total - 1);

        // For block selection, use consistent column range across all lines
        let is_block = selection.mode == SelectionMode::Block;
        let (block_start_col, block_end_col) = if is_block {
            let (min_col, max_col) = if selection.anchor.col <= selection.end.col {
                (selection.anchor.col, selection.end.col)
            } else {
                (selection.end.col, selection.anchor.col)
            };
            (min_col, max_col)
        } else {
            (0, 0) // Not used for non-block selection
        };

        // Track last cell properties to minimize span changes
        let mut last_fg: Option<Color> = None;
        let mut last_bg: Option<Color> = None;
        let mut last_attrs: Option<CellAttrs> = None;
        let mut current_span_open = false;

        for line_idx in start.line..=end_line {
            let row = match self.get_row_by_absolute_line(line_idx) {
                Some(r) => r,
                None => continue,
            };

            let (start_col, end_col) = if is_block {
                (
                    block_start_col,
                    block_end_col.min(row.len().saturating_sub(1)),
                )
            } else {
                let sc = if line_idx == start.line { start.col } else { 0 };
                let ec = if line_idx == end.line {
                    end.col.min(row.len().saturating_sub(1))
                } else {
                    row.len().saturating_sub(1)
                };
                (sc, ec)
            };

            for col in start_col..=end_col {
                if let Some(cell) = row.get(col) {
                    // Skip wide character spacers
                    if cell.attrs.contains(CellAttrs::WIDE_SPACER) {
                        continue;
                    }

                    // Check if we need a new span
                    let needs_new_span = last_fg != Some(cell.fg)
                        || last_bg != Some(cell.bg)
                        || last_attrs != Some(cell.attrs);

                    if needs_new_span {
                        if current_span_open {
                            result.push_str("</span>");
                            current_span_open = false;
                        }

                        // Build style string
                        let mut style_parts = Vec::new();

                        // Foreground color (skip if default)
                        if !cell.fg.is_default() {
                            let rgb = self.resolve_color(cell.fg, palette);
                            style_parts
                                .push(format!("color: #{:02X}{:02X}{:02X}", rgb.r, rgb.g, rgb.b));
                        }

                        // Background color (skip if default)
                        if !cell.bg.is_default() {
                            let rgb = self.resolve_color(cell.bg, palette);
                            style_parts.push(format!(
                                "background-color: #{:02X}{:02X}{:02X}",
                                rgb.r, rgb.g, rgb.b
                            ));
                        }

                        // Bold
                        if cell.attrs.contains(CellAttrs::BOLD) {
                            style_parts.push("font-weight: bold".to_string());
                        }

                        // Dim
                        if cell.attrs.contains(CellAttrs::DIM) {
                            style_parts.push("opacity: 0.5".to_string());
                        }

                        // Italic
                        if cell.attrs.contains(CellAttrs::ITALIC) {
                            style_parts.push("font-style: italic".to_string());
                        }

                        // Text decorations
                        let has_underline = cell.attrs.has_underline();
                        let has_strikethrough = cell.attrs.contains(CellAttrs::STRIKETHROUGH);
                        let has_overline = cell.attrs.contains(CellAttrs::OVERLINE);

                        if has_underline || has_strikethrough || has_overline {
                            let mut decorations = Vec::new();
                            if has_underline {
                                decorations.push("underline");
                            }
                            if has_strikethrough {
                                decorations.push("line-through");
                            }
                            if has_overline {
                                decorations.push("overline");
                            }
                            style_parts.push(format!("text-decoration: {}", decorations.join(" ")));
                        }

                        if !style_parts.is_empty() {
                            result.push_str("<span style=\"");
                            result.push_str(&style_parts.join("; "));
                            result.push_str("\">");
                            current_span_open = true;
                        }

                        last_fg = Some(cell.fg);
                        last_bg = Some(cell.bg);
                        last_attrs = Some(cell.attrs);
                    }

                    // Append character (HTML-escaped)
                    for c in cell.text().chars() {
                        match c {
                            '<' => result.push_str("&lt;"),
                            '>' => result.push_str("&gt;"),
                            '&' => result.push_str("&amp;"),
                            '"' => result.push_str("&quot;"),
                            '\'' => result.push_str("&#39;"),
                            c => result.push(c),
                        }
                    }
                }
            }

            if current_span_open {
                result.push_str("</span>");
                current_span_open = false;
                last_fg = None;
                last_bg = None;
                last_attrs = None;
            }

            let next_is_wrapped = self
                .get_row_by_absolute_line(line_idx + 1)
                .is_some_and(|next| next.wrapped);
            if line_idx < end_line && (is_block || !next_is_wrapped) {
                result.push('\n');
            }
        }

        result.push_str("</pre>");

        if result.len() > "<pre style=\"\"></pre>".len() + 100 {
            Some(result)
        } else {
            None
        }
    }

    /// Get a row by absolute line index (0 = oldest scrollback line)
    fn get_row_by_absolute_line(&self, line: usize) -> Option<&Row> {
        let scrollback_len = self.scrollback.len();
        if line < scrollback_len {
            self.scrollback.get(line)
        } else {
            let grid_row = line - scrollback_len;
            self.grid.row(grid_row)
        }
    }
}

/// Screen clear mode
#[derive(Debug, Clone, Copy)]
pub enum ClearMode {
    /// Clear from cursor to end of screen
    Below,
    /// Clear from start of screen to cursor
    Above,
    /// Clear entire screen
    All,
    /// Clear scrollback buffer
    Scrollback,
}

/// Search result in terminal buffer
#[derive(Debug, Clone)]
pub struct SearchResult {
    /// Line index (0 = oldest scrollback line)
    pub line: usize,
    /// Column where match starts
    pub col: usize,
    /// Length of match
    pub len: usize,
}

/// Line clear mode
#[derive(Debug, Clone, Copy)]
pub enum LineClearMode {
    /// Clear from cursor to end of line
    Right,
    /// Clear from start of line to cursor
    Left,
    /// Clear entire line
    All,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_screen_new() {
        let screen = Screen::new(80, 24, ScreenConfig::default());
        assert_eq!(screen.width(), 80);
        assert_eq!(screen.height(), 24);
        assert_eq!(screen.cursor.row, 0);
        assert_eq!(screen.cursor.col, 0);
    }

    #[test]
    fn dynamic_palette_resolves_ansi_and_extended_indices() {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());
        let base = ColorPalette::default_dark();
        let ansi = Rgb::new(1, 2, 3);
        let extended = Rgb::new(4, 5, 6);

        screen.set_dynamic_color(ColorQuery::Palette(1), Some(ansi));
        screen.set_dynamic_color(ColorQuery::Palette(200), Some(extended));

        let resolved = screen.resolved_palette(&base);
        assert_eq!(resolved.ansi[1], ansi);
        assert_eq!(
            screen.resolve_color(Color::Ansi(crate::AnsiColor::Red), &resolved),
            ansi
        );
        assert_eq!(screen.resolve_color(Color::Indexed(1), &resolved), ansi);
        assert_eq!(
            screen.resolve_color(Color::Indexed(200), &resolved),
            extended
        );

        screen.reset_dynamic_palette();
        assert_ne!(
            screen.resolve_color(Color::Indexed(200), &resolved),
            extended
        );
    }

    #[test]
    fn terminal_images_have_stable_nonzero_ids() {
        let mut screen = Screen::new(10, 3, ScreenConfig::default());
        let image = || SixelImage {
            data: vec![255, 0, 0, 255],
            width: 1,
            height: 1,
        };

        screen.add_image_with_size(0, 0, 1, 1, image());
        screen.add_image_with_size(1, 0, 1, 1, image());

        let ids: Vec<_> = screen.images().iter().map(|image| image.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn partially_scrolled_image_remains_visible() {
        let mut screen = Screen::new(10, 2, ScreenConfig::default());
        screen.add_image_with_size(
            0,
            0,
            1,
            2,
            SixelImage {
                data: vec![255, 0, 0, 255],
                width: 1,
                height: 1,
            },
        );

        screen.cursor.row = 1;
        screen.line_feed();

        let image = screen.visible_images()[0];
        assert_eq!(screen.image_visible_row(image), Some(-1));
    }

    #[test]
    fn restored_image_ids_continue_without_collisions() {
        let mut screen = Screen::new(10, 2, ScreenConfig::default());
        screen.replace_images([TerminalImage {
            id: 7,
            col: 0,
            line: 0,
            cell_width: 1,
            cell_height: 1,
            data: Arc::new(vec![0, 255, 0, 255]),
            pixel_width: 1,
            pixel_height: 1,
        }]);

        screen.add_image_with_size(
            1,
            0,
            1,
            1,
            SixelImage {
                data: vec![255, 0, 0, 255],
                width: 1,
                height: 1,
            },
        );

        let ids: Vec<_> = screen.images().iter().map(|image| image.id).collect();
        assert_eq!(ids, vec![7, 8]);
    }

    #[test]
    fn test_put_char() {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());

        screen.put_char('H');
        screen.put_char('i');

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "H");
        assert_eq!(screen.get_cell(0, 1).unwrap().text(), "i");
        assert_eq!(screen.cursor.col, 2);
    }

    #[test]
    fn combines_unicode_scalars_into_grapheme_cells() {
        let mut screen = Screen::new(8, 2, ScreenConfig::default());

        for c in "e\u{301}👩\u{200d}💻🇨🇭".chars() {
            screen.put_char(c);
        }

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "e\u{301}");
        assert_eq!(screen.get_cell(0, 1).unwrap().text(), "👩\u{200d}💻");
        assert!(screen.get_cell(0, 1).unwrap().is_wide());
        assert!(screen.get_cell(0, 2).unwrap().is_wide_spacer());
        assert_eq!(screen.get_cell(0, 3).unwrap().text(), "🇨🇭");
        assert!(screen.get_cell(0, 3).unwrap().is_wide());
        assert!(screen.get_cell(0, 4).unwrap().is_wide_spacer());
        assert_eq!(screen.cursor.col, 5);
        assert_eq!(
            screen.grid().row(0).unwrap().text(),
            "e\u{301}👩\u{200d}💻🇨🇭"
        );
    }

    #[test]
    fn bounds_combining_character_growth_per_cell() {
        let mut screen = Screen::new(8, 2, ScreenConfig::default());
        screen.put_char('e');
        for _ in 0..100 {
            screen.put_char('\u{301}');
        }

        assert!(screen.get_cell(0, 0).unwrap().text().len() <= MAX_GRAPHEME_BYTES);
        assert_eq!(screen.cursor.col, 1);
    }

    #[test]
    fn wraps_wide_grapheme_before_last_column() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "abc界".chars() {
            screen.put_char(c);
        }

        assert_eq!(screen.grid().row(0).unwrap().text(), "abc");
        assert_eq!(screen.grid().row(1).unwrap().text(), "界");
        assert!(screen.grid().row(1).unwrap().wrapped);
        assert_eq!(screen.cursor.row, 1);
        assert_eq!(screen.cursor.col, 2);
    }

    #[test]
    fn resize_reflows_wrapped_graphemes_and_cursor() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "abcdefgh".chars() {
            screen.put_char(c);
        }

        screen.resize(3, 3);
        assert_eq!(screen.grid()[0].text(), "abc");
        assert_eq!(screen.grid()[1].text(), "def");
        assert_eq!(screen.grid()[2].text(), "gh");
        assert!(!screen.grid()[0].wrapped);
        assert!(screen.grid()[1].wrapped);
        assert!(screen.grid()[2].wrapped);
        assert_eq!((screen.cursor.row, screen.cursor.col), (2, 2));

        screen.resize(6, 2);
        assert_eq!(screen.grid()[0].text(), "abcdef");
        assert_eq!(screen.grid()[1].text(), "gh");
        assert_eq!((screen.cursor.row, screen.cursor.col), (1, 2));
    }

    #[test]
    fn resize_moves_only_required_top_rows_into_scrollback() {
        let mut screen = Screen::new(4, 4, ScreenConfig::default());
        for line in ['1', '2', '3', '4'] {
            screen.put_char(line);
            if line != '4' {
                screen.carriage_return();
                screen.line_feed();
            }
        }

        screen.resize(4, 2);
        assert_eq!(screen.scrollback().len(), 2);
        assert_eq!(screen.scrollback()[0].text(), "1");
        assert_eq!(screen.scrollback()[1].text(), "2");
        assert_eq!(screen.grid()[0].text(), "3");
        assert_eq!(screen.grid()[1].text(), "4");
        assert_eq!(screen.cursor.row, 1);

        screen.resize(4, 4);
        assert!(screen.scrollback().is_empty());
        assert_eq!(screen.grid()[0].text(), "1");
        assert_eq!(screen.grid()[3].text(), "4");
        assert_eq!(screen.cursor.row, 3);
    }

    #[test]
    fn resize_never_splits_wide_graphemes() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "ab界".chars() {
            screen.put_char(c);
        }

        screen.resize(3, 2);
        assert_eq!(screen.grid()[0].text(), "ab");
        assert_eq!(screen.grid()[1].text(), "界");
        assert!(screen.grid()[1][0].is_wide());
        assert!(screen.grid()[1][1].is_wide_spacer());
    }

    #[test]
    fn resize_keeps_selection_attached_to_reflowed_text() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "abcdefgh".chars() {
            screen.put_char(c);
        }
        screen.start_selection(0, 1, SelectionMode::Char);
        screen.extend_selection(1, 2);
        assert_eq!(screen.get_selected_text().as_deref(), Some("bcdefg"));

        screen.resize(3, 3);
        assert_eq!(screen.get_selected_text().as_deref(), Some("bcdefg"));
    }

    #[test]
    fn resize_keeps_shell_markers_attached_to_command_output() {
        let mut screen = Screen::new(6, 3, ScreenConfig::default());
        for c in "abcdefghi".chars() {
            screen.put_char(c);
        }
        screen.grid_mut()[0].shell_integration.prompt_marker = true;
        screen.grid_mut()[0].shell_integration.command_start = Some(2);
        screen.grid_mut()[1].shell_integration.command_end = Some(3);
        assert_eq!(screen.last_command_output().as_deref(), Some("cdefghi"));

        screen.resize(4, 3);

        assert!(screen.grid()[0].shell_integration.prompt_marker);
        assert_eq!(screen.grid()[0].shell_integration.command_start, Some(2));
        assert_eq!(screen.grid()[2].shell_integration.command_end, Some(1));
        assert_eq!(screen.last_command_output().as_deref(), Some("cdefghi"));
    }

    #[test]
    fn shell_prompt_navigation_matches_foot_view_edges() {
        let mut screen = Screen::new(
            10,
            2,
            ScreenConfig {
                scrollback_lines: 10,
            },
        );
        for text in ["one", "two", "three", "four"] {
            screen.mark_shell_prompt();
            for c in text.chars() {
                screen.put_char(c);
            }
            screen.carriage_return();
            screen.line_feed();
        }

        assert_eq!(screen.scroll_offset, 0);
        assert!(screen.scroll_to_previous_prompt());
        assert_eq!(screen.scroll_offset, 1);
        assert!(screen.scroll_to_previous_prompt());
        assert_eq!(screen.scroll_offset, 2);
        assert!(screen.scroll_to_next_prompt());
        assert_eq!(screen.scroll_offset, 1);
        assert!(screen.scroll_to_next_prompt());
        assert_eq!(screen.scroll_offset, 0);
        assert!(!screen.scroll_to_next_prompt());
    }

    #[test]
    fn resize_reflows_hidden_primary_screen() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "abcdefgh".chars() {
            screen.put_char(c);
        }
        screen.enter_alternate_screen();

        screen.resize(3, 3);
        screen.exit_alternate_screen();

        assert_eq!(screen.grid()[0].text(), "abc");
        assert_eq!(screen.grid()[1].text(), "def");
        assert_eq!(screen.grid()[2].text(), "gh");
        assert_eq!((screen.cursor.row, screen.cursor.col), (2, 2));
    }

    #[test]
    fn resize_moves_image_anchor_with_reflowed_cells() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());
        for c in "abcde".chars() {
            screen.put_char(c);
        }
        screen.add_image_with_size(
            0,
            1,
            1,
            1,
            SixelImage {
                data: vec![255, 0, 0, 255],
                width: 1,
                height: 1,
            },
        );

        screen.resize(3, 3);

        let image = screen.images()[0];
        assert_eq!((image.line, image.col), (1, 1));
        assert_eq!(screen.image_visible_row(image), Some(1));
    }

    #[test]
    fn zero_scrollback_configuration_retains_no_rows() {
        let mut screen = Screen::new(
            4,
            1,
            ScreenConfig {
                scrollback_lines: 0,
            },
        );
        screen.put_char('a');
        screen.line_feed();

        assert!(screen.scrollback().is_empty());
    }

    #[test]
    fn test_auto_wrap() {
        let mut screen = Screen::new(5, 3, ScreenConfig::default());

        for c in "Hello World".chars() {
            screen.put_char(c);
        }

        assert_eq!(screen.grid().row(0).unwrap().text(), "Hello");
        assert_eq!(screen.grid().row(1).unwrap().text(), " Worl");
        assert_eq!(screen.grid().row(2).unwrap().text(), "d");
    }

    #[test]
    fn test_scroll_up() {
        let mut screen = Screen::new(80, 3, ScreenConfig::default());

        // Fill screen
        screen.put_char('1');
        screen.line_feed();
        screen.carriage_return();
        screen.put_char('2');
        screen.line_feed();
        screen.carriage_return();
        screen.put_char('3');
        screen.line_feed(); // This should scroll

        assert_eq!(screen.scrollback.len(), 1);
        assert_eq!(screen.scrollback[0][0].text(), "1");
        assert_eq!(screen.grid()[0][0].text(), "2");
        assert_eq!(screen.grid()[1][0].text(), "3");
    }

    #[test]
    fn visible_rows_resolve_into_scrollback() {
        let mut screen = Screen::new(4, 2, ScreenConfig::default());

        for line in ['1', '2', '3'] {
            screen.put_char(line);
            screen.carriage_return();
            screen.line_feed();
        }

        assert_eq!(screen.scrollback.len(), 2);
        screen.scroll_offset = 2;

        let first_line = screen.visible_row_to_absolute_line(0);
        let second_line = screen.visible_row_to_absolute_line(1);
        assert_eq!(
            screen
                .get_cell_with_scrollback(first_line, 0)
                .unwrap()
                .text(),
            "1"
        );
        assert_eq!(
            screen
                .get_cell_with_scrollback(second_line, 0)
                .unwrap()
                .text(),
            "2"
        );
    }

    #[test]
    fn test_alternate_screen() {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());

        screen.put_char('A');
        screen.enter_alternate_screen();

        // Alternate screen should be empty
        assert_eq!(screen.get_cell(0, 0).unwrap().text(), " ");

        screen.put_char('B');
        screen.exit_alternate_screen();

        // Should restore primary with 'A'
        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "A");
    }

    #[test]
    fn test_clear_screen() {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());

        screen.put_char('X');
        screen.clear(ClearMode::All);

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), " ");
    }

    /// Helper: create a screen with text on the first line
    fn screen_with_text(text: &str) -> Screen {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());
        for c in text.chars() {
            screen.put_char(c);
        }
        screen
    }

    #[test]
    fn test_word_selection_stays_within_word() {
        // "hello world" - double-click on "hello" (col 2), then extend within "hello"
        let mut screen = screen_with_text("hello world");
        screen.start_selection(0, 2, SelectionMode::Word);

        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, SelectionPoint::new(0, 0));
        assert_eq!(sel.end, SelectionPoint::new(0, 4));

        // Extend to another position within the same word
        screen.extend_selection(0, 4);
        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, SelectionPoint::new(0, 0));
        assert_eq!(sel.end, SelectionPoint::new(0, 4));
    }

    #[test]
    fn test_word_selection_extend_forward() {
        // "hello world" - double-click on "hello", drag to "world"
        let mut screen = screen_with_text("hello world");
        screen.start_selection(0, 2, SelectionMode::Word);

        // Extend to "world" (col 8)
        screen.extend_selection(0, 8);
        let sel = screen.selection.as_ref().unwrap();
        // anchor should be start of original word, end should be end of "world"
        assert_eq!(sel.anchor, SelectionPoint::new(0, 0));
        assert_eq!(sel.end, SelectionPoint::new(0, 10));
    }

    #[test]
    fn test_word_selection_extend_backward() {
        // "foo bar baz" - double-click on "bar" (col 5), drag backward to "foo"
        let mut screen = screen_with_text("foo bar baz");
        screen.start_selection(0, 5, SelectionMode::Word);

        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, SelectionPoint::new(0, 4));
        assert_eq!(sel.end, SelectionPoint::new(0, 6));

        // Extend backward to "foo" (col 1)
        screen.extend_selection(0, 1);
        let sel = screen.selection.as_ref().unwrap();
        // anchor stays at original word start, end moves to start of "foo"
        assert_eq!(sel.anchor, SelectionPoint::new(0, 4));
        assert_eq!(sel.end, SelectionPoint::new(0, 0));
        // ordered() should give (0,0)..(0,6) covering "foo bar"
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 0));
        assert_eq!(end, SelectionPoint::new(0, 6));
    }

    #[test]
    fn test_word_selection_extend_and_return() {
        // "hello world" - double-click on "hello", drag to "world", then back to "hello"
        let mut screen = screen_with_text("hello world");
        screen.start_selection(0, 2, SelectionMode::Word);

        // Extend to "world"
        screen.extend_selection(0, 8);
        let sel = screen.selection.as_ref().unwrap();
        assert_eq!(sel.anchor, SelectionPoint::new(0, 0));
        assert_eq!(sel.end, SelectionPoint::new(0, 10));
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 0));
        assert_eq!(end, SelectionPoint::new(0, 10));

        // Return back to within original word
        screen.extend_selection(0, 3);
        let sel = screen.selection.as_ref().unwrap();
        // Should restore original word selection
        assert_eq!(sel.anchor, SelectionPoint::new(0, 0));
        assert_eq!(sel.end, SelectionPoint::new(0, 4));
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 0));
        assert_eq!(end, SelectionPoint::new(0, 4));
    }

    #[test]
    fn test_word_selection_direction_changes() {
        // "foo bar baz" - double-click on "bar", drag backward, then forward, then backward
        let mut screen = screen_with_text("foo bar baz");
        screen.start_selection(0, 5, SelectionMode::Word);

        // Initial: "bar" selected (cols 4-6)
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 4));
        assert_eq!(end, SelectionPoint::new(0, 6));

        // Step 1: drag backward to "foo"
        screen.extend_selection(0, 1);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 0)); // start of "foo"
        assert_eq!(end, SelectionPoint::new(0, 6)); // end of "bar" (anchor_end)

        // Step 2: drag forward to "baz" - this was the buggy case
        screen.extend_selection(0, 9);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 4)); // start of "bar" (anchor)
        assert_eq!(end, SelectionPoint::new(0, 10)); // end of "baz"

        // Step 3: drag backward again to "foo"
        screen.extend_selection(0, 1);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 0)); // start of "foo"
        assert_eq!(end, SelectionPoint::new(0, 6)); // end of "bar" (anchor_end)

        // Step 4: drag forward once more to "baz"
        screen.extend_selection(0, 9);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start, SelectionPoint::new(0, 4)); // start of "bar"
        assert_eq!(end, SelectionPoint::new(0, 10)); // end of "baz"
    }

    #[test]
    fn test_line_selection_direction_changes() {
        // Three lines, triple-click on middle line, drag up then down then up
        let mut screen = Screen::new(80, 3, ScreenConfig::default());
        for c in "first line".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "second line".chars() {
            screen.put_char(c);
        }
        screen.line_feed();
        screen.carriage_return();
        for c in "third line".chars() {
            screen.put_char(c);
        }

        // Triple-click on line 1 (second line)
        screen.start_selection(1, 3, SelectionMode::Line);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start.line, 1);
        assert_eq!(end.line, 1);

        // Drag up to line 0
        screen.extend_selection(0, 5);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start.line, 0);
        assert_eq!(end.line, 1);

        // Drag down to line 2
        screen.extend_selection(2, 5);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start.line, 1);
        assert_eq!(end.line, 2);

        // Drag up again to line 0
        screen.extend_selection(0, 5);
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        assert_eq!(start.line, 0);
        assert_eq!(end.line, 1);
    }

    #[test]
    fn test_word_selection_on_non_word_char() {
        // "hello world" - double-click on space (col 5)
        let mut screen = screen_with_text("hello world");
        screen.start_selection(0, 5, SelectionMode::Word);

        let sel = screen.selection.as_ref().unwrap();
        // Space is a non-word char, so anchor == end (single char range)
        assert_eq!(sel.anchor, SelectionPoint::new(0, 5));
        assert_eq!(sel.end, SelectionPoint::new(0, 5));
    }

    #[test]
    fn test_word_selection_survives_scrollback_wrap() {
        // Regression: when scrollback is full and a line is evicted,
        // anchor_end must be shifted along with anchor/end, otherwise
        // the ordered() range expands as new output arrives.
        let config = ScreenConfig {
            scrollback_lines: 2,
        };
        let mut screen = Screen::new(80, 3, config);

        // Fill scrollback to capacity. Grid has 3 rows; move cursor to the
        // last row first, then subsequent line_feeds push rows into scrollback.
        screen.line_feed();
        screen.line_feed(); // cursor now at last row, no scrollback yet
        assert_eq!(screen.scrollback.len(), 0);
        screen.line_feed();
        screen.line_feed();
        assert_eq!(screen.scrollback.len(), 2);

        // Write a word on the current (last) grid row.
        screen.carriage_return();
        for c in "hello world".chars() {
            screen.put_char(c);
        }
        let abs_line = screen.visible_row_to_absolute_line(2);

        // Double-click "hello".
        screen.start_selection(abs_line, 2, SelectionMode::Word);
        let (s0, e0) = screen.selection.as_ref().unwrap().ordered();
        assert_eq!(s0, SelectionPoint::new(abs_line, 0));
        assert_eq!(e0, SelectionPoint::new(abs_line, 4));

        // One more line_feed evicts the oldest scrollback row (lines_removed == 1).
        screen.line_feed();
        let sel = screen.selection.as_ref().unwrap();
        let (start, end) = sel.ordered();
        // Selection must stay on the same single word, just shifted up by one line.
        assert_eq!(start, SelectionPoint::new(abs_line - 1, 0));
        assert_eq!(end, SelectionPoint::new(abs_line - 1, 4));
    }

    #[test]
    fn authoritative_cursor_snapshot_clears_stale_decscusr_override() {
        let mut cursor = Cursor::default();
        cursor.configure(CursorStyle::Bar, false);
        cursor.restore_protocol_snapshot(Some(CursorStyle::Underline), Some(true), Some(true));
        assert_eq!(cursor.style, CursorStyle::Underline);
        assert_eq!(cursor.blink.decscusr(), Some(true));

        cursor.restore_protocol_snapshot(Some(CursorStyle::Block), None, Some(false));
        assert_eq!(cursor.style, CursorStyle::Bar);
        assert_eq!(cursor.blink.decscusr(), None);
        assert!(!cursor.blink.dec_mode_12());
        assert!(!cursor.blink.enabled());
    }

    #[test]
    fn alternate_screen_keeps_native_cursor_defaults() {
        let mut screen = Screen::new(80, 24, ScreenConfig::default());
        screen.configure_cursor(CursorStyle::Bar, false);
        screen.cursor.restore_protocol_snapshot(
            Some(CursorStyle::Underline),
            Some(true),
            Some(true),
        );

        screen.enter_alternate_screen();

        assert_eq!(screen.cursor.configured_style(), CursorStyle::Bar);
        assert!(!screen.cursor.blink.configured());
        assert_eq!(screen.cursor.style, CursorStyle::Bar);
        assert!(!screen.cursor.blink.enabled());
    }
}
