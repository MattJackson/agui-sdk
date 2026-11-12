//! SseDecoder over arbitrary bytes and chunking. Invariants: no panic; no
//! event over the limit is returned; the same events (and the same first
//! error) regardless of chunking. That buffering stays bounded is covered by
//! the unit and hostile-peer tests, not here.
#![no_main]

use agui_sdk::client::transport::sse::SseDecoder;
use agui_sdk::ErrorCode;
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

const LIMIT: usize = 64 * 1024;

fn decode(chunks: Vec<agui_sdk::wire::Chunk>) -> (Vec<Vec<u8>>, Option<ErrorCode>) {
    let mut decoder = SseDecoder::new(LIMIT);
    let mut events = Vec::new();
    for c in chunks {
        decoder.push(c);
        loop {
            match decoder.next_event() {
                Ok(Some(e)) => {
                    assert!(e.len() <= LIMIT, "event over the limit was returned");
                    events.push(e.to_vec());
                }
                Ok(None) => break,
                Err(e) => return (events, Some(e.code())),
            }
        }
    }
    (events, None)
}

fuzz_target!(|data: &[u8]| {
    let (size, payload) = common::chunking(data);
    let split = decode(common::chunks(payload, size));
    let whole = decode(common::chunks(payload, payload.len().max(1)));
    assert_eq!(split, whole, "decoding depends on chunking");
});
