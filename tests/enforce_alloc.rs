//! Allocation budget of enforcement : a conformant event
//! costs nothing beyond the typed value's own allocations.
//!
//! Allocations are counted on the measuring thread only
//! (`agui_test_support::alloc`), so the harness's threads and the other test
//! in this binary cannot land in a window.

use std::sync::Arc;

use agui_sdk::diagnostics::{NullSink, Reporter};
use agui_sdk::pipeline::enforce::{validate_strict, Enforcer};
use agui_sdk::pipeline::Limits;
use agui_sdk::Event;
use agui_test_support::alloc::{self, CountingAllocator};

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Conformant events covering the hot shapes: deltas (escapes included),
/// openers, open JSON, nested protocol objects and unions.
const CONFORMANT: &[(&str, &str)] = &[
    (
        "TextMessageContentEvent",
        r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m-1","delta":"Hello, world."}"#,
    ),
    (
        "TextMessageContentEvent",
        r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m-1","delta":"line one\nline \"two\" é"}"#,
    ),
    (
        "TextMessageStartEvent",
        r#"{"type":"TEXT_MESSAGE_START","messageId":"m-1","role":"assistant","metadata":{"k":[1,null]}}"#,
    ),
    (
        "StateDeltaEvent",
        r#"{"type":"STATE_DELTA","delta":[{"op":"replace","path":"/a/b","value":{"x":1}}]}"#,
    ),
    (
        "MessagesSnapshotEvent",
        r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"1","role":"user","content":[{"type":"text","text":"hi"},{"type":"image","source":{"type":"url","value":"https://x"}}]},{"id":"2","role":"assistant","toolCalls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}]}"#,
    ),
    (
        "RunFinishedEvent",
        r#"{"type":"RUN_FINISHED","threadId":"t","runId":"r","outcome":{"type":"interrupt","interrupts":[{"id":"i","reason":"approval"}]},"usage":[{"inputTokens":3}]}"#,
    ),
];

/// The fewest allocations `f` makes on this thread over several runs, so a
/// one-time lazy initialisation (a thread-local cache warming up) does not
/// count against the steady state.
fn count(f: impl Fn()) -> u64 {
    assert!(
        alloc::installed(),
        "the counting allocator is not installed"
    );
    (0..5)
        .map(|_| alloc::measure(&f).1.total())
        .min()
        .unwrap_or(u64::MAX)
}

/// Enforcement alone (strict validation builds nothing) allocates zero.
#[test]
fn enforcing_a_conformant_event_allocates_nothing() {
    for (anchor, frame) in CONFORMANT {
        let _ = validate_strict(anchor, frame.as_bytes()); // warm-up
        let n = count(|| assert!(validate_strict(anchor, frame.as_bytes()).is_ok()));
        assert_eq!(n, 0, "enforcing {frame} allocated {n} times");
    }
}

/// Enforcing into a typed event costs no more than decoding it unchecked.
#[test]
fn enforced_event_costs_what_the_event_costs() {
    let enforcer = Enforcer::new(Limits::default());
    let mut reporter = Reporter::new(Arc::new(NullSink), 256);
    for (_, frame) in CONFORMANT {
        let bytes = frame.as_bytes();
        assert!(
            enforcer.enforce_event_json(bytes, &mut reporter).is_ok(),
            "{frame}"
        );
        let ours = count(|| {
            let mut reporter = Reporter::new(Arc::new(NullSink), 256);
            assert!(enforcer.enforce_event_json(bytes, &mut reporter).is_ok());
        });
        // The comparison builds the same reporter, so its cost cancels out.
        let plain = count(|| {
            let _reporter = Reporter::new(Arc::new(NullSink), 256);
            assert!(serde_json::from_slice::<Event>(bytes).is_ok());
        });
        assert!(
            ours <= plain,
            "{frame}: {ours} allocations vs {plain} unchecked"
        );
    }
}
