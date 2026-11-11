//! Events per second through the server runner (emitter → verifier → bounded
//! queue → stream), with an allocation count.
//!
//! The deltas are built before measuring, so what is counted is what the
//! runner adds beyond the event itself. The steady-state figure is the slope
//! between two run sizes, which cancels the fixed per-run setup (the boxed
//! agent future, the queue, the verifier's first sets). Budget: 0.
//!
//! `cargo bench --features server --bench server_runner`

use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agui_sdk::server::input::{admit_input, InputPolicy};
use agui_sdk::server::{
    Agent, BoxError, CancelToken, Extensions, RunContext, RunOutcome, Runner, RunnerConfig,
};
use agui_sdk::text::TextMessageRole;
use agui_sdk::{Message, RunAgentInput};
use stats_alloc::{Region, StatsAlloc, INSTRUMENTED_SYSTEM};

#[global_allocator]
static GLOBAL: &StatsAlloc<std::alloc::System> = &INSTRUMENTED_SYSTEM;

struct Streamer {
    deltas: Arc<Mutex<Vec<String>>>,
}

impl Agent for Streamer {
    type State = serde_json::Value;

    fn run(&self, ctx: RunContext) -> impl Future<Output = Result<RunOutcome, BoxError>> + Send {
        let deltas = std::mem::take(&mut *self.deltas.lock().unwrap_or_else(|e| e.into_inner()));
        async move {
            let text = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
            for delta in deltas {
                text.push(delta).await?;
            }
            text.end().await?;
            Ok(RunOutcome::success())
        }
    }
}

/// Runs one stream of `n` content events; returns (allocations, seconds).
fn measure(n: usize, capacity: usize) -> (usize, f64) {
    let deltas = Arc::new(Mutex::new((0..n).map(|i| format!("token {i} ")).collect()));
    let mut config = RunnerConfig::default();
    config.queue_capacity = capacity;
    let runner = Runner::new(Streamer {
        deltas: Arc::clone(&deltas),
    })
    .config(config)
    .diagnostic_sink(agui_sdk::diagnostics::NullSink);
    let input =
        RunAgentInput::new("t", "r", vec![Message::user("go")]).with_protocol_version("1.0");
    let admitted = match admit_input(input, &InputPolicy::default()) {
        Ok(a) => a,
        Err(e) => panic!("{e}"),
    };
    let region = Region::new(GLOBAL);
    let start = Instant::now();
    let mut stream = match runner.run(admitted, CancelToken::new(), Extensions::new()) {
        Ok(stream) => stream,
        Err(e) => panic!("{e}"),
    };
    let count = futures_executor::block_on(async {
        let mut count = 0usize;
        while let Some(event) = stream.next().await {
            drop(event);
            count += 1;
        }
        count
    });
    let elapsed = start.elapsed().as_secs_f64();
    let allocations = region.change().allocations + region.change().reallocations;
    assert_eq!(count, n + 4);
    drop(stream);
    (allocations, elapsed)
}

fn main() {
    for capacity in [16usize, 1024] {
        let _ = measure(10_000, capacity); // warm-up
        let small = 100_000;
        let (a1, _) = measure(small, capacity);
        let (a2, t2) = measure(small * 2, capacity);
        let per_event = (a2.saturating_sub(a1)) as f64 / small as f64;
        let rate = (small * 2) as f64 / t2;
        println!(
            "server_runner capacity={capacity}: {:.0} events/s, {per_event:.4} allocs/event steady state \
             ({a1} allocs for {small} events, {a2} for {})",
            rate,
            small * 2
        );
        assert!(
            per_event < 0.01,
            "allocation budget exceeded: {per_event} allocs/event"
        );
    }
}
