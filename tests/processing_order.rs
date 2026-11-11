//! Stage order in the consumer pipeline (`basic/processing.mdx`, "Where
//! middleware sits"; `architecture.mdx`).
//!
//! These tests drive [`Processor`] directly, except the middleware order,
//! which is driven through `AgentClient` and its real middleware chain.

#![allow(clippy::unwrap_used, clippy::expect_used)]

#[cfg(feature = "client")]
mod support_client;
mod support_pipeline;

use agui_sdk::diagnostics::DiagnosticCode;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{expand, in_run, run, run_value, started, text, types};

/// Streams with a mix of verdicts, for path-parity checks.
fn verdict_corpus() -> Vec<Vec<Value>> {
    vec![
        in_run(&[
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "extra": 1}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
        ]),
        in_run(&[json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "x", "delta": "a"})]),
        in_run(&[json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": 7})]),
        in_run(&[json!({"type": "FUTURE_EVENT", "x": 1})]),
        vec![json!({"type": "TEXT_MESSAGE_START", "messageId": "m1"})],
        in_run(&[json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1"})]),
        vec![
            started("r1"),
            json!({"type": "RUN_ERROR", "message": "x"}),
            started("r2"),
        ],
    ]
}

/// covers: ARCH-01, PROC-28
#[test]
fn stages_run_in_the_specified_order() {
    // Expansion precedes verification: a continuation chunk with no explicit
    // start is legal, because by the time it is verified it has one.
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
    ]));
    r.assert_ok();
    // Enforcement precedes expansion: the unknown field on the chunk is
    // stripped with a warning before the chunk becomes events.
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "futureField": true}),
    ]));
    r.assert_ok();
    assert!(r.warned(DiagnosticCode::UnknownField));
    // The reducer runs last: a rejected event never reaches history.
    let mut body = text("m1", "kept").to_vec();
    body.push(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "never"}));
    let r = run(&in_run(&body));
    assert!(r.error.is_some());
    assert_eq!(r.message("m1")["content"], "kept");
}

/// covers: ARCH-02, PROC-29
#[cfg(feature = "client")]
#[test]
fn middleware_sees_material_before_enforcement_removes_it() {
    use std::sync::{Arc, Mutex};

    use agui_sdk::client::middleware::{map_frames, MiddlewareRequest, Next};
    use agui_sdk::client::{AgentClient, Frame, RunRequest};
    use agui_sdk::diagnostics::CollectingSink;
    use support_client::{block_on, mock, run_of};

    // The client's real chain: middleware installed on `AgentClient`, frames
    // from the transport, enforcement and the rest after the chain.
    let frames = run_of(
        "t1",
        "r1",
        vec![
            json!({"type": "CUSTOM", "name": "n", "value": 1, "vendorField": {"keep": "me"}}),
            json!({"type": "VENDOR_EVENT", "x": 1}),
        ],
    );
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let sink = Arc::new(CollectingSink::new());
    let client = AgentClient::with_transport(mock(frames))
        .diagnostic_sink(Arc::clone(&sink))
        .middleware(move |request: MiddlewareRequest, next: Next| {
            let log = Arc::clone(&log);
            map_frames(next.run(request), move |mut frame: Frame| {
                let value = frame.value_mut()?;
                log.lock().unwrap().push(value.clone());
                Ok(Some(frame))
            })
        })
        .build()
        .unwrap();
    let mut run = client.run(RunRequest::new().thread_id("t1").run_id("r1").user("hi"));
    let delivered: Vec<Value> = block_on(async {
        let mut out = Vec::new();
        while let Some(update) = run.next().await {
            out.push(serde_json::to_value(&*update.unwrap().event).unwrap());
        }
        out
    });
    // The middleware saw the unknown member and the unknown event whole...
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen[1]["vendorField"], json!({"keep": "me"}), "{seen:?}");
    assert_eq!(seen[2]["type"], "VENDOR_EVENT", "{seen:?}");
    // ...and enforcement, after the chain, removed both before delivery.
    assert_eq!(delivered[1]["type"], "CUSTOM");
    assert!(delivered[1].get("vendorField").is_none());
    assert!(!delivered.iter().any(|e| e["type"] == "VENDOR_EVENT"));
    let codes: Vec<_> = sink.take().into_iter().map(|d| d.code).collect();
    assert!(codes.contains(&DiagnosticCode::UnknownField), "{codes:?}");
    assert!(codes.contains(&DiagnosticCode::UnknownEvent), "{codes:?}");
}

/// covers: ARCH-03
#[test]
fn an_exchange_is_bracketed_by_the_run_lifecycle() {
    let r = run(&in_run(&text("m1", "hi")));
    r.assert_ok();
    assert_eq!(r.types().first(), Some(&"RUN_STARTED"));
    assert_eq!(r.types().last(), Some(&"RUN_FINISHED"));
    run(&text("m1", "unbracketed")).assert_rejected("First event must be 'RUN_STARTED'");
}

/// covers: ARCH-05, PROC-30
#[test]
fn unrecognised_material_is_stripped_and_never_reaches_the_application() {
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "unknownProp": 1}),
        json!({"type": "BRAND_NEW_EVENT", "messageId": "m1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1"}),
    ]));
    r.assert_ok();
    assert!(r.wire().iter().all(|e| e.get("unknownProp").is_none()));
    assert!(!r.wire().iter().any(|e| e["type"] == "BRAND_NEW_EVENT"));
    assert!(r.warned(DiagnosticCode::UnknownField));
    assert!(r.warned(DiagnosticCode::UnknownEvent));
}

/// covers: ARCH-06, PROC-31
#[test]
fn every_path_reaches_the_same_verdicts() {
    for stream in verdict_corpus() {
        let fast = run(&stream);
        let middleware = run_value(&stream);
        assert_eq!(
            fast.error.as_ref().map(|e| e.code()),
            middleware.error.as_ref().map(|e| e.code()),
            "{stream:?}"
        );
        assert_eq!(fast.wire(), middleware.wire(), "{stream:?}");
        assert_eq!(fast.messages, middleware.messages);
        assert_eq!(fast.termination, middleware.termination);
    }
}

/// covers: PROC-21
#[test]
fn enforcement_runs_before_verification_and_application_code() {
    // A malformed known value is judged by enforcement, even when the
    // sequence would also be wrong.
    let r = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": 5, "delta": "x"}),
    ]));
    assert_eq!(
        r.error.as_ref().map(|e| e.code()),
        Some(ErrorCode::InvalidValue)
    );
    // An event enforcement drops is never verified: an unknown event first
    // in the stream does not trip the first-event rule.
    run(&[
        json!({"type": "PRELUDE", "x": 1}),
        started("r1"),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r1"}),
    ])
    .assert_ok();
}

/// covers: PROC-22
#[test]
fn the_compatibility_boundary_sees_the_stream_first() {
    // A retired chunk shape reaches the processor's fast path and is
    // translated before enforcement would drop it as unknown.
    let r = run(&in_run(&[
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_TEXT_MESSAGE_START"}),
        json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "pondering"}),
        json!({"type": "THINKING_TEXT_MESSAGE_END"}),
        json!({"type": "THINKING_END"}),
    ]));
    r.assert_ok();
    assert!(r.types().contains(&"REASONING_MESSAGE_CONTENT"));
    assert!(!r.warned(DiagnosticCode::UnknownEvent));
    assert!(r
        .messages
        .iter()
        .any(|m| m["role"] == "reasoning" && m["content"] == "pondering"));
}

/// covers: PROC-23, PROC-24
#[test]
fn chunk_and_expanded_forms_are_equivalent_downstream() {
    let chunked = in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "delta": "b"}),
    ]);
    // What a chain that expands between its stages hands on.
    let expanded: Vec<Value> = expand(&chunked)
        .unwrap()
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .collect();
    assert_eq!(
        types(&expand(&chunked).unwrap())[1..4],
        [
            "TEXT_MESSAGE_START",
            "TEXT_MESSAGE_CONTENT",
            "TEXT_MESSAGE_CONTENT"
        ]
    );
    let a = run_value(&chunked);
    let b = run_value(&expanded);
    a.assert_ok();
    b.assert_ok();
    assert_eq!(a.wire(), b.wire());
    assert_eq!(a.messages, b.messages);
}

/// covers: PROC-25
#[test]
fn expansion_carries_protocol_material_but_not_the_chunk_timestamp() {
    let r = run(&in_run(&[json!({
        "type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": "a", "timestamp": 1700000000000_i64,
        "metadata": {"k": "v"}, "rawEvent": {"provider": "x"}, "subagentRunId": "s1",
        "newerField": 1
    })]));
    r.assert_ok();
    // On the canonical order the unknown field is judged (and warned about)
    // before expansion.
    assert!(r.warned(DiagnosticCode::UnknownField));
    let wire = r.wire();
    let start = &wire[1];
    assert_eq!(start["type"], "TEXT_MESSAGE_START");
    assert_eq!(start["metadata"], json!({"k": "v"}));
    assert_eq!(start["rawEvent"], json!({"provider": "x"}));
    assert_eq!(start["subagentRunId"], "s1");
    assert_eq!(wire[2]["delta"], "a");
    assert!(wire[1..=3].iter().all(|e| e.get("timestamp").is_none()));
    assert!(wire.iter().all(|e| e.get("newerField").is_none()));
}

/// covers: PROC-26, PROC-27
#[test]
fn a_malformed_chunk_is_judged_as_an_event_and_never_defaulted() {
    // A wrongly typed known value on a chunk is fatal, exactly as it is on
    // the expanded event, on both entry paths; nothing is synthesized from
    // the chunk before the verdict.
    for bad in [
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": 7, "delta": "x"}),
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "delta": 3}),
        json!({"type": "REASONING_MESSAGE_CHUNK", "messageId": ["m1"]}),
        json!({"type": "TOOL_CALL_CHUNK", "toolCallId": "t1", "toolCallName": false, "delta": "{}"}),
    ] {
        for (path, r) in [
            ("fast", run(&in_run(std::slice::from_ref(&bad)))),
            ("value", run_value(&in_run(std::slice::from_ref(&bad)))),
        ] {
            assert!(
                matches!(
                    r.error.as_ref().map(|e| e.code()),
                    Some(ErrorCode::InvalidValue | ErrorCode::MalformedEvent)
                ),
                "{path}: {bad} was not fatal: {:?}",
                r.types()
            );
            assert_eq!(r.types(), ["RUN_STARTED"], "{path}: {bad}");
        }
    }
    // The same value sent plainly gets the same verdict, so the form a
    // producer used does not decide whether its bug is reported.
    let plain = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": 7}),
    ]));
    let chunk = run(&in_run(&[
        json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": 7, "delta": "x"}),
    ]));
    assert_eq!(
        plain.error.as_ref().map(|e| e.code()),
        chunk.error.as_ref().map(|e| e.code())
    );
    // An unrecognised value of the closed role set is stripped with a
    // warning (PROC-17), on the chunk as on the plain event: reported, not
    // concealed.
    for r in [
        run(&in_run(&[
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "narrator", "delta": "x"}),
        ])),
        run_value(&in_run(&[
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m1", "role": "narrator", "delta": "x"}),
        ])),
    ] {
        r.assert_ok();
        assert!(r
            .diagnostics
            .iter()
            .any(|d| d.code == DiagnosticCode::UnknownEnumValue && d.pointer == "/role"));
    }
}
