//! A scripted in-process transport for client tests: serves fixture frames
//! (handed over directly, or encoded as SSE text or protobuf frames, split at
//! fixed byte boundaries and decoded again by the SDK's own decoders) and
//! records every request it receives.

use std::sync::{Arc, Mutex};

use serde_json::Value;

use agui_sdk::client::transport::sse::SseDecoder;
use agui_sdk::client::transport::TransportRequest;
use agui_sdk::client::{Frame, FrameStream, Transport};
use agui_sdk::error::{Error, ErrorCode};
use agui_sdk::proto;
use agui_sdk::wire::Chunk;

/// Frame size limit the mock's decoders use: large enough for any fixture.
pub const MOCK_FRAME_LIMIT: usize = 16 * 1024 * 1024;

/// How to deliver a script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Delivery {
    /// Frames handed over directly.
    Frames,
    /// Encoded as SSE text and re-decoded, split every `chunk` bytes.
    Sse {
        /// Chunk size.
        chunk: usize,
    },
    /// Encoded as protobuf frames and re-decoded, split every `chunk` bytes.
    Protobuf {
        /// Chunk size.
        chunk: usize,
    },
}

/// The scripted transport. Clones share the request log.
#[derive(Debug, Clone)]
pub struct MockTransport {
    frames: Vec<Value>,
    delivery: Delivery,
    requests: Arc<Mutex<Vec<Value>>>,
    fail_open: Option<(ErrorCode, String)>,
}

impl MockTransport {
    /// Serves `frames`.
    pub fn new(frames: Vec<Value>, delivery: Delivery) -> Self {
        MockTransport {
            frames,
            delivery,
            requests: Arc::new(Mutex::new(Vec::new())),
            fail_open: None,
        }
    }

    /// Refuses every `open` with this error instead (a pre-stream failure,
    /// TRN-05). The request is still recorded.
    pub fn refusing(mut self, code: ErrorCode, message: impl Into<String>) -> Self {
        self.fail_open = Some((code, message.into()));
        self
    }

    /// The last request body received, as JSON.
    pub fn last_request(&self) -> Option<Value> {
        self.requests().pop()
    }

    /// Every request body received, oldest first.
    pub fn requests(&self) -> Vec<Value> {
        self.requests.lock().map(|r| r.clone()).unwrap_or_default()
    }

    fn record(&self, body: &[u8]) {
        let value = serde_json::from_slice(body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(body).into_owned()));
        if let Ok(mut log) = self.requests.lock() {
            log.push(value);
        }
    }
}

/// SSE text for `frames`: one `data:` line per event, as a producer writes it.
pub fn sse_body(frames: &[Value]) -> Vec<u8> {
    let mut out = Vec::new();
    for frame in frames {
        out.extend_from_slice(b"data: ");
        // A `Value` always serializes.
        out.extend_from_slice(&serde_json::to_vec(frame).unwrap_or_default());
        out.extend_from_slice(b"\n\n");
    }
    out
}

/// Protobuf wire bytes for `frames`, through `proto::encode::encode_json` and
/// the length-prefixed framing.
///
/// # Errors
///
/// The index and error of the first event protobuf cannot carry.
pub fn proto_body(frames: &[Value]) -> Result<Vec<u8>, (usize, Error)> {
    let mut out = Vec::new();
    let mut payload = Vec::new();
    for (i, frame) in frames.iter().enumerate() {
        payload.clear();
        let json = serde_json::to_vec(frame).unwrap_or_default();
        proto::encode::encode_json(&json, &mut payload).map_err(|e| (i, e))?;
        proto::framing::write_frame(&payload, &mut out).map_err(|e| (i, e))?;
    }
    Ok(out)
}

/// Whether every event of a stream can travel over protobuf. This is how the
/// stream lane's protobuf skip list is computed rather than written by hand.
///
/// # Errors
///
/// Which event cannot, and why.
pub fn proto_expressible(frames: &[Value]) -> Result<(), String> {
    proto_body(frames)
        .map(|_| ())
        .map_err(|(i, e)| format!("event {i} cannot be encoded as protobuf: {e}"))
}

fn chunks(bytes: &[u8], size: usize) -> impl Iterator<Item = Chunk> + '_ {
    bytes.chunks(size.max(1)).map(Chunk::copy_from_slice)
}

fn decode_sse(frames: &[Value], chunk: usize) -> Vec<agui_sdk::Result<Frame>> {
    let body = sse_body(frames);
    let mut decoder = SseDecoder::new(MOCK_FRAME_LIMIT);
    let mut out = Vec::new();
    for piece in chunks(&body, chunk) {
        decoder.push(piece);
        loop {
            match decoder.next_event() {
                Ok(Some(data)) => out.push(Ok(Frame::from_json(data))),
                Ok(None) => break,
                Err(e) => {
                    out.push(Err(e));
                    return out;
                }
            }
        }
    }
    if decoder.has_partial() {
        out.push(Err(Error::new(
            ErrorCode::Truncated,
            "mock: SSE body ended mid-event",
        )));
    }
    out
}

fn decode_proto(frames: &[Value], chunk: usize) -> Vec<agui_sdk::Result<Frame>> {
    let body = match proto_body(frames) {
        Ok(body) => body,
        Err((i, e)) => {
            return vec![Err(Error::new(
                ErrorCode::Internal,
                format!("mock: event {i} cannot be encoded as protobuf ({e}); skip this fixture on the protobuf lane"),
            ))]
        }
    };
    let mut decoder = proto::framing::FrameDecoder::new(MOCK_FRAME_LIMIT);
    let mut out = Vec::new();
    for piece in chunks(&body, chunk) {
        decoder.push(piece);
        loop {
            match decoder.next_frame() {
                Ok(Some(payload)) => match proto::decode::decode_event(&payload) {
                    Ok(proto::decode::Decoded::Event(value)) => {
                        out.push(Ok(Frame::from_value(value)))
                    }
                    // An unknown arm is dropped (with a warning) by the real transport.
                    Ok(_) => {}
                    Err(e) => {
                        out.push(Err(e));
                        return out;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    out.push(Err(e));
                    return out;
                }
            }
        }
    }
    if decoder.has_partial() {
        out.push(Err(Error::new(
            ErrorCode::Truncated,
            "mock: protobuf body ended mid-frame",
        )));
    }
    out
}

impl Transport for MockTransport {
    async fn open(&self, request: TransportRequest) -> agui_sdk::Result<FrameStream> {
        self.record(&request.body);
        if let Some((code, message)) = &self.fail_open {
            return Err(Error::new(*code, message.clone()));
        }
        let frames = match self.delivery {
            Delivery::Frames => self
                .frames
                .iter()
                .map(|f| {
                    Ok(Frame::from_json(Chunk::from(
                        serde_json::to_vec(f).unwrap_or_default(),
                    )))
                })
                .collect(),
            Delivery::Sse { chunk } => decode_sse(&self.frames, chunk),
            Delivery::Protobuf { chunk } => decode_proto(&self.frames, chunk),
        };
        Ok(FrameStream::from_frames(frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_body_is_one_data_line_per_event() {
        let body = sse_body(&[
            serde_json::json!({"type": "A"}),
            serde_json::json!({"type": "B"}),
        ]);
        assert_eq!(
            body,
            b"data: {\"type\":\"A\"}\n\ndata: {\"type\":\"B\"}\n\n"
        );
    }

    #[test]
    fn requests_are_shared_between_clones() {
        let t = MockTransport::new(Vec::new(), Delivery::Frames);
        let c = t.clone();
        c.record(br#"{"threadId":"t"}"#);
        c.record(b"not json");
        assert_eq!(t.requests().len(), 2);
        assert_eq!(t.last_request(), Some(Value::String("not json".into())));
    }
}
