//! Bounded Kitty graphics protocol ingestion and placement.
//!
//! The APC interceptor, command vocabulary, chunk handling, and error model are
//! adapted from Zellij commit e839bfffa586992364309a685b2c71f3b23c247e
//! (`zellij-server/src/panes/kitty_graphics`, MIT). Storage and rendering are
//! integrated with cterm's existing cross-platform RGBA image pipeline.

use std::collections::HashMap;
use std::io::{Read, Seek};
use std::path::Path;
use std::sync::Arc;

use base64::alphabet::STANDARD as BASE64_ALPHABET;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, Engine as _};
use image::{imageops, Pixel, Rgba, RgbaImage};

use crate::image_decode::decode_image;
use crate::kitty_placeholder::{scan_cells, PlaceholderRun};
use crate::screen::{DecodedRgbaImage, Screen, TerminalImage};

const BASE64_DECODER: GeneralPurpose = GeneralPurpose::new(
    &BASE64_ALPHABET,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const MAX_APC_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENCODED_UPLOAD_BYTES: usize = 90 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 64 * 1024 * 1024;
const MAX_DIMENSION: u32 = 10_000;
const MAX_SHARED_MEMORY_NAME_BYTES: usize = 2 * 1024;
const STORE_QUOTA_BYTES: usize = 320 * 1024 * 1024;
const USAGE_HINT_TRANSIENT: u32 = 1;
const PARENT_DEPTH_LIMIT: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterceptorState {
    Ground,
    Escape,
    EscapeUnderscore,
    Capturing,
    CapturingEscape,
}

#[derive(Debug)]
struct KittyApcInterceptor {
    state: InterceptorState,
    buffer: Vec<u8>,
    overflowed: bool,
}

pub(crate) enum ForwardBytes {
    One([u8; 1]),
    Two([u8; 2]),
    Three([u8; 3]),
}

impl ForwardBytes {
    pub(crate) fn as_slice(&self) -> &[u8] {
        match self {
            Self::One(bytes) => bytes,
            Self::Two(bytes) => bytes,
            Self::Three(bytes) => bytes,
        }
    }
}

pub(crate) enum InterceptorResult {
    Forward(ForwardBytes),
    Swallow,
    Captured(Vec<u8>),
}

impl KittyApcInterceptor {
    fn new() -> Self {
        Self {
            state: InterceptorState::Ground,
            buffer: Vec::new(),
            overflowed: false,
        }
    }

    fn advance(&mut self, byte: u8) -> InterceptorResult {
        match self.state {
            InterceptorState::Ground => match byte {
                0x1b => {
                    self.state = InterceptorState::Escape;
                    InterceptorResult::Swallow
                }
                byte => InterceptorResult::Forward(ForwardBytes::One([byte])),
            },
            InterceptorState::Escape => match byte {
                b'_' => {
                    self.state = InterceptorState::EscapeUnderscore;
                    InterceptorResult::Swallow
                }
                0x1b => InterceptorResult::Forward(ForwardBytes::One([0x1b])),
                byte => {
                    self.state = InterceptorState::Ground;
                    InterceptorResult::Forward(ForwardBytes::Two([0x1b, byte]))
                }
            },
            InterceptorState::EscapeUnderscore => match byte {
                b'G' => {
                    self.state = InterceptorState::Capturing;
                    self.buffer.clear();
                    self.overflowed = false;
                    InterceptorResult::Swallow
                }
                0x1b => {
                    self.state = InterceptorState::Escape;
                    InterceptorResult::Forward(ForwardBytes::Two([0x1b, b'_']))
                }
                byte => {
                    self.state = InterceptorState::Ground;
                    InterceptorResult::Forward(ForwardBytes::Three([0x1b, b'_', byte]))
                }
            },
            InterceptorState::Capturing => self.advance_capture(byte),
            InterceptorState::CapturingEscape => match byte {
                b'\\' => self.finish_capture(),
                byte => {
                    self.push_capture(0x1b);
                    self.state = InterceptorState::Capturing;
                    self.advance_capture(byte)
                }
            },
        }
    }

    fn advance_capture(&mut self, byte: u8) -> InterceptorResult {
        match byte {
            0x1b => {
                self.state = InterceptorState::CapturingEscape;
                InterceptorResult::Swallow
            }
            0x9c => self.finish_capture(),
            0x18 | 0x1a => {
                self.state = InterceptorState::Ground;
                self.buffer.clear();
                self.overflowed = false;
                InterceptorResult::Forward(ForwardBytes::One([byte]))
            }
            byte => {
                self.push_capture(byte);
                InterceptorResult::Swallow
            }
        }
    }

    fn push_capture(&mut self, byte: u8) {
        if self.overflowed {
            return;
        }
        if self.buffer.len() == MAX_APC_BYTES {
            self.buffer.clear();
            self.overflowed = true;
        } else {
            self.buffer.push(byte);
        }
    }

    fn finish_capture(&mut self) -> InterceptorResult {
        self.state = InterceptorState::Ground;
        if self.overflowed {
            self.buffer.clear();
            self.overflowed = false;
            InterceptorResult::Swallow
        } else {
            InterceptorResult::Captured(std::mem::take(&mut self.buffer))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Transmit,
    TransmitAndDisplay,
    Display,
    Delete,
    Query,
    TransmitFrame,
    ControlAnimation,
    ComposeFrame,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameComposition {
    AlphaBlend,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnimationState {
    Stopped,
    Loading,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Rgb24,
    Rgba32,
    Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorCode {
    Invalid,
    NotFound,
    BadFile,
    NoData,
    NoSpace,
    NoParent,
    Cycle,
    TooDeep,
    #[cfg(not(any(
        target_os = "freebsd",
        target_os = "linux",
        target_os = "macos",
        windows
    )))]
    NotSupported,
}

impl ErrorCode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Invalid => "EINVAL",
            Self::NotFound => "ENOENT",
            Self::BadFile => "EBADF",
            Self::NoData => "ENODATA",
            Self::NoSpace => "ENOSPC",
            Self::NoParent => "ENOPARENT",
            Self::Cycle => "ECYCLE",
            Self::TooDeep => "ETOODEEP",
            #[cfg(not(any(
                target_os = "freebsd",
                target_os = "linux",
                target_os = "macos",
                windows
            )))]
            Self::NotSupported => "ENOTSUPPORTED",
        }
    }
}

#[derive(Debug, Clone)]
struct ProtocolError {
    code: ErrorCode,
    message: &'static str,
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    quiet: u8,
}

#[derive(Debug, Clone, Copy, Default)]
struct EchoFields {
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    quiet: u8,
}

impl EchoFields {
    fn error(self, code: ErrorCode, message: &'static str) -> ProtocolError {
        ProtocolError {
            code,
            message,
            image_id: self.image_id,
            image_number: self.image_number,
            placement_id: self.placement_id,
            quiet: self.quiet,
        }
    }
}

#[derive(Debug, Clone)]
struct DecodedImage {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone)]
struct Command {
    action: Action,
    format: Format,
    medium: Medium,
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    pixel_width: Option<u32>,
    pixel_height: Option<u32>,
    compressed: bool,
    more_chunks: bool,
    data_size: Option<u32>,
    data_offset: Option<u32>,
    source_x: u32,
    source_y: u32,
    source_w: u32,
    source_h: u32,
    columns: u32,
    rows: u32,
    cell_offset_x: u32,
    cell_offset_y: u32,
    z_index: i32,
    quiet: u8,
    suppress_cursor_movement: bool,
    unicode_placeholder: bool,
    usage_hints: u32,
    parent_image_id: u32,
    parent_placement_id: u32,
    horizontal_offset: i32,
    vertical_offset: i32,
    delete_specifier: Option<u8>,
    frame_number: u32,
    other_frame_number: u32,
    frame_gap: Option<i32>,
    frame_composition: FrameComposition,
    frame_background: u32,
    frame_source_x: u32,
    frame_source_y: u32,
    animation_state: Option<AnimationState>,
    loop_count: Option<u32>,
    image: Option<DecodedImage>,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            action: Action::Transmit,
            format: Format::Rgba32,
            medium: Medium::Direct,
            image_id: None,
            image_number: None,
            placement_id: None,
            pixel_width: None,
            pixel_height: None,
            compressed: false,
            more_chunks: false,
            data_size: None,
            data_offset: None,
            source_x: 0,
            source_y: 0,
            source_w: 0,
            source_h: 0,
            columns: 0,
            rows: 0,
            cell_offset_x: 0,
            cell_offset_y: 0,
            z_index: 0,
            quiet: 0,
            suppress_cursor_movement: false,
            unicode_placeholder: false,
            usage_hints: 0,
            parent_image_id: 0,
            parent_placement_id: 0,
            horizontal_offset: 0,
            vertical_offset: 0,
            delete_specifier: None,
            frame_number: 0,
            other_frame_number: 0,
            frame_gap: None,
            frame_composition: FrameComposition::AlphaBlend,
            frame_background: 0,
            frame_source_x: 0,
            frame_source_y: 0,
            animation_state: None,
            loop_count: None,
            image: None,
        }
    }
}

impl Command {
    fn echo(&self) -> EchoFields {
        EchoFields {
            image_id: self.image_id,
            image_number: self.image_number,
            placement_id: self.placement_id,
            quiet: self.quiet,
        }
    }

    fn is_transient(&self) -> bool {
        self.usage_hints & USAGE_HINT_TRANSIENT != 0
    }
}

#[derive(Debug, Clone)]
struct PendingUpload {
    command: Command,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
struct CommandParser {
    pending: Option<PendingUpload>,
}

impl CommandParser {
    fn parse(&mut self, raw: &[u8]) -> Option<Result<Command, ProtocolError>> {
        let (control, payload) = split_control_payload(raw);
        if let Some(mut pending) = self.pending.take() {
            let pairs = match tokenize(control) {
                Ok(pairs) => pairs,
                Err(()) => {
                    return Some(Err(pending
                        .command
                        .echo()
                        .error(ErrorCode::Invalid, "malformed control data")))
                }
            };
            if pairs
                .iter()
                .any(|(key, value)| *key == b"a" && *value == b"d")
            {
                return Some(parse_control_data(control));
            }
            let mut more = false;
            for (key, value) in pairs {
                match key {
                    b"m" => match parse_u32(value) {
                        Some(0) => more = false,
                        Some(1) => more = true,
                        _ => {
                            return Some(Err(pending
                                .command
                                .echo()
                                .error(ErrorCode::Invalid, "invalid chunk marker")))
                        }
                    },
                    b"q" => match parse_u32(value) {
                        Some(value @ 0..=2) => pending.command.quiet = value as u8,
                        _ => {
                            return Some(Err(pending
                                .command
                                .echo()
                                .error(ErrorCode::Invalid, "invalid quiet level")))
                        }
                    },
                    b"a" if value == b"f" => {}
                    _ => {}
                }
            }
            if pending.payload.len().saturating_add(payload.len()) > MAX_ENCODED_UPLOAD_BYTES {
                return Some(Err(pending
                    .command
                    .echo()
                    .error(ErrorCode::NoSpace, "encoded image data too large")));
            }
            pending.payload.extend_from_slice(payload);
            if more {
                self.pending = Some(pending);
                return None;
            }
            pending.command.more_chunks = false;
            return Some(decode_command_payload(pending.command, &pending.payload));
        }

        let command = match parse_control_data(control) {
            Ok(command) => command,
            Err(error) => return Some(Err(error)),
        };
        if matches!(
            command.action,
            Action::Delete | Action::Display | Action::ControlAnimation | Action::ComposeFrame
        ) {
            return Some(Ok(command));
        }
        if command.more_chunks {
            self.pending = Some(PendingUpload {
                command,
                payload: payload.to_vec(),
            });
            None
        } else {
            Some(decode_command_payload(command, payload))
        }
    }
}

fn split_control_payload(raw: &[u8]) -> (&[u8], &[u8]) {
    raw.iter()
        .position(|byte| *byte == b';')
        .map_or((raw, &raw[raw.len()..]), |position| {
            (&raw[..position], &raw[position + 1..])
        })
}

type ControlPair<'a> = (&'a [u8], &'a [u8]);

fn tokenize(control: &[u8]) -> Result<Vec<ControlPair<'_>>, ()> {
    control
        .split(|byte| *byte == b',')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let equals = segment.iter().position(|byte| *byte == b'=').ok_or(())?;
            (equals > 0)
                .then_some((&segment[..equals], &segment[equals + 1..]))
                .ok_or(())
        })
        .collect()
}

fn parse_u32(value: &[u8]) -> Option<u32> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn parse_i32(value: &[u8]) -> Option<i32> {
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn scan_echo_fields(control: &[u8]) -> EchoFields {
    let mut echo = EchoFields::default();
    for segment in control.split(|byte| *byte == b',') {
        let Some(equals) = segment.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, value) = (&segment[..equals], &segment[equals + 1..]);
        match key {
            b"i" => echo.image_id = parse_u32(value),
            b"I" => echo.image_number = parse_u32(value),
            b"p" => echo.placement_id = parse_u32(value),
            b"q" => echo.quiet = parse_u32(value).filter(|q| *q <= 2).unwrap_or(0) as u8,
            _ => {}
        }
    }
    echo
}

fn parse_control_data(control: &[u8]) -> Result<Command, ProtocolError> {
    let echo = scan_echo_fields(control);
    let mut command = Command::default();
    let pairs =
        tokenize(control).map_err(|()| echo.error(ErrorCode::Invalid, "malformed control data"))?;
    for (key, value) in &pairs {
        if *key != b"a" {
            continue;
        }
        command.action = match *value {
            b"t" => Action::Transmit,
            b"T" => Action::TransmitAndDisplay,
            b"p" => Action::Display,
            b"d" => Action::Delete,
            b"q" => Action::Query,
            b"f" => Action::TransmitFrame,
            b"a" => Action::ControlAnimation,
            b"c" => Action::ComposeFrame,
            _ => return Err(echo.error(ErrorCode::Invalid, "invalid action")),
        };
    }
    for (key, value) in pairs {
        match key {
            b"a" => {}
            b"f" => {
                command.format = match parse_u32(value) {
                    Some(24) => Format::Rgb24,
                    Some(32) => Format::Rgba32,
                    Some(100) => Format::Png,
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid format")),
                }
            }
            b"t" => {
                command.medium = match value {
                    b"d" => Medium::Direct,
                    b"f" => Medium::File,
                    b"t" => Medium::TempFile,
                    b"s" => Medium::SharedMemory,
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid medium")),
                }
            }
            b"s" if command.action == Action::ControlAnimation => {
                command.animation_state =
                    match required_u32(value, echo, "invalid animation state")? {
                        0 => None,
                        1 => Some(AnimationState::Stopped),
                        2 => Some(AnimationState::Loading),
                        3 => Some(AnimationState::Running),
                        _ => return Err(echo.error(ErrorCode::Invalid, "invalid animation state")),
                    };
            }
            b"s" => command.pixel_width = Some(required_u32(value, echo, "invalid width")?),
            b"v" if command.action == Action::ControlAnimation => {
                command.loop_count = Some(required_u32(value, echo, "invalid loop count")?);
            }
            b"v" => command.pixel_height = Some(required_u32(value, echo, "invalid height")?),
            b"S" => command.data_size = Some(required_u32(value, echo, "invalid data size")?),
            b"O" => command.data_offset = Some(required_u32(value, echo, "invalid offset")?),
            b"i" => command.image_id = Some(required_u32(value, echo, "invalid image id")?),
            b"I" => command.image_number = Some(required_u32(value, echo, "invalid image number")?),
            b"p" => command.placement_id = Some(required_u32(value, echo, "invalid placement id")?),
            b"x" => command.source_x = required_u32(value, echo, "invalid source x")?,
            b"y" => command.source_y = required_u32(value, echo, "invalid source y")?,
            b"w" => command.source_w = required_u32(value, echo, "invalid source width")?,
            b"h" => command.source_h = required_u32(value, echo, "invalid source height")?,
            b"c" if matches!(
                command.action,
                Action::TransmitFrame | Action::ControlAnimation | Action::ComposeFrame
            ) =>
            {
                command.other_frame_number =
                    required_u32(value, echo, "invalid other frame number")?;
            }
            b"c" => command.columns = required_u32(value, echo, "invalid columns")?,
            b"r" if matches!(
                command.action,
                Action::TransmitFrame | Action::ControlAnimation | Action::ComposeFrame
            ) =>
            {
                command.frame_number = required_u32(value, echo, "invalid frame number")?;
            }
            b"r" => command.rows = required_u32(value, echo, "invalid rows")?,
            b"X" if command.action == Action::TransmitFrame => {
                command.frame_composition = parse_frame_composition(value, echo)?;
            }
            b"X" if command.action == Action::ComposeFrame => {
                command.frame_source_x = required_u32(value, echo, "invalid frame source x")?;
            }
            b"X" => command.cell_offset_x = required_u32(value, echo, "invalid cell x offset")?,
            b"Y" if command.action == Action::TransmitFrame => {
                command.frame_background =
                    required_u32(value, echo, "invalid frame background color")?;
            }
            b"Y" if command.action == Action::ComposeFrame => {
                command.frame_source_y = required_u32(value, echo, "invalid frame source y")?;
            }
            b"Y" => command.cell_offset_y = required_u32(value, echo, "invalid cell y offset")?,
            b"z" if matches!(
                command.action,
                Action::TransmitFrame | Action::ControlAnimation
            ) =>
            {
                command.frame_gap = Some(
                    parse_i32(value)
                        .ok_or_else(|| echo.error(ErrorCode::Invalid, "invalid frame gap"))?,
                );
            }
            b"z" => {
                command.z_index = parse_i32(value)
                    .ok_or_else(|| echo.error(ErrorCode::Invalid, "invalid z index"))?;
            }
            b"o" if value == b"z" => command.compressed = true,
            b"o" => return Err(echo.error(ErrorCode::Invalid, "invalid compression")),
            b"m" => {
                command.more_chunks = match parse_u32(value) {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid chunk marker")),
                }
            }
            b"q" => {
                command.quiet = match parse_u32(value) {
                    Some(value @ 0..=2) => value as u8,
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid quiet level")),
                }
            }
            b"C" if command.action == Action::ComposeFrame => {
                command.frame_composition = parse_frame_composition(value, echo)?;
            }
            b"C" => {
                command.suppress_cursor_movement = match parse_u32(value) {
                    Some(0) => false,
                    Some(1) => true,
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid cursor flag")),
                }
            }
            b"d" if value.len() == 1 && b"aAiInNcCpPqQxXyYzZrR".contains(&value[0]) => {
                command.delete_specifier = Some(value[0]);
            }
            b"d" => return Err(echo.error(ErrorCode::Invalid, "invalid delete specifier")),
            b"U" => {
                command.unicode_placeholder = match parse_u32(value) {
                    Some(0) => false,
                    Some(1) => true,
                    _ => {
                        return Err(
                            echo.error(ErrorCode::Invalid, "invalid unicode placeholder flag")
                        )
                    }
                }
            }
            b"N" => command.usage_hints = required_u32(value, echo, "invalid usage hints")?,
            b"P" => command.parent_image_id = required_u32(value, echo, "invalid parent image id")?,
            b"Q" => {
                command.parent_placement_id =
                    required_u32(value, echo, "invalid parent placement id")?
            }
            b"H" => {
                command.horizontal_offset = parse_i32(value)
                    .ok_or_else(|| echo.error(ErrorCode::Invalid, "invalid horizontal offset"))?
            }
            b"V" => {
                command.vertical_offset = parse_i32(value)
                    .ok_or_else(|| echo.error(ErrorCode::Invalid, "invalid vertical offset"))?
            }
            _ => {}
        }
    }
    if command.image_id.unwrap_or(0) != 0 && command.image_number.is_some() {
        return Err(echo.error(
            ErrorCode::Invalid,
            "image id and number are mutually exclusive",
        ));
    }
    Ok(command)
}

fn parse_frame_composition(
    value: &[u8],
    echo: EchoFields,
) -> Result<FrameComposition, ProtocolError> {
    match parse_u32(value) {
        Some(0) => Ok(FrameComposition::AlphaBlend),
        Some(1) => Ok(FrameComposition::Overwrite),
        _ => Err(echo.error(ErrorCode::Invalid, "invalid frame composition mode")),
    }
}

fn required_u32(
    value: &[u8],
    echo: EchoFields,
    message: &'static str,
) -> Result<u32, ProtocolError> {
    parse_u32(value).ok_or_else(|| echo.error(ErrorCode::Invalid, message))
}

fn decode_command_payload(mut command: Command, encoded: &[u8]) -> Result<Command, ProtocolError> {
    let echo = command.echo();
    if encoded.len() > MAX_ENCODED_UPLOAD_BYTES {
        return Err(echo.error(ErrorCode::NoSpace, "encoded image data too large"));
    }
    let decoded = BASE64_DECODER
        .decode(encoded)
        .map_err(|_| echo.error(ErrorCode::Invalid, "invalid base64 payload"))?;
    let data = match command.medium {
        Medium::Direct => decoded,
        Medium::File | Medium::TempFile => read_file_payload(&command, decoded, echo)?,
        Medium::SharedMemory => read_shared_memory_payload(&command, decoded, echo)?,
    };
    let data = if command.compressed {
        let mut inflated = Vec::new();
        flate2::read::ZlibDecoder::new(data.as_slice())
            .take((MAX_DECODED_BYTES + 1) as u64)
            .read_to_end(&mut inflated)
            .map_err(|_| echo.error(ErrorCode::Invalid, "could not inflate payload"))?;
        if inflated.len() > MAX_DECODED_BYTES {
            return Err(echo.error(ErrorCode::NoSpace, "image data too large"));
        }
        inflated
    } else {
        data
    };

    command.image = Some(match command.format {
        Format::Rgb24 | Format::Rgba32 => decode_raw(&command, data, echo)?,
        Format::Png => decode_png(data, echo)?,
    });
    Ok(command)
}

fn read_file_payload(
    command: &Command,
    path_bytes: Vec<u8>,
    echo: EchoFields,
) -> Result<Vec<u8>, ProtocolError> {
    let path = String::from_utf8(path_bytes)
        .map(std::path::PathBuf::from)
        .map_err(|_| echo.error(ErrorCode::BadFile, "invalid file path"))?;
    let metadata = std::fs::metadata(&path)
        .map_err(|_| echo.error(ErrorCode::BadFile, "could not read file"))?;
    if !metadata.is_file() {
        return Err(echo.error(ErrorCode::BadFile, "path is not a regular file"));
    }
    let start = u64::from(command.data_offset.unwrap_or(0));
    if start > metadata.len() {
        return Err(echo.error(ErrorCode::BadFile, "file offset is out of range"));
    }
    let available = metadata.len() - start;
    let length = command
        .data_size
        .filter(|size| *size > 0)
        .map_or(available, |size| available.min(u64::from(size)));
    if length > MAX_DECODED_BYTES as u64 {
        return Err(echo.error(ErrorCode::NoSpace, "image data too large"));
    }
    let read_result = (|| -> std::io::Result<Vec<u8>> {
        let mut file = std::fs::File::open(&path)?;
        file.seek(std::io::SeekFrom::Start(start))?;
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(length).read_to_end(&mut bytes)?;
        Ok(bytes)
    })();
    if command.medium == Medium::TempFile && safe_temporary_graphics_path(&path) {
        let _ = std::fs::remove_file(&path);
    }
    read_result.map_err(|_| echo.error(ErrorCode::BadFile, "could not read file"))
}

#[cfg(any(target_os = "freebsd", target_os = "linux", target_os = "macos"))]
fn read_shared_memory_payload(
    command: &Command,
    name_bytes: Vec<u8>,
    echo: EchoFields,
) -> Result<Vec<u8>, ProtocolError> {
    if name_bytes.is_empty() || name_bytes.len() > MAX_SHARED_MEMORY_NAME_BYTES {
        return Err(echo.error(ErrorCode::BadFile, "invalid shared memory name"));
    }
    let name = String::from_utf8(name_bytes)
        .map_err(|_| echo.error(ErrorCode::BadFile, "invalid shared memory name"))?;
    if !name.starts_with('/') || name[1..].contains('/') {
        return Err(echo.error(ErrorCode::BadFile, "invalid POSIX shared memory name"));
    }
    let descriptor = nix::sys::mman::shm_open(
        name.as_str(),
        nix::fcntl::OFlag::O_RDONLY,
        nix::sys::stat::Mode::empty(),
    )
    .map_err(|_| echo.error(ErrorCode::BadFile, "could not open shared memory"))?;
    // Ownership passes to the terminal as soon as open succeeds. The live
    // descriptor remains readable after unlink and closes through File RAII.
    let _ = nix::sys::mman::shm_unlink(name.as_str());
    let file = std::fs::File::from(descriptor);
    let mapped_size = file
        .metadata()
        .map_err(|_| echo.error(ErrorCode::BadFile, "could not inspect shared memory"))?
        .len();

    let start = u64::from(command.data_offset.unwrap_or(0));
    if start > mapped_size {
        return Err(echo.error(ErrorCode::BadFile, "shared memory offset is out of range"));
    }
    let available = mapped_size - start;
    let length = command
        .data_size
        .filter(|size| *size > 0)
        .map_or(available, |size| available.min(u64::from(size)));
    if length > MAX_DECODED_BYTES as u64 {
        return Err(echo.error(ErrorCode::NoSpace, "image data too large"));
    }
    if length == 0 {
        return Ok(Vec::new());
    }
    // SAFETY: the sender has finished writing before it sends the APC. This
    // process owns a read-only descriptor and snapshots the checked byte range
    // into Rust-owned memory before decoding or closing the mapping.
    let mut options = memmap2::MmapOptions::new();
    options.offset(start).len(length as usize);
    let mapping = unsafe { options.map(&file) }
        .map_err(|_| echo.error(ErrorCode::BadFile, "could not map shared memory"))?;
    Ok(mapping.to_vec())
}

#[cfg(windows)]
fn read_shared_memory_payload(
    command: &Command,
    name_bytes: Vec<u8>,
    echo: EchoFields,
) -> Result<Vec<u8>, ProtocolError> {
    if name_bytes.is_empty() || name_bytes.len() > MAX_SHARED_MEMORY_NAME_BYTES {
        return Err(echo.error(ErrorCode::BadFile, "invalid shared memory name"));
    }
    let name = String::from_utf8(name_bytes)
        .map_err(|_| echo.error(ErrorCode::BadFile, "invalid shared memory name"))?;
    let mapping = shared_memory::ShmemConf::new()
        .os_id(name)
        .allow_raw(true)
        .open()
        .map_err(|_| echo.error(ErrorCode::BadFile, "could not open shared memory"))?;
    let start = command.data_offset.unwrap_or(0) as usize;
    if start > mapping.len() {
        return Err(echo.error(ErrorCode::BadFile, "shared memory offset is out of range"));
    }
    let available = mapping.len() - start;
    let length = command
        .data_size
        .filter(|size| *size > 0)
        .map_or(available, |size| available.min(size as usize));
    if length > MAX_DECODED_BYTES {
        return Err(echo.error(ErrorCode::NoSpace, "image data too large"));
    }
    let mut bytes = vec![0; length];
    // SAFETY: the mapping owns a live range of `mapping.len()` bytes and the
    // checked source range cannot overlap the newly allocated destination.
    unsafe {
        std::ptr::copy_nonoverlapping(
            mapping.as_ptr().add(start).cast_const(),
            bytes.as_mut_ptr(),
            length,
        );
    }
    Ok(bytes)
}

#[cfg(not(any(
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    windows
)))]
fn read_shared_memory_payload(
    _command: &Command,
    _name_bytes: Vec<u8>,
    echo: EchoFields,
) -> Result<Vec<u8>, ProtocolError> {
    Err(echo.error(
        ErrorCode::NotSupported,
        "shared memory transfer is not supported on this platform",
    ))
}

fn safe_temporary_graphics_path(path: &Path) -> bool {
    let named_for_protocol = path.to_string_lossy().contains("tty-graphics-protocol");
    let in_temp = path.starts_with(std::env::temp_dir())
        || path.starts_with("/tmp")
        || path.starts_with("/dev/shm");
    named_for_protocol && in_temp
}

fn decode_raw(
    command: &Command,
    mut data: Vec<u8>,
    echo: EchoFields,
) -> Result<DecodedImage, ProtocolError> {
    let width = command.pixel_width.unwrap_or(0);
    let height = command.pixel_height.unwrap_or(0);
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(echo.error(ErrorCode::Invalid, "invalid raw image dimensions"));
    }
    let bytes_per_pixel = if command.format == Format::Rgb24 {
        3
    } else {
        4
    };
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "image data too large"))?;
    let expected = pixels
        .checked_mul(bytes_per_pixel)
        .filter(|bytes| *bytes <= MAX_DECODED_BYTES as u64)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "image data too large"))?;
    pixels
        .checked_mul(4)
        .filter(|bytes| *bytes <= MAX_DECODED_BYTES as u64)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "decoded image is too large"))?;
    if data.len() < expected as usize {
        return Err(echo.error(ErrorCode::NoData, "insufficient image data"));
    }
    data.truncate(expected as usize);
    let rgba = if command.format == Format::Rgb24 {
        let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
        for pixel in data.as_chunks::<3>().0 {
            rgba.extend_from_slice(pixel);
            rgba.push(255);
        }
        rgba
    } else {
        data
    };
    Ok(DecodedImage {
        rgba,
        width,
        height,
    })
}

fn decode_png(data: Vec<u8>, echo: EchoFields) -> Result<DecodedImage, ProtocolError> {
    if !data.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Err(echo.error(ErrorCode::Invalid, "invalid PNG data"));
    }
    let decoded =
        decode_image(&data).map_err(|_| echo.error(ErrorCode::Invalid, "invalid PNG data"))?;
    if decoded.width == 0
        || decoded.height == 0
        || decoded.width > MAX_DIMENSION as usize
        || decoded.height > MAX_DIMENSION as usize
        || decoded.data.len() > MAX_DECODED_BYTES
    {
        return Err(echo.error(ErrorCode::NoSpace, "PNG image too large"));
    }
    Ok(DecodedImage {
        rgba: decoded.data,
        width: decoded.width as u32,
        height: decoded.height as u32,
    })
}

#[derive(Debug, Clone)]
struct AnimationFrame {
    rgba: Arc<Vec<u8>>,
    gap_ms: u32,
    transient: bool,
}

#[derive(Debug, Clone)]
struct StoredImage {
    rgba: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    frames: Vec<AnimationFrame>,
    current_frame: usize,
    animation_state: AnimationState,
    max_loops: u32,
    current_loop: u32,
    shown_at_ms: Option<u64>,
    lru: u64,
}

impl StoredImage {
    fn allocated_bytes(&self) -> usize {
        self.frames.iter().map(|frame| frame.rgba.len()).sum()
    }

    fn refresh_current(&mut self) {
        self.current_frame = self.current_frame.min(self.frames.len().saturating_sub(1));
        self.rgba = Arc::clone(&self.frames[self.current_frame].rgba);
    }

    fn animation_duration(&self) -> u64 {
        self.frames
            .iter()
            .map(|frame| u64::from(frame.gap_ms))
            .sum()
    }

    fn is_transient(&self) -> bool {
        self.frames.first().is_some_and(|frame| frame.transient)
    }
}

#[derive(Debug)]
struct PlacementRecord {
    image_id: u32,
    placement_id: u32,
    z_index: i32,
    allocated_bytes: usize,
    command: Command,
}

#[derive(Debug)]
struct VirtualPlacementRecord {
    id: u64,
    image_id: u32,
    placement_id: u32,
    command: Command,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlacementParent {
    Real(u64),
    Virtual(u64),
    Relative(u64),
}

#[derive(Debug)]
struct RelativePlacementRecord {
    id: u64,
    image_id: u32,
    placement_id: u32,
    parent: PlacementParent,
    command: Command,
    screen_id: Option<u64>,
    allocated_bytes: usize,
}

#[derive(Debug)]
struct PlaceholderFragment {
    screen_id: u64,
    virtual_id: u64,
    allocated_bytes: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaceholderProjection {
    absolute_line: usize,
    run: PlaceholderRun,
    virtual_id: u64,
}

/// Result of sampling Kitty image animations from a monotonic frontend clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GraphicsAnimationTick {
    /// At least one visible frame changed and the frontend should redraw.
    pub changed: bool,
    /// Absolute monotonic millisecond deadline for the next frame.
    pub next_wake_ms: Option<u64>,
}

#[derive(Debug)]
pub(crate) struct KittyGraphics {
    interceptor: KittyApcInterceptor,
    parser: CommandParser,
    images: HashMap<u32, StoredImage>,
    image_numbers: HashMap<u32, Vec<u32>>,
    /// Screen image ID to protocol placement metadata. The screen ID is unique
    /// even when the protocol placement ID is zero (anonymous and repeatable).
    placements: HashMap<u64, PlacementRecord>,
    /// Invisible `U=1` prototypes. Actual fragments are derived from text.
    virtual_placements: Vec<VirtualPlacementRecord>,
    relative_placements: Vec<RelativePlacementRecord>,
    /// Renderer images synthesized only for the current viewport.
    placeholder_fragments: Vec<PlaceholderFragment>,
    placeholder_projection: Vec<PlaceholderProjection>,
    placeholder_revision: u64,
    rendered_placeholder_revision: u64,
    next_virtual_id: u64,
    next_relative_id: u64,
    next_image_id: u32,
    next_lru: u64,
    total_bytes: usize,
    placement_bytes: usize,
}

impl Default for KittyGraphics {
    fn default() -> Self {
        Self {
            interceptor: KittyApcInterceptor::new(),
            parser: CommandParser::default(),
            images: HashMap::new(),
            image_numbers: HashMap::new(),
            placements: HashMap::new(),
            virtual_placements: Vec::new(),
            relative_placements: Vec::new(),
            placeholder_fragments: Vec::new(),
            placeholder_projection: Vec::new(),
            placeholder_revision: 1,
            rendered_placeholder_revision: 0,
            next_virtual_id: 1,
            next_relative_id: 1,
            next_image_id: 1,
            next_lru: 1,
            total_bytes: 0,
            placement_bytes: 0,
        }
    }
}

impl KittyGraphics {
    pub(crate) fn advance(&mut self, byte: u8) -> InterceptorResult {
        self.interceptor.advance(byte)
    }

    pub(crate) fn handle(&mut self, raw: &[u8], screen: &mut Screen) {
        let Some(result) = self.parser.parse(raw) else {
            return;
        };
        match result {
            Ok(command) => self.apply(command, screen),
            Err(error) => queue_error(screen, &error),
        }
    }

    fn apply(&mut self, mut command: Command, screen: &mut Screen) {
        match command.action {
            Action::Query => queue_success(screen, &command, command.image_id, true),
            Action::Transmit | Action::TransmitAndDisplay => {
                let image = command.image.take().expect("decoded transmit image");
                match self.store_image(&command, image, screen) {
                    Ok(image_id) => {
                        if command.action == Action::TransmitAndDisplay {
                            match self.place(&command, image_id, screen) {
                                Ok(()) => {
                                    queue_success(screen, &command, Some(image_id), false);
                                }
                                Err(error) => queue_error(screen, &error),
                            }
                        } else {
                            queue_success(screen, &command, Some(image_id), false);
                        }
                    }
                    Err(error) => queue_error(screen, &error),
                }
            }
            Action::Display => match self.resolve_image_id(&command) {
                Some(image_id) => {
                    if let Err(error) = self.place(&command, image_id, screen) {
                        queue_error(screen, &error);
                    } else {
                        queue_success(screen, &command, Some(image_id), false);
                    }
                }
                None => queue_error(
                    screen,
                    &command.echo().error(ErrorCode::NotFound, "image not found"),
                ),
            },
            Action::Delete => self.delete(&command, screen),
            Action::TransmitFrame => {
                let frame = command.image.take().expect("decoded animation frame");
                match self.store_frame(&command, frame, screen) {
                    Ok(image_id) => queue_success(screen, &command, Some(image_id), false),
                    Err(error) => queue_error(screen, &error),
                }
            }
            Action::ControlAnimation => {
                if let Err(error) = self.control_animation(&command, screen) {
                    queue_error(screen, &error);
                }
            }
            Action::ComposeFrame => match self.compose_frame(&command, screen) {
                Ok(image_id) => queue_success(screen, &command, Some(image_id), false),
                Err(error) => queue_error(screen, &error),
            },
        }
    }

    fn store_image(
        &mut self,
        command: &Command,
        image: DecodedImage,
        screen: &mut Screen,
    ) -> Result<u32, ProtocolError> {
        let size = image.rgba.len();
        if size > STORE_QUOTA_BYTES {
            return Err(command
                .echo()
                .error(ErrorCode::NoSpace, "image exceeds storage quota"));
        }
        let image_id = command
            .image_id
            .filter(|id| *id != 0)
            .unwrap_or_else(|| self.allocate_image_id());
        let replacement_id = self.images.contains_key(&image_id).then_some(image_id);
        self.evict_to_fit(size, replacement_id, screen);
        if self.projected_usage(size, replacement_id) > STORE_QUOTA_BYTES {
            return Err(command
                .echo()
                .error(ErrorCode::NoSpace, "image storage quota exhausted"));
        }
        if let Some(replacement_id) = replacement_id {
            self.remove_image_data(replacement_id, screen);
        }
        self.next_lru = self.next_lru.saturating_add(1);
        self.total_bytes += size;
        let rgba = Arc::new(image.rgba);
        self.images.insert(
            image_id,
            StoredImage {
                rgba: Arc::clone(&rgba),
                width: image.width,
                height: image.height,
                frames: vec![AnimationFrame {
                    rgba,
                    gap_ms: 0,
                    transient: command.is_transient(),
                }],
                current_frame: 0,
                animation_state: AnimationState::Stopped,
                max_loops: 0,
                current_loop: 0,
                shown_at_ms: None,
                lru: self.next_lru,
            },
        );
        if let Some(number) = command.image_number {
            self.image_numbers.entry(number).or_default().push(image_id);
        }
        Ok(image_id)
    }

    fn store_frame(
        &mut self,
        command: &Command,
        frame_data: DecodedImage,
        screen: &mut Screen,
    ) -> Result<u32, ProtocolError> {
        let image_id = self.resolve_animation_image_id(command)?;
        let image = self.images.get(&image_id).expect("resolved image exists");
        let fits = command
            .source_x
            .checked_add(frame_data.width)
            .zip(command.source_y.checked_add(frame_data.height))
            .is_some_and(|(right, bottom)| right <= image.width && bottom <= image.height);
        if !fits {
            return Err(command
                .echo()
                .error(ErrorCode::Invalid, "frame data rectangle is out of bounds"));
        }

        let edit_index = command
            .frame_number
            .checked_sub(1)
            .map(|index| index as usize);
        if edit_index.is_some_and(|index| index >= image.frames.len()) {
            return Err(command
                .echo()
                .error(ErrorCode::Invalid, "frame to edit does not exist"));
        }
        let canvas_bytes = image.rgba.len();
        let base_transient = if let Some(index) = edit_index {
            image.frames[index].transient
        } else if let Some(index) = command.other_frame_number.checked_sub(1) {
            image
                .frames
                .get(index as usize)
                .is_some_and(|frame| frame.transient)
        } else {
            false
        };
        if edit_index.is_none() {
            self.evict_additional_to_fit(canvas_bytes, image_id, screen);
            if self
                .total_bytes
                .saturating_add(self.placement_bytes)
                .saturating_add(canvas_bytes)
                > STORE_QUOTA_BYTES
            {
                return Err(command.echo().error(
                    ErrorCode::NoSpace,
                    "animation frame storage quota exhausted",
                ));
            }
        }

        let image = self.images.get(&image_id).expect("resolved image exists");
        let mut canvas = if let Some(index) = edit_index {
            image.frames[index].rgba.as_ref().clone()
        } else if let Some(index) = command.other_frame_number.checked_sub(1) {
            image
                .frames
                .get(index as usize)
                .ok_or_else(|| {
                    command
                        .echo()
                        .error(ErrorCode::Invalid, "base frame does not exist")
                })?
                .rgba
                .as_ref()
                .clone()
        } else {
            let color = command.frame_background.to_be_bytes();
            color
                .into_iter()
                .cycle()
                .take(canvas_bytes)
                .collect::<Vec<_>>()
        };
        composite_rgba_rect(
            &mut canvas,
            image.width,
            &frame_data.rgba,
            frame_data.width,
            frame_data.height,
            command.source_x,
            command.source_y,
            command.frame_composition,
        );

        let mut refresh_placements = false;
        let image = self
            .images
            .get_mut(&image_id)
            .expect("resolved image exists");
        if let Some(index) = edit_index {
            let gap_ms = command
                .frame_gap
                .filter(|gap| *gap != 0)
                .map(normalize_frame_gap)
                .unwrap_or(image.frames[index].gap_ms);
            image.frames[index] = AnimationFrame {
                rgba: Arc::new(canvas),
                gap_ms,
                transient: base_transient || command.is_transient(),
            };
            if index == image.current_frame {
                image.refresh_current();
                refresh_placements = true;
            }
        } else {
            let gap_ms = command
                .frame_gap
                .filter(|gap| *gap != 0)
                .map_or(40, normalize_frame_gap);
            image.frames.push(AnimationFrame {
                rgba: Arc::new(canvas),
                gap_ms,
                transient: base_transient || command.is_transient(),
            });
            self.total_bytes = self.total_bytes.saturating_add(canvas_bytes);
        }
        self.touch_image(image_id);
        if refresh_placements {
            self.refresh_placements(image_id, screen);
        }
        Ok(image_id)
    }

    fn control_animation(
        &mut self,
        command: &Command,
        screen: &mut Screen,
    ) -> Result<(), ProtocolError> {
        let image_id = self.resolve_animation_image_id(command)?;
        let image = self
            .images
            .get_mut(&image_id)
            .expect("resolved image exists");
        if let (Some(gap), Some(index)) = (
            command.frame_gap.filter(|gap| *gap != 0),
            command.frame_number.checked_sub(1),
        ) {
            if let Some(frame) = image.frames.get_mut(index as usize) {
                frame.gap_ms = normalize_frame_gap(gap);
            }
        }
        let mut refresh_placements = false;
        if let Some(index) = command.other_frame_number.checked_sub(1) {
            if (index as usize) < image.frames.len() && index as usize != image.current_frame {
                image.current_frame = index as usize;
                image.shown_at_ms = None;
                image.refresh_current();
                refresh_placements = true;
            }
        }
        if let Some(state) = command.animation_state {
            image.animation_state = state;
            image.current_loop = 0;
            image.shown_at_ms = None;
        }
        match command.loop_count {
            Some(0) | None => {}
            Some(1) => image.max_loops = 0,
            Some(count) => image.max_loops = count - 1,
        }
        self.touch_image(image_id);
        if refresh_placements {
            self.refresh_placements(image_id, screen);
        }
        Ok(())
    }

    fn compose_frame(
        &mut self,
        command: &Command,
        screen: &mut Screen,
    ) -> Result<u32, ProtocolError> {
        let image_id = self.resolve_animation_image_id(command)?;
        let source_index = command.frame_number.checked_sub(1).ok_or_else(|| {
            command
                .echo()
                .error(ErrorCode::NotFound, "source frame does not exist")
        })? as usize;
        let destination_index = command.other_frame_number.checked_sub(1).ok_or_else(|| {
            command
                .echo()
                .error(ErrorCode::NotFound, "destination frame does not exist")
        })? as usize;
        let image = self.images.get(&image_id).expect("resolved image exists");
        if source_index >= image.frames.len() || destination_index >= image.frames.len() {
            return Err(command
                .echo()
                .error(ErrorCode::NotFound, "animation frame does not exist"));
        }
        let width = if command.source_w == 0 {
            image.width
        } else {
            command.source_w
        };
        let height = if command.source_h == 0 {
            image.height
        } else {
            command.source_h
        };
        let source_fits = rect_fits(
            command.frame_source_x,
            command.frame_source_y,
            width,
            height,
            image.width,
            image.height,
        );
        let destination_fits = rect_fits(
            command.source_x,
            command.source_y,
            width,
            height,
            image.width,
            image.height,
        );
        if !source_fits || !destination_fits || width == 0 || height == 0 {
            return Err(command
                .echo()
                .error(ErrorCode::Invalid, "composition rectangle is out of bounds"));
        }
        if source_index == destination_index
            && rectangles_overlap(
                command.frame_source_x,
                command.frame_source_y,
                command.source_x,
                command.source_y,
                width,
                height,
            )
        {
            return Err(command.echo().error(
                ErrorCode::Invalid,
                "overlapping composition rectangles use the same frame",
            ));
        }

        let source = Arc::clone(&image.frames[source_index].rgba);
        let transient =
            image.frames[source_index].transient || image.frames[destination_index].transient;
        let mut destination = image.frames[destination_index].rgba.as_ref().clone();
        composite_rgba_region(
            &mut destination,
            image.width,
            &source,
            image.width,
            command.frame_source_x,
            command.frame_source_y,
            command.source_x,
            command.source_y,
            width,
            height,
            command.frame_composition,
        );
        let image = self
            .images
            .get_mut(&image_id)
            .expect("resolved image exists");
        image.frames[destination_index].rgba = Arc::new(destination);
        image.frames[destination_index].transient = transient;
        let refresh_placements = destination_index == image.current_frame;
        if refresh_placements {
            image.refresh_current();
        }
        self.touch_image(image_id);
        if refresh_placements {
            self.refresh_placements(image_id, screen);
        }
        Ok(image_id)
    }

    fn allocate_image_id(&mut self) -> u32 {
        loop {
            let candidate = self.next_image_id.max(1);
            self.next_image_id = self.next_image_id.wrapping_add(1).max(1);
            if !self.images.contains_key(&candidate) {
                return candidate;
            }
        }
    }

    fn resolve_animation_image_id(&self, command: &Command) -> Result<u32, ProtocolError> {
        let image_id = command
            .image_id
            .filter(|id| *id != 0)
            .or_else(|| {
                command
                    .image_number
                    .and_then(|number| self.image_numbers.get(&number)?.last().copied())
            })
            .filter(|id| self.images.contains_key(id));
        image_id.ok_or_else(|| {
            command
                .echo()
                .error(ErrorCode::NotFound, "animation image not found")
        })
    }

    fn touch_image(&mut self, image_id: u32) {
        self.next_lru = self.next_lru.saturating_add(1);
        if let Some(image) = self.images.get_mut(&image_id) {
            image.lru = self.next_lru;
        }
    }

    fn refresh_placements(&mut self, image_id: u32, screen: &mut Screen) {
        if self
            .virtual_placements
            .iter()
            .any(|placement| placement.image_id == image_id)
        {
            self.placeholder_revision = self.placeholder_revision.saturating_add(1);
        }
        let Some(image) = self.images.get(&image_id).cloned() else {
            return;
        };
        let placements: Vec<(u64, Command)> = self
            .placements
            .iter()
            .filter(|(_, placement)| placement.image_id == image_id)
            .map(|(screen_id, placement)| (*screen_id, placement.command.clone()))
            .collect();
        for (screen_id, command) in placements {
            let Ok(prepared) = prepare_placement(&image, &command, screen) else {
                continue;
            };
            let allocated_bytes = if Arc::ptr_eq(&prepared.pixels, &image.rgba) {
                0
            } else {
                prepared.pixels.len()
            };
            if screen.update_rgba_image(
                screen_id,
                DecodedRgbaImage {
                    data: prepared.pixels,
                    width: prepared.pixel_width,
                    height: prepared.pixel_height,
                    z_index: command.z_index,
                    protocol_image_id: image_id,
                    clear_cells: false,
                },
            ) {
                if let Some(placement) = self.placements.get_mut(&screen_id) {
                    self.placement_bytes = self
                        .placement_bytes
                        .saturating_sub(placement.allocated_bytes)
                        .saturating_add(allocated_bytes);
                    placement.allocated_bytes = allocated_bytes;
                }
            }
        }
        self.refresh_relative_placements(screen);
    }

    pub(crate) fn advance_animations(
        &mut self,
        now_ms: u64,
        screen: &mut Screen,
    ) -> GraphicsAnimationTick {
        let mut changed_images = Vec::new();
        let mut next_wake_ms: Option<u64> = None;
        for (image_id, image) in &mut self.images {
            if image.animation_state == AnimationState::Stopped
                || image.frames.len() < 2
                || image.animation_duration() == 0
            {
                continue;
            }
            let shown_at = *image.shown_at_ms.get_or_insert(now_ms);
            let due = shown_at.saturating_add(u64::from(image.frames[image.current_frame].gap_ms));
            if now_ms < due {
                next_wake_ms = Some(next_wake_ms.map_or(due, |next| next.min(due)));
                continue;
            }

            let mut advanced = false;
            let mut inspected = 0usize;
            while inspected < image.frames.len() {
                inspected += 1;
                if image.current_frame + 1 == image.frames.len() {
                    if image.animation_state == AnimationState::Loading {
                        break;
                    }
                    if image.max_loops != 0 {
                        image.current_loop = image.current_loop.saturating_add(1);
                        if image.current_loop >= image.max_loops {
                            image.animation_state = AnimationState::Stopped;
                            image.shown_at_ms = None;
                            break;
                        }
                    }
                    image.current_frame = 0;
                } else {
                    image.current_frame += 1;
                }
                advanced = true;
                if image.frames[image.current_frame].gap_ms != 0 {
                    break;
                }
            }
            if advanced {
                image.refresh_current();
                image.shown_at_ms = Some(now_ms);
                changed_images.push(*image_id);
            }
            if image.animation_state != AnimationState::Stopped
                && !(image.animation_state == AnimationState::Loading
                    && image.current_frame + 1 == image.frames.len())
            {
                let next =
                    now_ms.saturating_add(u64::from(image.frames[image.current_frame].gap_ms));
                next_wake_ms = Some(next_wake_ms.map_or(next, |deadline| deadline.min(next)));
            }
        }
        for image_id in &changed_images {
            self.refresh_placements(*image_id, screen);
        }
        if !changed_images.is_empty() {
            self.refresh_unicode_placements(screen);
            self.refresh_relative_placements(screen);
        }
        GraphicsAnimationTick {
            changed: !changed_images.is_empty(),
            next_wake_ms,
        }
    }

    fn resolve_image_id(&self, command: &Command) -> Option<u32> {
        command
            .image_id
            .filter(|id| *id != 0)
            .or_else(|| {
                command
                    .image_number
                    .and_then(|number| self.image_numbers.get(&number)?.last().copied())
            })
            .or_else(|| (self.images.len() == 1).then(|| *self.images.keys().next().unwrap()))
    }

    fn place(
        &mut self,
        command: &Command,
        image_id: u32,
        screen: &mut Screen,
    ) -> Result<(), ProtocolError> {
        if command.unicode_placeholder {
            if command.parent_image_id != 0 {
                return Err(command
                    .echo()
                    .error(ErrorCode::Invalid, "virtual placement cannot have a parent"));
            }
            return self.place_virtual(command, image_id, screen);
        }
        if command.parent_image_id != 0 {
            return self.place_relative(command, image_id, screen);
        }
        let stored = self
            .images
            .get(&image_id)
            .cloned()
            .ok_or_else(|| command.echo().error(ErrorCode::NotFound, "image not found"))?;
        let placement = prepare_placement(&stored, command, screen)?;
        let placement_id = command.placement_id.unwrap_or(0);
        let replacements = self.named_placement_parents(image_id, placement_id);
        let allocated_bytes = if Arc::ptr_eq(&placement.pixels, &stored.rgba) {
            0
        } else {
            placement.pixels.len()
        };
        let replaced_bytes: usize = replacements
            .iter()
            .map(|parent| self.parent_allocated_bytes(*parent))
            .sum();
        let projected = self
            .total_bytes
            .saturating_add(self.placement_bytes)
            .saturating_sub(replaced_bytes)
            .saturating_add(allocated_bytes);
        if projected > STORE_QUOTA_BYTES {
            return Err(command
                .echo()
                .error(ErrorCode::NoSpace, "placement storage quota exhausted"));
        }
        let replaced_virtual = replacements
            .iter()
            .any(|parent| matches!(parent, PlacementParent::Virtual(_)));
        for parent in &replacements {
            self.remove_for_replacement(*parent, screen);
        }
        let screen_id = screen.add_rgba_image_with_size(
            screen.cursor.col,
            screen.cursor.row,
            placement.cell_width,
            placement.cell_height,
            DecodedRgbaImage {
                data: placement.pixels,
                width: placement.pixel_width,
                height: placement.pixel_height,
                z_index: command.z_index,
                protocol_image_id: image_id,
                clear_cells: false,
            },
        );
        self.placement_bytes = self.placement_bytes.saturating_add(allocated_bytes);
        self.placements.insert(
            screen_id,
            PlacementRecord {
                image_id,
                placement_id,
                z_index: command.z_index,
                allocated_bytes,
                command: command.clone(),
            },
        );
        self.reparent(&replacements, PlacementParent::Real(screen_id));
        if replaced_virtual {
            self.placeholder_revision = self.placeholder_revision.saturating_add(1);
            self.refresh_unicode_placements(screen);
        }
        self.next_lru = self.next_lru.saturating_add(1);
        if let Some(image) = self.images.get_mut(&image_id) {
            image.lru = self.next_lru;
        }
        if !command.suppress_cursor_movement {
            advance_cursor(screen, placement.cell_width, placement.cell_height);
        }
        self.refresh_relative_placements(screen);
        Ok(())
    }

    fn named_placement_parents(&self, image_id: u32, placement_id: u32) -> Vec<PlacementParent> {
        if placement_id == 0 {
            return Vec::new();
        }
        let mut parents = Vec::new();
        parents.extend(self.placements.iter().filter_map(|(screen_id, placement)| {
            (placement.image_id == image_id && placement.placement_id == placement_id)
                .then_some(PlacementParent::Real(*screen_id))
        }));
        parents.extend(self.virtual_placements.iter().filter_map(|placement| {
            (placement.image_id == image_id && placement.placement_id == placement_id)
                .then_some(PlacementParent::Virtual(placement.id))
        }));
        parents.extend(self.relative_placements.iter().filter_map(|placement| {
            (placement.image_id == image_id && placement.placement_id == placement_id)
                .then_some(PlacementParent::Relative(placement.id))
        }));
        parents
    }

    fn parent_allocated_bytes(&self, parent: PlacementParent) -> usize {
        match parent {
            PlacementParent::Real(screen_id) => self
                .placements
                .get(&screen_id)
                .map_or(0, |placement| placement.allocated_bytes),
            PlacementParent::Virtual(_) => 0,
            PlacementParent::Relative(id) => self
                .relative_placements
                .iter()
                .find(|placement| placement.id == id)
                .map_or(0, |placement| placement.allocated_bytes),
        }
    }

    fn remove_for_replacement(&mut self, parent: PlacementParent, screen: &mut Screen) {
        match parent {
            PlacementParent::Real(screen_id) => {
                self.remove_real_placement_only(screen_id, screen);
            }
            PlacementParent::Virtual(id) => {
                self.virtual_placements
                    .retain(|placement| placement.id != id);
                self.clear_placeholder_fragments(screen);
            }
            PlacementParent::Relative(id) => {
                self.remove_relative_record_only(id, screen);
            }
        }
    }

    fn reparent(&mut self, old_parents: &[PlacementParent], new_parent: PlacementParent) {
        for relative in &mut self.relative_placements {
            if old_parents.contains(&relative.parent) {
                relative.parent = new_parent;
            }
        }
    }

    fn place_relative(
        &mut self,
        command: &Command,
        image_id: u32,
        screen: &mut Screen,
    ) -> Result<(), ProtocolError> {
        let stored = self
            .images
            .get(&image_id)
            .cloned()
            .ok_or_else(|| command.echo().error(ErrorCode::NotFound, "image not found"))?;
        let prepared = prepare_placement(&stored, command, screen)?;
        let parent = self
            .find_parent(command.parent_image_id, command.parent_placement_id)
            .ok_or_else(|| {
                command
                    .echo()
                    .error(ErrorCode::NoParent, "parent placement not found")
            })?;
        let placement_id = command.placement_id.unwrap_or(0);
        let replacements = self.named_placement_parents(image_id, placement_id);
        self.validate_parent_chain(parent, &replacements, command)?;

        let allocated_bytes = if Arc::ptr_eq(&prepared.pixels, &stored.rgba) {
            0
        } else {
            prepared.pixels.len()
        };
        let replaced_bytes: usize = replacements
            .iter()
            .map(|parent| self.parent_allocated_bytes(*parent))
            .sum();
        if self
            .total_bytes
            .saturating_add(self.placement_bytes)
            .saturating_sub(replaced_bytes)
            .saturating_add(allocated_bytes)
            > STORE_QUOTA_BYTES
        {
            return Err(command
                .echo()
                .error(ErrorCode::NoSpace, "placement storage quota exhausted"));
        }

        let id = if let Some(id) = replacements.iter().find_map(|parent| match parent {
            PlacementParent::Relative(id) => Some(*id),
            _ => None,
        }) {
            id
        } else {
            let id = self.next_relative_id;
            self.next_relative_id = self.next_relative_id.saturating_add(1);
            id
        };
        let replaced_virtual = replacements
            .iter()
            .any(|parent| matches!(parent, PlacementParent::Virtual(_)));
        for replacement in &replacements {
            self.remove_for_replacement(*replacement, screen);
        }
        let record = RelativePlacementRecord {
            id,
            image_id,
            placement_id,
            parent,
            command: command.clone(),
            screen_id: None,
            allocated_bytes: 0,
        };
        self.relative_placements.push(record);
        self.reparent(&replacements, PlacementParent::Relative(id));
        if replaced_virtual {
            self.placeholder_revision = self.placeholder_revision.saturating_add(1);
            self.refresh_unicode_placements(screen);
        }
        self.touch_image(image_id);
        self.refresh_relative_placements(screen);
        Ok(())
    }

    fn find_parent(&self, image_id: u32, placement_id: u32) -> Option<PlacementParent> {
        let matches = |candidate_image: u32, candidate_placement: u32| {
            candidate_image == image_id
                && (placement_id == 0 || candidate_placement == placement_id)
        };
        self.placements
            .iter()
            .filter(|(_, placement)| matches(placement.image_id, placement.placement_id))
            .min_by_key(|(screen_id, _)| *screen_id)
            .map(|(screen_id, _)| PlacementParent::Real(*screen_id))
            .or_else(|| {
                self.virtual_placements
                    .iter()
                    .find(|placement| matches(placement.image_id, placement.placement_id))
                    .map(|placement| PlacementParent::Virtual(placement.id))
            })
            .or_else(|| {
                self.relative_placements
                    .iter()
                    .find(|placement| matches(placement.image_id, placement.placement_id))
                    .map(|placement| PlacementParent::Relative(placement.id))
            })
    }

    fn validate_parent_chain(
        &self,
        mut parent: PlacementParent,
        replaced_parents: &[PlacementParent],
        command: &Command,
    ) -> Result<(), ProtocolError> {
        for depth in 0..PARENT_DEPTH_LIMIT {
            if replaced_parents.contains(&parent) {
                let (code, message) = if depth == 0 {
                    (ErrorCode::Invalid, "placement cannot be its own parent")
                } else {
                    (ErrorCode::Cycle, "relative placement creates a cycle")
                };
                return Err(command.echo().error(code, message));
            }
            let PlacementParent::Relative(parent_id) = parent else {
                return Ok(());
            };
            let Some(record) = self
                .relative_placements
                .iter()
                .find(|placement| placement.id == parent_id)
            else {
                return Err(command
                    .echo()
                    .error(ErrorCode::NoParent, "parent chain is incomplete"));
            };
            parent = record.parent;
        }
        if matches!(parent, PlacementParent::Relative(_)) {
            Err(command
                .echo()
                .error(ErrorCode::TooDeep, "relative placement chain is too deep"))
        } else {
            Ok(())
        }
    }

    fn relative_origin(&self, id: u64, screen: &Screen, depth: usize) -> Option<(i64, i64)> {
        if depth >= PARENT_DEPTH_LIMIT {
            return None;
        }
        let record = self
            .relative_placements
            .iter()
            .find(|placement| placement.id == id)?;
        let (col, line) = self.parent_origin(record.parent, screen, depth + 1)?;
        Some((
            col.checked_add(i64::from(record.command.horizontal_offset))?,
            line.checked_add(i64::from(record.command.vertical_offset))?,
        ))
    }

    fn parent_origin(
        &self,
        parent: PlacementParent,
        screen: &Screen,
        depth: usize,
    ) -> Option<(i64, i64)> {
        match parent {
            PlacementParent::Real(screen_id) => {
                let image = screen.image_by_id(screen_id)?;
                Some((
                    i64::try_from(image.col).ok()?,
                    i64::try_from(image.line).ok()?,
                ))
            }
            PlacementParent::Virtual(virtual_id) => {
                let mut origin: Option<(usize, usize)> = None;
                for image in self
                    .placeholder_fragments
                    .iter()
                    .filter(|fragment| fragment.virtual_id == virtual_id)
                    .filter_map(|fragment| screen.image_by_id(fragment.screen_id))
                {
                    origin = Some(origin.map_or((image.col, image.line), |(col, line)| {
                        (col.min(image.col), line.min(image.line))
                    }));
                }
                let (col, line) = origin?;
                Some((i64::try_from(col).ok()?, i64::try_from(line).ok()?))
            }
            PlacementParent::Relative(id) => self.relative_origin(id, screen, depth),
        }
    }

    pub(crate) fn refresh_relative_placements(&mut self, screen: &mut Screen) {
        self.sync_placements(screen);
        let plans: Vec<(u64, Option<PreparedRelativePlacement>)> = self
            .relative_placements
            .iter()
            .map(|record| {
                let plan = self
                    .relative_origin(record.id, screen, 0)
                    .zip(self.images.get(&record.image_id))
                    .and_then(|(origin, image)| {
                        let prepared = prepare_placement(image, &record.command, screen).ok()?;
                        let (col, line, prepared) =
                            clip_relative_placement(origin, prepared).ok().flatten()?;
                        let allocated_bytes = if Arc::ptr_eq(&prepared.pixels, &image.rgba) {
                            0
                        } else {
                            prepared.pixels.len()
                        };
                        Some(PreparedRelativePlacement {
                            col,
                            line,
                            allocated_bytes,
                            prepared,
                        })
                    });
                (record.id, plan)
            })
            .collect();

        for (id, plan) in plans {
            let Some(plan) = plan else {
                self.hide_relative_placement(id, screen);
                continue;
            };
            let Some(index) = self
                .relative_placements
                .iter()
                .position(|placement| placement.id == id)
            else {
                continue;
            };
            let old_screen_id = self.relative_placements[index].screen_id;
            let old_bytes = self.relative_placements[index].allocated_bytes;
            if self
                .total_bytes
                .saturating_add(self.placement_bytes)
                .saturating_sub(old_bytes)
                .saturating_add(plan.allocated_bytes)
                > STORE_QUOTA_BYTES
            {
                self.hide_relative_placement(id, screen);
                continue;
            }
            let image_id = self.relative_placements[index].image_id;
            let command = &self.relative_placements[index].command;
            let pixels = DecodedRgbaImage {
                data: plan.prepared.pixels,
                width: plan.prepared.pixel_width,
                height: plan.prepared.pixel_height,
                z_index: command.z_index,
                protocol_image_id: image_id,
                clear_cells: false,
            };
            let screen_id = if let Some(screen_id) = old_screen_id {
                if screen.update_rgba_image_geometry(
                    screen_id,
                    plan.col,
                    plan.line,
                    plan.prepared.cell_width,
                    plan.prepared.cell_height,
                    pixels.clone(),
                ) {
                    screen_id
                } else {
                    screen.add_rgba_image_at_absolute_line(
                        plan.col,
                        plan.line,
                        plan.prepared.cell_width,
                        plan.prepared.cell_height,
                        pixels,
                    )
                }
            } else {
                screen.add_rgba_image_at_absolute_line(
                    plan.col,
                    plan.line,
                    plan.prepared.cell_width,
                    plan.prepared.cell_height,
                    pixels,
                )
            };
            self.placement_bytes = self
                .placement_bytes
                .saturating_sub(old_bytes)
                .saturating_add(plan.allocated_bytes);
            self.relative_placements[index].screen_id = Some(screen_id);
            self.relative_placements[index].allocated_bytes = plan.allocated_bytes;
        }
    }

    fn hide_relative_placement(&mut self, id: u64, screen: &mut Screen) {
        let Some(record) = self
            .relative_placements
            .iter_mut()
            .find(|placement| placement.id == id)
        else {
            return;
        };
        if let Some(screen_id) = record.screen_id.take() {
            screen.remove_image(screen_id);
        }
        self.placement_bytes = self.placement_bytes.saturating_sub(record.allocated_bytes);
        record.allocated_bytes = 0;
    }

    fn remove_relative_record_only(
        &mut self,
        id: u64,
        screen: &mut Screen,
    ) -> Option<RelativePlacementRecord> {
        let index = self
            .relative_placements
            .iter()
            .position(|placement| placement.id == id)?;
        let record = self.relative_placements.swap_remove(index);
        if let Some(screen_id) = record.screen_id {
            screen.remove_image(screen_id);
        }
        self.placement_bytes = self.placement_bytes.saturating_sub(record.allocated_bytes);
        Some(record)
    }

    fn place_virtual(
        &mut self,
        command: &Command,
        image_id: u32,
        screen: &mut Screen,
    ) -> Result<(), ProtocolError> {
        let image = self
            .images
            .get(&image_id)
            .ok_or_else(|| command.echo().error(ErrorCode::NotFound, "image not found"))?;
        virtual_grid_dimensions(image.width, image.height, command, screen)?;
        let placement_id = command.placement_id.unwrap_or(0);
        let replacements = self.named_placement_parents(image_id, placement_id);
        for replacement in &replacements {
            self.remove_for_replacement(*replacement, screen);
        }
        let virtual_id = self.next_virtual_id;
        self.next_virtual_id = self.next_virtual_id.saturating_add(1);
        self.virtual_placements.push(VirtualPlacementRecord {
            id: virtual_id,
            image_id,
            placement_id,
            command: command.clone(),
        });
        self.reparent(&replacements, PlacementParent::Virtual(virtual_id));
        self.placeholder_revision = self.placeholder_revision.saturating_add(1);
        self.touch_image(image_id);
        self.refresh_unicode_placements(screen);
        self.refresh_relative_placements(screen);
        Ok(())
    }

    /// Rebuild only the current viewport's real images from `U+10EEEE` cells.
    /// The invisible protocol placements remain in `virtual_placements`.
    pub(crate) fn refresh_unicode_placements(&mut self, screen: &mut Screen) {
        if self.virtual_placements.is_empty() {
            self.clear_placeholder_fragments(screen);
            self.placeholder_projection.clear();
            self.rendered_placeholder_revision = self.placeholder_revision;
            return;
        }

        let mut projection = Vec::new();
        for visible_row in 0..screen.height() {
            let absolute_line = screen.visible_row_to_absolute_line(visible_row);
            let decoded = scan_cells(
                (0..screen.width())
                    .filter_map(|col| screen.get_cell_with_scrollback(absolute_line, col)),
            );
            projection.extend(decoded.into_iter().filter_map(|run| {
                let placement = self.virtual_placements.iter().find(|placement| {
                    placement.image_id == run.image_id
                        && (run.placement_id == 0 || placement.placement_id == run.placement_id)
                })?;
                Some(PlaceholderProjection {
                    absolute_line,
                    run,
                    virtual_id: placement.id,
                })
            }));
        }
        let fragments_are_live = self
            .placeholder_fragments
            .iter()
            .all(|fragment| screen.image_by_id(fragment.screen_id).is_some());
        if self.placeholder_projection == projection
            && self.rendered_placeholder_revision == self.placeholder_revision
            && fragments_are_live
        {
            return;
        }

        self.clear_placeholder_fragments(screen);
        let mut source_cache = HashMap::new();
        let mut complete = true;
        for projected in &projection {
            let absolute_line = projected.absolute_line;
            let run = projected.run;
            let Some(virtual_placement) = self
                .virtual_placements
                .iter()
                .find(|placement| placement.id == projected.virtual_id)
            else {
                complete = false;
                continue;
            };
            let Some(image) = self.images.get(&run.image_id) else {
                complete = false;
                continue;
            };
            let source = source_cache.entry(run.image_id).or_insert_with(|| {
                RgbaImage::from_raw(image.width, image.height, image.rgba.as_ref().clone())
                    .expect("stored Kitty image has validated RGBA dimensions")
            });
            let prepared =
                match prepare_placeholder_fragment(source, &virtual_placement.command, run, screen)
                {
                    Ok(Some(prepared)) => prepared,
                    Ok(None) => continue,
                    Err(_) => {
                        complete = false;
                        continue;
                    }
                };
            let allocated_bytes = prepared.pixels.len();
            if self
                .total_bytes
                .saturating_add(self.placement_bytes)
                .saturating_add(allocated_bytes)
                > STORE_QUOTA_BYTES
            {
                complete = false;
                continue;
            }
            let screen_id = screen.add_rgba_image_at_absolute_line(
                run.screen_col,
                absolute_line,
                prepared.cell_width,
                prepared.cell_height,
                DecodedRgbaImage {
                    data: prepared.pixels,
                    width: prepared.pixel_width,
                    height: prepared.pixel_height,
                    // Kitty's real cell images sit above the cell background,
                    // below text/cursor, independent of the prototype's z.
                    z_index: -1,
                    protocol_image_id: run.image_id,
                    clear_cells: false,
                },
            );
            self.placement_bytes = self.placement_bytes.saturating_add(allocated_bytes);
            self.placeholder_fragments.push(PlaceholderFragment {
                screen_id,
                virtual_id: virtual_placement.id,
                allocated_bytes,
            });
        }
        if complete {
            self.placeholder_projection = projection;
            self.rendered_placeholder_revision = self.placeholder_revision;
        } else {
            self.placeholder_projection.clear();
            self.rendered_placeholder_revision = 0;
        }
    }

    pub(crate) fn refresh_unicode_placement_geometry(&mut self, screen: &mut Screen) {
        self.placeholder_revision = self.placeholder_revision.saturating_add(1);
        self.refresh_unicode_placements(screen);
    }

    fn clear_placeholder_fragments(&mut self, screen: &mut Screen) {
        for fragment in self.placeholder_fragments.drain(..) {
            self.placement_bytes = self
                .placement_bytes
                .saturating_sub(fragment.allocated_bytes);
            screen.remove_image(fragment.screen_id);
        }
    }

    fn delete(&mut self, command: &Command, screen: &mut Screen) {
        self.sync_placements(screen);
        let specifier = command.delete_specifier.unwrap_or(b'a');
        let free_data = specifier.is_ascii_uppercase();
        let lower = specifier.to_ascii_lowercase();
        let live_top = screen.scrollback().len();
        let target_image = matches!(lower, b'i' | b'n')
            .then(|| self.resolve_image_id(command))
            .flatten();
        let removed_virtual: Vec<(u64, u32)> = self
            .virtual_placements
            .iter()
            .filter(|placement| match lower {
                b'i' | b'n' => {
                    target_image == Some(placement.image_id)
                        && command
                            .placement_id
                            .is_none_or(|id| id == placement.placement_id)
                }
                b'r' => {
                    placement.image_id >= command.source_x && placement.image_id <= command.source_y
                }
                _ => false,
            })
            .map(|placement| (placement.id, placement.image_id))
            .collect();
        let removed_virtual_images: Vec<u32> = removed_virtual
            .iter()
            .map(|(_, image_id)| *image_id)
            .collect();
        let screen_ids: Vec<u64> = self
            .placements
            .iter()
            .filter_map(|(screen_id, placement)| {
                let image = screen.image_by_id(*screen_id)?;
                let matches = match lower {
                    b'a' => {
                        image_intersects_rect(image, 0, live_top, screen.width(), screen.height())
                    }
                    b'i' | b'n' => {
                        target_image == Some(placement.image_id)
                            && command
                                .placement_id
                                .is_none_or(|id| id == placement.placement_id)
                    }
                    b'c' => image_intersects_cell(
                        image,
                        screen.cursor.col,
                        live_top.saturating_add(screen.cursor.row),
                    ),
                    b'p' | b'q' => command.source_x.checked_sub(1).is_some_and(|col| {
                        command.source_y.checked_sub(1).is_some_and(|row| {
                            (lower != b'q' || placement.z_index == command.z_index)
                                && image_intersects_cell(
                                    image,
                                    col as usize,
                                    live_top.saturating_add(row as usize),
                                )
                        })
                    }),
                    b'x' => command.source_x.checked_sub(1).is_some_and(|col| {
                        image.col <= col as usize
                            && image.col.saturating_add(image.cell_width) > col as usize
                    }),
                    b'y' => command.source_y.checked_sub(1).is_some_and(|row| {
                        let line = live_top.saturating_add(row as usize);
                        image.line <= line && image.line.saturating_add(image.cell_height) > line
                    }),
                    b'z' => placement.z_index == command.z_index,
                    b'r' => {
                        placement.image_id >= command.source_x
                            && placement.image_id <= command.source_y
                    }
                    _ => false,
                };
                matches.then_some(*screen_id)
            })
            .collect();
        let relative_ids: Vec<u64> = self
            .relative_placements
            .iter()
            .filter_map(|placement| {
                let image = placement
                    .screen_id
                    .and_then(|screen_id| screen.image_by_id(screen_id));
                let matches = match lower {
                    b'a' => image.is_some_and(|image| {
                        image_intersects_rect(image, 0, live_top, screen.width(), screen.height())
                    }),
                    b'i' | b'n' => {
                        target_image == Some(placement.image_id)
                            && command
                                .placement_id
                                .is_none_or(|id| id == placement.placement_id)
                    }
                    b'c' => image.is_some_and(|image| {
                        image_intersects_cell(
                            image,
                            screen.cursor.col,
                            live_top.saturating_add(screen.cursor.row),
                        )
                    }),
                    b'p' | b'q' => image.is_some_and(|image| {
                        command.source_x.checked_sub(1).is_some_and(|col| {
                            command.source_y.checked_sub(1).is_some_and(|row| {
                                (lower != b'q' || placement.command.z_index == command.z_index)
                                    && image_intersects_cell(
                                        image,
                                        col as usize,
                                        live_top.saturating_add(row as usize),
                                    )
                            })
                        })
                    }),
                    b'x' => image.is_some_and(|image| {
                        command.source_x.checked_sub(1).is_some_and(|col| {
                            image.col <= col as usize
                                && image.col.saturating_add(image.cell_width) > col as usize
                        })
                    }),
                    b'y' => image.is_some_and(|image| {
                        command.source_y.checked_sub(1).is_some_and(|row| {
                            let line = live_top.saturating_add(row as usize);
                            image.line <= line
                                && image.line.saturating_add(image.cell_height) > line
                        })
                    }),
                    b'z' => placement.command.z_index == command.z_index,
                    b'r' => {
                        placement.image_id >= command.source_x
                            && placement.image_id <= command.source_y
                    }
                    _ => false,
                };
                matches.then_some(placement.id)
            })
            .collect();
        let mut affected_images: Vec<u32> = screen_ids
            .iter()
            .filter_map(|id| self.placements.get(id).map(|placement| placement.image_id))
            .collect();
        affected_images.extend(&removed_virtual_images);
        affected_images.extend(relative_ids.iter().filter_map(|id| {
            self.relative_placements
                .iter()
                .find(|placement| placement.id == *id)
                .map(|placement| placement.image_id)
        }));
        if free_data {
            if matches!(lower, b'i' | b'n') {
                affected_images.extend(target_image);
            } else if lower == b'r' {
                affected_images.extend(
                    self.images
                        .keys()
                        .copied()
                        .filter(|id| *id >= command.source_x && *id <= command.source_y),
                );
            }
            affected_images.sort_unstable();
            affected_images.dedup();
        }
        for screen_id in screen_ids {
            self.remove_placement(screen_id, screen);
        }
        for relative_id in relative_ids {
            self.remove_relative_root(relative_id, screen, false);
        }
        if !removed_virtual.is_empty() {
            let virtual_ids: Vec<u64> = removed_virtual.iter().map(|(id, _)| *id).collect();
            self.virtual_placements
                .retain(|placement| !virtual_ids.contains(&placement.id));
            for virtual_id in virtual_ids {
                self.remove_relative_descendants(PlacementParent::Virtual(virtual_id), screen);
            }
            self.placeholder_revision = self.placeholder_revision.saturating_add(1);
            self.refresh_unicode_placements(screen);
        }
        if free_data {
            for image_id in affected_images {
                if !self.image_has_placements(image_id) {
                    self.remove_image_data(image_id, screen);
                }
            }
        }
        self.refresh_relative_placements(screen);
    }

    fn remove_placements_for_image(&mut self, image_id: u32, screen: &mut Screen) {
        let screen_ids: Vec<u64> = self
            .placements
            .iter()
            .filter_map(|(screen_id, placement)| {
                (placement.image_id == image_id).then_some(*screen_id)
            })
            .collect();
        for screen_id in screen_ids {
            self.remove_placement(screen_id, screen);
        }
    }

    fn remove_placement(&mut self, screen_id: u64, screen: &mut Screen) {
        if self.remove_real_placement_only(screen_id, screen).is_some() {
            self.remove_relative_descendants(PlacementParent::Real(screen_id), screen);
        }
    }

    fn remove_real_placement_only(
        &mut self,
        screen_id: u64,
        screen: &mut Screen,
    ) -> Option<PlacementRecord> {
        let placement = self.placements.remove(&screen_id)?;
        self.placement_bytes = self
            .placement_bytes
            .saturating_sub(placement.allocated_bytes);
        screen.remove_image(screen_id);
        Some(placement)
    }

    fn relative_tree_ids(&self, roots: impl IntoIterator<Item = u64>) -> Vec<u64> {
        let mut ids: Vec<u64> = roots.into_iter().collect();
        let mut index = 0;
        while index < ids.len() {
            let parent = ids[index];
            for child in &self.relative_placements {
                if child.parent == PlacementParent::Relative(parent) && !ids.contains(&child.id) {
                    ids.push(child.id);
                }
            }
            index += 1;
        }
        ids
    }

    fn remove_relative_records(&mut self, ids: &[u64], screen: &mut Screen) -> Vec<u32> {
        let mut removed_images = Vec::new();
        let mut released = 0usize;
        self.relative_placements.retain(|placement| {
            if ids.contains(&placement.id) {
                if let Some(screen_id) = placement.screen_id {
                    screen.remove_image(screen_id);
                }
                released = released.saturating_add(placement.allocated_bytes);
                removed_images.push(placement.image_id);
                false
            } else {
                true
            }
        });
        self.placement_bytes = self.placement_bytes.saturating_sub(released);
        removed_images
    }

    fn remove_relative_descendants(&mut self, parent: PlacementParent, screen: &mut Screen) {
        let roots: Vec<u64> = self
            .relative_placements
            .iter()
            .filter(|placement| placement.parent == parent)
            .map(|placement| placement.id)
            .collect();
        let ids = self.relative_tree_ids(roots);
        let image_ids = self.remove_relative_records(&ids, screen);
        self.remove_orphaned_relative_images(image_ids);
    }

    fn remove_relative_root(&mut self, id: u64, screen: &mut Screen, free_root_data: bool) {
        let ids = self.relative_tree_ids([id]);
        let root_image_id = self
            .relative_placements
            .iter()
            .find(|placement| placement.id == id)
            .map(|placement| placement.image_id);
        let image_ids = self.remove_relative_records(&ids, screen);
        self.remove_orphaned_relative_images(
            image_ids
                .into_iter()
                .filter(|image_id| free_root_data || Some(*image_id) != root_image_id),
        );
    }

    fn remove_orphaned_relative_images(&mut self, image_ids: impl IntoIterator<Item = u32>) {
        let mut image_ids: Vec<u32> = image_ids.into_iter().collect();
        image_ids.sort_unstable();
        image_ids.dedup();
        for image_id in image_ids {
            if !self.image_has_placements(image_id) {
                self.remove_stored_image_only(image_id);
            }
        }
    }

    fn remove_stored_image_only(&mut self, image_id: u32) {
        if let Some(image) = self.images.remove(&image_id) {
            self.total_bytes = self.total_bytes.saturating_sub(image.allocated_bytes());
        }
        for ids in self.image_numbers.values_mut() {
            ids.retain(|id| *id != image_id);
        }
        self.image_numbers.retain(|_, ids| !ids.is_empty());
    }

    fn remove_image_data(&mut self, image_id: u32, screen: &mut Screen) {
        self.remove_placements_for_image(image_id, screen);
        let virtual_ids: Vec<u64> = self
            .virtual_placements
            .iter()
            .filter(|placement| placement.image_id == image_id)
            .map(|placement| placement.id)
            .collect();
        let virtual_count = self.virtual_placements.len();
        self.virtual_placements
            .retain(|placement| placement.image_id != image_id);
        let removed_virtual = self.virtual_placements.len() != virtual_count;
        for virtual_id in virtual_ids {
            self.remove_relative_descendants(PlacementParent::Virtual(virtual_id), screen);
        }
        let relative_ids: Vec<u64> = self
            .relative_placements
            .iter()
            .filter(|placement| placement.image_id == image_id)
            .map(|placement| placement.id)
            .collect();
        for relative_id in relative_ids {
            self.remove_relative_root(relative_id, screen, false);
        }
        if removed_virtual {
            self.placeholder_revision = self.placeholder_revision.saturating_add(1);
            self.clear_placeholder_fragments(screen);
        }
        self.remove_stored_image_only(image_id);
        if removed_virtual {
            self.refresh_unicode_placements(screen);
        }
    }

    fn sync_placements(&mut self, screen: &mut Screen) {
        let missing: Vec<u64> = self
            .placements
            .keys()
            .filter(|screen_id| screen.image_by_id(**screen_id).is_none())
            .copied()
            .collect();
        for screen_id in missing {
            self.remove_placement(screen_id, screen);
        }
        let mut released = 0usize;
        for placement in &mut self.relative_placements {
            if placement
                .screen_id
                .is_some_and(|screen_id| screen.image_by_id(screen_id).is_none())
            {
                placement.screen_id = None;
                released = released.saturating_add(placement.allocated_bytes);
                placement.allocated_bytes = 0;
            }
        }
        self.placement_bytes = self.placement_bytes.saturating_sub(released);
    }

    fn placement_bytes_for_image(&self, image_id: u32) -> usize {
        self.placements
            .values()
            .filter(|placement| placement.image_id == image_id)
            .map(|placement| placement.allocated_bytes)
            .sum::<usize>()
            .saturating_add(
                self.relative_placements
                    .iter()
                    .filter(|placement| placement.image_id == image_id)
                    .map(|placement| placement.allocated_bytes)
                    .sum(),
            )
    }

    fn image_has_placements(&self, image_id: u32) -> bool {
        self.placements
            .values()
            .any(|placement| placement.image_id == image_id)
            || self
                .virtual_placements
                .iter()
                .any(|placement| placement.image_id == image_id)
            || self
                .relative_placements
                .iter()
                .any(|placement| placement.image_id == image_id)
    }

    fn projected_usage(&self, incoming: usize, replacement_id: Option<u32>) -> usize {
        let reclaimed = replacement_id.map_or(0, |image_id| {
            self.images
                .get(&image_id)
                .map_or(0, StoredImage::allocated_bytes)
                .saturating_add(self.placement_bytes_for_image(image_id))
        });
        self.total_bytes
            .saturating_add(self.placement_bytes)
            .saturating_add(incoming)
            .saturating_sub(reclaimed)
    }

    fn evict_to_fit(&mut self, incoming: usize, replacement_id: Option<u32>, screen: &mut Screen) {
        self.sync_placements(screen);
        while self.projected_usage(incoming, replacement_id) > STORE_QUOTA_BYTES {
            let Some(image_id) = self
                .images
                .iter()
                .filter(|(id, _)| Some(**id) != replacement_id && !self.image_has_placements(**id))
                .min_by_key(|(_, image)| (!image.is_transient(), image.lru))
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.remove_image_data(image_id, screen);
        }
    }

    fn evict_additional_to_fit(
        &mut self,
        incoming: usize,
        protected_image_id: u32,
        screen: &mut Screen,
    ) {
        self.sync_placements(screen);
        while self
            .total_bytes
            .saturating_add(self.placement_bytes)
            .saturating_add(incoming)
            > STORE_QUOTA_BYTES
        {
            let Some(image_id) = self
                .images
                .iter()
                .filter(|(id, _)| **id != protected_image_id && !self.image_has_placements(**id))
                .min_by_key(|(_, image)| (!image.is_transient(), image.lru))
                .map(|(id, _)| *id)
            else {
                break;
            };
            self.remove_image_data(image_id, screen);
        }
    }
}

struct PreparedPlacement {
    pixels: Arc<Vec<u8>>,
    pixel_width: usize,
    pixel_height: usize,
    cell_width: usize,
    cell_height: usize,
}

struct PreparedRelativePlacement {
    col: usize,
    line: usize,
    allocated_bytes: usize,
    prepared: PreparedPlacement,
}

fn clip_relative_placement(
    (col, line): (i64, i64),
    placement: PreparedPlacement,
) -> Result<Option<(usize, usize, PreparedPlacement)>, ()> {
    let hidden_cols = if col < 0 {
        usize::try_from(col.saturating_neg()).unwrap_or(usize::MAX)
    } else {
        0
    };
    let hidden_rows = if line < 0 {
        usize::try_from(line.saturating_neg()).unwrap_or(usize::MAX)
    } else {
        0
    };
    if hidden_cols >= placement.cell_width || hidden_rows >= placement.cell_height {
        return Ok(None);
    }
    let col = usize::try_from(col.max(0)).map_err(|_| ())?;
    let line = usize::try_from(line.max(0)).map_err(|_| ())?;
    if hidden_cols == 0 && hidden_rows == 0 {
        return Ok(Some((col, line, placement)));
    }

    let pixel_left = hidden_cols
        .saturating_mul(placement.pixel_width)
        .div_ceil(placement.cell_width);
    let pixel_top = hidden_rows
        .saturating_mul(placement.pixel_height)
        .div_ceil(placement.cell_height);
    if pixel_left >= placement.pixel_width || pixel_top >= placement.pixel_height {
        return Ok(None);
    }
    let source = RgbaImage::from_raw(
        placement.pixel_width as u32,
        placement.pixel_height as u32,
        placement.pixels.as_ref().clone(),
    )
    .ok_or(())?;
    let cropped = imageops::crop_imm(
        &source,
        pixel_left as u32,
        pixel_top as u32,
        (placement.pixel_width - pixel_left) as u32,
        (placement.pixel_height - pixel_top) as u32,
    )
    .to_image();
    let pixel_width = cropped.width() as usize;
    let pixel_height = cropped.height() as usize;
    Ok(Some((
        col,
        line,
        PreparedPlacement {
            pixels: Arc::new(cropped.into_raw()),
            pixel_width,
            pixel_height,
            cell_width: placement.cell_width - hidden_cols,
            cell_height: placement.cell_height - hidden_rows,
        },
    )))
}

fn virtual_grid_dimensions(
    image_width: u32,
    image_height: u32,
    command: &Command,
    screen: &Screen,
) -> Result<(u32, u32, u32, u32), ProtocolError> {
    let echo = command.echo();
    let cell_width = screen.cell_width_hint().round().max(1.0) as u32;
    let cell_height = screen.cell_height_hint().round().max(1.0) as u32;
    let columns = if command.columns == 0 {
        image_width.div_ceil(cell_width)
    } else {
        command.columns
    }
    .max(1);
    let rows = if command.rows == 0 {
        image_height.div_ceil(cell_height)
    } else {
        command.rows
    }
    .max(1);
    let box_width = columns
        .checked_mul(cell_width)
        .filter(|width| *width <= MAX_DIMENSION)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "virtual placement is too wide"))?;
    let box_height = rows
        .checked_mul(cell_height)
        .filter(|height| *height <= MAX_DIMENSION)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "virtual placement is too tall"))?;
    Ok((columns, rows, box_width, box_height))
}

fn prepare_placeholder_fragment(
    source: &RgbaImage,
    command: &Command,
    run: PlaceholderRun,
    screen: &Screen,
) -> Result<Option<PreparedPlacement>, ProtocolError> {
    let (columns, rows, box_width, box_height) =
        virtual_grid_dimensions(source.width(), source.height(), command, screen)?;
    if run.image_row >= rows || run.image_col >= columns {
        return Ok(None);
    }
    let run_columns = u32::try_from(run.columns)
        .unwrap_or(u32::MAX)
        .min(columns - run.image_col);
    if run_columns == 0 {
        return Ok(None);
    }
    let cell_width = screen.cell_width_hint().round().max(1.0) as u32;
    let cell_height = screen.cell_height_hint().round().max(1.0) as u32;
    let scale = (f64::from(box_width) / f64::from(source.width()))
        .min(f64::from(box_height) / f64::from(source.height()));
    let scaled_width = (f64::from(source.width()) * scale)
        .round()
        .clamp(1.0, f64::from(box_width)) as u32;
    let scaled_height = (f64::from(source.height()) * scale)
        .round()
        .clamp(1.0, f64::from(box_height)) as u32;
    let image_left = (box_width - scaled_width) / 2;
    let image_top = (box_height - scaled_height) / 2;
    let run_left = run.image_col * cell_width;
    let run_top = run.image_row * cell_height;
    let run_width = run_columns * cell_width;
    let run_bottom = run_top + cell_height;
    let intersection_left = run_left.max(image_left);
    let intersection_top = run_top.max(image_top);
    let intersection_right = (run_left + run_width).min(image_left + scaled_width);
    let intersection_bottom = run_bottom.min(image_top + scaled_height);
    if intersection_left >= intersection_right || intersection_top >= intersection_bottom {
        return Ok(None);
    }

    let source_left = u32::try_from(
        (u64::from(intersection_left - image_left) * u64::from(source.width()))
            / u64::from(scaled_width),
    )
    .unwrap_or(0);
    let source_top = u32::try_from(
        (u64::from(intersection_top - image_top) * u64::from(source.height()))
            / u64::from(scaled_height),
    )
    .unwrap_or(0);
    let source_right = u32::try_from(
        (u64::from(intersection_right - image_left) * u64::from(source.width()))
            .div_ceil(u64::from(scaled_width)),
    )
    .unwrap_or(source.width())
    .min(source.width());
    let source_bottom = u32::try_from(
        (u64::from(intersection_bottom - image_top) * u64::from(source.height()))
            .div_ceil(u64::from(scaled_height)),
    )
    .unwrap_or(source.height())
    .min(source.height());
    if source_left >= source_right || source_top >= source_bottom {
        return Ok(None);
    }
    let cropped = imageops::crop_imm(
        source,
        source_left,
        source_top,
        source_right - source_left,
        source_bottom - source_top,
    )
    .to_image();
    let intersection_width = intersection_right - intersection_left;
    let intersection_height = intersection_bottom - intersection_top;
    let resized = imageops::resize(
        &cropped,
        intersection_width,
        intersection_height,
        imageops::FilterType::Triangle,
    );
    let byte_count = u64::from(run_width)
        .checked_mul(u64::from(cell_height))
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_DECODED_BYTES as u64)
        .ok_or_else(|| {
            command
                .echo()
                .error(ErrorCode::NoSpace, "placeholder run is too large")
        })?;
    let mut fragment = RgbaImage::from_raw(run_width, cell_height, vec![0; byte_count as usize])
        .expect("validated placeholder dimensions");
    imageops::overlay(
        &mut fragment,
        &resized,
        i64::from(intersection_left - run_left),
        i64::from(intersection_top - run_top),
    );
    Ok(Some(PreparedPlacement {
        pixels: Arc::new(fragment.into_raw()),
        pixel_width: run_width as usize,
        pixel_height: cell_height as usize,
        cell_width: run_columns as usize,
        cell_height: 1,
    }))
}

fn normalize_frame_gap(gap: i32) -> u32 {
    u32::try_from(gap).unwrap_or(0)
}

fn rect_fits(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    canvas_width: u32,
    canvas_height: u32,
) -> bool {
    x.checked_add(width)
        .is_some_and(|right| right <= canvas_width)
        && y.checked_add(height)
            .is_some_and(|bottom| bottom <= canvas_height)
}

fn rectangles_overlap(
    source_x: u32,
    source_y: u32,
    destination_x: u32,
    destination_y: u32,
    width: u32,
    height: u32,
) -> bool {
    source_x < destination_x.saturating_add(width)
        && source_x.saturating_add(width) > destination_x
        && source_y < destination_y.saturating_add(height)
        && source_y.saturating_add(height) > destination_y
}

#[allow(clippy::too_many_arguments)]
fn composite_rgba_rect(
    destination: &mut [u8],
    destination_width: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    destination_x: u32,
    destination_y: u32,
    mode: FrameComposition,
) {
    composite_rgba_region(
        destination,
        destination_width,
        source,
        source_width,
        0,
        0,
        destination_x,
        destination_y,
        source_width,
        source_height,
        mode,
    );
}

#[allow(clippy::too_many_arguments)]
fn composite_rgba_region(
    destination: &mut [u8],
    canvas_width: u32,
    source: &[u8],
    source_stride: u32,
    source_x: u32,
    source_y: u32,
    destination_x: u32,
    destination_y: u32,
    width: u32,
    height: u32,
    mode: FrameComposition,
) {
    let destination_stride = canvas_width as usize;
    let source_stride = source_stride as usize;
    for row in 0..height as usize {
        for column in 0..width as usize {
            let source_pixel =
                ((source_y as usize + row) * source_stride + source_x as usize + column) * 4;
            let destination_pixel = ((destination_y as usize + row) * destination_stride
                + destination_x as usize
                + column)
                * 4;
            let source_rgba: [u8; 4] = source[source_pixel..source_pixel + 4]
                .try_into()
                .expect("validated source rectangle");
            if mode == FrameComposition::Overwrite {
                destination[destination_pixel..destination_pixel + 4].copy_from_slice(&source_rgba);
            } else {
                let mut destination_rgba = Rgba(
                    destination[destination_pixel..destination_pixel + 4]
                        .try_into()
                        .expect("validated destination rectangle"),
                );
                destination_rgba.blend(&Rgba(source_rgba));
                destination[destination_pixel..destination_pixel + 4]
                    .copy_from_slice(&destination_rgba.0);
            }
        }
    }
}

fn prepare_placement(
    image: &StoredImage,
    command: &Command,
    screen: &Screen,
) -> Result<PreparedPlacement, ProtocolError> {
    let echo = command.echo();
    let source_x = command.source_x.min(image.width);
    let source_y = command.source_y.min(image.height);
    let source_width = if command.source_w == 0 {
        image.width.saturating_sub(source_x)
    } else {
        command.source_w.min(image.width.saturating_sub(source_x))
    };
    let source_height = if command.source_h == 0 {
        image.height.saturating_sub(source_y)
    } else {
        command.source_h.min(image.height.saturating_sub(source_y))
    };
    if source_width == 0 || source_height == 0 {
        return Err(echo.error(ErrorCode::Invalid, "empty source rectangle"));
    }

    let cell_pixel_width = screen.cell_width_hint().round().max(1.0) as u32;
    let cell_pixel_height = screen.cell_height_hint().round().max(1.0) as u32;
    let mut target_width = command
        .columns
        .checked_mul(cell_pixel_width)
        .filter(|width| *width > 0)
        .unwrap_or(source_width);
    let mut target_height = command
        .rows
        .checked_mul(cell_pixel_height)
        .filter(|height| *height > 0)
        .unwrap_or(source_height);
    if command.columns > 0 && command.rows == 0 {
        target_height = ((u64::from(source_height) * u64::from(target_width))
            / u64::from(source_width))
        .max(1) as u32;
    } else if command.rows > 0 && command.columns == 0 {
        target_width = ((u64::from(source_width) * u64::from(target_height))
            / u64::from(source_height))
        .max(1) as u32;
    }
    if target_width > MAX_DIMENSION || target_height > MAX_DIMENSION {
        return Err(echo.error(ErrorCode::Invalid, "placement dimensions too large"));
    }

    let offset_x = command
        .cell_offset_x
        .min(cell_pixel_width.saturating_sub(1));
    let offset_y = command
        .cell_offset_y
        .min(cell_pixel_height.saturating_sub(1));
    let padded_width = target_width
        .checked_add(offset_x)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "placement is too large"))?;
    let padded_height = target_height
        .checked_add(offset_y)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "placement is too large"))?;
    let placement_bytes = u64::from(padded_width)
        .checked_mul(u64::from(padded_height))
        .and_then(|pixels| pixels.checked_mul(4))
        .filter(|bytes| *bytes <= MAX_DECODED_BYTES as u64)
        .ok_or_else(|| echo.error(ErrorCode::NoSpace, "placement is too large"))?;
    let cell_width = if command.columns > 0 {
        command.columns as usize
    } else {
        screen.image_cols_for_width(padded_width as usize)
    }
    .max(1);
    let cell_height = if command.rows > 0 {
        command.rows as usize
    } else {
        screen.image_rows_for_height(padded_height as usize)
    }
    .max(1);

    if source_x == 0
        && source_y == 0
        && source_width == image.width
        && source_height == image.height
        && target_width == image.width
        && target_height == image.height
        && offset_x == 0
        && offset_y == 0
    {
        return Ok(PreparedPlacement {
            pixels: Arc::clone(&image.rgba),
            pixel_width: image.width as usize,
            pixel_height: image.height as usize,
            cell_width,
            cell_height,
        });
    }

    let source = RgbaImage::from_raw(image.width, image.height, image.rgba.as_ref().clone())
        .ok_or_else(|| echo.error(ErrorCode::Invalid, "invalid stored image"))?;
    let cropped =
        imageops::crop_imm(&source, source_x, source_y, source_width, source_height).to_image();
    let resized = if source_width == target_width && source_height == target_height {
        cropped
    } else {
        imageops::resize(
            &cropped,
            target_width,
            target_height,
            imageops::FilterType::Triangle,
        )
    };

    let (rgba, pixel_width, pixel_height) = if offset_x == 0 && offset_y == 0 {
        (resized.into_raw(), target_width, target_height)
    } else {
        let mut padded = RgbaImage::from_raw(
            padded_width,
            padded_height,
            vec![0; placement_bytes as usize],
        )
        .expect("validated RGBA placement dimensions");
        imageops::overlay(
            &mut padded,
            &resized,
            i64::from(offset_x),
            i64::from(offset_y),
        );
        (padded.into_raw(), padded_width, padded_height)
    };
    Ok(PreparedPlacement {
        pixels: Arc::new(rgba),
        pixel_width: pixel_width as usize,
        pixel_height: pixel_height as usize,
        cell_width,
        cell_height,
    })
}

fn image_intersects_rect(
    image: &TerminalImage,
    col: usize,
    line: usize,
    width: usize,
    height: usize,
) -> bool {
    image.col < col.saturating_add(width)
        && image.col.saturating_add(image.cell_width) > col
        && image.line < line.saturating_add(height)
        && image.line.saturating_add(image.cell_height) > line
}

fn image_intersects_cell(image: &TerminalImage, col: usize, line: usize) -> bool {
    image_intersects_rect(image, col, line, 1, 1)
}

fn advance_cursor(screen: &mut Screen, columns: usize, rows: usize) {
    let width = screen.width().max(1);
    let mut lines = rows.saturating_sub(1);
    let target = screen.cursor.col.saturating_add(columns);
    if target >= width {
        screen.cursor.col = 0;
        lines = lines.saturating_add(1);
    } else {
        screen.cursor.col = target;
    }
    for _ in 0..lines {
        screen.line_feed();
    }
}

fn queue_success(screen: &mut Screen, command: &Command, image_id: Option<u32>, query: bool) {
    if command.quiet >= 1
        || (!query && command.image_id.unwrap_or(0) == 0 && command.image_number.is_none())
    {
        return;
    }
    screen.queue_response(format_reply(
        image_id,
        command.image_number,
        command.placement_id,
        "OK",
    ));
}

fn queue_error(screen: &mut Screen, error: &ProtocolError) {
    if error.quiet >= 2 {
        return;
    }
    screen.queue_response(format_reply(
        error.image_id,
        error.image_number,
        error.placement_id,
        &format!("{}:{}", error.code.as_str(), error.message),
    ));
}

fn format_reply(
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    status: &str,
) -> Vec<u8> {
    let mut keys = Vec::new();
    if let Some(image_id) = image_id {
        keys.push(format!("i={image_id}"));
    }
    if let Some(image_number) = image_number {
        keys.push(format!("I={image_number}"));
    }
    if let Some(placement_id) = placement_id {
        keys.push(format!("p={placement_id}"));
    }
    format!("\x1b_G{};{status}\x1b\\", keys.join(",")).into_bytes()
}

#[cfg(test)]
#[path = "kitty_graphics/tests.rs"]
mod tests;
