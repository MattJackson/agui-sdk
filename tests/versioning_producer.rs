//! Server-side versioning: the version the runner declares, how declared
//! client versions are admitted, and the downgrade for a legacy peer.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_server;

use std::sync::{Arc, Mutex};

use agui_sdk::content::{Content, ContentPart, DataSource, MediaPart, PartSource, TextPart};
use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, NullSink, Scope};
use agui_sdk::event::Event;
use agui_sdk::lifecycle::RunFinishedOutcome;
use agui_sdk::server::input::{admit_input, InputPolicy};
use agui_sdk::server::{RunOutcome, Runner, RunnerConfig};
use agui_sdk::text::TextMessageRole;
use agui_sdk::version::{LegacyEra, PeerVersion, ProtocolVersion};
use agui_sdk::{ErrorCode, EventType, Message, RunAgentInput};

use support_server::*;

fn legacy_input() -> RunAgentInput {
    // No protocolVersion: a pre-versioning client.
    RunAgentInput::new("t1", "r1", vec![Message::user("hi")])
}

fn downgrading() -> RunnerConfig {
    let mut config = RunnerConfig::default();
    config.legacy_peer = Some(LegacyEra::V0_0_39);
    config
}

/// An agent using subagents and a multimodal tool result.
fn rich_agent() -> impl agui_sdk::server::Agent<State = serde_json::Value> {
    rich_agent_recording(Arc::new(Mutex::new(Vec::new())))
}

/// [`rich_agent`], recording each id the emitter hands it: the tool call,
/// the tool result's message, the subagent's text message.
fn rich_agent_recording(
    ids: Arc<Mutex<Vec<String>>>,
) -> impl agui_sdk::server::Agent<State = serde_json::Value> {
    agent(move |ctx| {
        let ids = Arc::clone(&ids);
        async move {
            let e = ctx.emitter();
            let call = e.start_tool_call("screenshot", None).await?;
            call.push_args("{not json").await?; // never repaired
            let id = call.end().await?;
            ids.lock().unwrap().push(id.to_string());
            let result = e
                .tool_result(
                    id,
                    Content::Parts(vec![
                        ContentPart::Text(TextPart::new("here: ")),
                        ContentPart::Image(MediaPart::new(PartSource::Data(DataSource::new(
                            "AAAA",
                            "image/png",
                        )))),
                        ContentPart::Text(TextPart::new("done")),
                    ]),
                )
                .await?;
            ids.lock().unwrap().push(result.to_string());
            let sub = e.start_subagent("researcher").await?;
            let said = sub.emitter().say("found it").await?;
            ids.lock().unwrap().push(said.to_string());
            sub.finish(None, None).await?;
            Ok(RunOutcome::cancelled())
        }
    })
}

/// covers: VER-02
#[test]
fn stops_use_the_outcome_not_an_addition() {
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::cancelled()) }));
    let events = run(&runner, input());
    assert!(!types(&events).contains(&EventType::Custom));
    assert_eq!(
        finished(&events).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
}

/// covers: VER-03, VER-30, VER-06, VER-28
#[test]
fn a_configured_legacy_peer_gets_a_downgraded_stream() {
    let (events, diagnostics) = run_collecting(
        Runner::new(rich_agent()).config(downgrading()),
        legacy_input(),
    );
    let kinds = types(&events);
    assert!(!kinds.contains(&EventType::SubagentStarted));
    assert!(!kinds.contains(&EventType::SubagentFinished));
    for event in &events {
        let wire = serde_json::to_value(event).unwrap();
        assert!(
            wire.get("subagentRunId").is_none(),
            "attribution stripped: {wire}"
        );
    }
    // The stream declares what it speaks: a legacy stream declares nothing.
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    assert!(started.protocol_version.is_none());
    // Stripping attribution is lossless and silent; only drops warn.
    assert!(diagnostics
        .iter()
        .all(|d| d.code == DiagnosticCode::LossyDowngrade));
}

/// covers: VER-04, VER-05, VER-32
#[test]
fn a_downgrade_is_a_pure_transformation() {
    let plain = run(&Runner::new(rich_agent()), legacy_input());
    let handed = Arc::new(Mutex::new(Vec::new()));
    let down = run(
        &Runner::new(rich_agent_recording(Arc::clone(&handed))).config(downgrading()),
        legacy_input(),
    );
    let handed = handed.lock().unwrap().clone();
    // Same events minus the dropped ones, in the same order, same ids.
    let kept: Vec<EventType> = types(&plain)
        .into_iter()
        .filter(|t| {
            !matches!(
                t,
                EventType::SubagentStarted | EventType::SubagentFinished | EventType::SubagentError
            )
        })
        .collect();
    assert_eq!(types(&down), kept);
    let ids = |events: &[Event]| -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::TextMessageStart(s) => Some(s.message_id.to_string()),
                Event::ToolCallStart(s) => Some(s.tool_call_id.to_string()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(ids(&plain).len(), ids(&down).len());
    // Every identifier on the downgraded wire is the one the agent was
    // handed, unaltered: the call on each of its events, the result's
    // message, and the subagent's message (its attribution dropped, its id
    // kept).
    let (call, result, said) = (&handed[0], &handed[1], &handed[2]);
    let mut seen = 0;
    for event in &down {
        let wire = serde_json::to_value(event).unwrap();
        if let Some(id) = wire.get("toolCallId") {
            assert_eq!(id, call.as_str(), "{wire}");
            seen += 1;
        }
        match event {
            Event::ToolCallResult(r) => assert_eq!(r.message_id.as_str(), result),
            Event::TextMessageStart(_)
            | Event::TextMessageContent(_)
            | Event::TextMessageEnd(_) => {
                assert_eq!(wire["messageId"], said.as_str(), "{wire}");
                seen += 1;
            }
            _ => {}
        }
    }
    // START, ARGS, END and RESULT carry the call; START, CONTENT, END the text.
    assert_eq!(seen, 7, "{down:?}");
    // Outcome unchanged.
    assert_eq!(
        finished(&down).outcome,
        Some(RunFinishedOutcome::cancelled())
    );
    // Malformed arguments are not repaired, and nothing is invented.
    let args = down.iter().find_map(|e| match e {
        Event::ToolCallArgs(a) => Some(a.delta.clone()),
        _ => None,
    });
    assert_eq!(args.as_deref(), Some("{not json"));
    let result = down.iter().find_map(|e| match e {
        Event::ToolCallResult(r) => Some(r.content.clone()),
        _ => None,
    });
    assert_eq!(result, Some(Content::Text("here: done".into())));
}

/// covers: VER-07, VER-08, VER-09
#[test]
fn a_lossy_downgrade_says_what_was_lost_and_what_to_upgrade() {
    let (_, diagnostics) = run_collecting(
        Runner::new(rich_agent()).config(downgrading()),
        legacy_input(),
    );
    assert!(!diagnostics.is_empty());
    for d in &diagnostics {
        assert_eq!(d.code, DiagnosticCode::LossyDowngrade);
        assert!(d.message.contains("upgrade"), "{}", d.message);
        assert!(d.message.len() <= 192);
    }
    // The dropped shapes are identified.
    // The dropped shape is identified by its event type.
    assert!(diagnostics.iter().any(|d| {
        let named = format!("{:?}", d.event_type);
        named.contains("SubagentStarted") || named.contains("SUBAGENT_STARTED")
    }));
    assert!(diagnostics.iter().any(|d| d.message.contains("image")));
}

/// covers: VER-03, VER-07, VER-08, VER-09
#[test]
fn a_media_part_a_0_0_47_peer_cannot_read_is_dropped_with_its_own_warning() {
    let mut config = RunnerConfig::default();
    config.legacy_peer = Some(LegacyEra::V0_0_47);
    let runner = Runner::new(agent(|ctx| async move {
        let sub = ctx.emitter().start_subagent("researcher").await?;
        let call = sub.emitter().start_tool_call("screenshot", None).await?;
        let id = call.end().await?;
        sub.emitter()
            .tool_result(
                id,
                Content::Parts(vec![
                    ContentPart::Text(TextPart::new("here")),
                    ContentPart::Image(
                        MediaPart::new(PartSource::Data(DataSource::new("AAAA", "image/png")))
                            .with_metadata(serde_json::json!({"a": 1})),
                    ),
                ]),
            )
            .await?;
        sub.finish(None, None).await?;
        Ok(RunOutcome::success())
    }))
    .config(config);
    let (events, diagnostics) = run_collecting(runner, legacy_input());
    let wire: Vec<_> = events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    let result = wire
        .iter()
        .find(|w| w["type"] == "TOOL_CALL_RESULT")
        .expect("a tool result");
    // The attribution strip survives the media part.
    assert!(result.get("subagentRunId").is_none(), "{result}");
    // The part the peer cannot read is gone, and said so exactly once.
    let parts = result["content"].as_array().expect("parts");
    assert!(parts.iter().all(|p| p["type"] == "text"), "{result}");
    let images: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.message.contains("image"))
        .collect();
    assert_eq!(images.len(), 1, "{diagnostics:?}");
    assert_eq!(images[0].pointer, "/content/1");
    assert!(
        images[0].message.contains("dropped"),
        "{}",
        images[0].message
    );
    assert!(!diagnostics.iter().any(|d| d.message.contains("metadata")));
}

/// covers: VER-10, VER-11
#[test]
fn downgrade_warnings_are_on_unless_silenced() {
    // Not silenced by default configuration: the sink sees them.
    let sink = Arc::new(CollectingSink::new());
    let runner = Runner::new(rich_agent())
        .config(downgrading())
        .diagnostic_sink(sink.clone());
    run(&runner, legacy_input());
    assert!(!sink.take().is_empty());
    // An explicit NullSink silences them.
    let runner = Runner::new(rich_agent())
        .config(downgrading())
        .diagnostic_sink(NullSink);
    let events = run(&runner, legacy_input());
    assert!(!events.is_empty());
}

/// covers: VER-18, VER-22, VER-23, VER-25
#[test]
fn a_newer_minor_is_served_with_our_own_version_and_a_warning() {
    let admitted = admit_input(
        input().with_protocol_version("1.3"),
        &InputPolicy::default(),
    )
    .unwrap();
    assert!(matches!(admitted.peer_version, PeerVersion::NewerMinor(_)));
    assert_eq!(
        codes(&admitted.diagnostics),
        [DiagnosticCode::NewerMinorVersion]
    );
    assert_eq!(admitted.diagnostics[0].scope, Scope::RunInput);
    let sink = Arc::new(CollectingSink::new());
    let runner = Runner::new(agent(|_ctx| async { Ok(RunOutcome::success()) }))
        .diagnostic_sink(sink.clone());
    let events = drain(
        runner
            .run(
                admitted,
                agui_sdk::server::CancelToken::new(),
                Default::default(),
            )
            .unwrap(),
    );
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    assert_eq!(started.protocol_version.as_deref(), Some("1.0"));
    assert_eq!(codes(&sink.take()), [DiagnosticCode::NewerMinorVersion]);
}

/// covers: VER-19, VER-31
#[test]
fn an_absent_version_is_served_as_current() {
    let admitted = admit_input(legacy_input(), &InputPolicy::default()).unwrap();
    assert_eq!(admitted.peer_version, PeerVersion::Absent);
    assert!(admitted.diagnostics.is_empty());
    // Without configuration nothing is guessed downward.
    let events = run(&Runner::new(rich_agent()), legacy_input());
    assert!(types(&events).contains(&EventType::SubagentStarted));
    let Event::RunStarted(started) = &events[0] else {
        panic!()
    };
    assert_eq!(started.protocol_version.as_deref(), Some("1.0"));
    // A declared current version is never downgraded, configured or not.
    let events = run(&Runner::new(rich_agent()).config(downgrading()), input());
    assert!(types(&events).contains(&EventType::SubagentStarted));
}

/// covers: VER-20
#[test]
fn versions_compare_numerically() {
    let v110 = ProtocolVersion::parse("1.10").unwrap();
    let v19 = ProtocolVersion::parse("1.9").unwrap();
    assert!(v110 > v19);
    let admitted = admit_input(
        input().with_protocol_version("1.10"),
        &InputPolicy::default(),
    )
    .unwrap();
    assert_eq!(admitted.peer_version, PeerVersion::NewerMinor(v110));
}

/// covers: VER-21
#[test]
fn an_uninterpretable_declaration_is_served_with_a_warning() {
    let admitted = admit_input(
        input().with_protocol_version("v-next"),
        &InputPolicy::default(),
    )
    .unwrap();
    assert!(matches!(
        admitted.peer_version,
        PeerVersion::Uninterpretable(_)
    ));
    assert_eq!(
        codes(&admitted.diagnostics),
        [DiagnosticCode::UninterpretableVersion]
    );
    assert!(
        !admitted.diagnostics[0].message.contains("v-next"),
        "peer text is not quoted"
    );
}

/// covers: VER-24
#[test]
fn only_another_major_is_rejected_before_the_run() {
    let error = admit_input(
        input().with_protocol_version("2.0"),
        &InputPolicy::default(),
    )
    .unwrap_err();
    assert_eq!(error.code(), ErrorCode::UnsupportedVersion);
    assert_eq!(error.http_status(), Some(400));
    let mut policy = InputPolicy::default();
    policy.reject_unsupported_major = false;
    let admitted = admit_input(input().with_protocol_version("2.0"), &policy).unwrap();
    assert_eq!(
        codes(&admitted.diagnostics),
        [DiagnosticCode::UnsupportedMajorVersion]
    );
}

/// covers: VER-33
#[test]
fn optional_fields_without_a_value_are_omitted() {
    let runner = Runner::new(agent(|ctx| async move {
        let t = ctx.emitter().start_text(TextMessageRole::Assistant).await?;
        t.push("x").await?;
        t.end().await?;
        let s = ctx.emitter().start_subagent("s").await?;
        s.finish(None, None).await?;
        Ok(RunOutcome::success())
    }));
    for event in run(&runner, input()) {
        let text = serde_json::to_string(&event).unwrap();
        assert!(!text.contains("null"), "null on the wire: {text}");
    }
}

/// covers: VER-28
#[test]
fn a_replayed_run_declares_its_own_version_and_the_live_one_its_own() {
    use agui_sdk::pipeline::verify::{ProducerRule, ProducerVerifier};

    // Mixed-age history: a recorded pre-versioning run, replayed untranslated
    // ahead of the requested (live) run, which declares 1.0.
    let input: RunAgentInput =
        serde_json::from_value(serde_json::json!({"threadId": "t", "runId": "r2",
            "messages": [], "state": {}, "tools": [], "context": [], "forwardedProps": {}}))
        .unwrap();
    let stream: Vec<Event> = [
        serde_json::json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r1"}),
        serde_json::json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
        serde_json::json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r2", "protocolVersion": "1.0"}),
        serde_json::json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r2"}),
    ]
    .into_iter()
    .map(|v| serde_json::from_value(v).unwrap())
    .collect();
    let mut replay = ProducerVerifier::for_input(&input);
    for (i, e) in stream.iter().enumerate() {
        replay
            .check(e)
            .unwrap_or_else(|v| panic!("event {i} refused: {}", v.message));
    }
    // Without the input nothing tells replay from live: one version per
    // stream.
    let mut live = ProducerVerifier::new();
    let verdicts: Vec<_> = stream.iter().map(|e| live.check(e).err()).collect();
    assert!(verdicts[..2].iter().all(Option::is_none));
    assert_eq!(
        verdicts[2].as_ref().map(|v| v.rule),
        Some(ProducerRule::InvalidValue)
    );
}
