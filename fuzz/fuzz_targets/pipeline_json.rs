//! The client pipeline over an in-memory transport: arbitrary bytes are the
//! SSE response body, decoded by SseDecoder in arbitrary chunks and pushed
//! through Processor::push_json. Invariants: no panic; every event the
//! pipeline delivers strict-validates.
#![no_main]

use std::sync::Arc;

use agui_sdk::client::transport::sse::SseDecoder;
use agui_sdk::diagnostics::{Diagnostic, DiagnosticSink, Reporter};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let (size, body) = common::chunking(data);
    let mut decoder = SseDecoder::new(64 * 1024);
    let mut processor = Processor::new(PipelineConfig::default());
    // An enabled sink (unlike NullSink), so the diagnostic builders the
    // input reaches run under the fuzzer as they do in production.
    let sink: Arc<dyn DiagnosticSink> = Arc::new(|_: &Diagnostic| {});
    let mut reporter = Reporter::new(sink, 16);
    let mut out = Vec::new();
    for chunk in common::chunks(body, size) {
        decoder.push(chunk);
        loop {
            match decoder.next_event() {
                Ok(Some(frame)) => {
                    out.clear();
                    let pushed = processor.push_json(&frame, &mut reporter, &mut out);
                    // Events processed before a failing one are still
                    // delivered, so they are checked on the error path too.
                    for p in &out {
                        common::assert_strict(&p.event);
                    }
                    if pushed.is_err() {
                        return;
                    }
                }
                Ok(None) => break,
                Err(_) => return,
            }
        }
    }
    let _ = processor.finish();
});
