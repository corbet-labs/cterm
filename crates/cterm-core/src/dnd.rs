//! Wire types and bounded parsing for Kitty's OSC 72 drag-and-drop protocol.
//!
//! Native frontends own operating-system drag sessions and file permissions.
//! This module only validates the PTY-facing control plane, preserves chunk
//! metadata, and provides the capability response shared by every frontend.

/// Maximum encoded payload in one OSC 72 command.
pub const MAX_DND_CHUNK_BYTES: usize = 4096;

/// One of the thirteen command types defined by Kitty's DND protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DndCommandType {
    AcceptDrops,
    StopAcceptingDrops,
    DropMove,
    Drop,
    RequestData,
    RequestError,
    OfferDrag,
    PresentData,
    ChangeDragImage,
    DragOfferEvent,
    DragOfferError,
    UriListData,
    Query,
}

impl DndCommandType {
    fn parse(value: &[u8]) -> Option<Self> {
        Some(match value {
            b"a" => Self::AcceptDrops,
            b"A" => Self::StopAcceptingDrops,
            b"m" => Self::DropMove,
            b"M" => Self::Drop,
            b"r" => Self::RequestData,
            b"R" => Self::RequestError,
            b"o" => Self::OfferDrag,
            b"p" => Self::PresentData,
            b"P" => Self::ChangeDragImage,
            b"e" => Self::DragOfferEvent,
            b"E" => Self::DragOfferError,
            b"k" => Self::UriListData,
            b"q" => Self::Query,
            _ => return None,
        })
    }
}

/// A validated command emitted by the terminal application.
///
/// Coordinates use signed 32-bit values because `-1, -1` is the drop-leave
/// sentinel. `operation` remains an integer because it is also used as image
/// opacity by `t=p`, not only as copy/move flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DndCommand {
    pub command_type: DndCommandType,
    pub more: bool,
    pub client_id: u32,
    pub operation: u32,
    pub cell_x: i32,
    pub cell_y: i32,
    pub pixel_x: i32,
    pub pixel_y: i32,
    pub payload: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct DndMetadata {
    command_type: Option<DndCommandType>,
    more: bool,
    client_id: Option<u32>,
    operation: Option<u32>,
    cell_x: Option<i32>,
    cell_y: Option<i32>,
    pixel_x: Option<i32>,
    pixel_y: Option<i32>,
}

impl DndMetadata {
    fn into_command(self, payload: Vec<u8>) -> DndCommand {
        DndCommand {
            // The protocol defines `a` as the default when `t` is absent.
            command_type: self.command_type.unwrap_or(DndCommandType::AcceptDrops),
            more: self.more,
            client_id: self.client_id.unwrap_or(0),
            operation: self.operation.unwrap_or(0),
            cell_x: self.cell_x.unwrap_or(0),
            cell_y: self.cell_y.unwrap_or(0),
            pixel_x: self.pixel_x.unwrap_or(0),
            pixel_y: self.pixel_y.unwrap_or(0),
            payload,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DndFrame {
    metadata: DndMetadata,
    payload: Vec<u8>,
}

/// Metadata retained across an OSC 72 chunk chain.
///
/// The first chunk is authoritative. Continuations may contain only `m` and
/// `i`; any repeated location or operation fields are deliberately ignored.
#[derive(Debug, Default)]
pub(crate) struct DndProtocolState {
    active: Option<DndMetadata>,
}

impl DndProtocolState {
    pub(crate) fn reset(&mut self) {
        self.active = None;
    }

    pub(crate) fn parse(&mut self, params: &[&[u8]]) -> Option<DndCommand> {
        let Some(frame) = parse_frame(params) else {
            self.active = None;
            return None;
        };

        // Capability queries are explicitly allowed during a chunked transfer
        // and must not disturb the in-progress command.
        if frame.metadata.command_type == Some(DndCommandType::Query) {
            return Some(frame.metadata.into_command(frame.payload));
        }

        if let Some(first) = self.active.as_ref() {
            let first_command_type = first.command_type.unwrap_or(DndCommandType::AcceptDrops);
            if frame
                .metadata
                .command_type
                .is_some_and(|command_type| command_type != first_command_type)
                || frame
                    .metadata
                    .client_id
                    .is_some_and(|client_id| Some(client_id) != first.client_id)
            {
                self.active = None;
                return None;
            }

            let mut metadata = first.clone();
            metadata.more = frame.metadata.more;
            if !metadata.more {
                self.active = None;
            }
            return Some(metadata.into_command(frame.payload));
        }

        let metadata = frame.metadata;
        if metadata.more {
            self.active = Some(metadata.clone());
        }
        Some(metadata.into_command(frame.payload))
    }
}

/// Build the mandatory empty capability response, echoing a multiplexer id.
///
/// A native frontend must send this only after its OS drag adapter is ready;
/// parsing OSC 72 alone is not sufficient to advertise protocol support.
pub fn capability_response(client_id: u32) -> Vec<u8> {
    if client_id == 0 {
        b"\x1b]72;t=q;\x1b\\".to_vec()
    } else {
        format!("\x1b]72;t=q:i={client_id};\x1b\\").into_bytes()
    }
}

/// Parse VTE's semicolon-split OSC parameters for one OSC 72 frame.
fn parse_frame(params: &[&[u8]]) -> Option<DndFrame> {
    let metadata = params.get(1).copied().unwrap_or_default();
    let payload_parts = params.get(2..).unwrap_or_default();
    let payload_len = payload_parts
        .iter()
        .try_fold(0_usize, |length, part| length.checked_add(part.len()))?
        .checked_add(payload_parts.len().saturating_sub(1))?;
    if payload_len > MAX_DND_CHUNK_BYTES {
        return None;
    }

    let mut parsed = DndMetadata::default();
    let mut seen = [false; 8];
    if !metadata.is_empty() {
        for field in metadata.split(|byte| *byte == b':') {
            let (&key, value) = field.split_first()?;
            let value = value.strip_prefix(b"=")?;
            let index = match key {
                b't' => 0,
                b'm' => 1,
                b'i' => 2,
                b'o' => 3,
                b'x' => 4,
                b'y' => 5,
                b'X' => 6,
                b'Y' => 7,
                _ => return None,
            };
            if std::mem::replace(&mut seen[index], true) {
                return None;
            }

            match key {
                b't' => parsed.command_type = Some(DndCommandType::parse(value)?),
                b'm' => {
                    parsed.more = match value {
                        b"0" => false,
                        b"1" => true,
                        _ => return None,
                    };
                }
                b'i' => {
                    let client_id = parse_u32(value)?;
                    if client_id == 0 {
                        return None;
                    }
                    parsed.client_id = Some(client_id);
                }
                b'o' => parsed.operation = Some(parse_u32(value)?),
                b'x' => parsed.cell_x = Some(parse_i32(value)?),
                b'y' => parsed.cell_y = Some(parse_i32(value)?),
                b'X' => parsed.pixel_x = Some(parse_i32(value)?),
                b'Y' => parsed.pixel_y = Some(parse_i32(value)?),
                _ => unreachable!(),
            }
        }
    }

    let mut payload = Vec::with_capacity(payload_len);
    for (index, part) in payload_parts.iter().enumerate() {
        if index > 0 {
            payload.push(b';');
        }
        payload.extend_from_slice(part);
    }

    Some(DndFrame {
        metadata: parsed,
        payload,
    })
}

fn parse_u32(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

fn parse_i32(value: &[u8]) -> Option<i32> {
    if value.is_empty()
        || !value
            .strip_prefix(b"-")
            .unwrap_or(value)
            .iter()
            .all(u8::is_ascii_digit)
    {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(params: &[&[u8]]) -> Option<DndCommand> {
        DndProtocolState::default().parse(params)
    }

    #[test]
    fn recognizes_every_command_type() {
        let cases = [
            (b"a".as_slice(), DndCommandType::AcceptDrops),
            (b"A".as_slice(), DndCommandType::StopAcceptingDrops),
            (b"m".as_slice(), DndCommandType::DropMove),
            (b"M".as_slice(), DndCommandType::Drop),
            (b"r".as_slice(), DndCommandType::RequestData),
            (b"R".as_slice(), DndCommandType::RequestError),
            (b"o".as_slice(), DndCommandType::OfferDrag),
            (b"p".as_slice(), DndCommandType::PresentData),
            (b"P".as_slice(), DndCommandType::ChangeDragImage),
            (b"e".as_slice(), DndCommandType::DragOfferEvent),
            (b"E".as_slice(), DndCommandType::DragOfferError),
            (b"k".as_slice(), DndCommandType::UriListData),
            (b"q".as_slice(), DndCommandType::Query),
        ];

        for (wire, expected) in cases {
            let metadata = [b"t=".as_slice(), wire].concat();
            assert_eq!(parse(&[b"72", &metadata]).unwrap().command_type, expected);
        }
    }

    #[test]
    fn parses_coordinates_operations_and_payload() {
        let command = parse(&[
            b"72",
            b"t=M:i=7:x=12:y=5:X=320:Y=200:o=3",
            b"text/plain text/uri-list",
        ])
        .unwrap();
        assert_eq!(command.command_type, DndCommandType::Drop);
        assert_eq!(command.client_id, 7);
        assert_eq!(command.operation, 3);
        assert_eq!((command.cell_x, command.cell_y), (12, 5));
        assert_eq!((command.pixel_x, command.pixel_y), (320, 200));
        assert_eq!(command.payload, b"text/plain text/uri-list");
    }

    #[test]
    fn accepts_negative_leave_sentinel_and_rejoins_semicolons() {
        let command = parse(&[b"72", b"t=m:x=-1:y=-1", b"one", b"two"]).unwrap();
        assert_eq!((command.cell_x, command.cell_y), (-1, -1));
        assert_eq!(command.payload, b"one;two");
    }

    #[test]
    fn first_chunk_metadata_is_authoritative() {
        let mut state = DndProtocolState::default();
        let first = state
            .parse(&[b"72", b"t=p:i=9:x=2:y=32:X=10:Y=20:m=1", b"YWJj"])
            .unwrap();
        assert!(first.more);

        let last = state.parse(&[b"72", b"i=9:m=0", b"ZA=="]).unwrap();
        assert_eq!(last.command_type, DndCommandType::PresentData);
        assert_eq!(last.client_id, 9);
        assert_eq!(last.cell_x, 2);
        assert_eq!(last.cell_y, 32);
        assert_eq!((last.pixel_x, last.pixel_y), (10, 20));
        assert!(!last.more);
        assert_eq!(last.payload, b"ZA==");
    }

    #[test]
    fn explicit_default_type_can_continue_an_implicit_default_chain() {
        let mut state = DndProtocolState::default();
        let first = state.parse(&[b"72", b"i=9:m=1", b"text/plain"]).unwrap();
        assert_eq!(first.command_type, DndCommandType::AcceptDrops);

        let last = state
            .parse(&[b"72", b"t=a:i=9:m=0", b" text/html"])
            .unwrap();
        assert_eq!(last.command_type, DndCommandType::AcceptDrops);
        assert!(!last.more);
    }

    #[test]
    fn query_does_not_interrupt_chunk_chain() {
        let mut state = DndProtocolState::default();
        state.parse(&[b"72", b"t=a:i=4:m=1", b"text/plain"]);
        let query = state.parse(&[b"72", b"t=q:i=8"]).unwrap();
        assert_eq!(query.command_type, DndCommandType::Query);
        assert_eq!(query.client_id, 8);

        let last = state.parse(&[b"72", b"i=4:m=0", b" text/html"]).unwrap();
        assert_eq!(last.command_type, DndCommandType::AcceptDrops);
        assert_eq!(last.client_id, 4);
    }

    #[test]
    fn mismatched_chunk_resets_chain() {
        let mut state = DndProtocolState::default();
        assert!(state.parse(&[b"72", b"t=p:i=4:m=1", b"YWJj"]).is_some());
        assert!(state.parse(&[b"72", b"t=e:i=4:m=0", b"ZA=="]).is_none());
        assert!(state.parse(&[b"72", b"t=A"]).is_some());
    }

    #[test]
    fn rejects_malformed_and_oversized_frames() {
        assert!(parse(&[b"72", b"t=z"]).is_none());
        assert!(parse(&[b"72", b"t=m:t=M"]).is_none());
        assert!(parse(&[b"72", b"t=m:i=0"]).is_none());
        assert!(parse(&[b"72", b"t=m:x=2147483648"]).is_none());
        assert!(parse(&[b"72", b"t=m:o=-1"]).is_none());

        let oversized = vec![b'x'; MAX_DND_CHUNK_BYTES + 1];
        assert!(parse(&[b"72", b"t=p", &oversized]).is_none());
    }

    #[test]
    fn capability_response_echoes_optional_client_id() {
        assert_eq!(capability_response(0), b"\x1b]72;t=q;\x1b\\");
        assert_eq!(capability_response(42), b"\x1b]72;t=q:i=42;\x1b\\");
    }
}
