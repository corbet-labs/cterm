//! Bounded wire codec for Kitty's OSC 5113 file-transfer protocol.
//!
//! The terminal-side session and filesystem policy intentionally live above
//! this module. Decoding an escape sequence must never grant file access.
//! Protocol: <https://sw.kovidgoyal.net/kitty/file-transfer-protocol/>

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use thiserror::Error;

/// Maximum decoded payload in one `data` or `end_data` command.
pub const MAX_FILE_TRANSFER_CHUNK_BYTES: usize = 4096;
/// Maximum UTF-8 path length defined by the protocol.
pub const MAX_FILE_TRANSFER_PATH_BYTES: usize = 4096;
/// Defensive bound for identifiers and bypass credentials.
pub const MAX_FILE_TRANSFER_SAFE_STRING_BYTES: usize = 512;
/// Defensive bound for decoded status text.
pub const MAX_FILE_TRANSFER_STATUS_BYTES: usize = 4096;
/// Maximum decoded commands retained until the frontend drains the queue.
pub const MAX_PENDING_FILE_TRANSFER_COMMANDS: usize = 256;

/// OSC 5113 action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferAction {
    Send,
    File,
    Data,
    EndData,
    Receive,
    Cancel,
    Status,
    Finish,
    /// Receive-side completion is called `finished` in the protocol flow.
    Finished,
}

impl FileTransferAction {
    fn parse(value: &[u8]) -> Option<Self> {
        Some(match value {
            b"send" => Self::Send,
            b"file" => Self::File,
            b"data" => Self::Data,
            b"end_data" => Self::EndData,
            b"receive" => Self::Receive,
            b"cancel" => Self::Cancel,
            b"status" => Self::Status,
            b"finish" => Self::Finish,
            b"finished" => Self::Finished,
            _ => return None,
        })
    }

    fn as_bytes(self) -> &'static [u8] {
        match self {
            Self::Send => b"send",
            Self::File => b"file",
            Self::Data => b"data",
            Self::EndData => b"end_data",
            Self::Receive => b"receive",
            Self::Cancel => b"cancel",
            Self::Status => b"status",
            Self::Finish => b"finish",
            Self::Finished => b"finished",
        }
    }
}

/// Compression requested for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferCompression {
    None,
    Zlib,
}

/// File type carried by metadata commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransferType {
    Regular,
    Directory,
    Symlink,
    Link,
}

/// Payload transmission strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTransmissionType {
    Simple,
    Rsync,
}

/// One decoded OSC 5113 command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTransferCommand {
    pub action: FileTransferAction,
    pub id: String,
    pub file_id: Option<String>,
    pub bypass: Option<String>,
    pub quiet: u8,
    pub mtime: Option<i64>,
    pub permissions: Option<u32>,
    pub size: Option<u64>,
    pub name: Option<String>,
    pub status: Option<String>,
    pub parent: Option<String>,
    pub data: Vec<u8>,
    pub compression: Option<FileTransferCompression>,
    pub file_type: Option<FileTransferType>,
    pub transmission_type: Option<FileTransmissionType>,
}

/// Malformed or unbounded OSC 5113 input.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileTransferCodecError {
    #[error("missing OSC 5113 action")]
    MissingAction,
    #[error("missing OSC 5113 session id")]
    MissingId,
    #[error("invalid OSC 5113 field: {0}")]
    InvalidField(&'static str),
    #[error("duplicate OSC 5113 field: {0}")]
    DuplicateField(&'static str),
    #[error("OSC 5113 {0} exceeds its size limit")]
    LimitExceeded(&'static str),
}

impl FileTransferCommand {
    /// Encode this command with the seven-bit OSC and ST forms.
    pub fn encode(&self) -> Result<Vec<u8>, FileTransferCodecError> {
        validate_safe_string("id", self.id.as_bytes())?;
        if self.quiet > 2 {
            return Err(FileTransferCodecError::InvalidField("quiet"));
        }
        validate_optional_safe_string("file_id", self.file_id.as_deref())?;
        validate_optional_safe_string("bypass", self.bypass.as_deref())?;
        validate_optional_safe_string("parent", self.parent.as_deref())?;
        validate_decoded_lengths(self)?;

        let mut fields = Vec::with_capacity(256 + self.data.len().saturating_mul(4) / 3);
        fields.extend_from_slice(b"\x1b]5113;ac=");
        fields.extend_from_slice(self.action.as_bytes());
        append_raw(&mut fields, b"id", self.id.as_bytes());
        if let Some(value) = &self.file_id {
            append_raw(&mut fields, b"fid", value.as_bytes());
        }
        if let Some(value) = &self.bypass {
            append_raw(&mut fields, b"pw", value.as_bytes());
        }
        if self.quiet != 0 {
            append_raw(&mut fields, b"q", self.quiet.to_string().as_bytes());
        }
        if let Some(value) = self.mtime {
            append_raw(&mut fields, b"mod", value.to_string().as_bytes());
        }
        if let Some(value) = self.permissions {
            append_raw(&mut fields, b"prm", value.to_string().as_bytes());
        }
        if let Some(value) = self.size {
            append_raw(&mut fields, b"sz", value.to_string().as_bytes());
        }
        if let Some(value) = &self.name {
            append_base64(&mut fields, b"n", value.as_bytes());
        }
        if let Some(value) = &self.status {
            append_base64(&mut fields, b"st", value.as_bytes());
        }
        if let Some(value) = &self.parent {
            append_raw(&mut fields, b"pr", value.as_bytes());
        }
        if !self.data.is_empty() {
            append_base64(&mut fields, b"d", &self.data);
        }
        if let Some(value) = self.compression {
            append_raw(
                &mut fields,
                b"zip",
                match value {
                    FileTransferCompression::None => b"none",
                    FileTransferCompression::Zlib => b"zlib",
                },
            );
        }
        if let Some(value) = self.file_type {
            append_raw(
                &mut fields,
                b"ft",
                match value {
                    FileTransferType::Regular => b"regular",
                    FileTransferType::Directory => b"directory",
                    FileTransferType::Symlink => b"symlink",
                    FileTransferType::Link => b"link",
                },
            );
        }
        if let Some(value) = self.transmission_type {
            append_raw(
                &mut fields,
                b"tt",
                match value {
                    FileTransmissionType::Simple => b"simple",
                    FileTransmissionType::Rsync => b"rsync",
                },
            );
        }
        fields.extend_from_slice(b"\x1b\\");
        Ok(fields)
    }
}

/// Parse VTE's semicolon-split parameters for one OSC 5113 command.
pub fn parse_file_transfer_command(
    params: &[&[u8]],
) -> Result<FileTransferCommand, FileTransferCodecError> {
    let mut action = None;
    let mut id = None;
    let mut file_id = None;
    let mut bypass = None;
    let mut quiet = 0;
    let mut mtime = None;
    let mut permissions = None;
    let mut size = None;
    let mut name = None;
    let mut status = None;
    let mut parent = None;
    let mut data = Vec::new();
    let mut compression = None;
    let mut file_type = None;
    let mut transmission_type = None;
    let mut seen = 0_u16;

    for raw_field in params.get(1..).unwrap_or_default() {
        let field = raw_field.trim_ascii();
        if field.is_empty() {
            continue;
        }
        let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
            return Err(FileTransferCodecError::InvalidField("syntax"));
        };
        let (key, value) = (&field[..separator], &field[separator + 1..]);
        let key = key.trim_ascii();
        let value = value.trim_ascii();
        if key.is_empty()
            || !key
                .iter()
                .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
        {
            return Err(FileTransferCodecError::InvalidField("key"));
        }

        match key {
            b"ac" => {
                mark_seen(&mut seen, 0, "action")?;
                action = FileTransferAction::parse(value);
                if action.is_none() {
                    return Err(FileTransferCodecError::InvalidField("action"));
                }
            }
            b"id" => {
                mark_seen(&mut seen, 1, "id")?;
                id = Some(parse_safe_string("id", value)?);
            }
            b"fid" => {
                mark_seen(&mut seen, 2, "file_id")?;
                file_id = Some(parse_safe_string("file_id", value)?);
            }
            b"pw" => {
                mark_seen(&mut seen, 3, "bypass")?;
                bypass = Some(parse_safe_string("bypass", value)?);
            }
            b"q" => {
                mark_seen(&mut seen, 4, "quiet")?;
                quiet = parse_number("quiet", value)?;
                if quiet > 2 {
                    return Err(FileTransferCodecError::InvalidField("quiet"));
                }
            }
            b"mod" => {
                mark_seen(&mut seen, 5, "mtime")?;
                mtime = Some(parse_number("mtime", value)?);
            }
            b"prm" => {
                mark_seen(&mut seen, 6, "permissions")?;
                permissions = Some(parse_number("permissions", value)?);
            }
            b"sz" => {
                mark_seen(&mut seen, 7, "size")?;
                size = Some(parse_number("size", value)?);
            }
            b"n" => {
                mark_seen(&mut seen, 8, "name")?;
                name = Some(decode_utf8("name", value, MAX_FILE_TRANSFER_PATH_BYTES)?);
            }
            b"st" => {
                mark_seen(&mut seen, 9, "status")?;
                status = Some(decode_utf8(
                    "status",
                    value,
                    MAX_FILE_TRANSFER_STATUS_BYTES,
                )?);
            }
            b"pr" => {
                mark_seen(&mut seen, 10, "parent")?;
                parent = Some(parse_safe_string("parent", value)?);
            }
            b"d" => {
                mark_seen(&mut seen, 11, "data")?;
                data = decode_base64("data", value, MAX_FILE_TRANSFER_CHUNK_BYTES)?;
            }
            b"zip" => {
                mark_seen(&mut seen, 12, "compression")?;
                compression = Some(match value {
                    b"none" => FileTransferCompression::None,
                    b"zlib" => FileTransferCompression::Zlib,
                    _ => return Err(FileTransferCodecError::InvalidField("compression")),
                });
            }
            b"ft" => {
                mark_seen(&mut seen, 13, "file_type")?;
                file_type = Some(match value {
                    b"regular" => FileTransferType::Regular,
                    b"directory" => FileTransferType::Directory,
                    b"symlink" => FileTransferType::Symlink,
                    b"link" => FileTransferType::Link,
                    _ => return Err(FileTransferCodecError::InvalidField("file_type")),
                });
            }
            b"tt" => {
                mark_seen(&mut seen, 14, "transmission_type")?;
                transmission_type = Some(match value {
                    b"simple" => FileTransmissionType::Simple,
                    b"rsync" => FileTransmissionType::Rsync,
                    _ => return Err(FileTransferCodecError::InvalidField("transmission_type")),
                });
            }
            _ => {}
        }
    }

    Ok(FileTransferCommand {
        action: action.ok_or(FileTransferCodecError::MissingAction)?,
        id: id.ok_or(FileTransferCodecError::MissingId)?,
        file_id,
        bypass,
        quiet,
        mtime,
        permissions,
        size,
        name,
        status,
        parent,
        data,
        compression,
        file_type,
        transmission_type,
    })
}

fn mark_seen(seen: &mut u16, bit: u8, field: &'static str) -> Result<(), FileTransferCodecError> {
    let mask = 1_u16 << bit;
    if *seen & mask != 0 {
        return Err(FileTransferCodecError::DuplicateField(field));
    }
    *seen |= mask;
    Ok(())
}

fn parse_safe_string(field: &'static str, value: &[u8]) -> Result<String, FileTransferCodecError> {
    validate_safe_string(field, value)?;
    String::from_utf8(value.to_vec()).map_err(|_| FileTransferCodecError::InvalidField(field))
}

fn validate_safe_string(field: &'static str, value: &[u8]) -> Result<(), FileTransferCodecError> {
    if value.is_empty()
        || !value.iter().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'.' | b'/' | b'@' | b'-')
        })
    {
        return Err(FileTransferCodecError::InvalidField(field));
    }
    if value.len() > MAX_FILE_TRANSFER_SAFE_STRING_BYTES {
        return Err(FileTransferCodecError::LimitExceeded(field));
    }
    Ok(())
}

fn validate_optional_safe_string(
    field: &'static str,
    value: Option<&str>,
) -> Result<(), FileTransferCodecError> {
    if let Some(value) = value {
        validate_safe_string(field, value.as_bytes())?;
    }
    Ok(())
}

fn parse_number<T>(field: &'static str, value: &[u8]) -> Result<T, FileTransferCodecError>
where
    T: std::str::FromStr,
{
    let value = if value.is_empty() { b"0" } else { value };
    std::str::from_utf8(value)
        .ok()
        .and_then(|value| value.parse().ok())
        .ok_or(FileTransferCodecError::InvalidField(field))
}

fn decode_utf8(
    field: &'static str,
    value: &[u8],
    limit: usize,
) -> Result<String, FileTransferCodecError> {
    let decoded = decode_base64(field, value, limit)?;
    String::from_utf8(decoded).map_err(|_| FileTransferCodecError::InvalidField(field))
}

fn decode_base64(
    field: &'static str,
    value: &[u8],
    limit: usize,
) -> Result<Vec<u8>, FileTransferCodecError> {
    if value.len() > limit.saturating_add(2) / 3 * 4 {
        return Err(FileTransferCodecError::LimitExceeded(field));
    }
    let decoded = BASE64
        .decode(value)
        .map_err(|_| FileTransferCodecError::InvalidField(field))?;
    if decoded.len() > limit {
        return Err(FileTransferCodecError::LimitExceeded(field));
    }
    Ok(decoded)
}

fn validate_decoded_lengths(command: &FileTransferCommand) -> Result<(), FileTransferCodecError> {
    if command
        .name
        .as_ref()
        .is_some_and(|value| value.len() > MAX_FILE_TRANSFER_PATH_BYTES)
    {
        return Err(FileTransferCodecError::LimitExceeded("name"));
    }
    if command
        .status
        .as_ref()
        .is_some_and(|value| value.len() > MAX_FILE_TRANSFER_STATUS_BYTES)
    {
        return Err(FileTransferCodecError::LimitExceeded("status"));
    }
    if command.data.len() > MAX_FILE_TRANSFER_CHUNK_BYTES {
        return Err(FileTransferCodecError::LimitExceeded("data"));
    }
    Ok(())
}

fn append_raw(output: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    output.push(b';');
    output.extend_from_slice(key);
    output.push(b'=');
    output.extend_from_slice(value);
}

fn append_base64(output: &mut Vec<u8>, key: &[u8], value: &[u8]) {
    append_raw(output, key, BASE64.encode(value).as_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(encoded: &[u8]) -> Result<FileTransferCommand, FileTransferCodecError> {
        let body = encoded
            .strip_prefix(b"\x1b]5113;")
            .and_then(|body| body.strip_suffix(b"\x1b\\"))
            .unwrap_or(encoded);
        let mut params = vec![&b"5113"[..]];
        params.extend(body.split(|byte| *byte == b';'));
        parse_file_transfer_command(&params)
    }

    #[test]
    fn parses_spec_example_and_ignores_unknown_keys() {
        let command = parse(b"ac=send;id=test;n=c29tZWZpbGU=;sz=3;d=AQID;future=ignored").unwrap();
        assert_eq!(command.action, FileTransferAction::Send);
        assert_eq!(command.id, "test");
        assert_eq!(command.name.as_deref(), Some("somefile"));
        assert_eq!(command.size, Some(3));
        assert_eq!(command.data, [1, 2, 3]);
    }

    #[test]
    fn supports_all_metadata_and_round_trips() {
        let command = FileTransferCommand {
            action: FileTransferAction::File,
            id: "session-1".into(),
            file_id: Some("file-2".into()),
            bypass: Some("sha256:abcd".into()),
            quiet: 1,
            mtime: Some(1_725_000_000_000_000_000),
            permissions: Some(0o755),
            size: Some(12),
            name: Some("/tmp/Grüße.txt".into()),
            status: Some("STARTED".into()),
            parent: Some("root".into()),
            data: b"hello world!".to_vec(),
            compression: Some(FileTransferCompression::Zlib),
            file_type: Some(FileTransferType::Regular),
            transmission_type: Some(FileTransmissionType::Rsync),
        };

        assert_eq!(parse(&command.encode().unwrap()).unwrap(), command);
    }

    #[test]
    fn accepts_receive_completion_action_used_by_the_protocol_flow() {
        assert_eq!(
            parse(b"ac=finished;id=receive-1").unwrap().action,
            FileTransferAction::Finished
        );
    }

    #[test]
    fn rejects_duplicate_invalid_and_missing_required_fields() {
        assert_eq!(
            parse(b"ac=send;ac=receive;id=x"),
            Err(FileTransferCodecError::DuplicateField("action"))
        );
        assert_eq!(
            parse(b"ac=send;id=bad;id"),
            Err(FileTransferCodecError::InvalidField("syntax"))
        );
        assert_eq!(
            parse(b"ac=send;id=bad!value"),
            Err(FileTransferCodecError::InvalidField("id"))
        );
        assert_eq!(parse(b"id=x"), Err(FileTransferCodecError::MissingAction));
        assert_eq!(parse(b"ac=send"), Err(FileTransferCodecError::MissingId));
    }

    #[test]
    fn rejects_out_of_range_values_and_payloads() {
        assert_eq!(
            parse(b"ac=send;id=x;q=3"),
            Err(FileTransferCodecError::InvalidField("quiet"))
        );
        assert_eq!(
            parse(b"ac=file;id=x;prm=-1"),
            Err(FileTransferCodecError::InvalidField("permissions"))
        );

        let oversized = vec![0_u8; MAX_FILE_TRANSFER_CHUNK_BYTES + 1];
        let encoded = BASE64.encode(oversized);
        let command = format!("ac=data;id=x;fid=f;d={encoded}");
        assert_eq!(
            parse(command.as_bytes()),
            Err(FileTransferCodecError::LimitExceeded("data"))
        );
    }

    #[test]
    fn whitespace_in_the_documented_display_form_is_ignored() {
        let command = parse(b" ac=send ; id=test ; n=c29tZWZpbGU= ").unwrap();
        assert_eq!(command.name.as_deref(), Some("somefile"));
    }
}
