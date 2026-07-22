//! Messages and local IPC framing for the code-mode host boundary.
//!
//! Protocol version 2 multiplexes session operations and delegate callbacks by
//! request ID over one ordered connection and requires bounded cell identifiers
//! in both directions. Capability names provide an extension point without
//! weakening the versioned decoder contract.
//!
//! Compatibility guarantees apply to the serialized messages exchanged across
//! the host boundary. These Rust construction types are workspace-internal and
//! evolve together with their workspace callers.

mod codec;
mod error;
mod message;
mod payload;
mod types;
mod workflow_cell_id;

pub use codec::EncodedFrame;
pub use codec::FramedReader;
pub use codec::FramedWriter;
pub use codec::MAX_FRAME_BYTES;
pub use error::HandshakeRejectReason;
pub use message::ClientHello;
pub use message::ClientHelloError;
pub use message::ClientToHost;
pub use message::DelegateRequest;
pub use message::DelegateResponse;
pub use message::HostHello;
pub use message::HostRequest;
pub use message::HostResponse;
pub use message::HostToClient;
pub use message::WireResult;
pub use payload::DecodedWireExecuteRequest;
pub use payload::InvalidWireCellId;
pub use payload::WIRE_CELL_ID_MAX_BYTES;
pub use payload::WireCellId;
pub use payload::WireContentItem;
pub use payload::WireExecuteCellIdentity;
pub use payload::WireExecuteOutputPolicy;
pub use payload::WireExecuteRequest;
pub use payload::WireExecuteRequestConversionError;
pub use payload::WireImageDetail;
pub use payload::WireNestedToolCall;
pub use payload::WireRuntimeResponse;
pub use payload::WireToolDefinition;
pub use payload::WireToolKind;
pub use payload::WireToolName;
pub use payload::WireWaitOutcome;
pub use payload::WireWaitRequest;
pub use types::Capability;
pub use types::CapabilitySet;
pub use types::DelegateRequestId;
pub use types::DuplicateCapability;
pub use types::InvalidIdentifier;
pub use types::InvalidSupportedProtocolVersions;
pub use types::ProtocolVersion;
pub use types::RequestId;
pub use types::SAVED_WORKFLOW_CELL_ID_V1_CAPABILITY;
pub use types::SAVED_WORKFLOW_OUTPUT_V1_CAPABILITY;
pub use types::SessionId;
pub use types::SupportedProtocolVersions;
pub use workflow_cell_id::InvalidWireWorkflowCellId;
pub use workflow_cell_id::WireWorkflowCellId;

#[cfg(test)]
#[path = "host_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "codec_tests.rs"]
mod codec_tests;
