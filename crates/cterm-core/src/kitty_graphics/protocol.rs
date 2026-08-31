//! APC framing and bounded Kitty graphics command decoding.

use std::io::{Read, Seek};
use std::path::Path;

use base64::alphabet::STANDARD as BASE64_ALPHABET;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};
use base64::engine::{DecodePaddingMode, Engine as _};

use super::{MAX_DECODED_BYTES, MAX_DIMENSION};
use crate::image_decode::decode_image;

const BASE64_DECODER: GeneralPurpose = GeneralPurpose::new(
    &BASE64_ALPHABET,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);
const MAX_APC_BYTES: usize = 4 * 1024 * 1024;
const MAX_ENCODED_UPLOAD_BYTES: usize = 90 * 1024 * 1024;
const MAX_SHARED_MEMORY_NAME_BYTES: usize = 2 * 1024;
const USAGE_HINT_TRANSIENT: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterceptorState {
    Ground,
    Escape,
    EscapeUnderscore,
    Capturing,
    CapturingEscape,
}

#[derive(Debug)]
pub(super) struct KittyApcInterceptor {
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
    pub(super) fn new() -> Self {
        Self {
            state: InterceptorState::Ground,
            buffer: Vec::new(),
            overflowed: false,
        }
    }

    pub(super) fn advance(&mut self, byte: u8) -> InterceptorResult {
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
pub(super) enum Action {
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
pub(super) enum FrameComposition {
    AlphaBlend,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AnimationState {
    Stopped,
    Loading,
    Running,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Format {
    Rgb24,
    Rgba32,
    Png,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Medium {
    Direct,
    File,
    TempFile,
    SharedMemory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ErrorCode {
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
    pub(super) fn as_str(self) -> &'static str {
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
pub(super) struct ProtocolError {
    pub(super) code: ErrorCode,
    pub(super) message: &'static str,
    pub(super) image_id: Option<u32>,
    pub(super) image_number: Option<u32>,
    pub(super) placement_id: Option<u32>,
    pub(super) quiet: u8,
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct EchoFields {
    image_id: Option<u32>,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    quiet: u8,
}

impl EchoFields {
    pub(super) fn error(self, code: ErrorCode, message: &'static str) -> ProtocolError {
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
pub(super) struct DecodedImage {
    pub(super) rgba: Vec<u8>,
    pub(super) width: u32,
    pub(super) height: u32,
}

#[derive(Debug, Clone)]
pub(super) struct Command {
    pub(super) action: Action,
    pub(super) format: Format,
    pub(super) medium: Medium,
    pub(super) image_id: Option<u32>,
    pub(super) image_number: Option<u32>,
    pub(super) placement_id: Option<u32>,
    pub(super) pixel_width: Option<u32>,
    pub(super) pixel_height: Option<u32>,
    pub(super) compressed: bool,
    pub(super) more_chunks: bool,
    pub(super) data_size: Option<u32>,
    pub(super) data_offset: Option<u32>,
    pub(super) source_x: u32,
    pub(super) source_y: u32,
    pub(super) source_w: u32,
    pub(super) source_h: u32,
    pub(super) columns: u32,
    pub(super) rows: u32,
    pub(super) cell_offset_x: u32,
    pub(super) cell_offset_y: u32,
    pub(super) z_index: i32,
    pub(super) quiet: u8,
    pub(super) suppress_cursor_movement: bool,
    pub(super) unicode_placeholder: bool,
    pub(super) usage_hints: u32,
    pub(super) parent_image_id: u32,
    pub(super) parent_placement_id: u32,
    pub(super) horizontal_offset: i32,
    pub(super) vertical_offset: i32,
    pub(super) delete_specifier: Option<u8>,
    pub(super) frame_number: u32,
    pub(super) other_frame_number: u32,
    pub(super) frame_gap: Option<i32>,
    pub(super) frame_composition: FrameComposition,
    pub(super) frame_background: u32,
    pub(super) frame_source_x: u32,
    pub(super) frame_source_y: u32,
    pub(super) animation_state: Option<AnimationState>,
    pub(super) loop_count: Option<u32>,
    pub(super) image: Option<DecodedImage>,
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
    pub(super) fn echo(&self) -> EchoFields {
        EchoFields {
            image_id: self.image_id,
            image_number: self.image_number,
            placement_id: self.placement_id,
            quiet: self.quiet,
        }
    }

    pub(super) fn is_transient(&self) -> bool {
        self.usage_hints & USAGE_HINT_TRANSIENT != 0
    }
}

#[derive(Debug, Clone)]
struct PendingUpload {
    command: Command,
    payload: Vec<u8>,
}

#[derive(Debug, Default)]
pub(super) struct CommandParser {
    pending: Option<PendingUpload>,
}

impl CommandParser {
    pub(super) fn parse(&mut self, raw: &[u8]) -> Option<Result<Command, ProtocolError>> {
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

pub(super) fn decode_raw(
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
