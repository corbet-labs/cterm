//! ANSI/VT sequence parser
//!
//! Uses the `vte` crate for parsing escape sequences and generates
//! actions that can be applied to the terminal screen.
//!
//! Special handling is provided for OSC 1337 (iTerm2) file transfers
//! which are intercepted before VTE to enable streaming large files.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use vte::Params;

use crate::cell::{CellAttrs, Hyperlink};
use crate::color::{AnsiColor, Color, Rgb};
use crate::drcs::DecdldDecoder;
use crate::image_decode::decode_image;
use crate::iterm2::{Iterm2Dimension, Iterm2FileParams};
use crate::keyboard::KeyboardEnhancementFlags;
use crate::kitty_graphics::{InterceptorResult as KittyInterceptorResult, KittyGraphics};
use crate::osc1337::{InterceptorResult, Osc1337Interceptor};
#[cfg(test)]
use crate::screen::DesktopNotificationAction;
use crate::screen::{
    ClearMode, ClipboardOperation, ClipboardSelection, ColorQuery, CursorStyle,
    DesktopNotification, LineClearMode, MouseEncoding, MouseMode, NotificationUrgency, Screen,
};
use crate::sixel::{
    SixelDecoder, SixelDecoderConfig, SixelImage, DEFAULT_SIXEL_MAX_BYTES, MAX_SIXEL_COLORS,
    MAX_SIXEL_DIMENSION,
};

/// DCS (Device Control String) state for handling multi-byte sequences
enum DcsState {
    /// No DCS sequence active
    None,
    /// Sixel graphics sequence in progress
    Sixel {
        decoder: Box<SixelDecoder>,
        start_col: usize,
        start_row: usize,
        shared_palette: bool,
    },
    /// DECDLD (soft font download) in progress
    Decdld { decoder: DecdldDecoder },
    /// XTGETTCAP terminfo capability query in progress
    Xtgettcap { buffer: Vec<u8>, overflowed: bool },
    /// DECRQSS request-status-string query in progress
    Decrqss { query: Vec<u8> },
}

const XTGETTCAP_MAX_REQUEST_SIZE: usize = 64 * 1024;
const MAX_NOTIFICATION_TITLE_BYTES: usize = 1024;
const MAX_NOTIFICATION_BODY_BYTES: usize = 4096;

#[derive(Debug)]
struct SixelSessionState {
    palette_size: usize,
    shared_palette: Vec<[u8; 4]>,
    max_width: usize,
    max_height: usize,
}

impl Default for SixelSessionState {
    fn default() -> Self {
        Self {
            palette_size: MAX_SIXEL_COLORS,
            shared_palette: SixelDecoder::default_palette(MAX_SIXEL_COLORS),
            max_width: MAX_SIXEL_DIMENSION,
            max_height: MAX_SIXEL_DIMENSION,
        }
    }
}

impl SixelSessionState {
    fn reset(&mut self) {
        *self = Self::default();
    }

    fn set_palette_size(&mut self, size: usize) {
        self.palette_size = size.clamp(2, MAX_SIXEL_COLORS);
        self.shared_palette = SixelDecoder::default_palette(self.palette_size);
    }

    fn decoder_config(&self) -> SixelDecoderConfig {
        SixelDecoderConfig {
            max_width: self.max_width,
            max_height: self.max_height,
            max_bytes: DEFAULT_SIXEL_MAX_BYTES,
            palette_size: self.palette_size,
            ..SixelDecoderConfig::default()
        }
    }
}

#[derive(Debug)]
struct KittyNotificationBuilder {
    active: bool,
    id: Option<String>,
    title: String,
    body: String,
    urgency: NotificationUrgency,
    expire_time: Option<i32>,
    muted: bool,
    focus: bool,
}

impl Default for KittyNotificationBuilder {
    fn default() -> Self {
        Self {
            active: false,
            id: None,
            title: String::new(),
            body: String::new(),
            urgency: NotificationUrgency::Normal,
            expire_time: None,
            muted: false,
            focus: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum KittyPayloadType {
    Title,
    Body,
    Close,
    Alive,
    Ignored,
    Capabilities,
}

/// Parser wraps the vte parser and applies actions to a Screen
pub struct Parser {
    state_machine: vte::Parser,
    dcs_state: DcsState,
    /// Most recent graphic character for ECMA-48 REP.
    last_printed: Option<char>,
    /// Saved DEC private modes for xterm XTSAVE/XTRESTORE.
    saved_dec_modes: HashMap<usize, bool>,
    /// Bounded, all-or-none interceptor for streaming OSC 1337 File sequences.
    osc_1337: Osc1337Interceptor,
    /// In-progress chunked Kitty OSC 99 notification.
    kitty_notification: KittyNotificationBuilder,
    /// Bounded Kitty graphics APC parser and image store.
    kitty_graphics: KittyGraphics,
    /// Optimistically active Kitty notification identifiers for p=alive.
    active_notification_ids: HashSet<String>,
    /// Palette and resource limits shared by Sixel protocol sequences.
    sixel: SixelSessionState,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self {
            state_machine: vte::Parser::new(),
            dcs_state: DcsState::None,
            last_printed: None,
            saved_dec_modes: HashMap::new(),
            osc_1337: Osc1337Interceptor::new(),
            kitty_notification: KittyNotificationBuilder::default(),
            kitty_graphics: KittyGraphics::default(),
            active_notification_ids: HashSet::new(),
            sixel: SixelSessionState::default(),
        }
    }

    /// Parse input bytes and apply actions to the screen
    ///
    /// This method intercepts OSC 1337 File transfers before VTE can buffer them,
    /// enabling streaming of large files without exhausting memory.
    pub fn parse(&mut self, screen: &mut Screen, bytes: &[u8]) {
        for &byte in bytes {
            match self.osc_1337.advance(byte) {
                InterceptorResult::Forward(bytes) => {
                    self.advance_after_osc1337(screen, bytes.as_slice());
                }
                InterceptorResult::Replay(bytes) => {
                    if let Err(error) =
                        bytes.replay(|chunk| self.advance_after_osc1337(screen, chunk))
                    {
                        log::warn!("Failed to replay OSC 1337 through VTE: {error}");
                    }
                }
                InterceptorResult::Swallow => {}
                InterceptorResult::Finished(result) => {
                    self.finish_streaming_file_direct(result, screen);
                }
            }
        }
    }

    fn advance_after_osc1337(&mut self, screen: &mut Screen, bytes: &[u8]) {
        for byte in bytes {
            match self.kitty_graphics.advance(*byte) {
                KittyInterceptorResult::Forward(bytes) => {
                    self.advance_vte(screen, bytes.as_slice());
                }
                KittyInterceptorResult::Swallow => {}
                KittyInterceptorResult::Captured(raw) => {
                    self.kitty_graphics.handle(&raw, screen);
                }
            }
        }
    }

    fn advance_vte(&mut self, screen: &mut Screen, bytes: &[u8]) {
        let mut performer = ScreenPerformer {
            screen,
            dcs_state: &mut self.dcs_state,
            last_printed: &mut self.last_printed,
            saved_dec_modes: &mut self.saved_dec_modes,
            kitty_notification: &mut self.kitty_notification,
            active_notification_ids: &mut self.active_notification_ids,
            sixel: &mut self.sixel,
        };
        for &byte in bytes {
            self.state_machine.advance(&mut performer, byte);
        }
    }

    fn finish_streaming_file_direct(
        &mut self,
        result: crate::streaming_file::StreamingFileResult,
        screen: &mut Screen,
    ) {
        log::debug!(
            "OSC 1337 File streaming complete: {} bytes, name={:?}",
            result.total_bytes,
            result.params.name
        );

        if result.params.inline {
            self.handle_streaming_inline_image_direct(result, screen);
        } else {
            screen.queue_streaming_file_transfer(result);
        }
    }

    /// Handle an inline image from streaming (direct version without performer)
    fn handle_streaming_inline_image_direct(
        &self,
        result: crate::streaming_file::StreamingFileResult,
        screen: &mut Screen,
    ) {
        // Get the image data
        let data = match result.data.take() {
            Ok(d) => d,
            Err(e) => {
                log::warn!("Failed to read streamed image data: {}", e);
                return;
            }
        };

        // Decode and display
        let decoded = match decode_image(&data) {
            Ok(img) => img,
            Err(e) => {
                log::warn!("OSC 1337 inline image decode failed: {}", e);
                return;
            }
        };

        log::debug!(
            "OSC 1337 streamed inline image: {}x{} pixels",
            decoded.width,
            decoded.height
        );

        let cell_cols = screen.image_cols_for_width(decoded.width);
        let cell_rows = screen.image_rows_for_height(decoded.height);

        let col = screen.cursor.col;
        let row = screen.cursor.row;

        let sixel_image = SixelImage {
            data: decoded.data,
            width: decoded.width,
            height: decoded.height,
        };

        screen.add_image_with_size(col, row, cell_cols, cell_rows, sixel_image);

        // Move cursor
        let last_image_row = row + cell_rows.saturating_sub(1);
        if last_image_row >= screen.height() {
            let scroll_amount = last_image_row - screen.height() + 1;
            screen.scroll_up(scroll_amount);
            screen.cursor.row = screen.height() - 1;
        } else {
            screen.cursor.row = last_image_row;
        }
        screen.cursor.col = 0;
    }
}

/// Performer that applies VTE actions to a Screen
struct ScreenPerformer<'a> {
    screen: &'a mut Screen,
    dcs_state: &'a mut DcsState,
    last_printed: &'a mut Option<char>,
    saved_dec_modes: &'a mut HashMap<usize, bool>,
    kitty_notification: &'a mut KittyNotificationBuilder,
    active_notification_ids: &'a mut HashSet<String>,
    sixel: &'a mut SixelSessionState,
}

impl vte::Perform for ScreenPerformer<'_> {
    fn print(&mut self, c: char) {
        let c = self.screen.map_active_charset_char(c);
        *self.last_printed = Some(c);
        self.screen.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Bell (BEL)
            0x07 => {
                self.screen.bell = true;
                log::debug!("Bell");
            }
            // Backspace (BS)
            0x08 => self.screen.backspace(),
            // Horizontal Tab (HT)
            0x09 => {
                self.screen.tab_forward(1);
            }
            // Line Feed (LF), Vertical Tab (VT), Form Feed (FF)
            0x0a..=0x0c => {
                self.screen.line_feed();
                if self.screen.modes.line_feed_mode {
                    self.screen.carriage_return();
                }
            }
            // Carriage Return (CR)
            0x0d => {
                self.screen.carriage_return();
            }
            // Shift Out (SO) - switch to G1 charset
            0x0e => {
                self.screen.modes.charset_g1_active = true;
                log::trace!("Shift Out: activated G1 charset");
            }
            // Shift In (SI) - switch to G0 charset
            0x0f => {
                self.screen.modes.charset_g1_active = false;
                log::trace!("Shift In: activated G0 charset");
            }
            _ => {
                log::trace!("Unhandled execute byte: 0x{:02x}", byte);
            }
        }
    }

    fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        log::trace!(
            "DCS hook: params={:?}, intermediates={:?}, action={:?}",
            params_to_vec(params),
            intermediates,
            action
        );

        let params_vec: Vec<u16> = params
            .iter()
            .flat_map(|subparams| subparams.iter().copied())
            .collect();

        match action {
            // Sixel graphics: DCS Pn1 ; Pn2 ; Pn3 q
            'q' if intermediates.is_empty() => {
                log::debug!("Starting Sixel graphics sequence, params: {:?}", params_vec);

                let shared_palette = !self.screen.modes.sixel_private_palette;
                let config = self.sixel.decoder_config();
                let decoder = if shared_palette {
                    SixelDecoder::with_config_and_palette(
                        &params_vec,
                        config,
                        &self.sixel.shared_palette,
                    )
                } else {
                    SixelDecoder::with_config(&params_vec, config)
                };

                *self.dcs_state = DcsState::Sixel {
                    decoder: Box::new(decoder),
                    start_col: self.screen.cursor.col,
                    start_row: self.screen.cursor.row,
                    shared_palette,
                };
            }
            // Legacy application synchronized-update protocol.  Mode 2026 is
            // preferred, but foot supports this spelling for compatibility.
            's' if intermediates == b"=" => match params_vec.first().copied() {
                Some(1) => self.screen.set_application_sync_updates(true),
                Some(2) => self.screen.set_application_sync_updates(false),
                _ => {}
            },
            // Query the builtin terminal capability set.
            'q' if intermediates == b"+" => {
                *self.dcs_state = DcsState::Xtgettcap {
                    buffer: Vec::new(),
                    overflowed: false,
                };
            }
            // Request status string (DECSTBM, SGR, or DECSCUSR).
            'q' if intermediates == b"$" => {
                *self.dcs_state = DcsState::Decrqss {
                    query: Vec::with_capacity(2),
                };
            }
            // DECDLD (soft font download): DCS Pfn;Pcn;Pe;Pcmw;Pss;Pt;Pcmh;Pcss {
            '{' if intermediates.is_empty() => {
                log::debug!("Starting DECDLD sequence, params: {:?}", params_vec);

                *self.dcs_state = DcsState::Decdld {
                    decoder: DecdldDecoder::new(&params_vec),
                };
            }
            _ => {
                log::trace!("Unhandled DCS action: {:?}", action);
            }
        }
    }

    fn put(&mut self, byte: u8) {
        // DCS data - feed to the appropriate decoder
        match self.dcs_state {
            DcsState::Sixel {
                ref mut decoder, ..
            } => {
                decoder.put(byte);
            }
            DcsState::Decdld { ref mut decoder } => {
                decoder.put(byte);
            }
            DcsState::Xtgettcap {
                ref mut buffer,
                ref mut overflowed,
            } => {
                if buffer.len() < XTGETTCAP_MAX_REQUEST_SIZE {
                    buffer.push(byte);
                } else {
                    *overflowed = true;
                }
            }
            DcsState::Decrqss { ref mut query } => {
                if query.len() < 2 {
                    query.push(byte);
                }
            }
            DcsState::None => {}
        }
    }

    fn unhook(&mut self) {
        // End of DCS sequence - finalize and store the result
        let old_state = std::mem::replace(self.dcs_state, DcsState::None);

        match old_state {
            DcsState::Sixel {
                decoder,
                start_col,
                start_row,
                shared_palette,
            } => {
                let result = decoder.finish_with_palette();
                if shared_palette {
                    self.sixel.shared_palette = result.palette;
                }
                if result.truncated {
                    log::warn!("Sixel input exceeded the configured image resource limit");
                }
                if let Some(image) = result.image {
                    // Determine image position based on DECSDM mode
                    let (img_col, img_row) = if self.screen.modes.sixel_scrolling {
                        // Scrolling enabled: image at cursor position
                        (start_col, start_row)
                    } else {
                        // Scrolling disabled: image at top-left
                        (0, 0)
                    };

                    log::debug!(
                        "Sixel complete: {}x{} at ({}, {}), scrolling={}",
                        image.width,
                        image.height,
                        img_col,
                        img_row,
                        self.screen.modes.sixel_scrolling
                    );

                    // Calculate how many rows/cols the image spans
                    let rows_spanned = self.screen.image_rows_for_height(image.height);
                    let cols_spanned = self.screen.image_cols_for_width(image.width);

                    // Store the image in the screen (this also clears grid cells underneath)
                    self.screen.add_image_with_size(
                        img_col,
                        img_row,
                        cols_spanned,
                        rows_spanned,
                        image,
                    );

                    // Handle cursor positioning based on DECSDM mode
                    if self.screen.modes.sixel_scrolling {
                        // Sixel scrolling enabled: place the cursor on the last
                        // image row.  By default it remains at the image's left
                        // edge; xterm mode 8452 moves it just to the right.
                        let last_image_row = img_row + rows_spanned.saturating_sub(1);

                        if last_image_row >= self.screen.height() {
                            // Image extends past bottom - scroll and position at bottom
                            let scroll_amount = last_image_row - self.screen.height() + 1;
                            self.screen.scroll_up(scroll_amount);
                            self.screen.cursor.row = self.screen.height() - 1;
                        } else {
                            self.screen.cursor.row = last_image_row;
                        }
                        self.screen.cursor.col = if self.screen.modes.sixel_cursor_right {
                            (img_col + cols_spanned).min(self.screen.width().saturating_sub(1))
                        } else {
                            img_col
                        };
                    }
                    // If sixel_scrolling is false, cursor stays where it was (start_col, start_row)
                }
            }
            DcsState::Decdld { decoder } => {
                let erase_control = decoder.erase_control();
                let font_number = decoder.font_number();

                if let Some(font) = decoder.finish() {
                    log::debug!(
                        "DECDLD complete: font {} designator '{}' with {} glyphs ({}x{})",
                        font.font_number,
                        font.designator,
                        font.glyphs.len(),
                        font.cell_width,
                        font.cell_height
                    );

                    // Store the font in the screen
                    self.screen.add_drcs_font(font, erase_control, font_number);
                }
            }
            DcsState::Xtgettcap { buffer, overflowed } => {
                let response = if overflowed {
                    b"\x1bP0+r\x1b\\".to_vec()
                } else {
                    xtgettcap_response(&buffer, self.screen.width(), self.screen.height())
                };
                if !response.is_empty() {
                    self.screen.queue_response(response);
                }
            }
            DcsState::Decrqss { query } => {
                self.screen
                    .queue_response(decrqss_response(&query, self.screen));
            }
            DcsState::None => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        if params.is_empty() {
            return;
        }

        let command = match std::str::from_utf8(params[0]) {
            Ok(s) => s.parse::<u32>().unwrap_or(u32::MAX),
            Err(_) => return,
        };

        match command {
            // Set window title
            0 | 2 => {
                if params.len() > 1 {
                    if let Ok(title) = std::str::from_utf8(params[1]) {
                        self.screen.title = title.to_string();
                        log::debug!("Set title: {}", title);
                    }
                }
            }
            // Set icon name
            1 => {
                if params.len() > 1 {
                    if let Ok(name) = std::str::from_utf8(params[1]) {
                        self.screen.icon_name = name.to_string();
                    }
                }
            }
            // Shell-reported current working directory.
            7 => {
                if let Some(path) = parse_osc7_working_directory(&params[1..]) {
                    self.screen.set_current_working_directory(Some(path));
                }
            }
            // Hyperlink (OSC 8)
            8 => {
                if params.len() >= 3 {
                    let uri = std::str::from_utf8(params[2]).unwrap_or("");
                    if uri.is_empty() {
                        // End hyperlink
                        self.screen.style.hyperlink = None;
                    } else {
                        // Parse params for id
                        let param_str = std::str::from_utf8(params[1]).unwrap_or("");
                        let id = param_str
                            .split(';')
                            .find_map(|p| p.strip_prefix("id="))
                            .map(String::from);

                        let hyperlink = if let Some(id) = id {
                            Hyperlink::with_id(id, uri.to_string())
                        } else {
                            Hyperlink::new(uri.to_string())
                        };

                        self.screen.style.hyperlink = Some(Arc::new(hyperlink));
                    }
                }
            }
            // iTerm2 Growl notifications. Numeric prefixes belong to the
            // ConEmu/Windows Terminal OSC 9 extension and are intentionally
            // ignored, matching Foot.
            9 => {
                if let Some(notification) = parse_simple_notification(&params[1..], true) {
                    self.screen.queue_notification(notification);
                }
            }
            // Kitty desktop notifications, including chunking and capability,
            // close, and liveness requests.
            99 => self.handle_osc99(params, bell_terminated),
            // Set/query 256-color palette entries.
            4 => {
                for pair in params[1..].as_chunks::<2>().0 {
                    let Ok(index) = std::str::from_utf8(pair[0]).unwrap_or("").parse::<u8>() else {
                        continue;
                    };
                    let value = std::str::from_utf8(pair[1]).unwrap_or("");
                    if value == "?" {
                        self.screen.queue_palette_query(index);
                    } else if let Some(color) = Rgb::from_xparse_color(value) {
                        self.screen
                            .set_dynamic_color(ColorQuery::Palette(index), Some(color));
                    }
                }
            }
            // Set/query colors (10-19)
            // OSC 10 = foreground, 11 = background, 12 = cursor
            10..=12 => {
                for (offset, value) in params.iter().skip(1).enumerate() {
                    let Some(target) = ColorQuery::from_osc_code(command + offset as u32) else {
                        break;
                    };
                    let value = std::str::from_utf8(value).unwrap_or("");
                    if value == "?" {
                        self.screen.queue_color_query(target.osc_code());
                    } else if let Some(color) = Rgb::from_xparse_color(value) {
                        self.screen.set_dynamic_color(target, Some(color));
                    }
                }
            }
            // Other color OSCs (13-19) - less common
            13..=19 => {
                log::trace!("Unhandled color OSC: {}", command);
            }
            // Reset dynamic default foreground/background/cursor colors.
            110..=112 => {
                if let Some(target) = ColorQuery::from_osc_code(command - 100) {
                    self.screen.set_dynamic_color(target, None);
                }
            }
            // Reset one, several, or all 256-color palette entries.
            104 => {
                if params.get(1).is_none_or(|value| value.is_empty()) {
                    self.screen.reset_dynamic_palette();
                } else {
                    for value in &params[1..] {
                        if let Ok(index) = std::str::from_utf8(value).unwrap_or("").parse::<u8>() {
                            self.screen
                                .set_dynamic_color(ColorQuery::Palette(index), None);
                        }
                    }
                }
            }
            // FinalTerm/iTerm2 shell integration. Foot deliberately treats B
            // as informational and records A/C/D on the current physical row.
            133 => match params.get(1).and_then(|value| value.first()).copied() {
                Some(b'A') => self.screen.mark_shell_prompt(),
                Some(b'C') => self.screen.mark_command_start(),
                Some(b'D') => self.screen.mark_command_end(),
                _ => {}
            },
            // URxvt's generic extension; only its widely implemented notify
            // command is recognized.
            777 if params.get(1).copied() == Some(b"notify") => {
                if let Some(notification) = parse_simple_notification(&params[2..], false) {
                    self.screen.queue_notification(notification);
                }
            }
            // iTerm2 inline images and file transfer (1337)
            1337 => {
                self.handle_osc_1337(params);
            }
            // Copy to clipboard (52)
            52 => {
                // OSC 52 ; Pc ; Pd ST
                // Pc = clipboard selection (c=clipboard, p=primary, s=select)
                // Pd = base64 data or ? for query
                if params.len() >= 3 {
                    let selection_str = std::str::from_utf8(params[1]).unwrap_or("c");
                    let data_str = std::str::from_utf8(params[2]).unwrap_or("");

                    // Parse selection - default to clipboard
                    let selection = if selection_str.contains('p') {
                        ClipboardSelection::Primary
                    } else if selection_str.contains('s') {
                        ClipboardSelection::Select
                    } else {
                        ClipboardSelection::Clipboard
                    };

                    if data_str == "?" {
                        // Query clipboard
                        log::debug!("Clipboard query for {:?}", selection);
                        self.screen
                            .queue_clipboard_op(ClipboardOperation::Query { selection });
                    } else if !data_str.is_empty() {
                        // Set clipboard - decode base64
                        use base64::Engine;
                        match base64::engine::general_purpose::STANDARD.decode(data_str) {
                            Ok(decoded) => {
                                log::debug!(
                                    "Clipboard set {:?}: {} bytes",
                                    selection,
                                    decoded.len()
                                );
                                self.screen.queue_clipboard_op(ClipboardOperation::Set {
                                    selection,
                                    data: decoded,
                                });
                            }
                            Err(e) => {
                                log::warn!("Failed to decode OSC 52 base64 data: {}", e);
                            }
                        }
                    }
                }
            }
            _ => {
                log::trace!("Unhandled OSC: {}", command);
            }
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let params_vec = params_to_vec(params);

        match (action, intermediates) {
            // Cursor Up (CUU)
            ('A', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(-n, 0);
            }
            // Cursor Down (CUD) / Vertical Position Relative (VPR)
            ('B', []) | ('e', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(n, 0);
            }
            // Cursor Forward (CUF) / Horizontal Position Relative (HPR)
            ('C', []) | ('a', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(0, n);
            }
            // Cursor Back (CUB)
            ('D', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(0, -n);
            }
            // Cursor Next Line (CNL)
            ('E', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(n, 0);
                self.screen.cursor.col = 0;
            }
            // Cursor Previous Line (CPL)
            ('F', []) => {
                let n = first_param(&params_vec, 1) as i32;
                self.screen.move_cursor_relative(-n, 0);
                self.screen.cursor.col = 0;
            }
            // Cursor Horizontal Absolute (CHA) / Horizontal Position Absolute (HPA)
            ('G', []) | ('`', []) => {
                let col = first_param(&params_vec, 1).saturating_sub(1);
                self.screen.cursor.col = col.min(self.screen.width().saturating_sub(1));
            }
            // Cursor Position (CUP) / Horizontal and Vertical Position (HVP)
            ('H', []) | ('f', []) => {
                let row = first_param(&params_vec, 1).saturating_sub(1);
                let col = second_param(&params_vec, 1).saturating_sub(1);
                self.screen.move_cursor(row, col);
            }
            // Erase in Display (ED)
            ('J', []) => {
                let mode = first_param(&params_vec, 0);
                match mode {
                    0 => self.screen.clear(ClearMode::Below),
                    1 => self.screen.clear(ClearMode::Above),
                    2 => self.screen.clear(ClearMode::All),
                    3 => self.screen.clear(ClearMode::Scrollback),
                    _ => {}
                }
            }
            // Erase in Line (EL)
            ('K', []) => {
                let mode = first_param(&params_vec, 0);
                match mode {
                    0 => self.screen.clear_line(LineClearMode::Right),
                    1 => self.screen.clear_line(LineClearMode::Left),
                    2 => self.screen.clear_line(LineClearMode::All),
                    _ => {}
                }
            }
            // Insert Lines (IL)
            ('L', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.insert_lines(n);
            }
            // Delete Lines (DL)
            ('M', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.delete_lines(n);
            }
            // Delete Characters (DCH)
            ('P', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.delete_chars(n);
            }
            // DEC Sixel resource management. Match foot's replies while
            // reporting cterm's active palette and geometry limits honestly.
            ('S', [b'?']) => self.handle_sixel_management(&params_vec),
            // Scroll Up (SU)
            ('S', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.scroll_up(n);
            }
            // Scroll Down (SD)
            ('T', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.scroll_down(n);
            }
            // Erase Characters (ECH)
            ('X', []) => {
                let n = first_param(&params_vec, 1);
                let cursor_row = self.screen.cursor.row;
                let cursor_col = self.screen.cursor.col;
                let width = self.screen.width();
                let count = n.min(width.saturating_sub(cursor_col));
                if let Some(row) = self.screen.grid_mut().row_mut(cursor_row) {
                    for i in 0..count {
                        row[cursor_col + i].reset();
                    }
                }
            }
            // Repeat the preceding graphic character (REP). Bound the repeat
            // count to one screen, matching foot's denial-of-service guard.
            ('b', []) => {
                if let Some(c) = *self.last_printed {
                    let max_count = self.screen.width().saturating_mul(self.screen.height());
                    let count = first_param(&params_vec, 1).min(max_count);
                    for _ in 0..count {
                        self.screen.put_char(c);
                    }
                }
            }
            // Cursor Backward Tabulation (CBT)
            ('Z', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.tab_backward(n);
            }
            // Insert Characters (ICH)
            ('@', []) => {
                let n = first_param(&params_vec, 1);
                let cursor_row = self.screen.cursor.row;
                let col = self.screen.cursor.col;
                let width = self.screen.width();
                if let Some(row) = self.screen.grid_mut().row_mut(cursor_row) {
                    // Shift characters right
                    for i in (col + n..width).rev() {
                        row[i] = row[i - n].clone();
                    }
                    // Clear inserted positions
                    for i in col..col + n.min(width.saturating_sub(col)) {
                        row[i].reset();
                    }
                }
            }
            // Vertical Line Position Absolute (VPA)
            ('d', []) => {
                let row = first_param(&params_vec, 1).saturating_sub(1);
                self.screen.cursor.row = row.min(self.screen.height().saturating_sub(1));
            }
            // SGR - Select Graphic Rendition
            ('m', []) => {
                self.handle_sgr(params);
            }
            // Set xterm modifyOtherKeys. foot treats every value except 2 as
            // its backwards-compatible level 1.
            ('m', [b'>']) => {
                if first_param(&params_vec, 0) == 4 {
                    self.screen.modes.modify_other_keys = if second_param(&params_vec, 1) == 2 {
                        2
                    } else {
                        1
                    };
                }
            }
            // Query xterm keyboard modifier resources (XTQMODKEYS).
            ('m', [b'?']) => {
                if first_param(&params_vec, 0) == 4 {
                    let level = self.screen.modes.modify_other_keys;
                    self.screen
                        .queue_response(format!("\x1b[>4;{level}m").into_bytes());
                }
            }
            // Reset xterm keyboard modifier resource to foot's level 1.
            ('n', [b'>']) => {
                if first_param(&params_vec, 2) == 4 {
                    self.screen.modes.modify_other_keys = 1;
                }
            }
            // Device Status Report (DSR)
            ('n', []) => {
                let mode = first_param(&params_vec, 0);
                match mode {
                    5 => {
                        // Status report - respond "OK"
                        self.screen.queue_response(b"\x1b[0n".to_vec());
                    }
                    6 => {
                        // Cursor position report - respond with CSI row;col R
                        let row = self.screen.cursor.row + 1;
                        let col = self.screen.cursor.col + 1;
                        let response = format!("\x1b[{};{}R", row, col);
                        self.screen.queue_response(response.into_bytes());
                    }
                    _ => {
                        log::trace!("Unknown DSR mode: {}", mode);
                    }
                }
            }
            // foot theme and native-window visibility queries.
            ('n', [b'?']) => match first_param(&params_vec, 0) {
                996 => self.screen.queue_theme_report(),
                998 => self.screen.queue_visibility_report(),
                mode => log::trace!("Unknown private DSR mode: {mode}"),
            },
            // Primary Device Attributes (DA1). Advertise only capabilities
            // implemented end to end: Sixel, ANSI color, rectangular editing,
            // and OSC 52.
            ('c', []) => {
                let mode = first_param(&params_vec, 0);
                if mode == 0 {
                    self.screen.queue_response(b"\x1b[?62;4;22;28;52c".to_vec());
                }
            }
            // Secondary Device Attributes (DA2): VT220 plus cterm version.
            ('c', [b'>']) => {
                if first_param(&params_vec, 0) == 0 {
                    self.screen
                        .queue_response(secondary_device_attributes().into_bytes());
                }
            }
            // Tertiary Device Attributes (DA3): four-byte manufacturer ID.
            ('c', [b'=']) => {
                if first_param(&params_vec, 0) == 0 {
                    self.screen
                        .queue_response(b"\x1bP!|4354524D\x1b\\".to_vec());
                }
            }
            // Set Top and Bottom Margins (DECSTBM)
            ('r', []) => {
                let top = first_param(&params_vec, 1).saturating_sub(1);
                let bottom = if params_vec.len() > 1 {
                    params_vec[1]
                } else {
                    self.screen.height()
                };
                self.screen.set_scroll_region(top, bottom);
                self.screen.move_cursor(0, 0);
            }
            // Change Attributes in Rectangular Area (DECCARA).
            ('r', [b'$']) => {
                if let Some((top, left, bottom, right)) =
                    rectangular_area(self.screen, &params_vec, 0)
                {
                    self.screen.change_rectangular_attributes(
                        top,
                        left,
                        bottom,
                        right,
                        params_vec.get(4..).unwrap_or_default(),
                    );
                }
            }
            // Reverse Attributes in Rectangular Area (DECRARA).
            ('t', [b'$']) => {
                if let Some((top, left, bottom, right)) =
                    rectangular_area(self.screen, &params_vec, 0)
                {
                    self.screen.reverse_rectangular_attributes(
                        top,
                        left,
                        bottom,
                        right,
                        params_vec.get(4..).unwrap_or_default(),
                    );
                }
            }
            // Copy Rectangular Area (DECCRA). cterm, like foot, supports the
            // active page only; omitted or zero page parameters mean page 1.
            ('v', [b'$']) => {
                let source_page = param_or(&params_vec, 4, 1);
                let destination_page = param_or(&params_vec, 7, 1);
                if source_page == 1 && destination_page == 1 {
                    if let Some((src_top, src_left, src_bottom, src_right)) =
                        rectangular_area(self.screen, &params_vec, 0)
                    {
                        let destination_relative_row =
                            param_or(&params_vec, 5, 1).saturating_sub(1);
                        let destination_row =
                            relative_screen_row(self.screen, destination_relative_row);
                        let destination_col = param_or(&params_vec, 6, 1)
                            .saturating_sub(1)
                            .min(self.screen.width().saturating_sub(1));
                        let destination_bottom = relative_screen_row(
                            self.screen,
                            destination_relative_row.saturating_add(src_bottom - src_top),
                        );
                        let destination_right = destination_col
                            .saturating_add(src_right - src_left)
                            .min(self.screen.width().saturating_sub(1));
                        let clipped_src_bottom = src_top + (destination_bottom - destination_row);
                        let clipped_src_right = src_left + (destination_right - destination_col);
                        self.screen.copy_rectangular_area(
                            src_top,
                            src_left,
                            clipped_src_bottom,
                            clipped_src_right,
                            destination_row,
                            destination_col,
                        );
                    }
                }
            }
            // Fill Rectangular Area (DECFRA). DEC defines Pc as a single
            // ISO-8859-1 byte; reject control characters exactly as foot does.
            ('x', [b'$']) => {
                let character = params_vec
                    .first()
                    .copied()
                    .and_then(|value| u8::try_from(value).ok())
                    .filter(|&value| (32..126).contains(&value) || value >= 160)
                    .map(char::from);
                if let (Some(character), Some((top, left, bottom, right))) =
                    (character, rectangular_area(self.screen, &params_vec, 1))
                {
                    self.screen
                        .fill_rectangular_area(top, left, bottom, right, character);
                }
            }
            // Erase Rectangular Area (DECERA).
            ('z', [b'$']) => {
                if let Some((top, left, bottom, right)) =
                    rectangular_area(self.screen, &params_vec, 0)
                {
                    self.screen.erase_rectangular_area(top, left, bottom, right);
                }
            }
            // Save Cursor (DECSC)
            ('s', []) => {
                self.screen.save_cursor();
            }
            // Restore Cursor (DECRC)
            ('u', []) => {
                self.screen.restore_cursor();
            }
            // Window manipulation (XTWINOPS)
            ('t', []) => {
                let operation = first_param(&params_vec, 0);
                let cell_height = self.screen.cell_height_hint().round().max(1.0) as usize;
                let cell_width = self.screen.cell_width_hint().round().max(1.0) as usize;
                let rows = self.screen.height();
                let cols = self.screen.width();
                let response = match operation {
                    11 => Some("\x1b[1t".to_string()),
                    13 => Some("\x1b[3;0;0t".to_string()),
                    14 if params_vec.get(1).copied().unwrap_or(0) != 2 => Some(format!(
                        "\x1b[4;{};{}t",
                        rows.saturating_mul(cell_height),
                        cols.saturating_mul(cell_width)
                    )),
                    16 => Some(format!("\x1b[6;{cell_height};{cell_width}t")),
                    18 => Some(format!("\x1b[8;{rows};{cols}t")),
                    _ => None,
                };
                if let Some(response) = response {
                    self.screen.queue_response(response.into_bytes());
                } else {
                    log::trace!("Window manipulation: {params_vec:?}");
                }
            }
            // XTVERSION terminal name/version report.
            ('q', [b'>']) => {
                if first_param(&params_vec, 0) == 0 {
                    self.screen.queue_response(
                        format!("\x1bP>|cterm({})\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes(),
                    );
                }
            }
            // Query the active kitty keyboard progressive-enhancement flags.
            ('u', [b'?']) => {
                let flags = self.screen.keyboard_enhancement_flags().bits();
                self.screen
                    .queue_response(format!("\x1b[?{flags}u").into_bytes());
            }
            // Push keyboard flags. Unsupported bits are masked by Screen.
            ('u', [b'>']) => {
                let flags =
                    KeyboardEnhancementFlags::from_bits_retain(first_param(&params_vec, 0) as u8);
                self.screen.push_keyboard_enhancement_flags(flags);
            }
            // Pop one or more keyboard flag stack entries.
            ('u', [b'<']) => {
                self.screen
                    .pop_keyboard_enhancement_flags(first_param(&params_vec, 1));
            }
            // Set/reset keyboard flags without changing stack depth.
            ('u', [b'=']) => {
                let requested =
                    KeyboardEnhancementFlags::from_bits_retain(first_param(&params_vec, 0) as u8)
                        & KeyboardEnhancementFlags::SUPPORTED;
                let current = self.screen.keyboard_enhancement_flags();
                let flags = match second_param(&params_vec, 1) {
                    2 => current | requested,
                    3 => current & !requested,
                    _ => requested,
                };
                self.screen.set_keyboard_enhancement_flags(flags);
            }
            // Save the current xterm color palette (XTPUSHCOLORS).
            ('P', [b'#']) => {
                self.screen.push_color_palette(first_param(&params_vec, 0));
            }
            // Restore an xterm color palette (XTPOPCOLORS).
            ('Q', [b'#']) => {
                self.screen.pop_color_palette(first_param(&params_vec, 0));
            }
            // Report current and allocated xterm color-stack slots.
            ('R', [b'#']) => {
                let (current, size) = self.screen.color_palette_stack_status();
                self.screen
                    .queue_response(format!("\x1b[?{current};{size}#Q").into_bytes());
            }
            // Save DEC private modes (XTSAVE).
            ('s', [b'?']) => {
                for &mode in &params_vec {
                    if mode == 1048 {
                        self.screen.save_cursor();
                        continue;
                    }
                    match self.dec_private_mode_status(mode) {
                        1 => {
                            self.saved_dec_modes.insert(mode, true);
                        }
                        2 => {
                            self.saved_dec_modes.insert(mode, false);
                        }
                        _ => {}
                    }
                }
            }
            // Restore DEC private modes (XTRESTORE).
            ('r', [b'?']) => {
                for &mode in &params_vec {
                    if mode == 1048 {
                        self.screen.restore_cursor();
                    } else if let Some(enabled) = self.saved_dec_modes.get(&mode).copied() {
                        self.handle_dec_mode(mode, enabled);
                    }
                }
            }
            // Set Mode (SM) / Reset Mode (RM)
            ('h', [b'?']) | ('l', [b'?']) => {
                let set = action == 'h';
                for &param in &params_vec {
                    self.handle_dec_mode(param, set);
                }
            }
            // Request DEC private mode status (DECRQM).  Capability probing is
            // common in modern TUIs, so distinguish unsupported modes from
            // modes which are currently reset.
            ('p', [b'?', b'$']) => {
                let mode = first_param(&params_vec, 0);
                let status = self.dec_private_mode_status(mode);
                self.screen
                    .queue_response(format!("\x1b[?{mode};{status}$y").into_bytes());
            }
            // ANSI modes
            ('h', []) | ('l', []) => {
                let set = action == 'h';
                for &param in &params_vec {
                    self.handle_ansi_mode(param, set);
                }
            }
            // Request ECMA-48/ANSI mode status (DECRQM).
            ('p', [b'$']) => {
                let mode = first_param(&params_vec, 0);
                let status = self.ansi_mode_status(mode);
                self.screen
                    .queue_response(format!("\x1b[{mode};{status}$y").into_bytes());
            }
            // Soft reset (DECSTR)
            ('p', [b'!']) => {
                self.screen.style.reset();
                self.screen.modes.insert_mode = false;
                self.screen.modes.origin_mode = false;
                self.screen.reset_scroll_region();
            }
            // Set cursor style (DECSCUSR)
            ('q', [b' ']) => {
                let style = first_param(&params_vec, 0);
                match style {
                    0 => self.screen.cursor.reset_style_to_config(),
                    1 => {
                        self.screen.cursor.style = CursorStyle::Block;
                        self.screen.cursor.blink.set_decscusr(Some(true));
                    }
                    2 => {
                        self.screen.cursor.style = CursorStyle::Block;
                        self.screen.cursor.blink.set_decscusr(Some(false));
                    }
                    3 => {
                        self.screen.cursor.style = CursorStyle::Underline;
                        self.screen.cursor.blink.set_decscusr(Some(true));
                    }
                    4 => {
                        self.screen.cursor.style = CursorStyle::Underline;
                        self.screen.cursor.blink.set_decscusr(Some(false));
                    }
                    5 => {
                        self.screen.cursor.style = CursorStyle::Bar;
                        self.screen.cursor.blink.set_decscusr(Some(true));
                    }
                    6 => {
                        self.screen.cursor.style = CursorStyle::Bar;
                        self.screen.cursor.blink.set_decscusr(Some(false));
                    }
                    _ => {}
                }
            }
            // Cursor Horizontal Tab forward (CHT)
            ('I', []) => {
                let n = first_param(&params_vec, 1);
                self.screen.tab_forward(n);
            }
            // Tab Clear (TBC)
            ('g', []) => {
                let mode = first_param(&params_vec, 0);
                match mode {
                    0 => self.screen.clear_tab_stop(),
                    3 => self.screen.clear_all_tab_stops(),
                    _ => {}
                }
            }
            _ => {
                log::trace!(
                    "Unhandled CSI: action={:?}, intermediates={:?}, params={:?}",
                    action,
                    intermediates,
                    params_vec
                );
            }
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (byte, intermediates) {
            // Reset (RIS)
            (b'c', []) => {
                self.screen.reset();
                self.sixel.reset();
            }
            // Save Cursor (DECSC)
            (b'7', []) => {
                self.screen.save_cursor();
            }
            // Restore Cursor (DECRC)
            (b'8', []) => {
                self.screen.restore_cursor();
            }
            // Index (IND) - move cursor down, scroll if at bottom
            (b'D', []) => {
                self.screen.line_feed();
            }
            // Next Line (NEL)
            (b'E', []) => {
                self.screen.carriage_return();
                self.screen.line_feed();
            }
            // Reverse Index (RI) - move cursor up, scroll if at top
            (b'M', []) => {
                if self.screen.cursor.row == self.screen.scroll_region().top {
                    self.screen.scroll_down(1);
                } else if self.screen.cursor.row > 0 {
                    self.screen.cursor.row -= 1;
                }
            }
            // Application Keypad (DECKPAM)
            (b'=', []) => {
                self.screen.modes.application_keypad = true;
            }
            // Normal Keypad (DECKPNM)
            (b'>', []) => {
                self.screen.modes.application_keypad = false;
            }
            // Set tab stop at current column (HTS)
            (b'H', []) => {
                self.screen.set_tab_stop();
            }
            // SCS - Select Character Set (G0)
            // ESC ( Dscs - Designate G0
            (final_char @ 0x30..=0x7E, [b'(']) => {
                let designator = Self::parse_scs_designator(&[], final_char);
                log::debug!("SCS G0: {:?}", designator);
                self.screen.designate_charset(0, designator);
            }
            // ESC ( I Dscs - Designate G0 with intermediate
            (final_char @ 0x30..=0x7E, [b'(', i]) => {
                let designator = Self::parse_scs_designator(&[*i], final_char);
                log::debug!("SCS G0: {:?}", designator);
                self.screen.designate_charset(0, designator);
            }
            // SCS - Select Character Set (G1)
            // ESC ) Dscs - Designate G1
            (final_char @ 0x30..=0x7E, [b')']) => {
                let designator = Self::parse_scs_designator(&[], final_char);
                log::debug!("SCS G1: {:?}", designator);
                self.screen.designate_charset(1, designator);
            }
            // ESC ) I Dscs - Designate G1 with intermediate
            (final_char @ 0x30..=0x7E, [b')', i]) => {
                let designator = Self::parse_scs_designator(&[*i], final_char);
                log::debug!("SCS G1: {:?}", designator);
                self.screen.designate_charset(1, designator);
            }
            _ => {
                log::trace!(
                    "Unhandled ESC: byte=0x{:02x} ({:?}), intermediates={:?}",
                    byte,
                    byte as char,
                    intermediates
                );
            }
        }
    }
}

enum XtgettcapCapability {
    Boolean,
    Value(Vec<u8>),
}

fn secondary_device_attributes() -> String {
    format!(
        "\x1b[>1;{:0>2}{:0>2}{:0>2};0c",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        env!("CARGO_PKG_VERSION_PATCH")
    )
}

fn decrqss_response(query: &[u8], screen: &Screen) -> Vec<u8> {
    let setting = match query {
        b"r" => format!(
            "{};{}r",
            screen.scroll_region().top + 1,
            screen.scroll_region().bottom
        ),
        b"m" => sgr_status_string(screen),
        b" q" => {
            let mode = match (screen.cursor.style, screen.cursor.blink.style_enabled()) {
                (CursorStyle::Block, true) => 1,
                (CursorStyle::Block, false) => 2,
                (CursorStyle::Underline, true) => 3,
                (CursorStyle::Underline, false) => 4,
                (CursorStyle::Bar, true) => 5,
                (CursorStyle::Bar, false) => 6,
            };
            format!("{mode} q")
        }
        _ => return b"\x1bP0$r\x1b\\".to_vec(),
    };

    format!("\x1bP1$r{setting}\x1b\\").into_bytes()
}

fn sgr_status_string(screen: &Screen) -> String {
    let mut attributes = vec!["0".to_string()];
    let attrs = screen.style.attrs;

    for (flag, value) in [
        (CellAttrs::BOLD, "1"),
        (CellAttrs::DIM, "2"),
        (CellAttrs::ITALIC, "3"),
        (CellAttrs::BLINK, "5"),
        (CellAttrs::RAPID_BLINK, "6"),
        (CellAttrs::INVERSE, "7"),
        (CellAttrs::HIDDEN, "8"),
        (CellAttrs::STRIKETHROUGH, "9"),
        (CellAttrs::OVERLINE, "53"),
    ] {
        if attrs.contains(flag) {
            attributes.push(value.to_string());
        }
    }

    let underline = if attrs.contains(CellAttrs::DOUBLE_UNDERLINE) {
        Some("4:2")
    } else if attrs.contains(CellAttrs::CURLY_UNDERLINE) {
        Some("4:3")
    } else if attrs.contains(CellAttrs::DOTTED_UNDERLINE) {
        Some("4:4")
    } else if attrs.contains(CellAttrs::DASHED_UNDERLINE) {
        Some("4:5")
    } else if attrs.contains(CellAttrs::UNDERLINE) {
        Some("4")
    } else {
        None
    };
    if let Some(underline) = underline {
        attributes.push(underline.to_string());
    }

    if let Some(color) = sgr_color_status(screen.style.fg, 38, 30, 90) {
        attributes.push(color);
    }
    if let Some(color) = sgr_color_status(screen.style.bg, 48, 40, 100) {
        attributes.push(color);
    }
    if let Some(color) = screen
        .style
        .underline_color
        .and_then(|color| sgr_color_status(color, 58, 0, 0))
    {
        attributes.push(color);
    }

    attributes.join(";") + "m"
}

fn sgr_color_status(color: Color, extended: usize, base: usize, bright: usize) -> Option<String> {
    match color {
        Color::Default => None,
        Color::Ansi(color) if (color as usize) < 8 => Some((base + color as usize).to_string()),
        Color::Ansi(color) => Some((bright + color as usize - 8).to_string()),
        Color::Indexed(index) => Some(format!("{extended}:5:{index}")),
        Color::Rgb(rgb) => Some(format!("{extended}:2::{}:{}:{}", rgb.r, rgb.g, rgb.b)),
    }
}

/// Build foot/xterm-compatible XTGETTCAP replies. The shape and parsing are
/// adapted from Rio; the table deliberately contains only capabilities cterm
/// implements end to end.
fn xtgettcap_response(request: &[u8], cols: usize, lines: usize) -> Vec<u8> {
    if request.is_empty() {
        return b"\x1bP0+r\x1b\\".to_vec();
    }

    let mut response = Vec::new();
    for encoded_name in request.split(|byte| *byte == b';') {
        let Some(name) = decode_hex(encoded_name) else {
            continue;
        };

        match xtgettcap_capability(&name, cols, lines) {
            Some(XtgettcapCapability::Boolean) => {
                response.extend_from_slice(b"\x1bP1+r");
                response.extend_from_slice(encoded_name);
                response.extend_from_slice(b"\x1b\\");
            }
            Some(XtgettcapCapability::Value(value)) => {
                response.extend_from_slice(b"\x1bP1+r");
                response.extend_from_slice(encoded_name);
                response.push(b'=');
                encode_hex_into(&value, &mut response);
                response.extend_from_slice(b"\x1b\\");
            }
            None => {
                response.extend_from_slice(b"\x1bP0+r");
                response.extend_from_slice(encoded_name);
                response.extend_from_slice(b"\x1b\\");
            }
        }
    }

    response
}

fn decode_hex(encoded: &[u8]) -> Option<Vec<u8>> {
    if !encoded.len().is_multiple_of(2) {
        return None;
    }

    encoded
        .as_chunks::<2>()
        .0
        .iter()
        .map(|[high, low]| {
            let high = (*high as char).to_digit(16)?;
            let low = (*low as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}

fn encode_hex_into(bytes: &[u8], output: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    output.reserve(bytes.len() * 2);
    for byte in bytes {
        output.push(HEX[(byte >> 4) as usize]);
        output.push(HEX[(byte & 0x0f) as usize]);
    }
}

fn xtgettcap_capability(name: &[u8], cols: usize, lines: usize) -> Option<XtgettcapCapability> {
    use XtgettcapCapability::{Boolean, Value};

    let value = match name {
        b"TN" | b"name" => Value(b"cterm".to_vec()),
        b"Co" | b"colors" => Value(b"256".to_vec()),
        b"pa" | b"pairs" => Value(b"32767".to_vec()),
        b"RGB" => Value(b"8/8/8".to_vec()),
        b"co" | b"cols" => Value(cols.to_string().into_bytes()),
        b"li" | b"lines" => Value(lines.to_string().into_bytes()),
        b"it" => Value(b"8".to_vec()),
        b"OTbs" | b"bs" | b"am" | b"bce" | b"km" | b"mir" | b"msgr" | b"xenl" | b"xn" | b"AX"
        | b"XT" | b"XF" | b"npc" | b"Tc" | b"sixel" | b"iterm2" => Boolean,
        b"Ss" => Value(b"\x1b[%p1%d q".to_vec()),
        b"Se" => Value(b"\x1b[0 q".to_vec()),
        b"Smulx" => Value(b"\x1b[4:%p1%dm".to_vec()),
        b"rep" => Value(b"%p1%c\x1b[%p2%{1}%-%db".to_vec()),
        b"Sync" => Value(b"\x1bP=%p1%ds\x1b\\".to_vec()),
        b"kxIN" => Value(b"\x1b[I".to_vec()),
        b"kxOUT" => Value(b"\x1b[O".to_vec()),
        b"BE" => Value(b"\x1b[?2004h".to_vec()),
        b"BD" => Value(b"\x1b[?2004l".to_vec()),
        b"PS" => Value(b"\x1b[200~".to_vec()),
        b"PE" => Value(b"\x1b[201~".to_vec()),
        b"Ms" => Value(b"\x1b]52;%p1%s;%p2%s\x07".to_vec()),
        _ => return None,
    };

    Some(value)
}

impl ScreenPerformer<'_> {
    fn handle_osc99(&mut self, params: &[&[u8]], bell_terminated: bool) {
        use base64::Engine as _;

        let Some(parameters) = params
            .get(1)
            .and_then(|value| std::str::from_utf8(value).ok())
        else {
            return;
        };

        let mut id = None;
        let mut payload_type = KittyPayloadType::Title;
        let mut done = true;
        let mut base64_encoded = false;
        let mut urgency = None;
        let mut expire_time = None;
        let mut muted = None;
        let mut focus = None;

        for parameter in parameters.split(':') {
            let Some((key, value)) = parameter.split_once('=') else {
                continue;
            };
            if key.len() != 1 {
                continue;
            }
            match key.as_bytes()[0] {
                b'i' if kitty_notification_id_is_valid(value) => id = Some(value.to_owned()),
                b'p' => {
                    payload_type = match value {
                        "title" => KittyPayloadType::Title,
                        "body" => KittyPayloadType::Body,
                        "close" => KittyPayloadType::Close,
                        "alive" => KittyPayloadType::Alive,
                        "?" => KittyPayloadType::Capabilities,
                        _ => KittyPayloadType::Ignored,
                    };
                }
                b'd' => done = value != "0",
                b'e' => base64_encoded = value == "1",
                b'u' => {
                    urgency = match value {
                        "0" => Some(NotificationUrgency::Low),
                        "1" => Some(NotificationUrgency::Normal),
                        "2" => Some(NotificationUrgency::Critical),
                        _ => None,
                    };
                }
                b'w' => {
                    expire_time = value
                        .parse::<i64>()
                        .ok()
                        .and_then(|value| i32::try_from(value).ok());
                }
                b'a' => {
                    for action in value.split(',') {
                        if action == "focus" {
                            focus = Some(true);
                        } else if action == "-focus" {
                            focus = Some(false);
                        }
                    }
                }
                b's' => {
                    if let Ok(value) = base64::engine::general_purpose::STANDARD.decode(value) {
                        if value == b"silent" {
                            muted = Some(true);
                        } else if value == b"system" {
                            muted = Some(false);
                        }
                    }
                }
                _ => {}
            }
        }

        if payload_type == KittyPayloadType::Capabilities {
            let id = id.as_deref().unwrap_or("0");
            let terminator = if bell_terminated { "\x07" } else { "\x1b\\" };
            self.screen.queue_response(
                format!("\x1b]99;i={id}:p=?;p=title,body,?,close:a=focus:o=always:u=0,1,2:c=0{terminator}")
                    .into_bytes(),
            );
            return;
        }

        if payload_type == KittyPayloadType::Alive {
            let mut active: Vec<_> = self.active_notification_ids.iter().cloned().collect();
            active.sort_unstable();
            let terminator = if bell_terminated { "\x07" } else { "\x1b\\" };
            self.screen.queue_response(
                format!(
                    "\x1b]99;i={}:p=alive;{}{terminator}",
                    id.as_deref().unwrap_or("0"),
                    active.join(",")
                )
                .into_bytes(),
            );
            return;
        }

        if payload_type == KittyPayloadType::Close {
            if let Some(id) = id {
                self.active_notification_ids.remove(&id);
                self.screen.queue_notification_close(id);
            }
            *self.kitty_notification = KittyNotificationBuilder::default();
            return;
        }

        if !self.kitty_notification.active || self.kitty_notification.id != id {
            *self.kitty_notification = KittyNotificationBuilder {
                active: true,
                id,
                ..Default::default()
            };
        }
        if let Some(urgency) = urgency {
            self.kitty_notification.urgency = urgency;
        }
        if expire_time.is_some() {
            self.kitty_notification.expire_time = expire_time;
        }
        if let Some(muted) = muted {
            self.kitty_notification.muted = muted;
        }
        if let Some(focus) = focus {
            self.kitty_notification.focus = focus;
        }

        let mut payload = Vec::new();
        for (index, part) in params.iter().skip(2).enumerate() {
            if index > 0 {
                payload.push(b';');
            }
            payload.extend_from_slice(part);
        }
        let payload = if base64_encoded {
            match base64::engine::general_purpose::STANDARD.decode(payload) {
                Ok(payload) => payload,
                Err(_) => return,
            }
        } else {
            payload
        };

        if let Ok(payload) = std::str::from_utf8(&payload) {
            match payload_type {
                KittyPayloadType::Title => append_bounded(
                    &mut self.kitty_notification.title,
                    payload,
                    MAX_NOTIFICATION_TITLE_BYTES,
                ),
                KittyPayloadType::Body => append_bounded(
                    &mut self.kitty_notification.body,
                    payload,
                    MAX_NOTIFICATION_BODY_BYTES,
                ),
                KittyPayloadType::Ignored
                | KittyPayloadType::Close
                | KittyPayloadType::Alive
                | KittyPayloadType::Capabilities => {}
            }
        }

        if done {
            let mut completed = std::mem::take(self.kitty_notification);
            if completed.title.is_empty() && completed.body.is_empty() {
                return;
            }
            if completed.title.is_empty() {
                completed.title = std::mem::take(&mut completed.body);
            }
            if let Some(id) = completed.id.as_ref() {
                if self.active_notification_ids.len() >= 64 {
                    if let Some(oldest) = self.active_notification_ids.iter().next().cloned() {
                        self.active_notification_ids.remove(&oldest);
                    }
                }
                self.active_notification_ids.insert(id.clone());
            }
            self.screen.queue_notification(DesktopNotification {
                id: completed.id,
                title: completed.title,
                body: completed.body,
                urgency: completed.urgency,
                expire_time: completed.expire_time,
                muted: completed.muted,
                focus: completed.focus,
            });
        }
    }

    /// Handle OSC 1337 (iTerm2 inline images and file transfer)
    ///
    /// Protocol format: OSC 1337 ; File=[params] : base64data ST
    fn handle_osc_1337(&mut self, params: &[&[u8]]) {
        // Reconstruct the full content from all params after the command
        // VTE splits on `;` so we need to rejoin
        if params.len() < 2 {
            return;
        }

        // Join all params after the command number
        let content = params[1..]
            .iter()
            .filter_map(|p| std::str::from_utf8(p).ok())
            .collect::<Vec<_>>()
            .join(";");

        // Check for File= prefix
        if !content.starts_with("File=") {
            log::trace!("OSC 1337: unhandled subcommand");
            return;
        }

        let content = &content[5..]; // Strip "File="

        // Find the colon separator between params and base64 data
        let Some(colon_pos) = content.find(':') else {
            log::debug!("OSC 1337 File: no colon separator found");
            return;
        };

        let param_str = &content[..colon_pos];
        let base64_data = &content[colon_pos + 1..];

        log::debug!(
            "OSC 1337 File: params={:?}, data_len={}",
            param_str,
            base64_data.len()
        );

        // Parse parameters
        let file_params = Iterm2FileParams::parse(param_str);

        // Decode base64 data
        use base64::Engine;
        let decoded = match base64::engine::general_purpose::STANDARD.decode(base64_data) {
            Ok(data) => data,
            Err(e) => {
                log::warn!("OSC 1337 File: base64 decode failed: {}", e);
                return;
            }
        };

        if file_params.inline {
            // Inline image display
            self.handle_iterm2_inline_image(file_params, decoded);
        } else {
            // File transfer - queue for UI to handle
            log::debug!(
                "OSC 1337 File transfer: name={:?}, size={}",
                file_params.name,
                decoded.len()
            );
            self.screen.queue_file_transfer(file_params.name, decoded);
        }
    }

    /// Handle inline image display from iTerm2 protocol
    fn handle_iterm2_inline_image(&mut self, params: Iterm2FileParams, data: Vec<u8>) {
        // Decode the image
        let decoded = match decode_image(&data) {
            Ok(img) => img,
            Err(e) => {
                log::warn!("OSC 1337 inline image decode failed: {}", e);
                return;
            }
        };

        if decoded.width == 0 || decoded.height == 0 {
            log::warn!(
                "OSC 1337 inline image has zero dimension: {}x{}",
                decoded.width,
                decoded.height
            );
            return;
        }

        log::debug!(
            "OSC 1337 inline image: {}x{} pixels, name={:?}",
            decoded.width,
            decoded.height,
            params.name
        );

        // Calculate display dimensions
        let cell_width = self.screen.cell_width_hint();
        let cell_height = self.screen.cell_height_hint();
        let screen_cols = self.screen.width();
        let screen_rows = self.screen.height();

        // Calculate target pixel dimensions based on params
        let target_width = params
            .width
            .to_pixels(cell_width, screen_cols, decoded.width);
        let target_height = params
            .height
            .to_pixels(cell_height, screen_rows, decoded.height);

        // Handle aspect ratio preservation
        let (final_width, final_height) = if params.preserve_aspect_ratio {
            let aspect_ratio = decoded.width as f64 / decoded.height as f64;

            // If only width or height specified, calculate the other
            match (&params.width, &params.height) {
                (Iterm2Dimension::Auto, Iterm2Dimension::Auto) => (decoded.width, decoded.height),
                (Iterm2Dimension::Auto, _) => {
                    let w = (target_height as f64 * aspect_ratio).round() as usize;
                    (w, target_height)
                }
                (_, Iterm2Dimension::Auto) => {
                    let h = (target_width as f64 / aspect_ratio).round() as usize;
                    (target_width, h)
                }
                _ => {
                    // Both specified - fit within bounds while preserving aspect ratio
                    let scale_w = target_width as f64 / decoded.width as f64;
                    let scale_h = target_height as f64 / decoded.height as f64;
                    let scale = scale_w.min(scale_h);
                    (
                        (decoded.width as f64 * scale).round() as usize,
                        (decoded.height as f64 * scale).round() as usize,
                    )
                }
            }
        } else {
            (target_width, target_height)
        };

        // Calculate cell dimensions
        let cell_cols = self.screen.image_cols_for_width(final_width);
        let cell_rows = self.screen.image_rows_for_height(final_height);

        let col = self.screen.cursor.col;
        let row = self.screen.cursor.row;

        // Create SixelImage compatible structure (reuse existing image infrastructure)
        let sixel_image = SixelImage {
            data: decoded.data,
            width: decoded.width,
            height: decoded.height,
        };

        // Add the image to the screen
        self.screen
            .add_image_with_size(col, row, cell_cols, cell_rows, sixel_image);

        // Move cursor to the row after the image (iTerm2 behavior)
        let last_image_row = row + cell_rows.saturating_sub(1);
        if last_image_row >= self.screen.height() {
            let scroll_amount = last_image_row - self.screen.height() + 1;
            self.screen.scroll_up(scroll_amount);
            self.screen.cursor.row = self.screen.height() - 1;
        } else {
            self.screen.cursor.row = last_image_row;
        }
        self.screen.cursor.col = 0;

        log::debug!(
            "iTerm2 image placed at ({}, {}) spanning {}x{} cells",
            col,
            row,
            cell_cols,
            cell_rows
        );
    }

    /// Parse SCS designator from intermediates and final character
    fn parse_scs_designator(intermediates: &[u8], final_char: u8) -> Option<String> {
        // Standard character sets return None (use built-in)
        // B = ASCII, 0 = DEC Special Graphics, etc.
        match (intermediates, final_char) {
            ([], b'B') => None,                  // ASCII
            ([], b'0') => Some("0".to_string()), // DEC Special Graphics
            ([], b'A') => None,                  // UK
            _ => {
                // Build designator string for DRCS lookup
                let mut designator = String::new();
                for &i in intermediates {
                    designator.push(i as char);
                }
                designator.push(final_char as char);
                Some(designator)
            }
        }
    }

    /// Handle SGR (Select Graphic Rendition) sequences
    fn handle_sgr(&mut self, params: &Params) {
        if params.is_empty() {
            // Reset all attributes
            self.screen.style.reset();
            return;
        }

        let mut iter = params.iter().peekable();

        while let Some(param) = iter.next() {
            match param {
                // Reset
                [0] => self.screen.style.reset(),
                // Bold
                [1] => self.screen.style.attrs.insert(CellAttrs::BOLD),
                // Dim/faint
                [2] => self.screen.style.attrs.insert(CellAttrs::DIM),
                // Italic
                [3] => self.screen.style.attrs.insert(CellAttrs::ITALIC),
                // Underline
                [4, style @ ..] => {
                    self.screen.style.attrs.clear_underline();
                    match style.first().copied().unwrap_or(1) {
                        0 => {}
                        2 => self.screen.style.attrs.insert(CellAttrs::DOUBLE_UNDERLINE),
                        3 => self.screen.style.attrs.insert(CellAttrs::CURLY_UNDERLINE),
                        4 => self.screen.style.attrs.insert(CellAttrs::DOTTED_UNDERLINE),
                        5 => self.screen.style.attrs.insert(CellAttrs::DASHED_UNDERLINE),
                        _ => self.screen.style.attrs.insert(CellAttrs::UNDERLINE),
                    }
                }
                // Slow and rapid blink are mutually exclusive.
                [5] => {
                    self.screen.style.attrs.clear_blink();
                    self.screen.style.attrs.insert(CellAttrs::BLINK);
                }
                [6] => {
                    self.screen.style.attrs.clear_blink();
                    self.screen.style.attrs.insert(CellAttrs::RAPID_BLINK);
                }
                // Inverse
                [7] => self.screen.style.attrs.insert(CellAttrs::INVERSE),
                // Hidden
                [8] => self.screen.style.attrs.insert(CellAttrs::HIDDEN),
                // Strikethrough
                [9] => self.screen.style.attrs.insert(CellAttrs::STRIKETHROUGH),
                // Normal intensity (not bold or dim)
                [22] => {
                    self.screen.style.attrs.remove(CellAttrs::BOLD);
                    self.screen.style.attrs.remove(CellAttrs::DIM);
                }
                // Not italic
                [23] => self.screen.style.attrs.remove(CellAttrs::ITALIC),
                // Not underlined
                [24] => self.screen.style.attrs.clear_underline(),
                // Not blinking
                [25] => self.screen.style.attrs.clear_blink(),
                // Not inverse
                [27] => self.screen.style.attrs.remove(CellAttrs::INVERSE),
                // Not hidden
                [28] => self.screen.style.attrs.remove(CellAttrs::HIDDEN),
                // Not strikethrough
                [29] => self.screen.style.attrs.remove(CellAttrs::STRIKETHROUGH),
                // Foreground colors (30-37)
                [param @ 30..=37] => {
                    if let Some(color) = AnsiColor::from_index((*param - 30) as u8) {
                        self.screen.style.fg = Color::Ansi(color);
                    }
                }
                // Extended foreground color, semicolon form.
                [38] => {
                    let mut color_params = iter.by_ref().map(|param| param[0]);
                    if let Some(color) = parse_sgr_color(&mut color_params) {
                        self.screen.style.fg = color;
                    }
                }
                // Extended foreground color, colon form.
                [38, color_params @ ..] => {
                    if let Some(color) = parse_colon_sgr_color(color_params) {
                        self.screen.style.fg = color;
                    }
                }
                // Default foreground
                [39] => self.screen.style.fg = Color::Default,
                // Background colors (40-47)
                [param @ 40..=47] => {
                    if let Some(color) = AnsiColor::from_index((*param - 40) as u8) {
                        self.screen.style.bg = Color::Ansi(color);
                    }
                }
                // Extended background color, semicolon form.
                [48] => {
                    let mut color_params = iter.by_ref().map(|param| param[0]);
                    if let Some(color) = parse_sgr_color(&mut color_params) {
                        self.screen.style.bg = color;
                    }
                }
                // Extended background color, colon form.
                [48, color_params @ ..] => {
                    if let Some(color) = parse_colon_sgr_color(color_params) {
                        self.screen.style.bg = color;
                    }
                }
                // Default background
                [49] => self.screen.style.bg = Color::Default,
                // Overline
                [53] => self.screen.style.attrs.insert(CellAttrs::OVERLINE),
                // Not overline
                [55] => self.screen.style.attrs.remove(CellAttrs::OVERLINE),
                // Underline color, semicolon form.
                [58] => {
                    let mut color_params = iter.by_ref().map(|param| param[0]);
                    if let Some(color) = parse_sgr_color(&mut color_params) {
                        self.screen.style.underline_color = Some(color);
                    }
                }
                // Underline color, colon form.
                [58, color_params @ ..] => {
                    if let Some(color) = parse_colon_sgr_color(color_params) {
                        self.screen.style.underline_color = Some(color);
                    }
                }
                // Default underline color
                [59] => self.screen.style.underline_color = None,
                // Bright foreground colors (90-97)
                [param @ 90..=97] => {
                    if let Some(color) = AnsiColor::from_index((*param - 90 + 8) as u8) {
                        self.screen.style.fg = Color::Ansi(color);
                    }
                }
                // Bright background colors (100-107)
                [param @ 100..=107] => {
                    if let Some(color) = AnsiColor::from_index((*param - 100 + 8) as u8) {
                        self.screen.style.bg = Color::Ansi(color);
                    }
                }
                _ => {
                    log::trace!("Unknown SGR parameter: {:?}", param);
                }
            }
        }
    }

    fn handle_sixel_management(&mut self, params: &[usize]) {
        let target = params.first().copied().unwrap_or(0);
        let operation = params.get(1).copied().unwrap_or(0);

        match (target, operation) {
            // Report current number of addressable colors.
            (1, 1) => self.queue_sixel_color_report(self.sixel.palette_size),
            // Reset color capacity and both palette scopes.
            (1, 2) => {
                self.sixel.set_palette_size(MAX_SIXEL_COLORS);
                self.queue_sixel_color_report(self.sixel.palette_size);
            }
            // Set color capacity, clamped to Foot's supported 2..=1024 range.
            (1, 3) => {
                let requested = params.get(2).copied().unwrap_or(0);
                self.sixel.set_palette_size(requested);
                self.queue_sixel_color_report(self.sixel.palette_size);
            }
            // Report implementation maximum.
            (1, 4) => self.queue_sixel_color_report(MAX_SIXEL_COLORS),
            // Report the usable viewport, capped by configured decoder limits.
            (2, 1) => {
                let cell_width = self.screen.cell_width_hint().round().max(1.0) as usize;
                let cell_height = self.screen.cell_height_hint().round().max(1.0) as usize;
                let width = self
                    .screen
                    .width()
                    .saturating_mul(cell_width)
                    .min(self.sixel.max_width);
                let height = self
                    .screen
                    .height()
                    .saturating_mul(cell_height)
                    .min(self.sixel.max_height);
                self.queue_sixel_geometry_report(width, height);
            }
            // Reset geometry limits.
            (2, 2) => {
                self.sixel.max_width = MAX_SIXEL_DIMENSION;
                self.sixel.max_height = MAX_SIXEL_DIMENSION;
                self.report_current_sixel_geometry();
            }
            // Set geometry limits. A zero-sized decoder is not useful, so the
            // supported range is one through Foot's 10,000-pixel maximum.
            (2, 3) => {
                self.sixel.max_width = params
                    .get(2)
                    .copied()
                    .unwrap_or(0)
                    .clamp(1, MAX_SIXEL_DIMENSION);
                self.sixel.max_height = params
                    .get(3)
                    .copied()
                    .unwrap_or(0)
                    .clamp(1, MAX_SIXEL_DIMENSION);
                self.report_current_sixel_geometry();
            }
            // Foot reports the active maximum here, including a restriction
            // installed by operation 3.
            (2, 4) => self.queue_sixel_geometry_report(self.sixel.max_width, self.sixel.max_height),
            _ => {}
        }
    }

    fn report_current_sixel_geometry(&mut self) {
        let cell_width = self.screen.cell_width_hint().round().max(1.0) as usize;
        let cell_height = self.screen.cell_height_hint().round().max(1.0) as usize;
        let width = self
            .screen
            .width()
            .saturating_mul(cell_width)
            .min(self.sixel.max_width);
        let height = self
            .screen
            .height()
            .saturating_mul(cell_height)
            .min(self.sixel.max_height);
        self.queue_sixel_geometry_report(width, height);
    }

    fn queue_sixel_color_report(&mut self, count: usize) {
        self.screen
            .queue_response(format!("\x1b[?1;0;{count}S").into_bytes());
    }

    fn queue_sixel_geometry_report(&mut self, width: usize, height: usize) {
        self.screen
            .queue_response(format!("\x1b[?2;0;{width};{height}S").into_bytes());
    }

    /// Handle DEC private mode set/reset
    fn handle_dec_mode(&mut self, mode: usize, set: bool) {
        match mode {
            // DECCKM - Cursor Keys Mode
            1 => self.screen.modes.application_cursor = set,
            // DECOM - Origin Mode
            6 => {
                self.screen.modes.origin_mode = set;
                self.screen.move_cursor(0, 0);
            }
            // DECSCNM - Reverse screen mode
            5 => {
                self.screen.modes.reverse_video = set;
                self.screen.dirty = true;
            }
            // DECAWM - Auto Wrap Mode
            7 => self.screen.modes.auto_wrap = set,
            // Reverse Wraparound Mode
            45 => self.screen.modes.reverse_wrap = set,
            // X10 Mouse Reporting. Foot recognizes this legacy mode but keeps
            // it permanently reset.
            9 => {}
            // DECTCEM - Show Cursor
            25 => self.screen.modes.show_cursor = set,
            // Cursor blinking mode
            12 => self.screen.cursor.blink.set_dec_mode_12(set),
            // DECNKM - Numeric Keypad Mode.  This is equivalent to DECKPAM /
            // DECKPNM, but expressed as a DEC private mode.
            66 => self.screen.modes.application_keypad = set,
            // DECSDM - Sixel Display Mode (mode 80)
            // Note: The VT340 manual was wrong - 'set' actually DISABLES scrolling
            // When set (h): sixel scrolling OFF (image at top-left, no scroll)
            // When reset (l): sixel scrolling ON (image at cursor, can scroll)
            80 => self.screen.modes.sixel_scrolling = !set,
            // Normal Mouse Tracking
            1000 => self.set_mouse_mode(MouseMode::Normal, set),
            // Button Event Mouse Tracking
            1002 => self.set_mouse_mode(MouseMode::ButtonEvent, set),
            // Any Event Mouse Tracking
            1003 => self.set_mouse_mode(MouseMode::AnyEvent, set),
            // Focus Events
            1004 => self.screen.modes.focus_events = set,
            // UTF-8 Mouse Mode
            1005 => { /* UTF-8 encoding for mouse coordinates - not implemented */ }
            // Mouse-coordinate encodings are mutually exclusive. Resetting an
            // inactive encoding does not disturb the active one, matching foot.
            1006 => self.set_mouse_encoding(MouseEncoding::Sgr, set),
            // Alternate Scroll Mode: wheel -> cursor keys on the alternate screen
            1007 => self.screen.modes.alternate_scroll = set,
            // URXVT decimal mouse encoding.
            1015 => self.set_mouse_encoding(MouseEncoding::Urxvt, set),
            // SGR pixel-coordinate mouse encoding.
            1016 => self.set_mouse_encoding(MouseEncoding::SgrPixels, set),
            // Alternate Screen Buffer.  Mode 47 is the older xterm spelling.
            47 | 1047 => {
                if set {
                    self.screen.enter_alternate_screen();
                } else {
                    self.screen.exit_alternate_screen();
                }
            }
            // Save/Restore Cursor
            1048 => {
                if set {
                    self.screen.save_cursor();
                } else {
                    self.screen.restore_cursor();
                }
            }
            // Alternate Screen Buffer with cursor save/restore
            1049 => {
                if set {
                    self.screen.save_cursor();
                    self.screen.enter_alternate_screen();
                    self.screen.clear(ClearMode::All);
                } else {
                    self.screen.exit_alternate_screen();
                    self.screen.restore_cursor();
                }
            }
            // Bracketed Paste Mode
            2004 => self.screen.modes.bracketed_paste = set,
            // Application synchronized updates
            2026 => self.screen.set_application_sync_updates(set),
            // Use a fresh palette for every Sixel image. Resetting this mode
            // preserves definitions in a session-wide shared palette.
            1070 => self.screen.modes.sixel_private_palette = set,
            // Report frontend theme changes.
            2031 => self.screen.modes.theme_change_reports = set,
            // Report frontend visibility changes. foot immediately reports the
            // current state when this mode is enabled.
            2033 => {
                self.screen.modes.visibility_change_reports = set;
                if set {
                    self.screen.queue_visibility_report();
                }
            }
            // xterm Sixel Cursor Right of Graphics
            8452 => self.screen.modes.sixel_cursor_right = set,
            _ => {
                log::trace!("Unknown DEC mode: {} = {}", mode, set);
            }
        }
    }

    /// Return a DECRPM status value for a DEC private mode.
    ///
    /// 0 = unrecognized, 1 = set, 2 = reset, 3 = permanently set,
    /// 4 = permanently reset.
    fn dec_private_mode_status(&self, mode: usize) -> u8 {
        let enabled = match mode {
            1 => self.screen.modes.application_cursor,
            5 => self.screen.modes.reverse_video,
            6 => self.screen.modes.origin_mode,
            7 => self.screen.modes.auto_wrap,
            45 => self.screen.modes.reverse_wrap,
            12 => self.screen.cursor.blink.dec_mode_12(),
            25 => self.screen.modes.show_cursor,
            47 | 1047 | 1049 => self.screen.modes.alternate_screen,
            66 => self.screen.modes.application_keypad,
            // DECSDM is set when sixel scrolling is disabled.
            80 => !self.screen.modes.sixel_scrolling,
            1000 => self.screen.modes.mouse_mode == MouseMode::Normal,
            1002 => self.screen.modes.mouse_mode == MouseMode::ButtonEvent,
            1003 => self.screen.modes.mouse_mode == MouseMode::AnyEvent,
            1004 => self.screen.modes.focus_events,
            1006 => self.screen.modes.mouse_encoding == MouseEncoding::Sgr,
            1007 => self.screen.modes.alternate_scroll,
            1015 => self.screen.modes.mouse_encoding == MouseEncoding::Urxvt,
            1016 => self.screen.modes.mouse_encoding == MouseEncoding::SgrPixels,
            1070 => self.screen.modes.sixel_private_palette,
            2004 => self.screen.modes.bracketed_paste,
            2026 => self.screen.modes.application_sync_updates,
            2031 => self.screen.modes.theme_change_reports,
            2033 => self.screen.modes.visibility_change_reports,
            8452 => self.screen.modes.sixel_cursor_right,
            // Recognized legacy encodings which cterm deliberately never uses.
            9 | 67 | 1001 | 1005 => return 4,
            _ => return 0,
        };

        if enabled {
            1
        } else {
            2
        }
    }

    fn set_mouse_encoding(&mut self, encoding: MouseEncoding, set: bool) {
        if set {
            self.screen.modes.mouse_encoding = encoding;
        } else if self.screen.modes.mouse_encoding == encoding {
            self.screen.modes.mouse_encoding = MouseEncoding::Normal;
        }
    }

    fn set_mouse_mode(&mut self, mode: MouseMode, set: bool) {
        if set {
            self.screen.modes.mouse_mode = mode;
        } else if self.screen.modes.mouse_mode == mode {
            self.screen.modes.mouse_mode = MouseMode::None;
        }
    }

    /// Return a DECRPM status value for an ECMA-48/ANSI mode.
    fn ansi_mode_status(&self, mode: usize) -> u8 {
        let enabled = match mode {
            4 => self.screen.modes.insert_mode,
            20 => self.screen.modes.line_feed_mode,
            _ => return 0,
        };

        if enabled {
            1
        } else {
            2
        }
    }

    /// Handle ANSI mode set/reset
    fn handle_ansi_mode(&mut self, mode: usize, set: bool) {
        match mode {
            // IRM - Insert Mode
            4 => self.screen.modes.insert_mode = set,
            // LNM - Line Feed/New Line Mode
            20 => self.screen.modes.line_feed_mode = set,
            _ => {
                log::trace!("Unknown ANSI mode: {} = {}", mode, set);
            }
        }
    }
}

// Helper functions

fn parse_simple_notification(
    params: &[&[u8]],
    ignore_numeric_prefix: bool,
) -> Option<DesktopNotification> {
    let total_len = params.iter().try_fold(0_usize, |length, param| {
        length.checked_add(param.len())?.checked_add(1)
    })?;
    if total_len > MAX_NOTIFICATION_TITLE_BYTES + MAX_NOTIFICATION_BODY_BYTES + 2 {
        log::warn!("Ignoring oversized OSC desktop notification");
        return None;
    }

    let mut payload = Vec::with_capacity(total_len);
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            payload.push(b';');
        }
        payload.extend_from_slice(param);
    }
    let payload = std::str::from_utf8(&payload).ok()?;
    if payload.is_empty() {
        return None;
    }

    if ignore_numeric_prefix {
        if let Some((prefix, _)) = payload.split_once(';') {
            if prefix.parse::<u64>().is_ok() {
                return None;
            }
        }
    }

    let (title, body) = payload.split_once(';').unwrap_or((payload, ""));
    if title.is_empty() {
        return None;
    }

    Some(DesktopNotification {
        title: truncate_utf8(title, MAX_NOTIFICATION_TITLE_BYTES).to_owned(),
        body: truncate_utf8(body, MAX_NOTIFICATION_BODY_BYTES).to_owned(),
        focus: true,
        ..Default::default()
    })
}

fn kitty_notification_id_is_valid(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'.'))
}

fn append_bounded(destination: &mut String, payload: &str, max_bytes: usize) {
    let remaining = max_bytes.saturating_sub(destination.len());
    destination.push_str(truncate_utf8(payload, remaining));
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

fn parse_osc7_working_directory(params: &[&[u8]]) -> Option<std::path::PathBuf> {
    let mut encoded_uri = Vec::new();
    for (index, param) in params.iter().enumerate() {
        if index > 0 {
            encoded_uri.push(b';');
        }
        encoded_uri.extend_from_slice(param);
    }

    let mut uri = url::Url::parse(std::str::from_utf8(&encoded_uri).ok()?).ok()?;
    if uri.scheme() != "file" || !osc7_hostname_is_local(uri.host_str().unwrap_or("")) {
        return None;
    }

    // `Url::to_file_path` only treats an empty/localhost authority as local.
    // We have already verified the actual machine hostname, so normalize it
    // away before performing the platform-specific path conversion.
    uri.set_host(None).ok()?;
    let path = uri.to_file_path().ok()?;
    if path.as_os_str().as_encoded_bytes().contains(&0) {
        return None;
    }
    Some(path)
}

fn osc7_hostname_is_local(host: &str) -> bool {
    host.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || hostname::get()
            .ok()
            .is_some_and(|local| host.eq_ignore_ascii_case(&local.to_string_lossy()))
}

/// Parse the colon form of an SGR color while tolerating the optional color
/// space and tolerance fields accepted by foot and VTE.
fn parse_colon_sgr_color(params: &[u16]) -> Option<Color> {
    let components_start = if params.len() > 4 { 2 } else { 1 };
    let mut params =
        std::iter::once(*params.first()?).chain(params.get(components_start..)?.iter().copied());
    parse_sgr_color(&mut params)
}

fn parse_sgr_color(params: &mut dyn Iterator<Item = u16>) -> Option<Color> {
    match params.next()? {
        5 => Some(Color::Indexed(u8::try_from(params.next()?).ok()?)),
        2 => Some(Color::Rgb(Rgb::new(
            u8::try_from(params.next()?).ok()?,
            u8::try_from(params.next()?).ok()?,
            u8::try_from(params.next()?).ok()?,
        ))),
        _ => None,
    }
}

fn params_to_vec(params: &Params) -> Vec<usize> {
    let mut result = Vec::new();
    for item in params.iter() {
        for &subparam in item {
            result.push(subparam as usize);
        }
    }
    result
}

fn first_param(params: &[usize], default: usize) -> usize {
    params
        .first()
        .copied()
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

fn second_param(params: &[usize], default: usize) -> usize {
    params
        .get(1)
        .copied()
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

fn param_or(params: &[usize], index: usize, default: usize) -> usize {
    params
        .get(index)
        .copied()
        .filter(|&value| value != 0)
        .unwrap_or(default)
}

/// Convert a DEC screen-relative row to the active grid row. In origin mode,
/// coordinates are relative to and clipped by the scrolling margins.
fn relative_screen_row(screen: &Screen, row: usize) -> usize {
    if screen.modes.origin_mode {
        screen
            .scroll_region()
            .top
            .saturating_add(row)
            .min(screen.scroll_region().bottom.saturating_sub(1))
    } else {
        row.min(screen.height().saturating_sub(1))
    }
}

/// Parse the four one-based coordinates shared by DEC rectangular commands.
/// Defaults, validation order and clipping intentionally follow foot.
fn rectangular_area(
    screen: &Screen,
    params: &[usize],
    first_index: usize,
) -> Option<(usize, usize, usize, usize)> {
    let relative_top = param_or(params, first_index, 1).saturating_sub(1);
    let left = param_or(params, first_index + 1, 1)
        .saturating_sub(1)
        .min(screen.width().saturating_sub(1));
    let relative_bottom = param_or(params, first_index + 2, screen.height()).saturating_sub(1);
    let right = param_or(params, first_index + 3, screen.width())
        .saturating_sub(1)
        .min(screen.width().saturating_sub(1));

    if relative_top > relative_bottom || left > right {
        return None;
    }

    Some((
        relative_screen_row(screen, relative_top),
        left,
        relative_screen_row(screen, relative_bottom),
        right,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::screen::ScreenConfig;

    fn make_screen() -> Screen {
        Screen::new(80, 24, ScreenConfig::default())
    }

    #[test]
    fn test_print() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"Hello");

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "H");
        assert_eq!(screen.get_cell(0, 4).unwrap().text(), "o");
        assert_eq!(screen.cursor.col, 5);
    }

    #[test]
    fn test_cursor_movement() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // Move to position (5, 10) - CSI 6;11H (1-indexed)
        parser.parse(&mut screen, b"\x1b[6;11H");

        assert_eq!(screen.cursor.row, 5);
        assert_eq!(screen.cursor.col, 10);
    }

    #[test]
    fn test_foot_relative_cursor_aliases_and_repeat() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[2;2H\x1b[3a\x1b[2e\x1b[7`");
        assert_eq!((screen.cursor.row, screen.cursor.col), (3, 6));

        parser.parse(&mut screen, b"x\x1b[5b");
        assert_eq!(screen.grid().row(3).unwrap().text(), "      xxxxxx");
        assert_eq!(screen.cursor.col, 12);
    }

    #[test]
    fn test_device_and_window_reports_match_foot_shapes() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b[c\x1b[>c\x1b[=c\x1b[>q\x1b[11t\x1b[13t\x1b[14t\x1b[16t\x1b[18t",
        );

        assert_eq!(
            screen.take_pending_responses(),
            vec![
                b"\x1b[?62;4;22;28;52c".to_vec(),
                secondary_device_attributes().into_bytes(),
                b"\x1bP!|4354524D\x1b\\".to_vec(),
                format!("\x1bP>|cterm({})\x1b\\", env!("CARGO_PKG_VERSION")).into_bytes(),
                b"\x1b[1t".to_vec(),
                b"\x1b[3;0;0t".to_vec(),
                b"\x1b[4;384;640t".to_vec(),
                b"\x1b[6;16;8t".to_vec(),
                b"\x1b[8;24;80t".to_vec(),
            ]
        );
    }

    #[test]
    fn test_modify_other_keys_set_query_and_reset() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[?4m");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[>4;1m"]);

        parser.parse(&mut screen, b"\x1b[>4;2m\x1b[?4m");
        assert_eq!(screen.modes.modify_other_keys, 2);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[>4;2m"]);

        parser.parse(&mut screen, b"\x1b[>4n\x1b[?4m");
        assert_eq!(screen.modes.modify_other_keys, 1);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[>4;1m"]);
    }

    #[test]
    fn test_decrqss_reports_scroll_style_and_cursor_state() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            concat!(
                "\x1b[3;20r",
                "\x1b[1;3;4:3;38;2;1;2;3;48;5;42;58;2;4;5;6m",
                "\x1b[5 q",
                "\x1bP$qm\x1b\\",
                "\x1bP$qr\x1b\\",
                "\x1bP$q q\x1b\\",
                "\x1bP$qz\x1b\\",
            )
            .as_bytes(),
        );

        assert_eq!(
            screen.take_pending_responses(),
            vec![
                b"\x1bP1$r0;1;3;4:3;38:2::1:2:3;48:5:42;58:2::4:5:6m\x1b\\".to_vec(),
                b"\x1bP1$r3;20r\x1b\\".to_vec(),
                b"\x1bP1$r5 q\x1b\\".to_vec(),
                b"\x1bP0$r\x1b\\".to_vec(),
            ]
        );
    }

    #[test]
    fn test_sgr_colors() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // Red foreground
        parser.parse(&mut screen, b"\x1b[31m");
        assert_eq!(screen.style.fg, Color::Ansi(AnsiColor::Red));

        // Blue background
        parser.parse(&mut screen, b"\x1b[44m");
        assert_eq!(screen.style.bg, Color::Ansi(AnsiColor::Blue));

        // Reset
        parser.parse(&mut screen, b"\x1b[0m");
        assert_eq!(screen.style.fg, Color::Default);
        assert_eq!(screen.style.bg, Color::Default);
    }

    #[test]
    fn test_sgr_slow_and_rapid_blink_are_distinct() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[5mA\x1b[6mB\x1b[25mC");

        let slow = screen.get_cell(0, 0).unwrap().attrs;
        assert!(slow.contains(CellAttrs::BLINK));
        assert!(!slow.contains(CellAttrs::RAPID_BLINK));
        let rapid = screen.get_cell(0, 1).unwrap().attrs;
        assert!(!rapid.contains(CellAttrs::BLINK));
        assert!(rapid.contains(CellAttrs::RAPID_BLINK));
        assert!(!screen.get_cell(0, 2).unwrap().attrs.has_blink());
    }

    #[test]
    fn test_cursor_blink_sources_match_foot() {
        let mut screen = make_screen();
        screen.configure_cursor(CursorStyle::Bar, false);
        let mut parser = Parser::new();

        assert!(!screen.cursor.blink.enabled());
        parser.parse(&mut screen, b"\x1b[?12h\x1b[4 q");
        assert!(screen.cursor.blink.enabled());
        assert!(screen.cursor.blink.dec_mode_12());
        assert!(!screen.cursor.blink.style_enabled());
        assert_eq!(screen.cursor.style, CursorStyle::Underline);

        parser.parse(&mut screen, b"\x1bP$q q\x1b\\\x1b[?12$p");
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1bP1$r4 q\x1b\\".to_vec(), b"\x1b[?12;1$y".to_vec()]
        );

        parser.parse(&mut screen, b"\x1b[?12l");
        assert!(!screen.cursor.blink.enabled());
        parser.parse(&mut screen, b"\x1b[1 q");
        assert!(screen.cursor.blink.enabled());
        parser.parse(&mut screen, b"\x1b[0 q");
        assert_eq!(screen.cursor.style, CursorStyle::Bar);
        assert!(!screen.cursor.blink.enabled());

        parser.parse(&mut screen, b"\x1b[?12h");
        screen.reset();
        assert_eq!(screen.cursor.style, CursorStyle::Bar);
        assert!(!screen.cursor.blink.enabled());
    }

    #[test]
    fn test_sgr_256_color() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // 256-color: color index 196 (bright red)
        parser.parse(&mut screen, b"\x1b[38;5;196m");
        assert_eq!(screen.style.fg, Color::Indexed(196));
    }

    #[test]
    fn test_sgr_rgb_color() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // RGB: #ff8800
        parser.parse(&mut screen, b"\x1b[38;2;255;136;0m");
        assert_eq!(screen.style.fg, Color::Rgb(Rgb::new(255, 136, 0)));
    }

    #[test]
    fn test_sgr_colon_colors_match_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[4:3;38:2::1:2:3;48:5:42;58:2:0:4:5:6m");

        assert!(screen.style.attrs.contains(CellAttrs::CURLY_UNDERLINE));
        assert_eq!(screen.style.fg, Color::Rgb(Rgb::new(1, 2, 3)));
        assert_eq!(screen.style.bg, Color::Indexed(42));
        assert_eq!(
            screen.style.underline_color,
            Some(Color::Rgb(Rgb::new(4, 5, 6)))
        );
    }

    #[test]
    fn test_sgr_color_components_are_checked_not_truncated() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[31;48;5;300;58:2::256:2:3m");

        assert_eq!(screen.style.fg, Color::Ansi(AnsiColor::Red));
        assert_eq!(screen.style.bg, Color::Default);
        assert_eq!(screen.style.underline_color, None);
    }

    #[test]
    fn test_dynamic_default_colors_match_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]10;#123\x1b\\\x1b]11;rgb:40/80/c0\x07\x1b]12;#abcdef\x1b\\",
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Foreground),
            Some(Rgb::new(0x11, 0x22, 0x33))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Background),
            Some(Rgb::new(0x40, 0x80, 0xc0))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Cursor),
            Some(Rgb::new(0xab, 0xcd, 0xef))
        );

        parser.parse(&mut screen, b"\x1b]110\x1b\\\x1b]111\x1b\\\x1b]112\x1b\\");
        assert_eq!(screen.dynamic_color(ColorQuery::Foreground), None);
        assert_eq!(screen.dynamic_color(ColorQuery::Background), None);
        assert_eq!(screen.dynamic_color(ColorQuery::Cursor), None);
    }

    #[test]
    fn test_dynamic_color_multi_parameter_and_invalid_specs() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b]10;#102030;#405060;bogus\x1b\\");
        assert_eq!(
            screen.dynamic_color(ColorQuery::Foreground),
            Some(Rgb::new(0x10, 0x20, 0x30))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Background),
            Some(Rgb::new(0x40, 0x50, 0x60))
        );
        assert_eq!(screen.dynamic_color(ColorQuery::Cursor), None);
    }

    #[test]
    fn test_osc_palette_set_query_and_reset_match_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]4;1;#123;200;rgb:40/80/c0;1;?;200;?\x1b\\",
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Palette(1)),
            Some(Rgb::new(0x11, 0x22, 0x33))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Palette(200)),
            Some(Rgb::new(0x40, 0x80, 0xc0))
        );
        assert_eq!(
            screen.take_color_queries(),
            vec![
                (ColorQuery::Palette(1), Some(Rgb::new(0x11, 0x22, 0x33))),
                (ColorQuery::Palette(200), Some(Rgb::new(0x40, 0x80, 0xc0))),
            ]
        );

        parser.parse(&mut screen, b"\x1b]104;1;bogus;200\x1b\\");
        assert_eq!(screen.dynamic_color(ColorQuery::Palette(1)), None);
        assert_eq!(screen.dynamic_color(ColorQuery::Palette(200)), None);

        parser.parse(
            &mut screen,
            b"\x1b]4;2;#abcdef;255;#010203\x1b\\\x1b]104\x1b\\",
        );
        assert_eq!(screen.dynamic_color(ColorQuery::Palette(2)), None);
        assert_eq!(screen.dynamic_color(ColorQuery::Palette(255)), None);
    }

    #[test]
    fn test_osc_palette_rejects_invalid_indices_and_colors() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]4;256;#ffffff;3;invalid;not-an-index;?\x1b\\",
        );

        assert_eq!(screen.dynamic_palette_colors().count(), 0);
        assert!(screen.take_color_queries().is_empty());
    }

    #[test]
    fn test_xterm_color_stack_restores_dynamic_palette_like_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]10;#112233\x1b\\\x1b]4;200;#123456\x1b\\\x1b[#P",
        );
        parser.parse(
            &mut screen,
            b"\x1b]10;#445566\x1b\\\x1b]4;200;#abcdef\x1b\\\x1b[3#P",
        );
        parser.parse(&mut screen, b"\x1b]10;#778899\x1b\\\x1b[#R\x1b[#Q");

        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b[?3;3#Q".to_vec()]
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Foreground),
            Some(Rgb::new(0x44, 0x55, 0x66))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Palette(200)),
            Some(Rgb::new(0xab, 0xcd, 0xef))
        );

        // Slot two was initialized from the same active palette when the
        // explicit third slot grew the stack. Two further pops restore slot
        // two and then the original slot one while retaining allocated slots.
        parser.parse(&mut screen, b"\x1b[#Q\x1b[#Q\x1b[#R");
        assert_eq!(
            screen.dynamic_color(ColorQuery::Foreground),
            Some(Rgb::new(0x11, 0x22, 0x33))
        );
        assert_eq!(
            screen.dynamic_color(ColorQuery::Palette(200)),
            Some(Rgb::new(0x12, 0x34, 0x56))
        );
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b[?0;3#Q".to_vec()]
        );

        parser.parse(&mut screen, b"\x1b[128#P\x1b[999#P\x1b[#R\x1bc\x1b[#R");
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b[?128;128#Q".to_vec(), b"\x1b[?0;0#Q".to_vec()]
        );
    }

    #[test]
    fn test_osc7_tracks_local_file_uri_with_encoded_and_semicolon_path() {
        let mut screen = make_screen();
        let mut parser = Parser::new();
        let expected = std::env::temp_dir().join("cterm OSC7;directory");
        let mut uri = url::Url::from_file_path(&expected).unwrap();
        uri.set_host(Some("localhost")).unwrap();
        let sequence = format!("\x1b]7;{uri}\x1b\\");

        parser.parse(&mut screen, sequence.as_bytes());

        assert_eq!(screen.current_working_directory(), Some(expected.as_path()));
    }

    #[test]
    fn test_osc7_rejects_remote_hosts_and_non_file_uris() {
        let mut screen = make_screen();
        let mut parser = Parser::new();
        let original = std::env::temp_dir().join("cterm-osc7-original");
        screen.set_current_working_directory(Some(original.clone()));

        parser.parse(
            &mut screen,
            b"\x1b]7;file://definitely-remote.invalid/tmp/other\x1b\\\
              \x1b]7;https://localhost/tmp/other\x1b\\\
              \x1b]7;file://localhost/tmp/%00\x1b\\",
        );

        assert_eq!(screen.current_working_directory(), Some(original.as_path()));
    }

    #[test]
    fn test_osc7_accepts_the_machine_hostname() {
        let hostname = hostname::get().unwrap();
        let hostname = hostname.to_string_lossy();
        assert!(osc7_hostname_is_local(&hostname));
    }

    #[test]
    fn test_osc133_records_foot_shell_markers() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"prompt\x1b]133;A\x07\x1b]133;B\x07\x1b]133;C\x07output\x1b]133;D;0\x1b\\",
        );

        let integration = &screen.grid()[0].shell_integration;
        assert!(integration.prompt_marker);
        assert_eq!(integration.command_start, Some(6));
        assert_eq!(integration.command_end, Some(12));
        assert_eq!(screen.last_command_output().as_deref(), Some("output"));
    }

    #[test]
    fn test_osc9_and_777_desktop_notifications_match_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]9;Build;finished; successfully\x07\
              \x1b]9;4;ignored Windows taskbar form\x07\
              \x1b]777;notify;Deploy;server ready\x1b\\\
              \x1b]777;other;ignored\x1b\\",
        );

        assert_eq!(
            screen.take_notifications(),
            vec![
                DesktopNotificationAction::Show(DesktopNotification {
                    title: "Build".into(),
                    body: "finished; successfully".into(),
                    focus: true,
                    ..Default::default()
                }),
                DesktopNotificationAction::Show(DesktopNotification {
                    title: "Deploy".into(),
                    body: "server ready".into(),
                    focus: true,
                    ..Default::default()
                }),
            ]
        );
    }

    #[test]
    fn test_kitty_osc99_chunks_queries_and_closes_notifications() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]99;i=build:d=0:e=1:p=title;QnVpbGQ=\x1b\\\
              \x1b]99;i=build:d=1:p=body:u=2:s=c2lsZW50;finished\x1b\\",
        );
        assert_eq!(
            screen.take_notifications(),
            vec![DesktopNotificationAction::Show(DesktopNotification {
                id: Some("build".into()),
                title: "Build".into(),
                body: "finished".into(),
                urgency: NotificationUrgency::Critical,
                muted: true,
                focus: true,
                ..Default::default()
            })]
        );

        parser.parse(&mut screen, b"\x1b]99;i=query:p=alive;\x07");
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b]99;i=query:p=alive;build\x07".to_vec()]
        );

        parser.parse(&mut screen, b"\x1b]99;i=build:p=close;\x1b\\");
        assert_eq!(
            screen.take_notifications(),
            vec![DesktopNotificationAction::Close("build".into())]
        );

        parser.parse(&mut screen, b"\x1b]99;i=query:p=?;\x1b\\");
        let response = screen.take_pending_responses().pop().unwrap();
        let response = std::str::from_utf8(&response).unwrap();
        assert!(response.starts_with("\x1b]99;i=query:p=?;"));
        assert!(response.contains("p=title,body,?,close"));
        assert!(!response.contains("alive"));
        assert!(response.contains("a=focus"));
        assert!(response.ends_with("\x1b\\"));
    }

    #[test]
    fn test_kitty_body_only_notification_uses_body_as_native_title() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b]99;p=body;body only\x1b\\");

        assert_eq!(
            screen.take_notifications(),
            vec![DesktopNotificationAction::Show(DesktopNotification {
                title: "body only".into(),
                focus: true,
                ..Default::default()
            })]
        );
    }

    #[test]
    fn test_clear_screen() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"XXXXX");
        parser.parse(&mut screen, b"\x1b[2J"); // Clear all

        for col in 0..5 {
            assert_eq!(screen.get_cell(0, col).unwrap().text(), " ");
        }
    }

    #[test]
    fn test_dec_rectangular_fill_and_erase_match_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"abcdefgh\x1b[2;1Hijklmnop\x1b[1;44m\x1b[88;1;2;2;4$x",
        );

        assert_eq!(screen.grid().row(0).unwrap().text(), "aXXXefgh");
        assert_eq!(screen.grid().row(1).unwrap().text(), "iXXXmnop");
        let filled = screen.get_cell(0, 1).unwrap();
        assert_eq!(filled.bg, Color::Ansi(AnsiColor::Blue));
        assert!(filled.attrs.contains(CellAttrs::BOLD));

        parser.parse(&mut screen, b"\x1b[22;41m\x1b[1;3;2;3$z");
        for row in 0..2 {
            let erased = screen.get_cell(row, 2).unwrap();
            assert_eq!(erased.text(), " ");
            assert_eq!(erased.bg, Color::Ansi(AnsiColor::Red));
            assert!(erased.attrs.is_empty());
        }
        assert_eq!(screen.grid().row(0).unwrap().text(), "aX Xefgh");
    }

    #[test]
    fn test_dec_rectangular_copy_uses_overlap_safe_snapshot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b]8;;https://example.com\x1b\\abcdef\x1b]8;;\x1b\\\x1b[1;1;1;4;1;1;3;1$v",
        );

        assert_eq!(screen.grid().row(0).unwrap().text(), "ababcd");
        assert!(screen.get_cell(0, 0).unwrap().hyperlink.is_some());
        assert!(screen.get_cell(0, 2).unwrap().hyperlink.is_none());

        // Pages other than the active page are ignored.
        parser.parse(&mut screen, b"\x1b[1;1;1;2;2;2;1;1$v");
        assert!(screen.grid().row(1).unwrap().text().is_empty());
    }

    #[test]
    fn test_dec_rectangular_attribute_subset_preserves_other_style() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b[3;32mABCD\x1b[1;2;1;3;1;4;7$r\x1b[1;3;1;4;0$t",
        );

        let second = screen.get_cell(0, 1).unwrap();
        assert!(second.attrs.contains(CellAttrs::BOLD));
        assert!(second.attrs.contains(CellAttrs::UNDERLINE));
        assert!(second.attrs.contains(CellAttrs::INVERSE));
        assert!(second.attrs.contains(CellAttrs::ITALIC));
        assert_eq!(second.fg, Color::Ansi(AnsiColor::Green));

        let third = screen.get_cell(0, 2).unwrap();
        assert!(!third.attrs.contains(CellAttrs::BOLD));
        assert!(!third.attrs.has_underline());
        assert!(!third.attrs.contains(CellAttrs::INVERSE));
        assert!(third.attrs.contains(CellAttrs::BLINK));
        assert!(third.attrs.contains(CellAttrs::ITALIC));

        let fourth = screen.get_cell(0, 3).unwrap();
        assert!(fourth.attrs.contains(CellAttrs::BOLD));
        assert!(fourth.attrs.contains(CellAttrs::UNDERLINE));
        assert!(fourth.attrs.contains(CellAttrs::BLINK));
        assert!(fourth.attrs.contains(CellAttrs::INVERSE));
        assert!(fourth.attrs.contains(CellAttrs::ITALIC));
    }

    #[test]
    fn test_dec_rectangular_coordinates_respect_origin_mode() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[3;5r\x1b[?6h\x1b[90;1;1;1;2$x");

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), " ");
        assert_eq!(screen.get_cell(2, 0).unwrap().text(), "Z");
        assert_eq!(screen.get_cell(2, 1).unwrap().text(), "Z");
        assert_eq!(screen.get_cell(3, 0).unwrap().text(), " ");
    }

    #[test]
    fn test_osc_1337_streaming_multi_byte() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        let prefix = b"\x1b]1337;File=inline=0;size=4:";
        let data = b"AQAAAA==";
        parser.parse(&mut screen, prefix);

        for &byte in data {
            parser.parse(&mut screen, &[byte]);
        }

        parser.parse(&mut screen, b"\x07ordinary text");

        assert_eq!(
            screen.grid().row(0).unwrap().text().trim_end(),
            "ordinary text"
        );
        let transfers = screen.take_file_transfers();
        let [crate::screen::FileTransferOperation::StreamingFileReceived { result, .. }] =
            transfers.as_slice()
        else {
            panic!("expected one streaming file transfer, got {transfers:?}");
        };
        assert_eq!(result.data.to_bytes().unwrap(), [1, 0, 0, 0]);
    }

    #[test]
    fn test_osc_1337_cancel_and_malformed_payload_recover() {
        for abort in [0x18, 0x1a] {
            let mut screen = make_screen();
            let mut parser = Parser::new();
            let mut input = b"\x1b]1337;File=inline=0;size=4:AQ".to_vec();
            input.extend_from_slice(&[abort]);
            input.extend_from_slice(b"OK");

            parser.parse(&mut screen, &input);

            assert_eq!(screen.grid().row(0).unwrap().text().trim_end(), "OK");
            assert!(!screen.has_file_transfers());
        }

        let mut screen = make_screen();
        let mut parser = Parser::new();
        parser.parse(
            &mut screen,
            b"\x1b]1337;File=inline=0;size=4:!!!!\x07recovered",
        );
        assert_eq!(screen.grid().row(0).unwrap().text().trim_end(), "recovered");
        assert!(!screen.has_file_transfers());
    }

    #[test]
    fn test_alternate_screen() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"Primary");
        parser.parse(&mut screen, b"\x1b[?1049h"); // Enter alternate
        assert!(screen.modes.alternate_screen);

        parser.parse(&mut screen, b"\x1b[?1049l"); // Exit alternate
        assert!(!screen.modes.alternate_screen);
        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "P");
    }

    #[test]
    fn test_mouse_modes() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        assert_eq!(screen.modes.mouse_mode, MouseMode::None);
        parser.parse(&mut screen, b"\x1b[?1000h"); // normal tracking
        assert_eq!(screen.modes.mouse_mode, MouseMode::Normal);
        parser.parse(&mut screen, b"\x1b[?1002h"); // button-event tracking
        assert_eq!(screen.modes.mouse_mode, MouseMode::ButtonEvent);
        parser.parse(&mut screen, b"\x1b[?1000l"); // inactive reset is ignored
        assert_eq!(screen.modes.mouse_mode, MouseMode::ButtonEvent);
        parser.parse(&mut screen, b"\x1b[?1006h"); // SGR encoding
        assert_eq!(screen.modes.mouse_encoding, MouseEncoding::Sgr);
        parser.parse(&mut screen, b"\x1b[?1015h"); // URXVT replaces SGR
        assert_eq!(screen.modes.mouse_encoding, MouseEncoding::Urxvt);
        parser.parse(&mut screen, b"\x1b[?1006l"); // inactive reset is ignored
        assert_eq!(screen.modes.mouse_encoding, MouseEncoding::Urxvt);
        parser.parse(&mut screen, b"\x1b[?1015l");
        assert_eq!(screen.modes.mouse_encoding, MouseEncoding::Normal);
        parser.parse(&mut screen, b"\x1b[?1002l"); // disable active tracking
        assert_eq!(screen.modes.mouse_mode, MouseMode::None);
    }

    #[test]
    fn test_alternate_scroll_mode() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // Enabled by default so pagers scroll out of the box.
        assert!(screen.modes.alternate_scroll);
        parser.parse(&mut screen, b"\x1b[?1007l"); // disable
        assert!(!screen.modes.alternate_scroll);
        parser.parse(&mut screen, b"\x1b[?1007h"); // re-enable
        assert!(screen.modes.alternate_scroll);
    }

    #[test]
    fn test_dec_private_mode_queries_track_state() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[?7$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?7;1$y"]);

        parser.parse(&mut screen, b"\x1b[?7l\x1b[?7$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?7;2$y"]);

        parser.parse(&mut screen, b"\x1b[?5h\x1b[?5$p");
        assert!(screen.modes.reverse_video);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?5;1$y"]);

        parser.parse(&mut screen, b"\x1b[?5l\x1b[?5$p");
        assert!(!screen.modes.reverse_video);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?5;2$y"]);

        parser.parse(&mut screen, b"\x1b[?45$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?45;1$y"]);

        parser.parse(&mut screen, b"\x1b[?45l\x1b[?45$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?45;2$y"]);

        parser.parse(&mut screen, b"\x1b[?1005$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?1005;4$y"]);

        parser.parse(&mut screen, b"\x1b[?9$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?9;4$y"]);

        parser.parse(
            &mut screen,
            b"\x1b[?1015h\x1b[?1006$p\x1b[?1015$p\x1b[?1016$p",
        );
        assert_eq!(
            screen.take_pending_responses(),
            vec![
                b"\x1b[?1006;2$y".to_vec(),
                b"\x1b[?1015;1$y".to_vec(),
                b"\x1b[?1016;2$y".to_vec(),
            ]
        );

        parser.parse(&mut screen, b"\x1b[?2026$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?2026;2$y"]);
    }

    #[test]
    fn test_xtsave_and_xtrestore_preserve_private_modes_and_cursor() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[?1h\x1b[?45l\x1b[?2004h\x1b[?1;45;2004s");
        parser.parse(&mut screen, b"\x1b[?1l\x1b[?45h\x1b[?2004l\x1b[?1;45;2004r");
        assert!(screen.modes.application_cursor);
        assert!(!screen.modes.reverse_wrap);
        assert!(screen.modes.bracketed_paste);

        parser.parse(&mut screen, b"\x1b[?1016h\x1b[?1016s\x1b[?1006h\x1b[?1016r");
        assert_eq!(screen.modes.mouse_encoding, MouseEncoding::SgrPixels);

        parser.parse(&mut screen, b"\x1b[8;12H\x1b[?1048s\x1b[1;1H\x1b[?1048r");
        assert_eq!((screen.cursor.row, screen.cursor.col), (7, 11));
    }

    #[test]
    fn test_xtgettcap_reports_only_supported_capabilities() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1bP+q544E;436F;524742;636F;6C69;616D;5858\x1b\\",
        );

        assert_eq!(
            screen.take_pending_responses(),
            vec![concat!(
                "\x1bP1+r544E=637465726D\x1b\\",
                "\x1bP1+r436F=323536\x1b\\",
                "\x1bP1+r524742=382F382F38\x1b\\",
                "\x1bP1+r636F=3830\x1b\\",
                "\x1bP1+r6C69=3234\x1b\\",
                "\x1bP1+r616D\x1b\\",
                "\x1bP0+r5858\x1b\\",
            )
            .as_bytes()]
        );
    }

    #[test]
    fn test_xtgettcap_empty_and_malformed_queries_are_bounded() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1bP+q\x1b\\");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1bP0+r\x1b\\"]);

        parser.parse(&mut screen, b"\x1bP+qnot-hex\x1b\\");
        assert!(screen.take_pending_responses().is_empty());
    }

    #[test]
    fn test_backspace_reverse_wrap_matches_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[2;1H\x08");
        assert_eq!((screen.cursor.row, screen.cursor.col), (0, 79));

        parser.parse(&mut screen, b"\x1b[?45l\x1b[2;1H\x08");
        assert_eq!((screen.cursor.row, screen.cursor.col), (1, 0));

        parser.parse(&mut screen, b"\x1b[?45h\x1b[?7l\x1b[2;1H\x08");
        assert_eq!((screen.cursor.row, screen.cursor.col), (1, 0));

        parser.parse(&mut screen, b"\x1b[?7h\x1b[2;24r\x1b[2;1H\x08");
        assert_eq!((screen.cursor.row, screen.cursor.col), (1, 0));
    }

    #[test]
    fn test_ansi_mode_queries_track_state() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[4$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[4;2$y"]);

        parser.parse(&mut screen, b"\x1b[4h\x1b[4$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[4;1$y"]);

        parser.parse(&mut screen, b"\x1b[9999$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[9999;0$y"]);
    }

    #[test]
    fn test_legacy_alternate_screen_and_numeric_keypad_modes() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(&mut screen, b"\x1b[?47h\x1b[?47$p");
        assert!(screen.modes.alternate_screen);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?47;1$y"]);

        parser.parse(&mut screen, b"\x1b[?47l\x1b[?66h\x1b[?66$p");
        assert!(!screen.modes.alternate_screen);
        assert!(screen.modes.application_keypad);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?66;1$y"]);
    }

    #[test]
    fn test_dec_special_graphics_maps_g0_and_g1_like_foot() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b(0ABCabcdefghijklmnopqrstuvwxyzDEF\r\n\x1b(Bhello",
        );
        assert_eq!(
            screen.grid().row(0).unwrap().text().trim_end(),
            "ABC▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥DEF"
        );
        assert_eq!(screen.grid().row(1).unwrap().text().trim_end(), "hello");

        let mut screen = make_screen();
        parser.parse(&mut screen, b"\x1b)0\x0eSO-lqk\x0f-SI");
        assert_eq!(screen.grid().row(0).unwrap().text().trim_end(), "SO-┌─┐-SI");
    }

    #[test]
    fn test_sixel_cursor_position_matches_mode_8452() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        // Draw a one-cell sixel from column 5.  The DEC default leaves the
        // cursor at the image's left edge, not at column zero.
        parser.parse(&mut screen, b"\x1b[6G\x1bPq~\x1b\\");
        assert_eq!(screen.cursor.col, 5);

        parser.parse(&mut screen, b"\x1b[?8452h\x1b[6G\x1bPq~\x1b\\");
        assert_eq!(screen.cursor.col, 6);

        parser.parse(&mut screen, b"\x1b[?8452$p");
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?8452;1$y"]);
    }

    #[test]
    fn test_sixel_private_palette_mode_is_queryable_and_restorable() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b[?1070$p\x1b[?1070s\x1b[?1070l\x1b[?1070$p",
        );
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b[?1070;1$y", b"\x1b[?1070;2$y"]
        );
        assert!(!screen.modes.sixel_private_palette);

        parser.parse(&mut screen, b"\x1b[?1070r\x1b[?1070$p");
        assert!(screen.modes.sixel_private_palette);
        assert_eq!(screen.take_pending_responses(), vec![b"\x1b[?1070;1$y"]);
    }

    #[test]
    fn test_sixel_palette_can_be_shared_between_images() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"\x1b[?1070l\x1bP7;1q#42;2;100;0;0~\x1b\\\x1bP7;1q#42~\x1b\\",
        );

        let images = screen.images();
        assert_eq!(images.len(), 2);
        assert_eq!(&images[0].data[..4], &[255, 0, 0, 255]);
        assert_eq!(&images[1].data[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn test_sixel_resource_management_matches_foot_replies() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            concat!(
                "\x1b[?1;1S",
                "\x1b[?1;3;64S",
                "\x1b[?1;4S",
                "\x1b[?2;1S",
                "\x1b[?2;3;320;200S",
                "\x1b[?2;4S",
            )
            .as_bytes(),
        );

        assert_eq!(
            screen.take_pending_responses(),
            vec![
                b"\x1b[?1;0;1024S".to_vec(),
                b"\x1b[?1;0;64S".to_vec(),
                b"\x1b[?1;0;1024S".to_vec(),
                b"\x1b[?2;0;640;384S".to_vec(),
                b"\x1b[?2;0;320;200S".to_vec(),
                b"\x1b[?2;0;320;200S".to_vec(),
            ]
        );
    }

    #[test]
    fn kitty_graphics_apc_is_captured_alongside_plain_vte_input() {
        let mut screen = make_screen();
        let mut parser = Parser::new();

        parser.parse(
            &mut screen,
            b"a\x1b_Ga=T,f=32,s=1,v=1,i=7,C=1;/wAA/w==\x1b\\b",
        );

        assert_eq!(screen.images().len(), 1);
        assert_eq!(&screen.images()[0].data[..], &[255, 0, 0, 255]);
        assert_eq!(screen.grid().row(0).unwrap().text(), "ab");
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b_Gi=7;OK\x1b\\".to_vec()]
        );
    }
}
