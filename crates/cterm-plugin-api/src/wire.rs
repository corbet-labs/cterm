use prost::Message;
use thiserror::Error;

use crate::{proto, ActionScope, CommandId, ABI_MAJOR, ABI_MINOR};

/// Maximum complete length-delimited protobuf frame accepted from a guest.
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_ACTIONS: usize = 32;
pub const MAX_DIAGNOSTICS: usize = 32;
pub const MAX_DIAGNOSTIC_BYTES: usize = 4096;

pub fn encode_request_frame(request: &proto::PluginRequest) -> Result<Vec<u8>, WireError> {
    validate_request(request)?;
    encode_frame(request)
}

pub fn decode_request_frame(bytes: &[u8]) -> Result<proto::PluginRequest, WireError> {
    let request = decode_frame(bytes)?;
    validate_request(&request)?;
    Ok(request)
}

pub fn encode_response_frame(response: &proto::PluginResponse) -> Result<Vec<u8>, WireError> {
    validate_response(response)?;
    encode_frame(response)
}

pub fn decode_response_frame(bytes: &[u8]) -> Result<proto::PluginResponse, WireError> {
    let response = decode_frame(bytes)?;
    validate_response(&response)?;
    Ok(response)
}

pub fn validate_request(request: &proto::PluginRequest) -> Result<(), WireError> {
    validate_abi(request.abi_major, request.abi_minor)?;
    CommandId::parse(&request.command_id)
        .map_err(|_| WireError::InvalidCommandId(request.command_id.clone()))?;
    Ok(())
}

pub fn validate_response(response: &proto::PluginResponse) -> Result<(), WireError> {
    validate_abi(response.abi_major, response.abi_minor)?;
    if response.actions.len() > MAX_ACTIONS {
        return Err(WireError::TooManyActions {
            found: response.actions.len(),
            limit: MAX_ACTIONS,
        });
    }
    if response.diagnostics.len() > MAX_DIAGNOSTICS {
        return Err(WireError::TooManyDiagnostics {
            found: response.diagnostics.len(),
            limit: MAX_DIAGNOSTICS,
        });
    }

    for action in &response.actions {
        ActionScope::parse(&action.id)
            .map_err(|_| WireError::InvalidActionId(action.id.clone()))?;
        validate_parameter(action)?;
    }
    for diagnostic in &response.diagnostics {
        if diagnostic.len() > MAX_DIAGNOSTIC_BYTES {
            return Err(WireError::DiagnosticTooLarge {
                found: diagnostic.len(),
                limit: MAX_DIAGNOSTIC_BYTES,
            });
        }
        if diagnostic
            .chars()
            .any(|character| character.is_control() && !matches!(character, '\n' | '\r' | '\t'))
        {
            return Err(WireError::InvalidDiagnostic);
        }
    }
    Ok(())
}

fn validate_parameter(action: &proto::ActionInvocation) -> Result<(), WireError> {
    match action.parameter {
        None => Ok(()),
        Some(proto::action_invocation::Parameter::Tab(tab)) => u8::try_from(tab)
            .map(|_| ())
            .map_err(|_| WireError::TabOutOfRange(tab)),
        Some(proto::action_invocation::Parameter::SplitDirection(value)) => {
            let direction = proto::SplitDirection::try_from(value)
                .map_err(|_| WireError::InvalidEnum("split direction", value))?;
            if direction == proto::SplitDirection::Unspecified {
                Err(WireError::UnspecifiedEnum("split direction"))
            } else {
                Ok(())
            }
        }
        Some(proto::action_invocation::Parameter::PaneDirection(value)) => {
            let direction = proto::PaneDirection::try_from(value)
                .map_err(|_| WireError::InvalidEnum("pane direction", value))?;
            if direction == proto::PaneDirection::Unspecified {
                Err(WireError::UnspecifiedEnum("pane direction"))
            } else {
                Ok(())
            }
        }
    }
}

fn validate_abi(major: u32, minor: u32) -> Result<(), WireError> {
    if major == ABI_MAJOR && minor == ABI_MINOR {
        Ok(())
    } else {
        Err(WireError::UnsupportedAbi {
            found_major: major,
            found_minor: minor,
            supported_major: ABI_MAJOR,
            supported_minor: ABI_MINOR,
        })
    }
}

fn encode_frame<M: Message>(message: &M) -> Result<Vec<u8>, WireError> {
    let mut bytes = Vec::with_capacity(message.encoded_len() + 5);
    message
        .encode_length_delimited(&mut bytes)
        .map_err(WireError::Encode)?;
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge {
            found: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    Ok(bytes)
}

fn decode_frame<M: Message + Default>(bytes: &[u8]) -> Result<M, WireError> {
    if bytes.len() > MAX_FRAME_BYTES {
        return Err(WireError::FrameTooLarge {
            found: bytes.len(),
            limit: MAX_FRAME_BYTES,
        });
    }
    let mut remaining = bytes;
    let message = M::decode_length_delimited(&mut remaining).map_err(WireError::Decode)?;
    if !remaining.is_empty() {
        return Err(WireError::TrailingBytes(remaining.len()));
    }
    Ok(message)
}

#[derive(Debug, Error)]
pub enum WireError {
    #[error(
        "plugin ABI {found_major}.{found_minor} is unsupported; this build supports {supported_major}.{supported_minor}"
    )]
    UnsupportedAbi {
        found_major: u32,
        found_minor: u32,
        supported_major: u32,
        supported_minor: u32,
    },
    #[error("invalid plugin command identifier `{0}`")]
    InvalidCommandId(String),
    #[error("invalid plugin action identifier `{0}`")]
    InvalidActionId(String),
    #[error("plugin returned {found} actions; limit is {limit}")]
    TooManyActions { found: usize, limit: usize },
    #[error("plugin returned {found} diagnostics; limit is {limit}")]
    TooManyDiagnostics { found: usize, limit: usize },
    #[error("plugin diagnostic is {found} bytes; limit is {limit}")]
    DiagnosticTooLarge { found: usize, limit: usize },
    #[error("plugin diagnostic contains an unsupported control character")]
    InvalidDiagnostic,
    #[error("tab parameter {0} is outside the supported byte range")]
    TabOutOfRange(u32),
    #[error("{0} must not be unspecified")]
    UnspecifiedEnum(&'static str),
    #[error("invalid {0} enum value {1}")]
    InvalidEnum(&'static str, i32),
    #[error("plugin frame is {found} bytes; limit is {limit}")]
    FrameTooLarge { found: usize, limit: usize },
    #[error("plugin frame has {0} trailing bytes")]
    TrailingBytes(usize),
    #[error("failed to encode plugin frame: {0}")]
    Encode(#[source] prost::EncodeError),
    #[error("failed to decode plugin frame: {0}")]
    Decode(#[source] prost::DecodeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> proto::PluginRequest {
        proto::PluginRequest {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            command_id: "run".to_string(),
        }
    }

    fn response() -> proto::PluginResponse {
        proto::PluginResponse {
            abi_major: ABI_MAJOR,
            abi_minor: ABI_MINOR,
            actions: vec![
                proto::ActionInvocation {
                    id: "cterm:new-tab".to_string(),
                    parameter: Some(proto::action_invocation::Parameter::Tab(7)),
                },
                proto::ActionInvocation {
                    id: "cterm:split-pane".to_string(),
                    parameter: Some(proto::action_invocation::Parameter::SplitDirection(
                        proto::SplitDirection::Vertical.into(),
                    )),
                },
                proto::ActionInvocation {
                    id: "cterm:focus-pane".to_string(),
                    parameter: Some(proto::action_invocation::Parameter::PaneDirection(
                        proto::PaneDirection::Left.into(),
                    )),
                },
            ],
            diagnostics: vec!["done".to_string()],
        }
    }

    #[test]
    fn request_has_stable_golden_bytes() {
        let bytes = encode_request_frame(&request()).unwrap();
        assert_eq!(bytes, [7, 8, 1, 26, 3, b'r', b'u', b'n']);
        assert_eq!(decode_request_frame(&bytes).unwrap(), request());
    }

    #[test]
    fn action_parameters_round_trip_without_rust_enum_layout() {
        let expected = response();
        let bytes = encode_response_frame(&expected).unwrap();
        assert_eq!(decode_response_frame(&bytes).unwrap(), expected);
    }

    #[test]
    fn unsupported_versions_and_invalid_identifiers_fail_closed() {
        let mut invalid_request = request();
        invalid_request.abi_minor = ABI_MINOR + 1;
        assert!(matches!(
            validate_request(&invalid_request),
            Err(WireError::UnsupportedAbi { .. })
        ));
        invalid_request = request();
        invalid_request.command_id = "../run".to_string();
        assert!(matches!(
            validate_request(&invalid_request),
            Err(WireError::InvalidCommandId(_))
        ));

        let mut invalid_response = response();
        invalid_response.actions[0].id = "plugin:other/run".to_string();
        assert!(matches!(
            validate_response(&invalid_response),
            Err(WireError::InvalidActionId(_))
        ));
    }

    #[test]
    fn invalid_action_parameters_never_truncate_or_default() {
        let mut invalid = response();
        invalid.actions[0].parameter = Some(proto::action_invocation::Parameter::Tab(
            u32::from(u8::MAX) + 1,
        ));
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::TabOutOfRange(256))
        ));

        invalid = response();
        invalid.actions[1].parameter = Some(proto::action_invocation::Parameter::SplitDirection(
            proto::SplitDirection::Unspecified.into(),
        ));
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::UnspecifiedEnum("split direction"))
        ));

        invalid = response();
        invalid.actions[2].parameter = Some(proto::action_invocation::Parameter::PaneDirection(99));
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::InvalidEnum("pane direction", 99))
        ));
    }

    #[test]
    fn output_counts_sizes_and_controls_are_bounded() {
        let mut invalid = response();
        invalid.actions = vec![invalid.actions[0].clone(); MAX_ACTIONS + 1];
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::TooManyActions { .. })
        ));

        invalid = response();
        invalid.diagnostics = vec![String::new(); MAX_DIAGNOSTICS + 1];
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::TooManyDiagnostics { .. })
        ));

        invalid = response();
        invalid.diagnostics = vec!["x".repeat(MAX_DIAGNOSTIC_BYTES + 1)];
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::DiagnosticTooLarge { .. })
        ));

        invalid = response();
        invalid.diagnostics = vec!["bad\0text".to_string()];
        assert!(matches!(
            validate_response(&invalid),
            Err(WireError::InvalidDiagnostic)
        ));
    }

    #[test]
    fn malformed_oversized_and_multi_frame_input_is_rejected() {
        assert!(matches!(
            decode_request_frame(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(WireError::FrameTooLarge { .. })
        ));
        assert!(matches!(
            decode_request_frame(&[0x80]),
            Err(WireError::Decode(_))
        ));

        let mut two_frames = encode_request_frame(&request()).unwrap();
        two_frames.extend_from_slice(&encode_request_frame(&request()).unwrap());
        assert!(matches!(
            decode_request_frame(&two_frames),
            Err(WireError::TrailingBytes(_))
        ));
    }

    #[test]
    fn protobuf_unknown_fields_remain_forward_compatible() {
        let mut payload = request().encode_to_vec();
        // Unknown field 99 with varint value 7.
        payload.extend_from_slice(&[0x98, 0x06, 0x07]);
        let mut frame = vec![u8::try_from(payload.len()).unwrap()];
        frame.extend_from_slice(&payload);

        let decoded = decode_request_frame(&frame).unwrap();
        assert_eq!(decoded, request());
    }
}
