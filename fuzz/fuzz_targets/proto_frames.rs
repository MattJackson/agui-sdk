//! FrameDecoder over arbitrary bytes and chunking. Invariants: no panic; an
//! oversize frame is rejected (before buffering it); the same frames
//! regardless of chunking.
#![no_main]

use agui_sdk::proto::framing::FrameDecoder;
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

const LIMIT: usize = 4096;

fn decode(chunks: Vec<agui_sdk::wire::Chunk>) -> (Vec<Vec<u8>>, bool) {
    let mut decoder = FrameDecoder::new(LIMIT);
    let mut frames = Vec::new();
    for c in chunks {
        decoder.push(c);
        loop {
            match decoder.next_frame() {
                Ok(Some(f)) => {
                    assert!(f.len() <= LIMIT, "oversize frame returned");
                    frames.push(f.to_vec());
                }
                Ok(None) => break,
                Err(_) => return (frames, true),
            }
        }
    }
    (frames, false)
}

fuzz_target!(|data: &[u8]| {
    let (size, payload) = common::chunking(data);
    assert_eq!(
        decode(common::chunks(payload, size)),
        decode(common::chunks(payload, payload.len().max(1))),
        "framing depends on chunking"
    );
});
