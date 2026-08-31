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
use image::{imageops, RgbaImage};

use crate::image_decode::decode_image;
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
    delete_specifier: Option<u8>,
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
            delete_specifier: None,
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
        if matches!(command.action, Action::Delete | Action::Display) {
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
    for (key, value) in
        tokenize(control).map_err(|()| echo.error(ErrorCode::Invalid, "malformed control data"))?
    {
        match key {
            b"a" => {
                command.action = match value {
                    b"t" => Action::Transmit,
                    b"T" => Action::TransmitAndDisplay,
                    b"p" => Action::Display,
                    b"d" => Action::Delete,
                    b"q" => Action::Query,
                    b"f" | b"a" | b"c" => {
                        return Err(
                            echo.error(ErrorCode::NotSupported, "animation is not supported")
                        )
                    }
                    _ => return Err(echo.error(ErrorCode::Invalid, "invalid action")),
                }
            }
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
            b"s" => command.pixel_width = Some(required_u32(value, echo, "invalid width")?),
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
            b"c" => command.columns = required_u32(value, echo, "invalid columns")?,
            b"r" => command.rows = required_u32(value, echo, "invalid rows")?,
            b"X" => command.cell_offset_x = required_u32(value, echo, "invalid cell x offset")?,
            b"Y" => command.cell_offset_y = required_u32(value, echo, "invalid cell y offset")?,
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
            b"U" if value == b"0" => {}
            b"U" => {
                return Err(echo.error(
                    ErrorCode::NotSupported,
                    "unicode placeholders are not supported",
                ))
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

#[cfg(any(
    target_os = "freebsd",
    target_os = "linux",
    target_os = "macos",
    windows
))]
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
    #[cfg(unix)]
    if !name.starts_with('/') || name[1..].contains('/') {
        return Err(echo.error(ErrorCode::BadFile, "invalid POSIX shared memory name"));
    }
    let configuration = shared_memory::ShmemConf::new().os_id(name);
    #[cfg(windows)]
    let configuration = configuration.allow_raw(true);
    let mut mapping = configuration
        .open()
        .map_err(|_| echo.error(ErrorCode::BadFile, "could not open shared memory"))?;

    // Kitty transfers ownership of POSIX shared memory cleanup to the
    // terminal. Windows named mappings disappear when all handles close.
    #[cfg(unix)]
    mapping.set_owner(true);

    let start = usize::try_from(command.data_offset.unwrap_or(0))
        .map_err(|_| echo.error(ErrorCode::BadFile, "shared memory offset is out of range"))?;
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
    // SAFETY: `mapping` owns a live mapping of `mapping.len()` bytes. The
    // checked source range and newly allocated destination cannot overlap.
    // The protocol requires the sender to finish writing before sending the
    // APC command; after this snapshot all decoding uses ordinary Rust-owned
    // memory.
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
struct StoredImage {
    rgba: Arc<Vec<u8>>,
    width: u32,
    height: u32,
    lru: u64,
}

#[derive(Debug)]
struct PlacementRecord {
    image_id: u32,
    placement_id: u32,
    z_index: i32,
    allocated_bytes: usize,
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
        self.images.insert(
            image_id,
            StoredImage {
                rgba: Arc::new(image.rgba),
                width: image.width,
                height: image.height,
                lru: self.next_lru,
            },
        );
        if let Some(number) = command.image_number {
            self.image_numbers.entry(number).or_default().push(image_id);
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
        let stored = self
            .images
            .get(&image_id)
            .cloned()
            .ok_or_else(|| command.echo().error(ErrorCode::NotFound, "image not found"))?;
        let placement = prepare_placement(&stored, command, screen)?;
        let placement_id = command.placement_id.unwrap_or(0);
        let replacement = (placement_id != 0).then(|| {
            self.placements
                .iter()
                .find(|(_, placement)| {
                    placement.image_id == image_id && placement.placement_id == placement_id
                })
                .map(|(screen_id, placement)| (*screen_id, placement.allocated_bytes))
        });
        let allocated_bytes = if Arc::ptr_eq(&placement.pixels, &stored.rgba) {
            0
        } else {
            placement.pixels.len()
        };
        let replaced_bytes = replacement.flatten().map_or(0, |(_, bytes)| bytes);
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
        if let Some((screen_id, _)) = replacement.flatten() {
            self.remove_placement(screen_id, screen);
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
            },
        );
        self.next_lru = self.next_lru.saturating_add(1);
        if let Some(image) = self.images.get_mut(&image_id) {
            image.lru = self.next_lru;
        }
        if !command.suppress_cursor_movement {
            advance_cursor(screen, placement.cell_width, placement.cell_height);
        }
        Ok(())
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
        let mut affected_images: Vec<u32> = screen_ids
            .iter()
            .filter_map(|id| self.placements.get(id).map(|placement| placement.image_id))
            .collect();
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
        if free_data {
            for image_id in affected_images {
                if !self.image_has_placements(image_id) {
                    self.remove_image_data(image_id, screen);
                }
            }
        }
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
        if let Some(placement) = self.placements.remove(&screen_id) {
            self.placement_bytes = self
                .placement_bytes
                .saturating_sub(placement.allocated_bytes);
            screen.remove_image(screen_id);
        }
    }

    fn remove_image_data(&mut self, image_id: u32, screen: &mut Screen) {
        self.remove_placements_for_image(image_id, screen);
        if let Some(image) = self.images.remove(&image_id) {
            self.total_bytes = self.total_bytes.saturating_sub(image.rgba.len());
        }
        for ids in self.image_numbers.values_mut() {
            ids.retain(|id| *id != image_id);
        }
        self.image_numbers.retain(|_, ids| !ids.is_empty());
    }

    fn sync_placements(&mut self, screen: &Screen) {
        let mut released = 0usize;
        self.placements.retain(|screen_id, placement| {
            if screen.image_by_id(*screen_id).is_none() {
                released = released.saturating_add(placement.allocated_bytes);
                false
            } else {
                true
            }
        });
        self.placement_bytes = self.placement_bytes.saturating_sub(released);
    }

    fn placement_bytes_for_image(&self, image_id: u32) -> usize {
        self.placements
            .values()
            .filter(|placement| placement.image_id == image_id)
            .map(|placement| placement.allocated_bytes)
            .sum()
    }

    fn image_has_placements(&self, image_id: u32) -> bool {
        self.placements
            .values()
            .any(|placement| placement.image_id == image_id)
    }

    fn projected_usage(&self, incoming: usize, replacement_id: Option<u32>) -> usize {
        let reclaimed = replacement_id.map_or(0, |image_id| {
            self.images
                .get(&image_id)
                .map_or(0, |image| image.rgba.len())
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
                .min_by_key(|(_, image)| image.lru)
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
mod tests {
    use super::*;
    use crate::screen::ScreenConfig;
    use base64::engine::general_purpose::STANDARD;
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;
    use tempfile::{Builder, NamedTempFile};

    fn sequence(control: &str, payload: &[u8]) -> Vec<u8> {
        format!("\x1b_G{control};{}\x1b\\", STANDARD.encode(payload)).into_bytes()
    }

    fn feed(graphics: &mut KittyGraphics, screen: &mut Screen, bytes: &[u8]) -> Vec<u8> {
        let mut forwarded = Vec::new();
        for byte in bytes {
            match graphics.advance(*byte) {
                InterceptorResult::Forward(bytes) => forwarded.extend_from_slice(bytes.as_slice()),
                InterceptorResult::Swallow => {}
                InterceptorResult::Captured(raw) => graphics.handle(&raw, screen),
            }
        }
        forwarded
    }

    #[test]
    fn interceptor_is_lossless_for_non_kitty_sequences() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let input = b"a\x1b[31mb\x1b_not-kitty\x1b\\c";
        assert_eq!(feed(&mut graphics, &mut screen, input), input);
    }

    #[test]
    fn direct_rgb_transmit_and_display_uses_shared_image_pipeline() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let pixels = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=24,s=2,v=2,i=7,C=1", &pixels),
        );

        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].pixel_width, images[0].pixel_height), (2, 2));
        assert_eq!(images[0].data.len(), 16);
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b_Gi=7;OK\x1b\\".to_vec()]
        );
    }

    #[test]
    fn zlib_compressed_rgba_is_inflated_before_display() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let pixels = [10, 20, 30, 40, 50, 60, 70, 80];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&pixels).unwrap();
        let compressed = encoder.finish().unwrap();

        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=2,v=1,o=z,i=8,C=1", &compressed),
        );

        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].data.as_slice(), pixels);
    }

    #[test]
    fn png_payload_is_decoded_into_the_shared_rgba_pipeline() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let png = STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAAAAAA6fptVAAAACklEQVR4AWOwBQAAPwA+Eq7IEAAAAABJRU5ErkJggg==")
            .unwrap();

        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=100,i=12,C=1", &png),
        );

        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].pixel_width, images[0].pixel_height), (1, 1));
        assert_eq!(images[0].data.len(), 4);
    }

    #[test]
    fn file_transfer_obeys_offset_and_size_without_deleting_source() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let mut file = NamedTempFile::new().unwrap();
        file.write_all(&[99, 98, 1, 2, 3, 4, 97]).unwrap();
        file.flush().unwrap();
        let path = file.path().to_owned();

        feed(
            &mut graphics,
            &mut screen,
            &sequence(
                "a=T,t=f,f=32,s=1,v=1,O=2,S=4,i=13,C=1",
                path.as_os_str().as_encoded_bytes(),
            ),
        );

        assert!(path.exists());
        assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
    }

    #[cfg(any(
        target_os = "freebsd",
        target_os = "linux",
        target_os = "macos",
        windows
    ))]
    #[test]
    fn shared_memory_transfer_obeys_range_and_cleanup_semantics() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let mut mapping = shared_memory::ShmemConf::new().size(7).create().unwrap();
        let name = mapping.get_os_id().to_owned();
        // SAFETY: this test created and exclusively owns the mapping, and the
        // terminal is not asked to open it until after the write completes.
        unsafe {
            mapping
                .as_slice_mut()
                .copy_from_slice(&[99, 98, 1, 2, 3, 4, 97]);
        }

        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,t=s,f=32,s=1,v=1,O=2,S=4,i=14,C=1", name.as_bytes()),
        );

        assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b_Gi=14;OK\x1b\\".to_vec()]
        );
        #[cfg(unix)]
        assert!(shared_memory::ShmemConf::new().os_id(name).open().is_err());
    }

    #[cfg(unix)]
    #[test]
    fn shared_memory_errors_still_unlink_the_posix_object() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let mapping = shared_memory::ShmemConf::new().size(4).create().unwrap();
        let name = mapping.get_os_id().to_owned();

        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,t=s,f=32,s=1,v=1,O=5,i=15,C=1", name.as_bytes()),
        );

        assert!(screen.images().is_empty());
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b_Gi=15;EBADF:shared memory offset is out of range\x1b\\".to_vec()]
        );
        assert!(shared_memory::ShmemConf::new().os_id(name).open().is_err());
    }

    #[test]
    fn protocol_named_temp_transfer_is_removed_after_reading() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let mut file = Builder::new()
            .prefix("tty-graphics-protocol-")
            .tempfile()
            .unwrap();
        file.write_all(&[1, 2, 3, 4]).unwrap();
        file.flush().unwrap();
        let (handle, path) = file.keep().unwrap();
        drop(handle);

        feed(
            &mut graphics,
            &mut screen,
            &sequence(
                "a=T,t=t,f=32,s=1,v=1,i=14,C=1",
                path.as_os_str().as_encoded_bytes(),
            ),
        );

        assert!(!path.exists());
        assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
    }

    #[test]
    fn crop_scale_and_offsets_produce_bounded_placement_geometry() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        screen.set_cell_width_hint(4.0);
        screen.set_cell_height_hint(4.0);
        let pixels = [1, 2, 3, 4, 10, 20, 30, 40, 5, 6, 7, 8, 50, 60, 70, 80];

        feed(
            &mut graphics,
            &mut screen,
            &sequence(
                "a=T,f=32,s=2,v=2,x=1,y=0,w=1,h=2,c=1,r=1,X=1,Y=2,i=15,C=1",
                &pixels,
            ),
        );

        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_eq!((images[0].pixel_width, images[0].pixel_height), (5, 6));
        assert_eq!((images[0].cell_width, images[0].cell_height), (1, 1));
        assert_eq!(&images[0].data[..4 * 5 * 2], &[0; 4 * 5 * 2]);
        assert_eq!(&images[0].data[4 * 5 * 2..4 * 5 * 2 + 4], &[0; 4]);
        assert_ne!(&images[0].data[4 * 5 * 2 + 4..4 * 5 * 2 + 8], &[0; 4]);
        assert_eq!(graphics.placement_bytes, 5 * 6 * 4);
    }

    #[test]
    fn placement_cursor_motion_uses_the_full_rectangle_and_wrap_policy() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(6, 6, ScreenConfig::default());
        screen.cursor.col = 1;
        screen.cursor.row = 1;
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,c=2,r=3,i=16", &[1, 2, 3, 4]),
        );
        assert_eq!((screen.cursor.col, screen.cursor.row), (3, 3));

        screen.cursor.col = 5;
        screen.cursor.row = 1;
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=16,c=2,r=2\x1b\\");
        assert_eq!((screen.cursor.col, screen.cursor.row), (0, 3));
    }

    #[test]
    fn chunked_upload_and_later_placement_preserve_identity() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        let encoded = STANDARD.encode([1, 2, 3, 255]);
        let first = format!("\x1b_Ga=t,f=32,s=1,v=1,i=9,m=1;{}\x1b\\", &encoded[..4]);
        let second = format!("\x1b_Gm=0;{}\x1b\\", &encoded[4..]);
        feed(&mut graphics, &mut screen, first.as_bytes());
        assert!(screen.images().is_empty());
        feed(&mut graphics, &mut screen, second.as_bytes());
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=9,p=4,C=1\x1b\\");
        assert_eq!(screen.images().len(), 1);
        assert_eq!(screen.take_pending_responses().len(), 2);
    }

    #[test]
    fn query_replies_without_storing_the_probe() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=q,f=24,s=1,v=1,i=31", &[0, 0, 0]),
        );
        assert!(screen.images().is_empty());
        assert!(graphics.images.is_empty());
        assert_eq!(
            screen.take_pending_responses(),
            vec![b"\x1b_Gi=31;OK\x1b\\".to_vec()]
        );
    }

    #[test]
    fn uppercase_delete_removes_placement_and_image_data() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=5,C=1", &[1, 2, 3, 4]),
        );
        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=5\x1b\\");
        assert!(screen.images().is_empty());
        assert!(graphics.images.is_empty());
    }

    #[test]
    fn replacing_a_placement_removes_the_previous_screen_image() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=21,p=2,C=1", &[1, 2, 3, 4]),
        );
        let first_screen_id = screen.images()[0].id;

        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=21,p=2,C=1\x1b\\");

        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_ne!(images[0].id, first_screen_id);
    }

    #[test]
    fn anonymous_placements_accumulate_while_named_placements_replace() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,i=40", &[1, 2, 3, 4]),
        );
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,C=1\x1b\\");
        screen.cursor.col = 2;
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,C=1\x1b\\");
        assert_eq!(screen.images().len(), 2);
        assert_eq!(graphics.placements.len(), 2);

        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,p=7,C=1\x1b\\");
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=40,p=7,C=1\x1b\\");
        assert_eq!(screen.images().len(), 3);
        assert_eq!(graphics.placements.len(), 3);
    }

    #[test]
    fn image_delete_can_target_one_named_placement_without_freeing_shared_data() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,i=50", &[1, 2, 3, 4]),
        );
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=50,p=1,C=1\x1b\\");
        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,i=50,p=2,C=1\x1b\\");

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=50,p=1\x1b\\");
        assert_eq!(screen.images().len(), 1);
        assert!(graphics.images.contains_key(&50));
        assert_eq!(graphics.placements.len(), 1);

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=I,i=50,p=2\x1b\\");
        assert!(screen.images().is_empty());
        assert!(!graphics.images.contains_key(&50));
    }

    #[test]
    fn image_numbers_keep_history_and_delete_only_the_newest_match() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,I=9", &[1, 2, 3, 4]),
        );
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,I=9", &[5, 6, 7, 8]),
        );
        assert_eq!(graphics.images.len(), 2);
        assert_eq!(graphics.image_numbers.get(&9).unwrap().len(), 2);

        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,I=9,p=1,C=1\x1b\\");
        assert_eq!(screen.images()[0].data.as_slice(), [5, 6, 7, 8]);
        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=N,I=9\x1b\\");
        assert_eq!(graphics.images.len(), 1);
        assert_eq!(graphics.image_numbers.get(&9).unwrap().len(), 1);

        feed(&mut graphics, &mut screen, b"\x1b_Ga=p,I=9,p=2,C=1\x1b\\");
        assert_eq!(screen.images()[0].data.as_slice(), [1, 2, 3, 4]);
    }

    #[test]
    fn geometric_and_z_delete_selectors_match_only_intersecting_placements() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,i=60", &[1, 2, 3, 4]),
        );
        screen.cursor.col = 1;
        feed(
            &mut graphics,
            &mut screen,
            b"\x1b_Ga=p,i=60,p=1,z=-1,C=1\x1b\\",
        );
        screen.cursor.col = 4;
        feed(
            &mut graphics,
            &mut screen,
            b"\x1b_Ga=p,i=60,p=2,z=3,C=1\x1b\\",
        );

        feed(
            &mut graphics,
            &mut screen,
            b"\x1b_Ga=d,d=q,x=2,y=1,z=-1\x1b\\",
        );
        assert_eq!(screen.images().len(), 1);
        assert_eq!(graphics.placements.values().next().unwrap().z_index, 3);

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=x,x=5\x1b\\");
        assert!(screen.images().is_empty());
        assert!(graphics.images.contains_key(&60));
    }

    #[test]
    fn placement_preserves_text_and_exposes_renderer_layer_metadata() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        screen.put_char('X');
        screen.cursor.col = 0;

        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=70,z=-1,C=1", &[1, 2, 3, 4]),
        );

        assert_eq!(screen.get_cell(0, 0).unwrap().text(), "X");
        let image = screen.images()[0];
        assert_eq!(image.z_index, -1);
        assert_eq!(image.protocol_image_id, 70);
        assert_eq!(image.layer(), crate::ImageLayer::BehindText);
    }

    #[test]
    fn cursor_row_and_id_range_delete_selectors_are_independent() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        screen.cursor.col = 1;
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=80,p=1,C=1", &[1, 2, 3, 4]),
        );
        screen.cursor.col = 4;
        screen.cursor.row = 1;
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=81,p=1,C=1", &[5, 6, 7, 8]),
        );
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,i=82", &[9, 10, 11, 12]),
        );

        screen.cursor.col = 1;
        screen.cursor.row = 0;
        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=c\x1b\\");
        assert_eq!(screen.images().len(), 1);
        assert!(graphics.images.contains_key(&80));

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=y,y=2\x1b\\");
        assert!(screen.images().is_empty());
        assert!(graphics.images.contains_key(&81));

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=R,x=80,y=81\x1b\\");
        assert!(!graphics.images.contains_key(&80));
        assert!(!graphics.images.contains_key(&81));
        assert!(graphics.images.contains_key(&82));
    }

    #[test]
    fn delete_all_affects_the_live_viewport_but_preserves_history_placements() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 3, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=70,C=1", &[1, 2, 3, 4]),
        );
        screen.cursor.row = 2;
        screen.line_feed();
        screen.cursor.row = 2;
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=71,C=1", &[5, 6, 7, 8]),
        );

        feed(&mut graphics, &mut screen, b"\x1b_Ga=d,d=A\x1b\\");
        let images = screen.images();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].line, 0);
        assert!(graphics.images.contains_key(&70));
        assert!(!graphics.images.contains_key(&71));
    }

    #[test]
    fn quota_eviction_preserves_visible_images_and_reconciles_cleared_placements() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=t,f=32,s=1,v=1,i=30", &[1, 2, 3, 4]),
        );
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=31,C=1", &[5, 6, 7, 8]),
        );

        graphics.evict_to_fit(STORE_QUOTA_BYTES, None, &mut screen);
        assert!(!graphics.images.contains_key(&30));
        assert!(graphics.images.contains_key(&31));
        assert_eq!(screen.images().len(), 1);

        screen.clear_images();
        graphics.evict_to_fit(STORE_QUOTA_BYTES, None, &mut screen);
        assert!(graphics.images.is_empty());
        assert!(graphics.placements.is_empty());
        assert_eq!(graphics.placement_bytes, 0);
    }

    #[test]
    fn rgb_expansion_is_rejected_before_it_can_exceed_the_decoded_budget() {
        let command = Command {
            format: Format::Rgb24,
            pixel_width: Some(10_000),
            pixel_height: Some(1_700),
            ..Command::default()
        };

        let error = decode_raw(&command, Vec::new(), command.echo()).unwrap_err();
        assert_eq!(error.code, ErrorCode::NoSpace);
    }

    #[test]
    fn quiet_levels_suppress_success_and_then_error_replies() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            &sequence("a=T,f=32,s=1,v=1,i=22,q=1,C=1", &[1, 2, 3, 4]),
        );
        assert!(screen.take_pending_responses().is_empty());

        feed(
            &mut graphics,
            &mut screen,
            b"\x1b_Ga=T,f=32,s=2,v=2,i=23,q=2;AAAA\x1b\\",
        );
        assert!(screen.take_pending_responses().is_empty());
    }

    #[test]
    fn invalid_payload_returns_a_bounded_protocol_error() {
        let mut graphics = KittyGraphics::default();
        let mut screen = Screen::new(10, 5, ScreenConfig::default());
        feed(
            &mut graphics,
            &mut screen,
            b"\x1b_Ga=T,f=24,s=2,v=2,i=3;AAAA\x1b\\",
        );
        let reply = screen.take_pending_responses().pop().unwrap();
        assert!(String::from_utf8(reply).unwrap().contains("ENODATA"));
        assert!(screen.images().is_empty());
    }
}
