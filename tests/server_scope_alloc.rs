//! Allocation budget of closing a subagent scope: staging the closers of one
//! scope must not cost anything that depends on how many unrelated items the
//! rest of the run holds open.
//!
//! Ids are `Arc<str>` and clone without allocating, so what this measures is
//! the growth of the staging `Vec` and the events built into it: the red build
//! paid about 190 more allocations at 4096 outside items than at 64, which is
//! what the 96-allocation slack is judged against (a cost linear in the
//! outside items would be thousands more).
//!
//! Allocations are counted on the measuring thread only
//! (`agui_test_support::alloc`); the run is driven inline on that thread.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use agui_sdk::server::{RunOutcome, Runner, RunnerConfig};
use agui_sdk::text::TextMessageRole;
use agui_test_support::alloc::{self, CountingAllocator};

use support_server::*;

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

const SCOPED: usize = 32;

/// The fewest allocations a whole run makes over several runs. With
/// `explicit_ends` the scope's texts are ended by the agent, so closing the
/// scope stages nothing.
fn allocations(outside: usize, explicit_ends: bool) -> u64 {
    let mut config = RunnerConfig::default();
    config.queue_capacity = 1;
    let runner = Runner::new(agent(move |ctx| async move {
        for _ in 0..outside {
            let text = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
            std::mem::forget(text);
        }
        let sub = ctx.emitter().start_subagent("s").await?;
        for _ in 0..SCOPED {
            let text = sub.emitter().start_text(TextMessageRole::Assistant).await?;
            if explicit_ends {
                text.end().await?;
            } else {
                std::mem::forget(text);
            }
        }
        sub.finish(None, None).await?;
        Ok(RunOutcome::success())
    }))
    .config(config);
    assert!(alloc::installed());
    (0..5)
        .map(|_| alloc::measure(|| drop(run(&runner, input()))).1.total())
        .min()
        .unwrap()
}

/// Allocations attributable to `close_open` staging `SCOPED` closers.
fn staging(outside: usize) -> i64 {
    allocations(outside, false) as i64 - allocations(outside, true) as i64
}

/// covers: LIFE-12, STRM-26
#[test]
fn closing_a_scope_costs_nothing_for_items_outside_it() {
    let _ = staging(8); // warm-up
    let (small, large) = (staging(64), staging(4096));
    // Quadratic-ish staging grew by about 190 allocations here. The slack
    // absorbs hash-seed noise and the log-scale container growth some std
    // versions show over 64x the items (+48 on Rust 1.75, macOS).
    assert!(
        large <= small + 96,
        "staging allocated {small} times beside 64 open items and {large} beside 4096"
    );
}
