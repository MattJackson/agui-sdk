//! patch::apply on an arbitrary document and operations (input: document
//! JSON, a NUL byte, operations JSON). Invariants: no panic; atomic (a failed
//! patch leaves the document identical); the budget's node count matches a
//! recount after success.
#![no_main]

use agui_sdk::patch::{apply, PatchBudget, PatchLimits, PatchOperation};
use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    let Some(nul) = data.iter().position(|b| *b == 0) else {
        return;
    };
    let Ok(mut doc) = serde_json::from_slice::<Value>(&data[..nul]) else {
        return;
    };
    let Ok(ops) = serde_json::from_slice::<Vec<PatchOperation>>(&data[nul + 1..]) else {
        return;
    };
    let before = doc.clone();
    let mut budget = PatchBudget::for_document(&doc, PatchLimits::default());
    match apply(&mut doc, &ops, &mut budget) {
        Err(_) => assert_eq!(doc, before, "failed patch changed the document"),
        Ok(()) => {
            let recount = PatchBudget::for_document(&doc, PatchLimits::default()).nodes();
            assert_eq!(budget.nodes(), recount, "budget drifted from the document");
        }
    }
});
