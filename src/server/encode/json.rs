//! Newline-delimited JSON, one event per line, for custom transports that
//! carry JSON frames (TRN-15). Same JSON as the SSE `data` payload.

use crate::error::{Error, ErrorCode, Result};
use crate::event::Event;

/// The NDJSON encoder.
///
/// ```
/// use agui_sdk::lifecycle::RunStarted;
/// use agui_sdk::server::encode::json::NdjsonEncoder;
/// use agui_sdk::Event;
///
/// let mut encoder = NdjsonEncoder::new();
/// let mut out = Vec::new();
/// encoder.encode(&Event::RunStarted(RunStarted::new("t-1", "r-1")), &mut out)?;
/// assert_eq!(out, b"{\"type\":\"RUN_STARTED\",\"threadId\":\"t-1\",\"runId\":\"r-1\"}\n");
/// # Ok::<(), agui_sdk::Error>(())
/// ```
#[derive(Debug, Default)]
pub struct NdjsonEncoder {
    _private: (),
}

impl NdjsonEncoder {
    /// An encoder.
    #[must_use]
    pub fn new() -> Self {
        NdjsonEncoder { _private: () }
    }

    /// Appends one event and a `\n`.
    ///
    /// # Errors
    ///
    /// [`ErrorCode::Encode`].
    pub fn encode(&mut self, event: &Event, out: &mut Vec<u8>) -> Result<()> {
        let start = out.len();
        if let Err(error) = serde_json::to_writer(&mut *out, event) {
            out.truncate(start);
            return Err(Error::new(
                ErrorCode::Encode,
                format!(
                    "{} could not be serialized as JSON",
                    event.event_type().as_str()
                ),
            )
            .with_source(error));
        }
        out.push(b'\n');
        Ok(())
    }
}
