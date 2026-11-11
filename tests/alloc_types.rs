//! Allocation budgets of the protocol types (API.md §8).
//!
//! Counting is per thread (`agui_test_support::alloc`): only allocations made
//! by the measured closure on this test's thread are counted. A process-wide
//! counter also sees the test harness's main thread, which allocates for its
//! own bookkeeping (the running-test map, the timeout queue, the result
//! channel's waker) right after it spawns this test's thread. When that
//! thread is descheduled (a loaded CI runner), its allocations land inside
//! this test's windows and read as a few spurious ones, in whichever check
//! happens to be measuring: the expectations below are exact for every
//! feature set, the old counter was not.

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use agui_sdk::content::{Content, ContentPart, TextPart};
use agui_sdk::event::{Event, TextMessageContent};
use agui_sdk::ids::MessageId;
use agui_sdk::interrupt::{Expiry, Interrupt};
use agui_sdk::json::JsonObject;
use agui_sdk::usage::TokenUsageBuilder;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::Value;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Allocations (plus reallocations) made by `f` on this thread, and its result.
fn count<T>(f: impl FnOnce() -> T) -> (u64, T) {
    let (out, stats) = alloc::measure(f);
    (stats.total(), out)
}

fn parse(json: &str) -> Event {
    serde_json::from_str(json).unwrap()
}

/// covers: BASIC-01
#[test]
fn protocol_type_allocation_budgets() {
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    // Id clone is a reference-count bump.
    let id = MessageId::new("m1");
    let (n, clone) = count(|| id.clone());
    assert_eq!(n, 0, "id clone");
    assert!(Arc::ptr_eq(clone.as_arc(), id.as_arc()));

    // Parsing allocates once per owned string the event keeps, nothing more,
    // and an id read recently on this thread (`m1`, `c1` again below) shares
    // the earlier buffer instead. The one exception is serde_json's own
    // scratch buffer, allocated once per parse the first time a string
    // contains an escape (`\"` below).
    let cases: &[(&str, u64)] = &[
        (
            r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m1","delta":"hello"}"#,
            2,
        ),
        (r#"{"type":"TEXT_MESSAGE_END","messageId":"m1"}"#, 0),
        (
            r#"{"type":"TOOL_CALL_ARGS","toolCallId":"c1","delta":"{\"q\":","subagentRunId":"s1"}"#,
            3 + 1,
        ),
        (
            r#"{"type":"TOOL_CALL_START","toolCallId":"c1","toolCallName":"search","parentMessageId":"m1","timestamp":5}"#,
            1,
        ),
        (
            r#"{"type":"RUN_FINISHED","threadId":"t1","runId":"r1","outcome":{"type":"success"}}"#,
            2,
        ),
        (
            r#"{"type":"REASONING_MESSAGE_CONTENT","messageId":"n1","delta":"thinking"}"#,
            2,
        ),
    ];
    for (json, budget) in cases {
        let (n, event) = count(|| parse(json));
        assert_eq!(n, *budget, "parse {json}");
        drop(event);
    }

    // Open JSON costs what the same `Value` costs on its own: no temporary tree.
    let metadata = r#"{"acme.trace":"t-1","n":[1,2]}"#;
    let (value_cost, value) = count(|| serde_json::from_str::<Value>(metadata).unwrap());
    drop(value);
    let event = format!(r#"{{"type":"TEXT_MESSAGE_END","messageId":"m1","metadata":{metadata}}}"#);
    let (n, parsed) = count(|| parse(&event));
    assert_eq!(n, 1 + value_cost, "parse with metadata");
    drop(parsed);

    // Serializing into a reused buffer allocates nothing after warm-up.
    let typical = [
        parse(cases[0].0),
        parse(cases[2].0),
        parse(cases[4].0),
        parse(&event),
    ];
    let mut buffer = Vec::with_capacity(64);
    for e in &typical {
        e.write_json(&mut buffer);
    }
    for e in &typical {
        let (n, ()) = count(|| e.write_json(&mut buffer));
        assert_eq!(n, 0, "write_json {:?}", e.event_type());
    }
    let content = Event::from(TextMessageContent::new("m1", "x".repeat(10)));
    content.write_json(&mut buffer);
    let (n, ()) = count(|| content.write_json(&mut buffer));
    assert_eq!(n, 0);

    // Merging moves `newer`'s members in; nothing is cloned.
    let mut held = JsonObject::new();
    held.insert("a", 1);
    let mut newer = JsonObject::new();
    newer.insert("b", "two");
    newer.insert("a", Value::Null);
    let (n, ()) = count(|| held.merge_from(newer));
    assert_eq!(n, 0, "merge_from");
    assert_eq!(held.get("a"), Some(&Value::Null));

    // Flattening borrows plain text, and a single text part.
    let text = Content::from("plain");
    let (n, flat) = count(|| text.flatten_text().len());
    assert_eq!((n, flat), (0, 5));
    let one_part = Content::Parts(vec![ContentPart::Text(TextPart::new("only"))]);
    let (n, _) = count(|| one_part.flatten_text().len());
    assert_eq!(n, 0);

    // The usage builder allocates the provider and model strings only.
    let (n, usage) = count(|| {
        TokenUsageBuilder::new("anthropic", "claude-x")
            .input(1200, true)
            .cached_input(1000)
            .output(300, true)
            .reasoning(20)
            .build()
    });
    assert_eq!(n, 2, "usage builder");
    assert_eq!(usage.total_tokens, Some(1500));

    // Expiry is read in place.
    let interrupt =
        Interrupt::new("i1", "approval").with_expires_at("2026-08-20T17:00:00.123+02:00");
    let (n, expiry) = count(|| interrupt.expiry());
    assert_eq!(n, 0, "expiry");
    assert!(matches!(expiry, Expiry::At(_)));
}
