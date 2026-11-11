//! Shared helpers for the server-side rule tests: a closure agent, a
//! single-threaded executor drive, and an independent stream checker that
//! does not reuse the SDK's own verifier.

#![allow(dead_code)]

use std::collections::HashSet;
use std::future::Future;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex};

use agui_sdk::diagnostics::{CollectingSink, Diagnostic, DiagnosticCode};
use agui_sdk::event::Event;
use agui_sdk::server::input::{admit_input, AdmittedInput, InputPolicy};
use agui_sdk::server::{
    Agent, BoxError, CancelToken, Extensions, RunContext, RunOutcome, RunStream, Runner,
};
use agui_sdk::{EventType, Message, RunAgentInput};

/// An agent made from a closure.
pub struct FnAgent<F, Fut>(F, PhantomData<fn() -> Fut>);

/// Wraps a closure `|ctx| async move { … }` as an [`Agent`].
pub fn agent<F, Fut>(f: F) -> FnAgent<F, Fut>
where
    F: Fn(RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RunOutcome, BoxError>> + Send + 'static,
{
    FnAgent(f, PhantomData)
}

impl<F, Fut> Agent for FnAgent<F, Fut>
where
    F: Fn(RunContext) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<RunOutcome, BoxError>> + Send + 'static,
{
    type State = serde_json::Value;

    fn run(&self, ctx: RunContext) -> impl Future<Output = Result<RunOutcome, BoxError>> + Send {
        (self.0)(ctx)
    }
}

/// A minimal input: thread `t1`, run `r1`, one user message.
pub fn input() -> RunAgentInput {
    RunAgentInput::new("t1", "r1", vec![Message::user("hi")]).with_protocol_version("1.0")
}

/// Admits a typed input with the default policy.
pub fn admitted(input: RunAgentInput) -> AdmittedInput {
    match admit_input(input, &InputPolicy::default()) {
        Ok(admitted) => admitted,
        Err(e) => panic!("input not admitted: {e}"),
    }
}

/// Drains a stream on the current thread.
pub fn drain(mut stream: RunStream) -> Vec<Event> {
    futures_executor::block_on(async move {
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            out.push(event);
        }
        out
    })
}

/// Starts a run with no extensions.
pub fn start<A: Agent>(runner: &Runner<A>, input: RunAgentInput, cancel: CancelToken) -> RunStream {
    match runner.run(admitted(input), cancel, Extensions::new()) {
        Ok(stream) => stream,
        Err(e) => panic!("run refused: {e}"),
    }
}

/// Runs `agent` once on `input` and collects the stream.
pub fn run<A: Agent>(runner: &Runner<A>, input: RunAgentInput) -> Vec<Event> {
    drain(start(runner, input, CancelToken::new()))
}

/// Runs with a collecting diagnostic sink.
pub fn run_collecting<A: Agent>(
    runner: Runner<A>,
    input: RunAgentInput,
) -> (Vec<Event>, Vec<Diagnostic>) {
    let sink = Arc::new(CollectingSink::new());
    let runner = runner.diagnostic_sink(sink.clone());
    let events = run(&runner, input);
    (events, sink.take())
}

pub fn types(events: &[Event]) -> Vec<EventType> {
    events.iter().map(Event::event_type).collect()
}

pub fn codes(diagnostics: &[Diagnostic]) -> Vec<DiagnosticCode> {
    diagnostics.iter().map(|d| d.code).collect()
}

/// The single terminal event of a one-run stream.
pub fn terminal(events: &[Event]) -> &Event {
    match events.last() {
        Some(e) if e.is_run_terminal() => e,
        other => panic!("stream does not end with a terminal: {other:?}"),
    }
}

pub fn finished(events: &[Event]) -> &agui_sdk::lifecycle::RunFinished {
    match terminal(events) {
        Event::RunFinished(f) => f,
        other => panic!("expected RUN_FINISHED, got {other:?}"),
    }
}

pub fn run_error(events: &[Event]) -> &agui_sdk::lifecycle::RunError {
    match terminal(events) {
        Event::RunError(e) => e,
        other => panic!("expected RUN_ERROR, got {other:?}"),
    }
}

/// Independent structural check of one requested run (after any replayed
/// runs): it opens with RUN_STARTED or RUN_ERROR, ends with exactly one
/// terminal, nothing follows it, and a RUN_FINISHED is preceded by the
/// closing of everything opened.
pub fn assert_one_valid_run(events: &[Event]) {
    assert!(!events.is_empty(), "empty stream");
    let terminals = events.iter().filter(|e| e.is_run_terminal()).count();
    // Replayed runs each contribute one RUN_FINISHED; the requested run is the last.
    let starts = events
        .iter()
        .filter(|e| matches!(e, Event::RunStarted(_)))
        .count();
    assert!(
        terminals == starts || (terminals == starts + 1 && starts == 0),
        "terminals {terminals} vs starts {starts}: {:?}",
        types(events)
    );
    assert!(
        matches!(events[0], Event::RunStarted(_) | Event::RunError(_)),
        "first event {:?}",
        events[0].event_type()
    );
    assert!(terminal(events).is_run_terminal());
    let mut open: HashSet<String> = HashSet::new();
    for event in events {
        match event {
            Event::RunStarted(_) => open.clear(),
            Event::TextMessageStart(e) => assert!(open.insert(format!("text:{}", e.message_id))),
            Event::TextMessageContent(e) => {
                assert!(open.contains(&format!("text:{}", e.message_id)))
            }
            Event::TextMessageEnd(e) => assert!(open.remove(&format!("text:{}", e.message_id))),
            Event::ToolCallStart(e) => assert!(open.insert(format!("tool:{}", e.tool_call_id))),
            Event::ToolCallArgs(e) => assert!(open.contains(&format!("tool:{}", e.tool_call_id))),
            Event::ToolCallEnd(e) => assert!(open.remove(&format!("tool:{}", e.tool_call_id))),
            Event::ReasoningStart(e) => assert!(open.insert(format!("rspan:{}", e.message_id))),
            Event::ReasoningEnd(e) => assert!(open.remove(&format!("rspan:{}", e.message_id))),
            Event::ReasoningMessageStart(e) => {
                assert!(open.insert(format!("rmsg:{}", e.message_id)))
            }
            Event::ReasoningMessageEnd(e) => {
                assert!(open.remove(&format!("rmsg:{}", e.message_id)))
            }
            Event::StepStarted(e) => {
                assert!(open.insert(format!("step:{}:{:?}", e.step_name, e.subagent_run_id)))
            }
            Event::StepFinished(e) => {
                assert!(open.remove(&format!("step:{}:{:?}", e.step_name, e.subagent_run_id)))
            }
            Event::SubagentStarted(e) => assert!(open.insert(format!("sub:{}", e.subagent_run_id))),
            Event::SubagentFinished(e) => {
                assert!(open.remove(&format!("sub:{}", e.subagent_run_id)))
            }
            Event::SubagentError(e) => assert!(open.remove(&format!("sub:{}", e.subagent_run_id))),
            Event::RunFinished(_) => assert!(open.is_empty(), "open at RUN_FINISHED: {open:?}"),
            _ => {}
        }
    }
}

/// A shared, poisonable slot used to inject store failures.
pub fn poisoned<T: Send + 'static>(value: T) -> Arc<Mutex<T>> {
    let shared = Arc::new(Mutex::new(value));
    let clone = Arc::clone(&shared);
    let _ = std::thread::spawn(move || {
        let _guard = clone.lock();
        panic!("poison the store lock");
    })
    .join();
    shared
}
