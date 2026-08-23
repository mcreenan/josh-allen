#![doc = "Bounded, strict wire primitives for the JOSH protocol."]

mod framing;
mod handshake;
mod message;
mod payload;
mod state;
mod strict_json;
mod version;

pub use framing::{DEFAULT_MAX_FRAME_BYTES, FrameReader, SerializedWriter, encode_frame};
pub use handshake::{
    ExecutionMode, InitializeParams, InitializeResult, InvokingSessionId, PeerInfo, ProtocolLimits,
    RuntimeReadyParams,
};
pub use message::{
    ProtocolError, WireError, WireErrorCode, WireMessage, decode_message, decode_value,
    encode_message, validate_id, validate_method, validate_reason,
};
pub use payload::*;
pub use state::{ConnectionState, PeerRole, ProtocolTracker, ReceiveAction, RequestStateError};
pub use version::{FEATURES, LANGUAGE_VERSION, PROTOCOL_VERSION};
