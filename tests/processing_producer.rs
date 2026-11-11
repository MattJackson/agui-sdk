//! Producer-side processing (`processing.mdx`, `architecture.mdx`): the
//! exchange shape, the strict writer, wire-independent semantics, admission
//! of received input, and open objects.

#![cfg(feature = "server")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_producer;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::pipeline::verify::{ProducerRule, ProducerVerifier};
use agui_sdk::server::encode::Encoder;
use agui_sdk::server::http::{handle, HttpRequest, ServeConfig};
use agui_sdk::server::input::{admit, InputPolicy};
use agui_sdk::server::{CancelToken, RunOutcome, Runner};
use agui_sdk::wire::WireFormat;
use agui_sdk::{Event, RunAgentInput};
use serde_json::{json, Value};
use support_producer::{agent, ev, finished, refuses, started, verify_with};

fn chatty() -> Runner<impl agui_sdk::server::Agent<State = Value>> {
    Runner::new(agent(|ctx| async move {
        let e = ctx.emitter();
        e.say("hello").await?;
        let call = e.start_tool_call("lookup", None).await?;
        call.push_args("{\"q\":1}").await?;
        let id = call.end().await?;
        e.tool_result(id, "found").await?;
        e.custom("vendor:note", json!({"k": [1, 2]})).await?;
        Ok(RunOutcome::success())
    }))
}

fn run_events(
    runner: &Runner<impl agui_sdk::server::Agent<State = Value>>,
    body: &[u8],
) -> Vec<Event> {
    let admitted = admit(body, &InputPolicy::default()).unwrap();
    let mut stream = runner
        .run(
            admitted,
            CancelToken::new(),
            agui_sdk::server::Extensions::new(),
        )
        .unwrap();
    support_producer::runtime().block_on(async move {
        let mut out = Vec::new();
        while let Some(event) = stream.next().await {
            out.push(event);
        }
        out
    })
}

/// covers: ARCH-03
#[test]
fn one_input_opens_an_exchange_bracketed_by_the_run_lifecycle() {
    let events = run_events(&chatty(), &support_producer::input_json("t-arch", "r-arch"));
    let Event::RunStarted(first) = &events[0] else {
        panic!("{:?}", events[0])
    };
    let Event::RunFinished(last) = events.last().unwrap() else {
        panic!()
    };
    assert_eq!(
        (first.thread_id.as_str(), first.run_id.as_str()),
        ("t-arch", "r-arch")
    );
    assert_eq!(
        (last.thread_id.as_str(), last.run_id.as_str()),
        ("t-arch", "r-arch")
    );
    // The verifier for that input refuses a stream answering another thread.
    let input: RunAgentInput =
        serde_json::from_slice(&support_producer::input_json("t-arch", "r-arch")).unwrap();
    let refused = verify_with(
        ProducerVerifier::for_input(&input),
        &[json!({"type": "RUN_STARTED", "threadId": "other", "runId": "r-arch"})],
    );
    assert!(
        matches!(refused, Err((0, ProducerRule::Identity, _))),
        "{refused:?}"
    );
    // ...and anything after the requested run (RUNIN-31).
    let refused = verify_with(
        ProducerVerifier::for_input(&input),
        &[
            json!({"type": "RUN_STARTED", "threadId": "t-arch", "runId": "r-arch"}),
            json!({"type": "RUN_FINISHED", "threadId": "t-arch", "runId": "r-arch"}),
            json!({"type": "RUN_STARTED", "threadId": "t-arch", "runId": "r-next"}),
        ],
    );
    assert!(
        matches!(refused, Err((2, ProducerRule::Identity, _))),
        "{refused:?}"
    );
}

/// covers: ARCH-04
#[test]
fn the_server_writes_only_what_the_schema_defines() {
    // Everything the runner wrote validates strictly.
    for event in run_events(&chatty(), &support_producer::input_json("t1", "r1")) {
        let mut bytes = Vec::new();
        event.write_json(&mut bytes);
        Event::parse_strict(&bytes)
            .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&bytes)));
    }
    // Values the types admit but the schema rejects are refused.
    // (The builders normalise `null` to absent; the public fields do not.)
    let mut custom = agui_sdk::passthrough::Custom::new("n", json!(1));
    custom.raw_event = Some(Value::Null);
    let mut verifier = ProducerVerifier::new();
    verifier.check(&ev(started("r1"))).unwrap();
    let refused = verifier.check(&Event::from(custom)).unwrap_err();
    assert_eq!(refused.rule, ProducerRule::InvalidValue);
    assert!(refused.message.contains("BASIC-09"), "{}", refused.message);
    let mut finished_null = agui_sdk::lifecycle::RunFinished::new("t-prod", "r1");
    finished_null.result = Some(Value::Null);
    assert_eq!(
        verifier
            .check(&Event::from(finished_null))
            .unwrap_err()
            .rule,
        ProducerRule::InvalidValue
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "STEP_STARTED", "stepName": "s", "timestamp": 9_007_199_254_740_992_i64}),
        ],
        1,
        ProducerRule::InvalidValue,
    );
    refuses(
        &[
            started("r1"),
            json!({"type": "RUN_FINISHED", "threadId": "t-prod", "runId": "r1", "usage": [{"inputTokens": 1, "outputTokens": 1, "totalTokens": 3}]}),
        ],
        1,
        ProducerRule::InvalidValue,
    );
}

/// covers: ARCH-06
#[test]
fn semantics_do_not_vary_by_wire() {
    let events = run_events(&chatty(), &support_producer::input_json("t1", "r1"));
    let mut sse = Encoder::for_format(WireFormat::Sse);
    let mut text = Vec::new();
    for event in &events {
        sse.encode(event, &mut text).unwrap();
    }
    let from_sse: Vec<Value> = support_producer::sse_events(&text);
    let typed: Vec<Value> = events
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(from_sse, typed);
    #[cfg(feature = "proto")]
    {
        let mut proto = Encoder::for_format(WireFormat::Protobuf);
        let mut bytes = Vec::new();
        for event in &events {
            proto.encode(event, &mut bytes).unwrap();
        }
        let mut frames = agui_sdk::proto::framing::FrameDecoder::new(1 << 20);
        frames.push(agui_sdk::wire::Chunk::from(bytes));
        let mut decoded = Vec::new();
        while let Some(frame) = frames.next_frame().unwrap() {
            match agui_sdk::proto::decode::decode_event(&frame).unwrap() {
                agui_sdk::proto::decode::Decoded::Event(value) => decoded.push(value),
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(decoded.len(), typed.len());
        for (wire, original) in decoded.iter().zip(&typed) {
            let back: Event = serde_json::from_value(wire.clone()).unwrap();
            let original: Event = serde_json::from_value(original.clone()).unwrap();
            assert_eq!(back, original);
        }
    }
}

/// covers: PROC-35
#[test]
fn received_input_is_stripped_when_unrecognised_and_refused_when_malformed() {
    let runner = chatty();
    let config = ServeConfig::default();
    let mut body: Value =
        serde_json::from_slice(&support_producer::input_json("t1", "r1")).unwrap();
    body["futureMember"] = json!({"x": 1});
    let body = serde_json::to_vec(&body).unwrap();
    let admitted = admit(&body, &InputPolicy::default()).unwrap();
    assert!(
        admitted
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::UnknownField),
        "{:?}",
        admitted.diagnostics
    );
    let ok = handle(
        &runner,
        &config,
        HttpRequest::new(Some("application/json"), None, &body),
        CancelToken::new(),
    );
    assert!(ok.is_ok());
    let malformed = br#"{"threadId": 7, "runId": "r1", "messages": []}"#;
    let refused = handle(
        &runner,
        &config,
        HttpRequest::new(Some("application/json"), None, malformed),
        CancelToken::new(),
    )
    .unwrap_err();
    assert_eq!(refused.status, 400);
    assert_eq!(refused.code, agui_sdk::ErrorCode::InputRejected);
}

/// covers: PROC-36
#[test]
fn only_described_event_types_can_be_emitted() {
    // `Event` is closed: an undescribed type is not an event at all.
    let retired: Result<Event, _> = serde_json::from_value(json!({"type": "THINKING_START"}));
    assert!(retired.is_err());
    // Vendor material goes through CUSTOM, which the verifier accepts anywhere.
    support_producer::accepts(&[
        started("r1"),
        json!({"type": "CUSTOM", "name": "vendor:thinking", "value": {"phase": "start"}}),
        finished("r1"),
    ]);
}

/// covers: SCHF-02
#[test]
fn metadata_and_patch_operations_stay_open() {
    let op = json!({"op": "add", "path": "/n", "value": 1, "x-vendor": true});
    let delta =
        ev(json!({"type": "STATE_DELTA", "delta": [op], "metadata": {"vendor:k": {"any": null}}}));
    let mut verifier = ProducerVerifier::new();
    verifier.check(&ev(started("r1"))).unwrap();
    verifier.check(&delta).unwrap();
    let wire = serde_json::to_value(&delta).unwrap();
    assert_eq!(wire["delta"][0]["x-vendor"], true);
    assert_eq!(wire["metadata"]["vendor:k"]["any"], Value::Null);
}
