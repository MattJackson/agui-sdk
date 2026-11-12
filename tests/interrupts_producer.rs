//! Server-side interrupts: the interrupt outcome, resume handling, and the
//! opt-in interrupt store.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter};
use agui_sdk::event::Event;
use agui_sdk::ids::ThreadId;
use agui_sdk::interrupt::{Interrupt, ResumeEntry};
use agui_sdk::json::JsonObject;
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::server::helpers::{InterruptStore, MemoryInterruptStore, ResumeCheck};
use agui_sdk::server::{Agent, BoxError, RunContext, RunOutcome, Runner};
use agui_sdk::usage::TokenUsage;
use agui_sdk::EventType;
use serde_json::json;

use support_server::*;

fn interrupts_of(events: &[agui_sdk::event::Event]) -> Vec<Interrupt> {
    match &finished(events).outcome {
        Some(RunFinishedOutcome::Interrupt { interrupts, .. }) => interrupts.clone(),
        other => panic!("expected an interrupt outcome, got {other:?}"),
    }
}

/// covers: INTR-01, INTR-02
#[test]
fn a_run_that_needs_input_ends_with_the_interrupt_outcome() {
    let runner = Runner::new(agent(|_ctx| async {
        Ok(RunOutcome::interrupt([Interrupt::new("i1", "approval")]))
    }));
    let events = run(&runner, input());
    assert_eq!(interrupts_of(&events).len(), 1);

    // An empty list cannot be sent as an interrupt, nor turned into success.
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::interrupt([])) }));
    let events = run(&runner, input());
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
}

/// covers: INTR-03
#[test]
fn a_frontend_tool_stop_is_success_and_a_told_stop_is_cancelled() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter()
            .start_tool_call("confirm", None)
            .await?
            .end()
            .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(
        &runner,
        input().with_tools([agui_sdk::tool::Tool::new("confirm", "ask")]),
    );
    assert!(matches!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::Success { .. })
    ));
    let runner = Runner::new(agent(|ctx| async move {
        ctx.cancellation().cancel();
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
}

/// covers: INTR-04
#[test]
fn interrupt_ids_must_be_unique() {
    let runner = Runner::new(agent(|_ctx| async {
        Ok(RunOutcome::interrupt([
            Interrupt::new("same", "a"),
            Interrupt::new("same", "b"),
        ]))
    }));
    let events = run(&runner, input());
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
}

/// covers: INTR-05, INTR-06
#[test]
fn interrupt_fields_pass_through_verbatim() {
    let mut schema = JsonObject::new();
    schema.insert("type", json!("object"));
    let interrupt = Interrupt::new("i1", "x-vendor:custom-reason")
        .with_message("Approve the transfer?")
        .with_tool_call_id("call-9")
        .with_response_schema(schema)
        .with_subagent_run_id("sub-1");
    let expected = interrupt.clone();
    let runner = Runner::new(agent(move |_ctx| {
        let interrupt = interrupt.clone();
        async move { Ok(RunOutcome::interrupt([interrupt])) }
    }));
    let events = run(&runner, input());
    let got = interrupts_of(&events);
    assert_eq!(got, [expected]);
    assert_eq!(
        got[0].subagent_run_id.as_ref().map(|s| s.as_str()),
        Some("sub-1")
    );
}

/// An agent that keeps interrupts in a store across runs, the way the
/// helpers are meant to be used: check, warn, act only on answers, keep
/// whatever is still open.
struct Approver {
    store: Arc<MemoryInterruptStore>,
    sink: Arc<CollectingSink>,
    acted: Arc<Mutex<Vec<String>>>,
    raise: Option<Interrupt>,
}

impl Approver {
    fn new(store: Arc<MemoryInterruptStore>) -> Self {
        Self {
            store,
            sink: Arc::new(CollectingSink::new()),
            acted: Arc::new(Mutex::new(Vec::new())),
            raise: None,
        }
    }
}

impl Agent for Approver {
    type State = serde_json::Value;

    async fn run(&self, ctx: RunContext) -> Result<RunOutcome, BoxError> {
        let thread = ctx.thread_id().clone();
        let check: ResumeCheck = self.store.check(&thread, ctx.resume()).await?;
        let mut reporter = Reporter::new(self.sink.clone(), 256);
        check.report_unrecognised(ctx.resume(), &mut reporter);
        for entry in &check.recognised {
            self.acted
                .lock()
                .unwrap()
                .push(entry.interrupt_id.to_string());
            self.store.resolve(&thread, &entry.interrupt_id).await?;
        }
        ctx.emitter().record_usage(
            TokenUsage::default()
                .with_provider("anthropic")
                .with_model("m")
                .with_input_tokens(5)
                .with_output_tokens(1),
        );
        let mut open = Vec::new();
        open.extend(self.raise.clone());
        open.extend(check.uncovered);
        self.store.record(&thread, open.clone()).await?;
        if open.is_empty() {
            Ok(RunOutcome::success())
        } else {
            Ok(RunOutcome::interrupt(open))
        }
    }
}

fn block<F: std::future::Future>(future: F) -> F::Output {
    futures_executor::block_on(future)
}

/// covers: INTR-16
///
/// One run on a fresh runner, so this cannot see usage carried over from an
/// earlier run; INTR-17 is covered by the two-run LIFE-36 test in
/// `tests/lifecycle_usage_producer.rs`.
#[test]
fn a_resuming_run_keeps_the_thread_and_reports_only_its_own_usage() {
    let store = Arc::new(MemoryInterruptStore::default());
    block(store.record(&ThreadId::new("t1"), vec![Interrupt::new("i1", "approval")])).unwrap();
    let seen = Arc::new(Mutex::new(None));
    let seen2 = Arc::clone(&seen);
    let runner = Runner::new(agent(move |ctx| {
        let seen = Arc::clone(&seen2);
        async move {
            *seen.lock().unwrap() = Some((
                ctx.messages().len(),
                ctx.state().clone(),
                ctx.resume().len(),
            ));
            ctx.emitter().record_usage(
                TokenUsage::default()
                    .with_provider("anthropic")
                    .with_model("m")
                    .with_input_tokens(5)
                    .with_output_tokens(1),
            );
            Ok(RunOutcome::success())
        }
    }));
    let resuming = input()
        .with_state(json!({"left": 1}))
        .with_resume([ResumeEntry::resolved("i1", json!({"approved": true}))]);
    let events = run(&runner, resuming);
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    assert_eq!(started.thread_id.as_str(), "t1");
    assert_eq!(*seen.lock().unwrap(), Some((1, json!({"left": 1}), 1)));
    let usage = finished(&events).usage.clone().unwrap();
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0].input_tokens, Some(5));
}

/// covers: INTR-18, INTR-19, INTR-24
#[test]
fn unrecognised_resume_entries_are_warned_about_and_ignored() {
    let store = Arc::new(MemoryInterruptStore::default());
    block(store.record(&ThreadId::new("t1"), vec![Interrupt::new("i1", "approval")])).unwrap();
    let approver = Approver::new(Arc::clone(&store));
    let sink = Arc::clone(&approver.sink);
    let acted = Arc::clone(&approver.acted);
    let resuming = input().with_resume([
        ResumeEntry::resolved("i1", json!(1)),
        ResumeEntry::resolved("never-raised", json!(2)),
    ]);
    let events = run(&Runner::new(approver), resuming);
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::success())
    );
    assert_eq!(*acted.lock().unwrap(), ["i1"]);
    let diagnostics = sink.take();
    assert_eq!(codes(&diagnostics), [DiagnosticCode::UnknownResumeEntry]);
    assert_eq!(diagnostics[0].pointer, "/resume/1");

    // A resume list on a run with nothing to continue: every entry is
    // unrecognised, and the run is an ordinary one.
    let check = block(
        MemoryInterruptStore::default()
            .check(&ThreadId::new("fresh"), &[ResumeEntry::cancelled("x")]),
    )
    .unwrap();
    assert!(check.recognised.is_empty() && check.uncovered.is_empty());
    assert_eq!(check.unrecognised.len(), 1);
}

/// covers: INTR-18, INTR-19
#[test]
fn unrecognised_resume_pointers_name_each_entry() {
    let report = |check: &ResumeCheck, entries: &[ResumeEntry]| -> Vec<String> {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink.clone(), 256);
        check.report_unrecognised(entries, &mut reporter);
        sink.take().into_iter().map(|d| d.pointer).collect()
    };
    // Duplicate unknown ids each point at their own entry.
    let entries = [
        ResumeEntry::resolved("x", json!(1)),
        ResumeEntry::resolved("i1", json!(2)),
        ResumeEntry::resolved("x", json!(3)),
        ResumeEntry::resolved("y", json!(4)),
    ];
    let check = ResumeCheck::classify(&[Interrupt::new("i1", "approval")], &entries);
    assert_eq!(
        report(&check, &entries),
        ["/resume/0", "/resume/2", "/resume/3"]
    );

    // An entry the check lists but the list lacks names the list, never an
    // invented index.
    let ghost = ResumeCheck::new(vec![], vec![ResumeEntry::cancelled("ghost")], vec![]);
    assert_eq!(report(&ghost, &entries), ["/resume"]);
}

/// covers: INTR-20, INTR-21, INTR-22, INTR-23
#[test]
fn an_uncovered_interrupt_stays_open_and_blocks_success() {
    let store = Arc::new(MemoryInterruptStore::default());
    let thread = ThreadId::new("t1");
    block(store.record(
        &thread,
        vec![
            Interrupt::new("i1", "approval"),
            Interrupt::new("i2", "approval"),
        ],
    ))
    .unwrap();
    let approver = Approver::new(Arc::clone(&store));
    let acted = Arc::clone(&approver.acted);
    let resuming = input().with_resume([ResumeEntry::resolved("i1", json!(true))]);
    let events = run(&Runner::new(approver), resuming);
    assert_eq!(
        *acted.lock().unwrap(),
        ["i1"],
        "never act on the absent entry"
    );
    let still_open = interrupts_of(&events);
    let ids: Vec<String> = still_open.iter().map(|i| i.id.to_string()).collect();
    assert_eq!(ids, ["i2"]);
    let check = block(store.check(&thread, &[])).unwrap();
    assert_eq!(
        check.uncovered.len(),
        1,
        "the open interrupt is kept, not abandoned"
    );

    // An agent raising its own interrupt keeps the open one alongside.
    let mut approver = Approver::new(Arc::clone(&store));
    approver.raise = Some(Interrupt::new("i3", "more"));
    let events = run(&Runner::new(approver), input());
    let ids: Vec<String> = interrupts_of(&events)
        .iter()
        .map(|i| i.id.to_string())
        .collect();
    assert_eq!(ids, ["i3", "i2"]);
}

#[test]
fn the_memory_store_records_resolves_and_is_bounded() {
    let store = MemoryInterruptStore::new(2, None);
    let (a, b, c) = (ThreadId::new("a"), ThreadId::new("b"), ThreadId::new("c"));
    block(store.record(&a, vec![Interrupt::new("x", "r"), Interrupt::new("y", "r")])).unwrap();
    block(store.resolve(&a, &"x".into())).unwrap();
    assert_eq!(block(store.check(&a, &[])).unwrap().uncovered.len(), 1);
    block(store.resolve(&a, &"y".into())).unwrap();
    assert!(block(store.check(&a, &[])).unwrap().uncovered.is_empty());
    // An empty record clears the thread.
    block(store.record(&a, vec![Interrupt::new("z", "r")])).unwrap();
    block(store.record(&a, vec![])).unwrap();
    assert!(block(store.check(&a, &[])).unwrap().uncovered.is_empty());
    // At most two threads: the least recently recorded goes first, and its
    // entries come back unrecognised, never covered.
    block(store.record(&a, vec![Interrupt::new("1", "r")])).unwrap();
    block(store.record(&b, vec![Interrupt::new("2", "r")])).unwrap();
    block(store.record(&c, vec![Interrupt::new("3", "r")])).unwrap();
    let evicted = block(store.check(&a, &[ResumeEntry::resolved("1", json!(0))])).unwrap();
    assert!(evicted.recognised.is_empty());
    assert_eq!(evicted.unrecognised.len(), 1);
    assert_eq!(block(store.check(&c, &[])).unwrap().uncovered.len(), 1);

    // Expired records read as nothing raised.
    let store = MemoryInterruptStore::new(10, Some(Duration::from_millis(1)));
    block(store.record(&a, vec![Interrupt::new("1", "r")])).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    assert!(block(store.check(&a, &[])).unwrap().uncovered.is_empty());
}

/// Thread ids are the client's, so the store also bounds their bytes (1 KiB per
/// allowed thread), evicting the least recently recorded threads first.
#[test]
fn the_memory_store_bounds_thread_id_bytes() {
    let store = MemoryInterruptStore::new(10, None);
    let thread = |i: usize| ThreadId::new(format!("{i}{}", "t".repeat(4095)));
    for i in 1..=4 {
        block(store.record(&thread(i), vec![Interrupt::new("x", "r")])).unwrap();
    }
    let entry = [ResumeEntry::resolved("x", json!(0))];
    for evicted in [1, 2] {
        let check = block(store.check(&thread(evicted), &entry)).unwrap();
        assert!(check.recognised.is_empty(), "thread {evicted} kept");
        assert_eq!(check.unrecognised.len(), 1);
    }
    for kept in [3, 4] {
        let check = block(store.check(&thread(kept), &entry)).unwrap();
        assert_eq!(check.recognised.len(), 1, "thread {kept} evicted");
    }
    // One id over the whole budget is still kept, alone.
    let huge = ThreadId::new("h".repeat(64 * 1024));
    block(store.record(&huge, vec![Interrupt::new("x", "r")])).unwrap();
    assert_eq!(
        block(store.check(&huge, &entry)).unwrap().recognised.len(),
        1
    );
    assert!(block(store.check(&thread(4), &entry))
        .unwrap()
        .recognised
        .is_empty());

    // Honest ids never reach the budget: ten UUID-sized ids all stay.
    let store = MemoryInterruptStore::new(10, None);
    let uuid = |i: usize| ThreadId::new(format!("{i:036}"));
    for i in 0..10 {
        block(store.record(&uuid(i), vec![Interrupt::new("x", "r")])).unwrap();
    }
    for i in 0..10 {
        assert_eq!(
            block(store.check(&uuid(i), &entry))
                .unwrap()
                .recognised
                .len(),
            1
        );
    }
}

/// A store whose backend is down.
struct DownStore;

impl InterruptStore for DownStore {
    async fn record(&self, _: &ThreadId, _: Vec<Interrupt>) -> agui_sdk::Result<()> {
        Err(agui_sdk::Error::new(
            agui_sdk::ErrorCode::Internal,
            "store down",
        ))
    }
    async fn check(&self, _: &ThreadId, _: &[ResumeEntry]) -> agui_sdk::Result<ResumeCheck> {
        Err(agui_sdk::Error::new(
            agui_sdk::ErrorCode::Internal,
            "store down",
        ))
    }
    async fn resolve(&self, _: &ThreadId, _: &agui_sdk::ids::InterruptId) -> agui_sdk::Result<()> {
        Err(agui_sdk::Error::new(
            agui_sdk::ErrorCode::Internal,
            "store down",
        ))
    }
}

#[test]
fn a_failing_store_fails_the_run_cleanly() {
    let runner = Runner::new(agent(|ctx| async move {
        let check = DownStore.check(ctx.thread_id(), ctx.resume()).await?;
        let _ = check;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_one_valid_run(&events);
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
}
