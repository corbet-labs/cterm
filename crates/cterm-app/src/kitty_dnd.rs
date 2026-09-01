//! UI-neutral destination state for Kitty's OSC 72 drag-and-drop protocol.
//!
//! Native frontends translate OS drag events into calls on [`DndDestination`]
//! and write the returned frames to the PTY. Keeping negotiation and resource
//! limits here gives GTK, Cocoa, and Win32 the same protocol behavior.

use base64::Engine;
use cterm_core::{dnd::capability_response, DndCommand, DndCommandType, MAX_DND_CHUNK_BYTES};

/// MIME type used for local file URL drops on every supported desktop.
pub const URI_LIST_MIME: &str = "text/uri-list";
/// Kitty's reference implementation caps accumulated MIME lists at one MiB.
pub const MAX_DND_MIME_LIST_BYTES: usize = 1024 * 1024;
/// Bound retained OS data until the terminal application requests it.
pub const MAX_LOCAL_DROP_BYTES: usize = 16 * 1024 * 1024;
const MAX_DND_MIME_TYPES: usize = 4096;

/// Copy/move operation negotiated between the OS and terminal application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DndOperation {
    #[default]
    None,
    Copy,
    Move,
    Either,
}

impl DndOperation {
    fn from_client(value: u32) -> Self {
        match value {
            1 => Self::Copy,
            2 => Self::Move,
            _ => Self::None,
        }
    }

    fn protocol_value(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Copy => 1,
            Self::Move => 2,
            Self::Either => 3,
        }
    }

    fn permits(self, selected: Self) -> bool {
        matches!(
            (self, selected),
            (Self::Copy, Self::Copy)
                | (Self::Move, Self::Move)
                | (Self::Either, Self::Copy | Self::Move)
        )
    }
}

/// Cell and pixel position reported with a native drag event.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DndLocation {
    pub cell_x: i32,
    pub cell_y: i32,
    pub pixel_x: i32,
    pub pixel_y: i32,
}

/// One lazily requested MIME value retained after an OS drop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DropData {
    pub mime_type: String,
    pub data: Vec<u8>,
}

impl DropData {
    /// Construct a drop value after validating its MIME token.
    pub fn new(mime_type: impl Into<String>, data: Vec<u8>) -> Result<Self, DndError> {
        let mime_type = mime_type.into();
        validate_mime(&mime_type)?;
        Ok(Self { mime_type, data })
    }

    /// Construct a local `text/uri-list` value.
    pub fn uri_list(data: Vec<u8>) -> Self {
        Self {
            mime_type: URI_LIST_MIME.to_string(),
            data,
        }
    }
}

/// Work for a native frontend after processing a PTY command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DndAdapterAction {
    /// Write one complete OSC 72 frame to the PTY.
    Write(Vec<u8>),
    /// Update native MIME registration for this terminal surface.
    RegistrationChanged {
        enabled: bool,
        mime_types: Vec<String>,
    },
    /// Tell the operating system that the retained drop is complete.
    DropFinished(DndOperation),
}

/// Validation or state error at the native DND boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DndError {
    InvalidMime,
    MimeListTooLarge,
    DropDataTooLarge,
    NotEnabled,
    NotAccepted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct HoverState {
    offered_mimes: Vec<String>,
    allowed_operation: DndOperation,
}

/// Destination-side OSC 72 state shared by all native frontends.
#[derive(Debug, Default)]
pub struct DndDestination {
    enabled: bool,
    client_id: u32,
    registered_mimes: Vec<String>,
    registration_payload: Vec<u8>,
    registration_chunking: bool,
    registration_discarding: bool,
    acceptance_payload: Vec<u8>,
    acceptance_chunking: bool,
    acceptance_discarding: bool,
    acceptance_operation: DndOperation,
    accepted_mimes: Option<Vec<String>>,
    hover: Option<HoverState>,
    dropped_data: Option<Vec<DropData>>,
}

impl DndDestination {
    /// Whether the terminal application currently accepts native drops.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// MIME types explicitly registered by the terminal application.
    pub fn registered_mimes(&self) -> &[String] {
        &self.registered_mimes
    }

    /// Operation the terminal application currently permits for the OS drag.
    pub fn accepted_operation(&self) -> DndOperation {
        let Some(hover) = self.hover.as_ref() else {
            return DndOperation::None;
        };
        if !hover.allowed_operation.permits(self.acceptance_operation)
            || !self.has_accepted_offered_mime(&hover.offered_mimes)
        {
            return DndOperation::None;
        }
        self.acceptance_operation
    }

    /// Apply one validated command emitted by the terminal application.
    pub fn handle_command(&mut self, command: DndCommand) -> Vec<DndAdapterAction> {
        match command.command_type {
            DndCommandType::Query => vec![DndAdapterAction::Write(capability_response(
                command.client_id,
            ))],
            DndCommandType::AcceptDrops => self.accept_registration(command),
            DndCommandType::StopAcceptingDrops => {
                self.reset();
                vec![DndAdapterAction::RegistrationChanged {
                    enabled: false,
                    mime_types: Vec::new(),
                }]
            }
            DndCommandType::DropMove => {
                self.accept_drop_status(command);
                Vec::new()
            }
            DndCommandType::RequestData => self.serve_drop_request(command),
            _ => Vec::new(),
        }
    }

    /// Report a native drag entering or moving over the terminal surface.
    pub fn drag_moved(
        &mut self,
        location: DndLocation,
        allowed_operation: DndOperation,
        offered_mimes: &[String],
    ) -> Result<Vec<Vec<u8>>, DndError> {
        if !self.enabled {
            return Err(DndError::NotEnabled);
        }
        validate_mime_list(offered_mimes)?;

        let offer_changed = self.hover.as_ref().is_none_or(|hover| {
            hover.offered_mimes != offered_mimes || hover.allowed_operation != allowed_operation
        });
        if offer_changed {
            self.acceptance_operation = DndOperation::None;
            self.accepted_mimes = None;
        }
        let payload = if offer_changed {
            join_mimes(offered_mimes)
        } else {
            Vec::new()
        };
        self.hover = Some(HoverState {
            offered_mimes: offered_mimes.to_vec(),
            allowed_operation,
        });

        Ok(encode_frames(
            DndCommandType::DropMove,
            self.client_id,
            Some(allowed_operation),
            Some(location),
            &payload,
        ))
    }

    /// Report a drag leaving the surface and discard every retained resource.
    pub fn drag_left(&mut self) -> Vec<Vec<u8>> {
        // GTK and AppKit can emit a leave callback while finalizing a successful
        // drop. Retain its data until the client sends the explicit finish.
        if self.dropped_data.is_some() {
            return Vec::new();
        }
        if !self.enabled || self.hover.is_none() {
            return Vec::new();
        }
        self.clear_drag();
        encode_frames(
            DndCommandType::DropMove,
            self.client_id,
            Some(DndOperation::None),
            Some(DndLocation {
                cell_x: -1,
                cell_y: -1,
                pixel_x: 0,
                pixel_y: 0,
            }),
            &[],
        )
    }

    /// Retain OS-provided MIME values and report that the user dropped them.
    pub fn dropped(
        &mut self,
        location: DndLocation,
        allowed_operation: DndOperation,
        data: Vec<DropData>,
    ) -> Result<Vec<Vec<u8>>, DndError> {
        if !self.enabled {
            return Err(DndError::NotEnabled);
        }
        let accepted_operation = self.accepted_operation();
        if accepted_operation == DndOperation::None
            || !allowed_operation.permits(accepted_operation)
        {
            self.clear_drag();
            return Err(DndError::NotAccepted);
        }
        let total = match data.iter().try_fold(0_usize, |size, item| {
            validate_mime(&item.mime_type)?;
            size.checked_add(item.data.len())
                .ok_or(DndError::DropDataTooLarge)
        }) {
            Ok(total) => total,
            Err(error) => {
                self.clear_drag();
                return Err(error);
            }
        };
        if total > MAX_LOCAL_DROP_BYTES {
            self.clear_drag();
            return Err(DndError::DropDataTooLarge);
        }
        let offered_mimes = data
            .iter()
            .map(|item| item.mime_type.clone())
            .collect::<Vec<_>>();
        if let Err(error) = validate_mime_list(&offered_mimes) {
            self.clear_drag();
            return Err(error);
        }
        if !self.has_accepted_offered_mime(&offered_mimes) {
            self.clear_drag();
            return Err(DndError::NotAccepted);
        }
        self.dropped_data = Some(data);

        Ok(encode_frames(
            DndCommandType::Drop,
            self.client_id,
            Some(allowed_operation),
            Some(location),
            &join_mimes(&offered_mimes),
        ))
    }

    fn accept_registration(&mut self, command: DndCommand) -> Vec<DndAdapterAction> {
        if !self.registration_chunking && !self.registration_discarding {
            self.registration_payload.clear();
            self.client_id = command.client_id;
        }
        if self.registration_discarding {
            self.registration_chunking = command.more;
            self.registration_discarding = command.more;
            if command.more {
                return Vec::new();
            }
            self.enabled = false;
            self.registered_mimes.clear();
            return vec![DndAdapterAction::RegistrationChanged {
                enabled: false,
                mime_types: Vec::new(),
            }];
        }
        if self.registration_payload.len() + command.payload.len() > MAX_DND_MIME_LIST_BYTES {
            self.registration_payload.clear();
            self.registration_chunking = command.more;
            self.registration_discarding = command.more;
            self.enabled = false;
            self.registered_mimes.clear();
            if command.more {
                return Vec::new();
            }
            return vec![DndAdapterAction::RegistrationChanged {
                enabled: false,
                mime_types: Vec::new(),
            }];
        }
        self.registration_payload
            .extend_from_slice(&command.payload);
        self.registration_chunking = command.more;
        if command.more {
            return Vec::new();
        }

        let Ok(mime_types) = parse_mime_payload(&self.registration_payload) else {
            self.enabled = false;
            self.registered_mimes.clear();
            return vec![DndAdapterAction::RegistrationChanged {
                enabled: false,
                mime_types: Vec::new(),
            }];
        };
        self.enabled = true;
        self.registered_mimes = mime_types;
        vec![DndAdapterAction::RegistrationChanged {
            enabled: true,
            mime_types: self.registered_mimes.clone(),
        }]
    }

    fn accept_drop_status(&mut self, command: DndCommand) {
        if self.hover.is_none() {
            return;
        }
        if !self.acceptance_chunking && !self.acceptance_discarding {
            self.acceptance_payload.clear();
            self.acceptance_operation = DndOperation::from_client(command.operation);
        }
        if self.acceptance_discarding {
            self.acceptance_chunking = command.more;
            self.acceptance_discarding = command.more;
            return;
        }
        if self.acceptance_payload.len() + command.payload.len() > MAX_DND_MIME_LIST_BYTES {
            self.acceptance_payload.clear();
            self.acceptance_chunking = command.more;
            self.acceptance_discarding = command.more;
            self.acceptance_operation = DndOperation::None;
            self.accepted_mimes = Some(Vec::new());
            return;
        }
        self.acceptance_payload.extend_from_slice(&command.payload);
        self.acceptance_chunking = command.more;
        if command.more {
            return;
        }
        self.accepted_mimes = if self.acceptance_payload.is_empty() {
            None
        } else {
            match parse_mime_payload(&self.acceptance_payload) {
                Ok(mime_types) => Some(mime_types),
                Err(_) => {
                    self.acceptance_operation = DndOperation::None;
                    Some(Vec::new())
                }
            }
        };
    }

    fn serve_drop_request(&mut self, command: DndCommand) -> Vec<DndAdapterAction> {
        if command.cell_x == 0 && command.cell_y == 0 && command.pixel_y == 0 {
            let operation = DndOperation::from_client(command.operation);
            self.clear_drag();
            return vec![DndAdapterAction::DropFinished(operation)];
        }

        let Some(data) = self.dropped_data.as_ref() else {
            return error_actions(
                self.client_id,
                &command,
                "EPERM:drop data can only be requested after a drop",
            );
        };
        if command.cell_y != 0 || command.pixel_x != 0 || command.pixel_y != 0 {
            let actions = error_actions(
                self.client_id,
                &command,
                "EINVAL:remote item and directory requests are not enabled",
            );
            self.clear_drag();
            return finish_after_error(actions);
        }
        let Some(index) = command
            .cell_x
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
        else {
            let actions = error_actions(self.client_id, &command, "ENOENT:invalid MIME index");
            self.clear_drag();
            return finish_after_error(actions);
        };
        let Some(item) = data.get(index) else {
            let actions = error_actions(self.client_id, &command, "ENOENT:invalid MIME index");
            self.clear_drag();
            return finish_after_error(actions);
        };

        data_actions(self.client_id, &command, &item.data)
    }

    fn has_accepted_offered_mime(&self, offered_mimes: &[String]) -> bool {
        match self.accepted_mimes.as_ref() {
            None => !offered_mimes.is_empty(),
            Some(accepted) => accepted
                .iter()
                .any(|mime| offered_mimes.iter().any(|offered| offered == mime)),
        }
    }

    fn clear_drag(&mut self) {
        self.acceptance_payload.clear();
        self.acceptance_chunking = false;
        self.acceptance_discarding = false;
        self.acceptance_operation = DndOperation::None;
        self.accepted_mimes = None;
        self.hover = None;
        self.dropped_data = None;
    }

    fn reset(&mut self) {
        self.enabled = false;
        self.client_id = 0;
        self.registered_mimes.clear();
        self.registration_payload.clear();
        self.registration_chunking = false;
        self.registration_discarding = false;
        self.clear_drag();
    }
}

fn finish_after_error(mut actions: Vec<DndAdapterAction>) -> Vec<DndAdapterAction> {
    actions.push(DndAdapterAction::DropFinished(DndOperation::None));
    actions
}

fn data_actions(client_id: u32, request: &DndCommand, data: &[u8]) -> Vec<DndAdapterAction> {
    let encoded = base64::engine::general_purpose::STANDARD_NO_PAD.encode(data);
    let location = request_location(request);
    let mut frames = if encoded.is_empty() {
        Vec::new()
    } else {
        encode_frames(
            DndCommandType::RequestData,
            client_id,
            None,
            Some(location),
            encoded.as_bytes(),
        )
    };
    frames.extend(encode_frames(
        DndCommandType::RequestData,
        client_id,
        None,
        Some(location),
        &[],
    ));
    frames.into_iter().map(DndAdapterAction::Write).collect()
}

fn error_actions(client_id: u32, request: &DndCommand, message: &str) -> Vec<DndAdapterAction> {
    encode_frames(
        DndCommandType::RequestError,
        client_id,
        None,
        Some(request_location(request)),
        message.as_bytes(),
    )
    .into_iter()
    .map(DndAdapterAction::Write)
    .collect()
}

fn request_location(request: &DndCommand) -> DndLocation {
    DndLocation {
        cell_x: request.cell_x,
        cell_y: request.cell_y,
        pixel_x: request.pixel_x,
        pixel_y: request.pixel_y,
    }
}

fn encode_frames(
    command_type: DndCommandType,
    client_id: u32,
    operation: Option<DndOperation>,
    location: Option<DndLocation>,
    payload: &[u8],
) -> Vec<Vec<u8>> {
    let chunks = payload.len().div_ceil(MAX_DND_CHUNK_BYTES).max(1);
    (0..chunks)
        .map(|index| {
            let start = index * MAX_DND_CHUNK_BYTES;
            let end = (start + MAX_DND_CHUNK_BYTES).min(payload.len());
            let chunk = &payload[start..end];
            let more = index + 1 < chunks;
            let mut frame = Vec::with_capacity(chunk.len() + 96);
            frame.extend_from_slice(b"\x1b]72;");
            if index == 0 {
                frame.extend_from_slice(b"t=");
                frame.push(command_byte(command_type));
                if let Some(operation) = operation {
                    append_u32(&mut frame, b'o', operation.protocol_value());
                }
                if let Some(location) = location {
                    append_i32(&mut frame, b'x', location.cell_x);
                    append_i32(&mut frame, b'y', location.cell_y);
                    append_i32(&mut frame, b'X', location.pixel_x);
                    append_i32(&mut frame, b'Y', location.pixel_y);
                }
            }
            if more || index > 0 {
                append_u32(&mut frame, b'm', u32::from(more));
            }
            if client_id != 0 {
                append_u32(&mut frame, b'i', client_id);
            }
            frame.push(b';');
            frame.extend_from_slice(chunk);
            frame.extend_from_slice(b"\x1b\\");
            frame
        })
        .collect()
}

fn command_byte(command_type: DndCommandType) -> u8 {
    match command_type {
        DndCommandType::AcceptDrops => b'a',
        DndCommandType::StopAcceptingDrops => b'A',
        DndCommandType::DropMove => b'm',
        DndCommandType::Drop => b'M',
        DndCommandType::RequestData => b'r',
        DndCommandType::RequestError => b'R',
        DndCommandType::OfferDrag => b'o',
        DndCommandType::PresentData => b'p',
        DndCommandType::ChangeDragImage => b'P',
        DndCommandType::DragOfferEvent => b'e',
        DndCommandType::DragOfferError => b'E',
        DndCommandType::UriListData => b'k',
        DndCommandType::Query => b'q',
    }
}

fn append_u32(frame: &mut Vec<u8>, key: u8, value: u32) {
    frame.push(b':');
    frame.push(key);
    frame.push(b'=');
    frame.extend_from_slice(value.to_string().as_bytes());
}

fn append_i32(frame: &mut Vec<u8>, key: u8, value: i32) {
    frame.push(b':');
    frame.push(key);
    frame.push(b'=');
    frame.extend_from_slice(value.to_string().as_bytes());
}

fn parse_mime_payload(payload: &[u8]) -> Result<Vec<String>, DndError> {
    let text = std::str::from_utf8(payload).map_err(|_| DndError::InvalidMime)?;
    let mime_types = text
        .split(' ')
        .filter(|mime| !mime.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    validate_mime_list(&mime_types)?;
    Ok(mime_types)
}

fn validate_mime_list(mime_types: &[String]) -> Result<(), DndError> {
    if mime_types.len() > MAX_DND_MIME_TYPES {
        return Err(DndError::MimeListTooLarge);
    }
    let total = mime_types.iter().try_fold(0_usize, |size, mime| {
        validate_mime(mime)?;
        size.checked_add(mime.len() + usize::from(size > 0))
            .ok_or(DndError::MimeListTooLarge)
    })?;
    if total > MAX_DND_MIME_LIST_BYTES {
        return Err(DndError::MimeListTooLarge);
    }
    Ok(())
}

fn validate_mime(mime: &str) -> Result<(), DndError> {
    if mime.is_empty()
        || !mime.is_ascii()
        || mime
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
    {
        return Err(DndError::InvalidMime);
    }
    Ok(())
}

fn join_mimes(mime_types: &[String]) -> Vec<u8> {
    mime_types.join(" ").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(command_type: DndCommandType) -> DndCommand {
        DndCommand {
            command_type,
            more: false,
            client_id: 0,
            operation: 0,
            cell_x: 0,
            cell_y: 0,
            pixel_x: 0,
            pixel_y: 0,
            payload: Vec::new(),
        }
    }

    fn enable(destination: &mut DndDestination) {
        let mut accept = command(DndCommandType::AcceptDrops);
        accept.client_id = 7;
        accept.payload = URI_LIST_MIME.as_bytes().to_vec();
        assert_eq!(
            destination.handle_command(accept),
            vec![DndAdapterAction::RegistrationChanged {
                enabled: true,
                mime_types: vec![URI_LIST_MIME.to_string()],
            }]
        );
    }

    fn uri_mimes() -> Vec<String> {
        vec![URI_LIST_MIME.to_string()]
    }

    fn location() -> DndLocation {
        DndLocation {
            cell_x: 2,
            cell_y: 3,
            pixel_x: 20,
            pixel_y: 30,
        }
    }

    #[test]
    fn query_is_answered_only_when_a_ready_adapter_handles_it() {
        let mut destination = DndDestination::default();
        let mut query = command(DndCommandType::Query);
        query.client_id = 42;
        assert_eq!(
            destination.handle_command(query),
            vec![DndAdapterAction::Write(b"\x1b]72;t=q:i=42;\x1b\\".to_vec())]
        );
    }

    #[test]
    fn registration_is_chunked_bounded_and_disable_resets_it() {
        let mut destination = DndDestination::default();
        let mut first = command(DndCommandType::AcceptDrops);
        first.more = true;
        first.client_id = 3;
        first.payload = b"text/uri-".to_vec();
        assert!(destination.handle_command(first).is_empty());

        let mut last = command(DndCommandType::AcceptDrops);
        last.client_id = 3;
        last.payload = b"list text/plain".to_vec();
        destination.handle_command(last);
        assert!(destination.is_enabled());
        assert_eq!(
            destination.registered_mimes(),
            [URI_LIST_MIME.to_string(), "text/plain".to_string()]
        );

        destination.handle_command(command(DndCommandType::StopAcceptingDrops));
        assert!(!destination.is_enabled());
        assert!(destination.registered_mimes().is_empty());
    }

    #[test]
    fn oversized_registration_discards_every_remaining_chunk() {
        let mut destination = DndDestination::default();
        for _ in 0..(MAX_DND_MIME_LIST_BYTES / MAX_DND_CHUNK_BYTES) {
            let mut chunk = command(DndCommandType::AcceptDrops);
            chunk.more = true;
            chunk.payload = vec![b'x'; MAX_DND_CHUNK_BYTES];
            assert!(destination.handle_command(chunk).is_empty());
        }
        let mut overflow = command(DndCommandType::AcceptDrops);
        overflow.more = true;
        overflow.payload = vec![b'x'];
        assert!(destination.handle_command(overflow).is_empty());

        let mut plausible_tail = command(DndCommandType::AcceptDrops);
        plausible_tail.payload = URI_LIST_MIME.as_bytes().to_vec();
        assert_eq!(
            destination.handle_command(plausible_tail),
            [DndAdapterAction::RegistrationChanged {
                enabled: false,
                mime_types: Vec::new(),
            }]
        );
        assert!(!destination.is_enabled());
    }

    #[test]
    fn drag_reports_mimes_once_and_discards_resources_on_leave() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        let first = destination
            .drag_moved(location(), DndOperation::Either, &uri_mimes())
            .unwrap();
        assert_eq!(
            first,
            [b"\x1b]72;t=m:o=3:x=2:y=3:X=20:Y=30:i=7;text/uri-list\x1b\\".to_vec()]
        );
        let repeated = destination
            .drag_moved(location(), DndOperation::Either, &uri_mimes())
            .unwrap();
        assert_eq!(
            repeated,
            [b"\x1b]72;t=m:o=3:x=2:y=3:X=20:Y=30:i=7;\x1b\\".to_vec()]
        );
        assert_eq!(
            destination.drag_left(),
            [b"\x1b]72;t=m:o=0:x=-1:y=-1:X=0:Y=0:i=7;\x1b\\".to_vec()]
        );
        assert_eq!(destination.accepted_operation(), DndOperation::None);
    }

    #[test]
    fn application_acceptance_gates_the_native_drop() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Either, &uri_mimes())
            .unwrap();
        assert!(matches!(
            destination.dropped(
                location(),
                DndOperation::Either,
                vec![DropData::uri_list(b"file:///tmp/a".to_vec())]
            ),
            Err(DndError::NotAccepted)
        ));

        destination
            .drag_moved(location(), DndOperation::Either, &uri_mimes())
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        destination.handle_command(accept);
        assert_eq!(destination.accepted_operation(), DndOperation::Copy);
        assert_eq!(
            destination
                .dropped(
                    location(),
                    DndOperation::Either,
                    vec![DropData::uri_list(b"file:///tmp/a".to_vec())]
                )
                .unwrap(),
            [b"\x1b]72;t=M:o=3:x=2:y=3:X=20:Y=30:i=7;text/uri-list\x1b\\".to_vec()]
        );
    }

    #[test]
    fn changed_offer_and_invalid_acceptance_require_fresh_consent() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Either, &uri_mimes())
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        destination.handle_command(accept);
        assert_eq!(destination.accepted_operation(), DndOperation::Copy);

        destination
            .drag_moved(location(), DndOperation::Move, &uri_mimes())
            .unwrap();
        assert_eq!(destination.accepted_operation(), DndOperation::None);

        let mut invalid = command(DndCommandType::DropMove);
        invalid.operation = 2;
        invalid.payload = b"text/plain\n".to_vec();
        destination.handle_command(invalid);
        assert_eq!(destination.accepted_operation(), DndOperation::None);
    }

    #[test]
    fn final_drop_must_contain_a_mime_accepted_by_the_application() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        let offered = vec![URI_LIST_MIME.to_string(), "text/plain".to_string()];
        destination
            .drag_moved(location(), DndOperation::Copy, &offered)
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        accept.payload = URI_LIST_MIME.as_bytes().to_vec();
        destination.handle_command(accept);

        assert_eq!(
            destination.dropped(
                location(),
                DndOperation::Copy,
                vec![DropData::new("text/plain", b"hello".to_vec()).unwrap()],
            ),
            Err(DndError::NotAccepted)
        );
    }

    #[test]
    fn request_before_drop_is_eperm_without_ending_the_hover() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Copy, &uri_mimes())
            .unwrap();
        let mut request = command(DndCommandType::RequestData);
        request.cell_x = 1;
        let actions = destination.handle_command(request);
        assert!(matches!(
            actions.as_slice(),
            [DndAdapterAction::Write(frame)] if frame.windows(5).any(|part| part == b"EPERM")
        ));
        assert!(destination.hover.is_some());
    }

    #[test]
    fn dropped_data_is_base64_chunked_terminated_and_finished() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Copy, &uri_mimes())
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        destination.handle_command(accept);
        let data = vec![b'x'; 4096];
        destination
            .dropped(
                location(),
                DndOperation::Copy,
                vec![DropData::uri_list(data)],
            )
            .unwrap();
        assert!(destination.drag_left().is_empty());
        assert!(destination.dropped_data.is_some());

        let mut request = command(DndCommandType::RequestData);
        request.cell_x = 1;
        let actions = destination.handle_command(request);
        assert_eq!(actions.len(), 3);
        assert!(matches!(
            &actions[0],
            DndAdapterAction::Write(frame) if frame.starts_with(b"\x1b]72;t=r:x=1:y=0:X=0:Y=0:m=1:i=7;")
        ));
        assert!(matches!(
            actions.last(),
            Some(DndAdapterAction::Write(frame)) if frame == b"\x1b]72;t=r:x=1:y=0:X=0:Y=0:i=7;\x1b\\"
        ));

        let mut finish = command(DndCommandType::RequestData);
        finish.operation = 1;
        assert_eq!(
            destination.handle_command(finish),
            [DndAdapterAction::DropFinished(DndOperation::Copy)]
        );
        assert!(destination.dropped_data.is_none());
    }

    #[test]
    fn invalid_mime_index_ends_the_drop() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Copy, &uri_mimes())
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        destination.handle_command(accept);
        destination
            .dropped(
                location(),
                DndOperation::Copy,
                vec![DropData::uri_list(b"file:///tmp/a".to_vec())],
            )
            .unwrap();

        let mut request = command(DndCommandType::RequestData);
        request.cell_x = 2;
        let actions = destination.handle_command(request);
        assert!(matches!(
            actions.as_slice(),
            [DndAdapterAction::Write(frame), DndAdapterAction::DropFinished(DndOperation::None)]
                if frame.windows(6).any(|part| part == b"ENOENT")
        ));
        assert!(destination.dropped_data.is_none());
    }

    #[test]
    fn remote_item_request_fails_closed_until_that_slice_is_implemented() {
        let mut destination = DndDestination::default();
        enable(&mut destination);
        destination
            .drag_moved(location(), DndOperation::Copy, &uri_mimes())
            .unwrap();
        let mut accept = command(DndCommandType::DropMove);
        accept.operation = 1;
        destination.handle_command(accept);
        destination
            .dropped(
                location(),
                DndOperation::Copy,
                vec![DropData::uri_list(b"file:///tmp/a".to_vec())],
            )
            .unwrap();

        let mut request = command(DndCommandType::RequestData);
        request.cell_x = 1;
        request.cell_y = 1;
        let actions = destination.handle_command(request);
        assert!(matches!(
            actions.as_slice(),
            [DndAdapterAction::Write(frame), DndAdapterAction::DropFinished(DndOperation::None)]
                if frame.windows(6).any(|part| part == b"EINVAL")
        ));
    }
}
