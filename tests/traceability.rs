//! The traceability gate (TEST-PLAN §5): every gated ledger row has its
//! proving test(s), every tag names a gated row, and every open row is
//! pending. Same check as
//! `cargo run -p agui-test-support --bin agui-trace -- gate`.

use agui_test_support::traceability::{gate_with, Inputs, Mode};

#[test]
fn traceability_gate() {
    if let Err(problems) = gate_with(&Inputs::repo(), Mode::Pending) {
        panic!(
            "traceability gate: {} problem(s):\n  {}",
            problems.len(),
            problems.join("\n  ")
        );
    }
}
