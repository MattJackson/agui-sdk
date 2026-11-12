//! Enforcer::enforce_event_json over arbitrary bytes. Invariants: no panic;
//! every accepted event strict-validates.
#![no_main]

use std::sync::Arc;

use agui_sdk::diagnostics::{Diagnostic, DiagnosticSink, Reporter};
use agui_sdk::pipeline::enforce::Enforcer;
use agui_sdk::pipeline::Limits;
use libfuzzer_sys::fuzz_target;

#[path = "common.rs"]
mod common;

fuzz_target!(|data: &[u8]| {
    let enforcer = Enforcer::new(Limits::default());
    // An enabled sink (unlike NullSink), so the diagnostic builders the
    // input reaches run under the fuzzer as they do in production.
    let sink: Arc<dyn DiagnosticSink> = Arc::new(|_: &Diagnostic| {});
    let mut reporter = Reporter::new(sink, 16);
    if let Ok(Some(event)) = enforcer.enforce_event_json(data, &mut reporter) {
        common::assert_strict(&event);
    }
});
