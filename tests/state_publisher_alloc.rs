//! Allocation cost of a publication that falls back to a snapshot: the
//! publisher must not build (and drop) a root `replace` of the whole state
//! on the way.
//!
//! Allocations are counted on the measuring thread only
//! (`agui_test_support::alloc`).

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agui_sdk::server::state::StatePublisher;
use agui_sdk::Event;
use agui_test_support::alloc::{self, CountingAllocator};
use serde_json::json;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// State above the default node ceiling, so any addition makes `diff` give
/// up: publishing it copies the state for the snapshot and for the new
/// baseline, and nothing more.
#[test]
fn a_snapshot_fallback_copies_the_state_only_for_the_snapshot_and_baseline() {
    assert!(alloc::installed());
    let baseline = json!({"a": vec![0; 200_000]});
    let mut next = baseline.clone();
    next["b"] = json!(1);
    let (_, one_copy) = alloc::measure(|| next.clone());
    let mut publisher = StatePublisher::new(baseline);
    let (event, stats) = alloc::measure(|| publisher.publish(&next));
    assert!(matches!(event, Some(Event::StateSnapshot(_))), "{event:?}");
    let copies = stats.bytes as f64 / one_copy.bytes as f64;
    assert!(copies < 2.5, "{copies:.2} copies of the state");
}
