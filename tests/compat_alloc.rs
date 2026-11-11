//! Allocation budget of the compatibility stages : on 1.0
//! frames the boundary allocates nothing, and so do the era shims on frames
//! they leave alone. Allocations are counted on this test's thread only
//! (`agui_test_support::alloc`), so the harness's own threads cannot land in
//! the window.

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::compat::{Boundary, EraShims};
use agui_sdk::version::LegacyEra;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::json;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

#[test]
fn modern_frames_cost_no_allocation() {
    let mut reporter = Reporter::new(Arc::new(NullSink), 256);
    let mut frames = [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "protocolVersion": "1.0",
        "input": {"threadId": "t", "runId": "r", "forwardedProps": {"a": 1},
                  "tools": [{"name": "n", "description": "d", "parameters": {}}],
                  "messages": [{"id": "u", "role": "user", "content": [
                      {"type": "text", "text": "hi"},
                      {"type": "image", "source": {"type": "url", "value": "u"}, "metadata": {}}
                  ]}]}}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "d", "rawEvent": {}}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n",
               "parentMessageId": "m"}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "x", "toolCallId": "c", "content": [
            {"type": "text", "text": "r"}]}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "u", "role": "user", "content": [
            {"type": "document", "source": {"type": "url", "value": "u"}}]}]}),
        json!({"type": "REASONING_START", "messageId": "s"}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
               "outcome": {"type": "success", "pendingToolCallIds": []},
               "usage": [{"provider": "p", "inputTokens": 1}]}),
    ];
    let mut input = frames[0]["input"].clone();
    let mut boundary = Boundary::new();
    let shims = EraShims::new(LegacyEra::V0_0_57);

    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    let ((), change) = alloc::measure(|| {
        for frame in frames.iter_mut() {
            assert!(boundary.upgrade(frame, &mut reporter));
            assert!(shims.upgrade_event(frame, &mut reporter));
            assert!(shims.downgrade_event(frame, &mut reporter));
        }
        Boundary::upgrade_input(&mut input, &mut reporter);
    });
    assert_eq!(change.allocations, 0, "{change:?}");
    assert_eq!(change.reallocations, 0, "{change:?}");
}
