//! Server-side tool calls: emitting calls and results, frontend calls left
//! for the application, and the content-part helpers.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use std::sync::{Arc, Mutex};

use agui_sdk::content::{Content, ContentPart, DataSource, MediaPart, PartSource, TextPart};
use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter};
use agui_sdk::event::Event;
use agui_sdk::ids::ToolCallId;
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::server::helpers::{flatten_for_legacy_peer, PartFilter};
use agui_sdk::server::{RunOutcome, Runner};
use agui_sdk::text::TextMessageRole;
use agui_sdk::tool::{Tool, ToolCallStart};
use agui_sdk::{ErrorCode, EventType};
use serde_json::json;

use support_server::*;

fn reporter() -> (Arc<CollectingSink>, Reporter) {
    let sink = Arc::new(CollectingSink::new());
    (sink.clone(), Reporter::new(sink, 256))
}

fn image() -> ContentPart {
    ContentPart::Image(MediaPart::new(PartSource::Data(DataSource::new(
        "AAAA",
        "image/png",
    ))))
}

fn starts(events: &[Event]) -> Vec<ToolCallStart> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallStart(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

/// covers: TOOL-01, TOOL-02
#[test]
fn calls_attach_to_their_message_and_its_owner() {
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("s").await?;
        let text = sub.emitter().start_text(TextMessageRole::Assistant).await?;
        let parent = text.id().clone();
        text.push("calling").await?;
        text.end().await?;
        sub.emitter()
            .start_tool_call("lookup", Some(parent))
            .await?
            .end()
            .await?;
        sub.finish(None, None).await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let text_start = events
        .iter()
        .find_map(|e| match e {
            Event::TextMessageStart(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap();
    let call = &starts(&events)[0];
    assert_eq!(
        call.parent_message_id.as_ref(),
        Some(&text_start.message_id)
    );
    assert_eq!(call.subagent_run_id, text_start.subagent_run_id);
    assert!(call.subagent_run_id.is_some());
}

/// covers: TOOL-06, TOOL-07
#[test]
fn a_closed_call_reopens_only_when_it_agrees() {
    let runner = Runner::new(agent(|ctx| async move {
        let call = ctx.emitter().start_tool_call("search", None).await?;
        call.push_args("{\"q\":").await?;
        let id = call.end().await?;
        ctx.emitter()
            .emit(ToolCallStart::new(id.clone(), "search"))
            .await?;
        ctx.emitter()
            .emit(agui_sdk::tool::ToolCallArgs::new(id.clone(), "1}"))
            .await?;
        ctx.emitter()
            .emit(agui_sdk::tool::ToolCallEnd::new(id))
            .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(starts(&events).len(), 2);
    assert_eq!(terminal(&events).event_type(), EventType::RunFinished);

    let runner = Runner::new(agent(|ctx| async move {
        let id = ctx
            .emitter()
            .start_tool_call("search", None)
            .await?
            .end()
            .await?;
        let renamed = ctx.emitter().emit(ToolCallStart::new(id, "other")).await;
        assert_eq!(
            renamed.err().map(|e| e.code()),
            Some(ErrorCode::ProducerViolation)
        );
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    assert_eq!(
        run_error(&events).code.as_deref(),
        Some("server.producer_violation")
    );
}

/// covers: TOOL-12, TOOL-13, TOOL-15
#[test]
fn agent_executed_calls_are_answered_in_the_same_run() {
    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let a = e.start_tool_call("weather", None).await?.end().await?;
        // Structured data travels as serialized text.
        e.tool_result(a, json!({"temp": 21}).to_string()).await?;
        let b = e.start_tool_call("chart", None).await?.end().await?;
        e.tool_result(
            b,
            Content::Parts(vec![ContentPart::Text(TextPart::new("chart:")), image()]),
        )
        .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input());
    let results: Vec<Content> = events
        .iter()
        .filter_map(|e| match e {
            Event::ToolCallResult(r) => Some(r.content.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(results[0], Content::Text("{\"temp\":21}".into()));
    assert!(matches!(&results[1], Content::Parts(p) if p.len() == 2));
    // Both answered: nothing pending.
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::success())
    );
}

/// covers: TOOL-17, TOOL-18
#[test]
fn unusable_result_parts_are_dropped_and_the_call_still_answered() {
    let (sink, mut reporter) = reporter();
    let filter = PartFilter::text_only();
    let kept = filter.filter_result(
        Content::Parts(vec![ContentPart::Text(TextPart::new("a")), image()]),
        &mut reporter,
    );
    assert_eq!(
        kept,
        Content::Parts(vec![ContentPart::Text(TextPart::new("a"))])
    );
    let none = filter.filter_result(Content::Parts(vec![image(), image()]), &mut reporter);
    assert_eq!(none, Content::Text(String::new()));
    assert_eq!(codes(&sink.take()), [DiagnosticCode::ContentPartSkipped; 3]);
    let text = filter.filter_result(Content::Text("plain".into()), &mut reporter);
    assert_eq!(text, Content::Text("plain".into()));
}

/// covers: TOOL-21, TOOL-22, TOOL-23
#[test]
fn flattening_for_a_legacy_peer_warns_and_leaves_no_placeholder() {
    let (sink, mut reporter) = reporter();
    let flat = flatten_for_legacy_peer(
        Content::Parts(vec![
            ContentPart::Text(TextPart::new("before ")),
            image(),
            ContentPart::Text(TextPart::new("after")),
        ]),
        &mut reporter,
    );
    assert_eq!(flat, "before after");
    let diagnostics = sink.take();
    assert_eq!(codes(&diagnostics), [DiagnosticCode::LossyDowngrade]);
    assert_eq!(diagnostics[0].pointer, "/content/1");
    assert_eq!(
        flatten_for_legacy_peer(Content::Text("s".into()), &mut reporter),
        "s"
    );
}

fn frontend_tools() -> [Tool; 2] {
    [
        Tool::new("confirm", "ask the user"),
        Tool::new("navigate", "go somewhere"),
    ]
}

/// covers: TOOL-24
#[test]
fn a_frontend_call_is_never_answered_by_the_agent() {
    // The refusal is carried out of the agent and checked here: an assertion
    // inside the agent would only panic, and the runner turns a panic into a
    // RUN_ERROR that the outer checks would accept.
    let answered: Arc<Mutex<Option<Option<ErrorCode>>>> = Arc::default();
    let seen = Arc::clone(&answered);
    let runner = Runner::new(agent(move |ctx| {
        let seen = Arc::clone(&seen);
        async move {
            let id = ctx
                .emitter()
                .start_tool_call("confirm", None)
                .await?
                .end()
                .await?;
            let answered = ctx.emitter().tool_result(id, "yes").await;
            *seen.lock().unwrap() = Some(answered.err().map(|e| e.code()));
            Ok(RunOutcome::success())
        }
    }));
    let events = run(&runner, input().with_tools(frontend_tools()));
    assert_eq!(
        *answered.lock().unwrap(),
        Some(Some(ErrorCode::ProducerViolation))
    );
    assert!(!types(&events).contains(&EventType::ToolCallResult));
    assert_eq!(terminal(&events).event_type(), EventType::RunError);
    assert_eq!(
        run_error(&events).code.as_deref(),
        Some("server.producer_violation")
    );
}

/// covers: TOOL-25, TOOL-26, TOOL-28, TOOL-29, TOOL-30, TOOL-31
#[test]
fn unanswered_calls_are_listed_in_order_on_a_successful_finish() {
    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let first = e.start_tool_call("navigate", None).await?.end().await?;
        let own = e.start_tool_call("weather", None).await?.end().await?;
        e.tool_result(own, "sunny").await?;
        let second = e.start_tool_call("confirm", None).await?.end().await?;
        let _ = (first, second);
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_tools(frontend_tools()));
    let calls = starts(&events);
    let expected: Vec<ToolCallId> =
        vec![calls[0].tool_call_id.clone(), calls[2].tool_call_id.clone()];
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::success_with_pending(expected))
    );
}

/// An agent that returns `RunOutcome::success()` as soon as only the
/// frontend call remains is closed at once: the runner never waits for a
/// frontend result (it never returns `Pending`), and `RUN_FINISHED` with the
/// call in `pendingToolCallIds` directly follows the call's `TOOL_CALL_END`.
/// covers: TOOL-27
#[test]
fn a_run_left_waiting_only_on_a_frontend_result_finishes_at_once() {
    use std::task::{Context, Poll, Wake, Waker};

    struct Noop;
    impl Wake for Noop {
        fn wake(self: Arc<Self>) {}
    }

    let runner = Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        let own = e.start_tool_call("weather", None).await?.end().await?;
        e.tool_result(own, "sunny").await?;
        // Nothing left to do but the application's answer: finish now.
        e.start_tool_call("confirm", None).await?.end().await?;
        Ok(RunOutcome::success())
    }))
    .diagnostic_sink(agui_sdk::diagnostics::NullSink);
    let mut stream = start(
        &runner,
        input().with_tools(frontend_tools()),
        Default::default(),
    );
    let waker = Waker::from(Arc::new(Noop));
    let mut cx = Context::from_waker(&waker);
    let mut events = Vec::new();
    loop {
        match stream.poll_next(&mut cx) {
            Poll::Ready(Some(event)) => events.push(event),
            Poll::Ready(None) => break,
            Poll::Pending => panic!(
                "the runner parked after {:?}: it must not wait for a frontend result",
                types(&events)
            ),
        }
    }
    assert_one_valid_run(&events);
    let calls = starts(&events);
    let frontend = calls[1].tool_call_id.clone();
    assert_eq!(
        types(&events)[events.len() - 2..],
        [EventType::ToolCallEnd, EventType::RunFinished]
    );
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::success_with_pending([frontend]))
    );
}

#[test]
fn an_agent_can_finish_promptly_with_a_frontend_call_unanswered() {
    // Promptness itself is the agent author's duty (documented on
    // `server::Agent`). What the SDK owes is that finishing does not wait:
    // an agent that ends a frontend call and returns success at once gets
    // RUN_FINISHED right after the call, naming it as pending, with no
    // result invented and nothing held back.
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter()
            .start_tool_call("confirm", None)
            .await?
            .end()
            .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_tools(frontend_tools()));
    let kinds = types(&events);
    assert_eq!(
        &kinds[kinds.len() - 2..],
        [EventType::ToolCallEnd, EventType::RunFinished],
        "{kinds:?}"
    );
    assert!(!kinds.contains(&EventType::ToolCallResult));
    let call = starts(&events)[0].tool_call_id.clone();
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::success_with_pending(vec![call]))
    );
}

/// covers: TOOL-35
#[test]
fn an_unadvertised_call_is_not_a_violation() {
    let runner = Runner::new(agent(|ctx| async move {
        ctx.emitter()
            .start_tool_call("not_advertised", None)
            .await?
            .end()
            .await?;
        Ok(RunOutcome::success())
    }));
    let events = run(&runner, input().with_tools(frontend_tools()));
    assert!(matches!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::Success { pending_tool_call_ids: Some(ref ids), .. }) if ids.len() == 1
    ));
}

/// covers: TOOL-36
#[test]
fn reserved_part_kinds_cannot_be_emitted() {
    // The part union has no reasoning, tool-call or assistant arm, so a
    // producer cannot construct one; such JSON does not become a part.
    for kind in ["reasoning", "toolCall", "assistant"] {
        let parsed: Result<ContentPart, _> =
            serde_json::from_value(json!({"type": kind, "text": "x"}));
        assert!(parsed.is_err(), "{kind}");
    }
}
