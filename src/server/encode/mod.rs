//! Writing events to the wire.
//!
//! [`Encoder`] is an enum over the formats, not a trait object, so the per-event
//! call is a direct match. Every encoder appends to a caller-owned buffer;
//! the HTTP layer reuses one buffer per response.
//!
//! | Format | Encoder | Frame |
//! |---|---|---|
//! | SSE (`text/event-stream`) | [`sse::SseEncoder`] | `data: <json>\n\n`, LF only |
//! | protobuf (`application/vnd.ag-ui.event+proto`, feature `proto`) | [`proto::ProtoEncoder`] | 4-byte big-endian length + message |
//! | NDJSON (custom transports) | [`json::NdjsonEncoder`] | `<json>\n` |
#![cfg_attr(
    not(feature = "proto"),
    doc = "",
    doc = "[`proto::ProtoEncoder`]: https://docs.rs/agui-sdk/latest/agui_sdk/server/encode/proto/struct.ProtoEncoder.html"
)]

use crate::error::Result;
use crate::event::Event;
use crate::wire::WireFormat;

pub mod json;
pub mod sse;

#[cfg(feature = "proto")]
#[cfg_attr(docsrs, doc(cfg(feature = "proto")))]
pub mod proto;

/// An encoder for one response.
#[derive(Debug)]
#[non_exhaustive]
pub enum Encoder {
    /// SSE.
    Sse(sse::SseEncoder),
    /// Length-prefixed protobuf.
    #[cfg(feature = "proto")]
    #[cfg_attr(docsrs, doc(cfg(feature = "proto")))]
    Protobuf(proto::ProtoEncoder),
}

impl Encoder {
    /// The encoder for a negotiated format. With feature `proto` off,
    /// [`WireFormat::Protobuf`] falls back to SSE (never negotiated then).
    #[must_use]
    pub fn for_format(format: WireFormat) -> Self {
        match format {
            #[cfg(feature = "proto")]
            WireFormat::Protobuf => Encoder::Protobuf(proto::ProtoEncoder::new()),
            _ => Encoder::Sse(sse::SseEncoder::new()),
        }
    }

    /// Response `Content-Type`.
    #[must_use]
    pub fn content_type(&self) -> &'static str {
        self.format().media_type()
    }

    /// The wire format this encoder writes.
    pub(crate) fn format(&self) -> WireFormat {
        match self {
            Encoder::Sse(_) => WireFormat::Sse,
            #[cfg(feature = "proto")]
            Encoder::Protobuf(_) => WireFormat::Protobuf,
        }
    }

    /// Appends one event.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Encode`](crate::ErrorCode::Encode).
    pub fn encode(&mut self, event: &Event, out: &mut Vec<u8>) -> Result<()> {
        match self {
            Encoder::Sse(encoder) => encoder.encode(event, out),
            #[cfg(feature = "proto")]
            Encoder::Protobuf(encoder) => encoder.encode(event, out),
        }
    }

    /// Appends a keep-alive if the format has one (SSE comment line, SSE-09).
    /// Returns whether anything was written.
    pub fn keepalive(&self, out: &mut Vec<u8>) -> bool {
        match self.keepalive_bytes() {
            Some(bytes) => {
                out.extend_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    /// The keep-alive bytes of the format, if it has any: the one decision
    /// behind [`keepalive`](Self::keepalive) and the response body's timer.
    pub(crate) fn keepalive_bytes(&self) -> Option<&'static [u8]> {
        match self {
            Encoder::Sse(_) => Some(sse::KEEPALIVE),
            #[cfg(feature = "proto")]
            Encoder::Protobuf(_) => None,
        }
    }
}
