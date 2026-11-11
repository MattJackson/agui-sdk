//! server::input::admit over arbitrary request bodies. Invariants: no panic;
//! admitted input strict-validates as RunAgentInput.
#![no_main]

use agui_sdk::pipeline::enforce::validate_strict;
use agui_sdk::server::input::{admit, InputPolicy};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(admitted) = admit(data, &InputPolicy::default()) {
        let bytes = serde_json::to_vec(&admitted.input).unwrap_or_else(|e| panic!("{e}"));
        if let Err(e) = validate_strict("RunAgentInput", &bytes) {
            panic!("admitted input is not strictly valid: {e}");
        }
    }
});
