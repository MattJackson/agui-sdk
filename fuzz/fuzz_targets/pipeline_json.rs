//! The client pipeline over an in-memory transport: arbitrary bytes are the
//! SSE response body, decoded by SseDecoder in arbitrary chunks and pushed
//! through Processor::push_json. Invariants: no panic; every event the
//! pipeline delivers strict-validates.
#![no_main]

use std::sync::Arc;

use agui_sdk::client::transport::sse::SseDecoder;
use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let (size, body) = common::chunking(data);
    let mut decoder = SseDecoder::new(64 * 1024);
    let mut processor = Processor::new(PipelineConfig::default());
    let mut reporter = Reporter::new(Arc::new(NullSink), 16);
    let mut out = Vec::new();
    for chunk in common::chunks(body, size) {
        decoder.push(chunk);
        loop {
            match decoder.next_event() {
                Ok(Some(frame)) => {
                    out.clear();
                    if processor
                        .push_json(&frame, &mut reporter, &mut out)
                        .is_err()
                    {
                        return;
                    }
                    for p in &out {
                        common::assert_strict(&p.event);
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    let _ = processor.finish();
});
