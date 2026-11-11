//! Versioning, consumer side: the compatibility boundary and the era shims
//! (`basic/versioning.mdx`, `DEPRECATIONS.md`).

#![allow(clippy::expect_used)] // a failed expectation is the test failing

#[cfg(feature = "client")]
mod support_client;
mod support_pipeline;

use std::sync::Arc;

use agui_sdk::diagnostics::{
    CollectingSink, Diagnostic, DiagnosticCode, NullSink, Reporter, Scope,
};
use agui_sdk::event::EventType;
use agui_sdk::message::Message;
use agui_sdk::pipeline::compat::{Boundary, EraShims};
use agui_sdk::pipeline::{PipelineConfig, Processor};
use agui_sdk::version::{LegacyEra, PeerPin, ProtocolVersion};
use agui_sdk::ErrorCode;
use serde_json::{json, Value};
use support_pipeline::{drive_with, Run};

fn collecting() -> (Reporter, Arc<CollectingSink>) {
    let sink = Arc::new(CollectingSink::new());
    (Reporter::new(sink.clone(), 256), sink)
}

/// Runs frames through a fresh boundary, keeping what continues.
fn through_boundary(frames: Vec<Value>, reporter: &mut Reporter) -> Vec<Value> {
    let mut boundary = Boundary::new();
    frames
        .into_iter()
        .filter_map(|mut frame| boundary.upgrade(&mut frame, reporter).then_some(frame))
        .collect()
}

/// Runs frames through a boundary and then the era's upgrade shims.
fn through_era(era: LegacyEra, frames: Vec<Value>, reporter: &mut Reporter) -> Vec<Value> {
    let shims = EraShims::new(era);
    through_boundary(frames, reporter)
        .into_iter()
        .filter_map(|mut frame| shims.upgrade_event(&mut frame, reporter).then_some(frame))
        .collect()
}

/// The shipped no-middleware path: each frame as bytes into
/// `Processor::push_json`, which owns the boundary on this path.
fn shipped(frames: &[Value]) -> Run {
    support_pipeline::run(frames)
}

/// The middleware path a host builds: the boundary first, then
/// `Processor::push_value`. Diagnostics of both stages are kept.
fn boundary_then_value(frames: Vec<Value>) -> Run {
    let (mut reporter, sink) = collecting();
    let upgraded = through_boundary(frames, &mut reporter);
    let mut run = drive_with(Processor::new(PipelineConfig::default()), &upgraded, true);
    let mut diagnostics = sink.take();
    diagnostics.append(&mut run.diagnostics);
    run.diagnostics = diagnostics;
    run
}

fn run_finished(run: &str) -> Value {
    json!({"type": "RUN_FINISHED", "threadId": "t", "runId": run})
}

/// [`thinking_run`] closed with `RUN_FINISHED`: a complete 0.x run.
fn complete_thinking_run(run: &str) -> Vec<Value> {
    let mut frames = thinking_run(run);
    frames.push(run_finished(run));
    frames
}

/// The reasoning messages in a run's history: (id, content).
fn reasoning(run: &Run) -> Vec<(String, String)> {
    run.messages
        .iter()
        .filter(|m| m["role"] == "reasoning")
        .map(|m| {
            (
                m["id"].as_str().unwrap_or_default().to_owned(),
                m["content"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn types(frames: &[Value]) -> Vec<&str> {
    frames
        .iter()
        .map(|frame| frame["type"].as_str().unwrap_or("?"))
        .collect()
}

fn with_code(diagnostics: &[Diagnostic], code: DiagnosticCode) -> Vec<&Diagnostic> {
    diagnostics.iter().filter(|d| d.code == code).collect()
}

/// Resolves a dotted path (`a.b.0.c`): objects by key, arrays by index.
fn dotted<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(v, |node, key| match node {
        Value::Object(map) => map.get(key),
        Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => None,
    })
}

#[test]
fn dotted_paths_resolve_nested_members() {
    let v = json!({"a": {"b": [0, {"c": 1}]}, "protocolVersion": "1.0"});
    assert_eq!(dotted(&v, "a.b.1.c"), Some(&json!(1)));
    assert!(dotted(&v, "a.x").is_none());
    assert!(dotted(&v, "a.b.7").is_none());
    assert_eq!(dotted(&v, "protocolVersion"), Some(&json!("1.0")));
}

fn run_started(run: &str) -> Value {
    json!({"type": "RUN_STARTED", "threadId": "t", "runId": run})
}

fn thinking_run(run: &str) -> Vec<Value> {
    vec![
        run_started(run),
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_TEXT_MESSAGE_START"}),
        json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "Let me "}),
        json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "check."}),
        json!({"type": "THINKING_TEXT_MESSAGE_END"}),
        json!({"type": "THINKING_END"}),
    ]
}

// ---------------------------------------------------------------------------
// Retired shapes: THINKING_* -> REASONING_*
// ---------------------------------------------------------------------------

/// covers: VER-13, VER-14
#[test]
fn thinking_events_are_translated_to_reasoning_events() {
    let (mut reporter, _sink) = collecting();
    let out = through_boundary(thinking_run("r-1"), &mut reporter);
    assert_eq!(
        types(&out),
        [
            "RUN_STARTED",
            "REASONING_START",
            "REASONING_MESSAGE_START",
            "REASONING_MESSAGE_CONTENT",
            "REASONING_MESSAGE_CONTENT",
            "REASONING_MESSAGE_END",
            "REASONING_END",
        ]
    );
    let span = out[1]["messageId"].as_str().expect("span named");
    let message = out[2]["messageId"].as_str().expect("message named");
    assert_ne!(span, message);
    assert_eq!(out[2]["role"], "reasoning");
    assert_eq!(out[3]["messageId"], message);
    assert_eq!(out[3]["delta"], "Let me ");
    assert_eq!(out[4]["messageId"], message);
    assert_eq!(out[5]["messageId"], message);
    assert_eq!(out[6]["messageId"], span);

    // The shipped path: the same 0.x run as bytes with no middleware is
    // translated before enforcement, with the same names the boundary gives.
    let run = shipped(&complete_thinking_run("r-1"));
    run.assert_ok();
    assert_eq!(
        run.types(),
        [
            "RUN_STARTED",
            "REASONING_START",
            "REASONING_MESSAGE_START",
            "REASONING_MESSAGE_CONTENT",
            "REASONING_MESSAGE_CONTENT",
            "REASONING_MESSAGE_END",
            "REASONING_END",
            "RUN_FINISHED",
        ]
    );
    let wire = run.wire();
    assert_eq!(wire[1]["messageId"], span);
    assert_eq!(wire[2]["messageId"], message);
    assert_eq!(
        reasoning(&run),
        [(message.to_owned(), "Let me check.".to_owned())]
    );
}

/// covers: VER-13, VER-32
#[test]
fn translated_names_are_unique_across_runs_and_keep_producer_ids() {
    let (mut reporter, _sink) = collecting();
    let mut frames = thinking_run("r-1");
    frames.extend(thinking_run("r-2"));
    frames.push(json!({"type": "THINKING_TEXT_MESSAGE_START", "messageId": "mine"}));
    frames.push(json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "x"}));
    frames.push(json!({"type": "THINKING_TEXT_MESSAGE_END"}));
    let out = through_boundary(frames, &mut reporter);
    let mut names: Vec<&str> = [1, 2, 8, 9]
        .iter()
        .map(|at| out[*at]["messageId"].as_str().expect("named"))
        .collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), 4, "{names:?}");
    // A second boundary on the same thread (a new stream) and the same run id
    // names identically; a different run id names differently.
    let again = through_boundary(thinking_run("r-1"), &mut reporter);
    assert_eq!(again[1]["messageId"], out[1]["messageId"]);
    let other = through_boundary(thinking_run("r-9"), &mut reporter);
    assert_ne!(other[1]["messageId"], out[1]["messageId"]);
    assert_eq!(out[14]["messageId"], "mine");
    assert_eq!(out[15]["messageId"], "mine");
    assert_eq!(out[16]["messageId"], "mine");

    // The shipped path, as a thread runs it: each run on a fresh processor
    // holding the history so far, frames as bytes, no middleware. The second
    // run's names must not collide with the first run's (BASIC-15), so the
    // thread holds two reasoning messages.
    let first = shipped(&complete_thinking_run("r-1"));
    first.assert_ok();
    let history: Vec<Message> = first
        .messages
        .iter()
        .map(|m| serde_json::from_value(m.clone()).expect("history message"))
        .collect();
    let mut second_run = complete_thinking_run("r-2");
    second_run[4] = json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "Again."});
    let second = drive_with(
        Processor::with_history(PipelineConfig::default(), history, json!({})),
        &second_run,
        false,
    );
    second.assert_ok();
    let held = reasoning(&second);
    assert_eq!(held.len(), 2, "{held:?}");
    assert_ne!(held[0].0, held[1].0);
    assert_eq!(held[0].1, "Let me check.");
    assert_eq!(held[1].1, "Let me Again.");
    // The names do not depend on the path: a boundary over the same stream
    // gives the same ones.
    let alone = through_boundary(second_run, &mut reporter);
    assert_eq!(second.wire()[2]["messageId"], alone[2]["messageId"]);
}

/// covers: VER-13
#[test]
fn nested_thinking_spans_close_innermost_first() {
    let (mut reporter, _sink) = collecting();
    let frames = vec![
        run_started("r-1"),
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_START"}),
        json!({"type": "THINKING_END"}),
        json!({"type": "THINKING_END"}),
    ];
    let out = through_boundary(frames, &mut reporter);
    assert_ne!(out[1]["messageId"], out[2]["messageId"]);
    assert_eq!(out[3]["messageId"], out[2]["messageId"]);
    assert_eq!(out[4]["messageId"], out[1]["messageId"]);
}

/// covers: VER-15
#[test]
fn a_thinking_title_is_dropped_with_a_warning_every_time() {
    let (mut reporter, sink) = collecting();
    let frames = vec![
        run_started("r-1"),
        json!({"type": "THINKING_START", "title": "Working"}),
        json!({"type": "THINKING_END"}),
        json!({"type": "THINKING_START", "title": "Again"}),
        json!({"type": "THINKING_END"}),
        json!({"type": "THINKING_START", "title": null}),
    ];
    let out = through_boundary(frames, &mut reporter);
    assert!(out[1].get("title").is_none());
    assert!(out[5].get("title").is_none());
    let diagnostics = sink.take();
    let lost: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.pointer == "/title")
        .collect();
    assert_eq!(lost.len(), 2, "{diagnostics:#?}");
    for diagnostic in lost {
        assert_eq!(diagnostic.code, DiagnosticCode::RetiredShape);
        assert_eq!(diagnostic.scope, Scope::Event);
        assert!(diagnostic.message.contains("THINKING_START"));
        assert!(
            diagnostic.message.contains("REASONING_"),
            "names the upgrade"
        );
        assert!(!diagnostic.message.contains("Working"), "no payload values");
    }
}

/// covers: VER-13
#[test]
fn each_retired_event_is_noticed_once_per_run() {
    let (mut reporter, sink) = collecting();
    let mut frames = thinking_run("r-1");
    frames.extend(thinking_run("r-2"));
    through_boundary(frames, &mut reporter);
    let notices = with_code(&sink.take(), DiagnosticCode::RetiredShape)
        .into_iter()
        .map(|d| d.event_type)
        .collect::<Vec<_>>();
    // Five retired types, two runs; the repeated CONTENT is noticed once.
    assert_eq!(notices.len(), 10, "{notices:?}");
    assert_eq!(
        notices
            .iter()
            .filter(|t| **t == Some(EventType::ReasoningMessageContent))
            .count(),
        2
    );

    // The shipped path notices per run too: its RUN_STARTED frames never
    // need the boundary, yet the boundary must learn each run.
    let mut stream = complete_thinking_run("r-1");
    stream.extend(complete_thinking_run("r-2"));
    let run = shipped(&stream);
    run.assert_ok();
    assert_eq!(
        with_code(&run.diagnostics, DiagnosticCode::RetiredShape).len(),
        10,
        "{:#?}",
        run.diagnostics
    );
}

/// JSON may spell a retired name with escapes; the parsed name is what the
/// boundary translates, on the shipped path as on every other.
///
/// covers: VER-13, VER-14
#[test]
fn escaped_spellings_of_retired_shapes_are_translated_too() {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let mut processor = Processor::new(PipelineConfig::default());
    let mut out = Vec::new();
    let frames: [&[u8]; 6] = [
        br#"{"type":"RUN_STARTED","threadId":"t","runId":"r"}"#,
        br#"{"type":"THINKING\u005fSTART"}"#,
        br#"{"type":"THINKING_TEXT_MESSAGE_\u0053TART"}"#,
        br#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"u","role":"user","content":[
            {"type":"bin\u0061ry","mimeType":"image/png","url":"https://x/a.png"}]}]}"#,
        br#"{"type":"THINKING_TEXT_MESSAGE_END"}"#,
        br#"{"type":"\u0054HINKING_END"}"#,
    ];
    for frame in frames {
        processor
            .push_json(frame, &mut reporter, &mut out)
            .expect("accepted");
    }
    let types: Vec<&str> = out.iter().map(|p| p.event.event_type().as_str()).collect();
    assert_eq!(
        types,
        [
            "RUN_STARTED",
            "REASONING_START",
            "REASONING_MESSAGE_START",
            "MESSAGES_SNAPSHOT",
            "REASONING_MESSAGE_END",
            "REASONING_END",
        ]
    );
    let snapshot = serde_json::to_value(&*out[3].event).expect("serializes");
    assert_eq!(snapshot["messages"][0]["content"][0]["type"], "image");
    assert!(sink.take().iter().all(
        |d| d.code != DiagnosticCode::UnknownEvent && d.code != DiagnosticCode::UnknownVariant
    ));
}

/// The THINKING_* shim on a thread, run after run, over a JSON transport
/// (the no-middleware fast path) and over one that hands on parsed frames
/// (as protobuf does): the same names, never colliding across runs.
///
/// covers: VER-13, VER-32
#[cfg(feature = "client")]
#[test]
fn a_thread_keeps_each_runs_translated_reasoning_apart_on_every_transport() {
    use agui_sdk::client::{AgentClient, RunRequest};
    use support_client::{block_on, RunScripts};

    let one_thought = |run: &str, delta: &str| {
        vec![
            run_started(run),
            json!({"type": "THINKING_START"}),
            json!({"type": "THINKING_TEXT_MESSAGE_START"}),
            json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": delta}),
            json!({"type": "THINKING_TEXT_MESSAGE_END"}),
            json!({"type": "THINKING_END"}),
            run_finished(run),
        ]
    };
    let first = one_thought("r1", "first-run thought");
    let second = one_thought("r2", "second-run thought");
    let mut held = Vec::new();
    for transport in [
        RunScripts::json(vec![first.clone(), second.clone()]),
        RunScripts::parsed(vec![first.clone(), second.clone()]),
    ] {
        let client = AgentClient::with_transport(transport)
            .diagnostic_sink(NullSink)
            .build()
            .expect("client");
        let mut thread = client.thread_with_id("t");
        for (run, delta) in [("r1", "first-run thought"), ("r2", "second-run thought")] {
            let report = block_on(thread.run_and_absorb(RunRequest::new().run_id(run)))
                .expect("run succeeds");
            let added: Vec<_> = report
                .new_messages()
                .filter(|m| m.role() == agui_sdk::message::Role::Reasoning)
                .map(|m| m.text().map(str::to_owned))
                .collect();
            assert_eq!(added, [Some(delta.to_owned())], "{run}");
        }
        let ids: Vec<String> = thread
            .messages()
            .iter()
            .filter(|m| m.role() == agui_sdk::message::Role::Reasoning)
            .map(|m| m.id().as_str().to_owned())
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_ne!(ids[0], ids[1]);
        held.push(ids);
    }
    assert_eq!(held[0], held[1], "the verdict depends on the transport");
}

/// covers: VER-05, VER-34
#[test]
fn thinking_fragments_with_nothing_open_are_not_repaired() {
    let (mut reporter, _sink) = collecting();
    let frames = vec![
        run_started("r-1"),
        json!({"type": "THINKING_TEXT_MESSAGE_CONTENT", "delta": "orphan"}),
        json!({"type": "THINKING_TEXT_MESSAGE_END"}),
        json!({"type": "THINKING_END"}),
        json!({"type": "THINKING_TEXT_MESSAGE_START", "messageId": 7}),
        json!({"type": "THINKING_TEXT_MESSAGE_CONTENT"}),
    ];
    let out = through_boundary(frames, &mut reporter);
    assert_eq!(out[1]["type"], "REASONING_MESSAGE_CONTENT");
    // No messageId is invented for what nothing opened: enforcement and
    // verification still see the defect.
    for frame in &out[1..4] {
        assert!(frame.get("messageId").is_none(), "{frame}");
    }
    assert_eq!(out[4]["messageId"], 7, "a malformed id is left as sent");
    assert!(out[5].get("messageId").is_none());
    assert!(
        out[5].get("delta").is_none(),
        "a missing delta is not defaulted"
    );
}

/// covers: VER-16
#[test]
fn shapes_outside_the_registry_pass_through_unrecognised() {
    let (mut reporter, sink) = collecting();
    let unregistered = json!({"type": "THINKING_TEXT_MESSAGE_CHUNK", "delta": "x"});
    let out = through_boundary(vec![run_started("r"), unregistered.clone()], &mut reporter);
    assert_eq!(out[1], unregistered, "left for enforcement to drop");
    assert!(sink.take().is_empty());
}

// ---------------------------------------------------------------------------
// Retired shapes: legacy binary content
// ---------------------------------------------------------------------------

/// covers: VER-13
#[test]
fn legacy_binary_parts_become_media_parts() {
    let (mut reporter, sink) = collecting();
    let snapshot = json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "u1", "role": "user", "content": [
            {"type": "text", "text": "see"},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/a.png"},
            {"type": "binary", "mimeType": "audio/wav", "data": "AAAA", "filename": "a.wav"},
            {"type": "binary", "mimeType": "video/mp4", "id": "file-1"},
            {"type": "binary", "mimeType": "application/pdf", "url": "https://x/d.pdf"}
        ]}
    ]});
    let out = through_boundary(vec![run_started("r"), snapshot], &mut reporter);
    assert_eq!(
        out[1]["messages"][0]["content"],
        json!([
            {"type": "text", "text": "see"},
            {"type": "image", "source": {"type": "url", "value": "https://x/a.png", "mimeType": "image/png"}},
            {"type": "audio", "source": {"type": "data", "value": "AAAA", "mimeType": "audio/wav"},
             "metadata": {"filename": "a.wav"}},
            {"type": "video", "source": {"type": "file", "value": "file-1", "mimeType": "video/mp4"}},
            {"type": "document", "source": {"type": "url", "value": "https://x/d.pdf", "mimeType": "application/pdf"}}
        ])
    );
    let diagnostics = sink.take();
    assert_eq!(diagnostics.len(), 1, "one notice per run: {diagnostics:#?}");
    assert_eq!(diagnostics[0].code, DiagnosticCode::LegacyContent);
    assert_eq!(diagnostics[0].pointer, "/messages/0/content/1");
}

/// covers: VER-13, VER-15
#[test]
fn legacy_parts_in_results_and_echoed_input_are_upgraded_and_losses_warn() {
    let (mut reporter, sink) = collecting();
    let frames = vec![
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "input": {
        "threadId": "t", "runId": "r", "messages": [
            {"id": "u", "role": "user", "content": [
                {"type": "binary", "mimeType": "image/png", "data": "AAAA", "url": "https://x"}
            ]}
        ]}}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c", "content": [
            {"type": "binary", "mimeType": "image/gif", "url": "https://x/g.gif"}
        ]}),
    ];
    let out = through_boundary(frames, &mut reporter);
    assert_eq!(
        out[0]["input"]["messages"][0]["content"][0],
        json!({"type": "image", "source": {"type": "data", "value": "AAAA", "mimeType": "image/png"}})
    );
    assert_eq!(out[1]["content"][0]["type"], "image");
    let diagnostics = sink.take();
    let lossy: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.pointer == "/input/messages/0/content/0/url")
        .collect();
    assert_eq!(lossy.len(), 1, "{diagnostics:#?}");
    assert_eq!(lossy[0].code, DiagnosticCode::LegacyContent);
    assert!(!lossy[0].message.contains("https://x"), "no payload values");
}

/// covers: VER-05, VER-34
#[test]
fn malformed_legacy_parts_are_left_for_enforcement() {
    let (mut reporter, sink) = collecting();
    let parts = json!([
        {"type": "binary", "url": "https://x"},
        {"type": "binary", "mimeType": 3, "url": "https://x"},
        {"type": "binary", "mimeType": "image/png"},
        {"type": "binary", "mimeType": "image/png", "url": 4},
        {"type": "binary", "mimeType": "image/png", "url": "https://x", "filename": 5}
    ]);
    let frame = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
                       "content": parts.clone()});
    let out = through_boundary(vec![frame], &mut reporter);
    assert_eq!(out[0]["content"], parts);
    assert!(sink.take().is_empty());
}

/// covers: VER-13, VER-32
#[test]
fn a_legacy_mirror_of_a_modern_part_is_removed_and_nothing_else_is() {
    let mut input = json!({"threadId": "t", "runId": "r", "messages": [
        {"id": "u1", "role": "user", "content": [
            {"type": "image", "source": {"type": "url", "value": "https://x/a.png", "mimeType": "image/png"},
             "metadata": {"filename": "a.png"}},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/a.png", "filename": "a.png"},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/b.png"},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/b.png"},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/a.png", "filename": "other.png"}
        ]}
    ]});
    let (mut reporter, sink) = collecting();
    Boundary::upgrade_input(&mut input, &mut reporter);
    let content = input["messages"][0]["content"].as_array().expect("parts");
    assert_eq!(content.len(), 4, "{content:#?}");
    assert_eq!(content[0]["metadata"]["filename"], "a.png");
    assert_eq!(content[1]["source"]["value"], "https://x/b.png");
    assert_eq!(
        content[2]["source"]["value"], "https://x/b.png",
        "legacy-only repeats stay"
    );
    assert_eq!(
        content[3]["metadata"]["filename"], "other.png",
        "a filename is never lost"
    );
    assert!(
        sink.take().is_empty(),
        "the outgoing upgrade is lossless and silent"
    );
}

// ---------------------------------------------------------------------------
// Retired shapes: optional null read as absent
// ---------------------------------------------------------------------------

/// covers: VER-13
#[test]
fn registered_optional_nulls_are_read_as_absent_once_per_run() {
    let (mut reporter, sink) = collecting();
    let frames = vec![
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "parentRunId": null,
               "rawEvent": null, "input": {"threadId": "t", "runId": "r", "messages": [
                   {"id": "u", "role": "user", "content": [
                       {"type": "image", "source": {"type": "url", "value": "u"}, "metadata": null}
                   ]}
               ], "forwardedProps": null,
               "tools": [{"name": "n", "description": "d", "parameters": null}],
               "resume": [{"interruptId": "i", "status": "resolved", "payload": null}]}}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n",
               "parentMessageId": null, "subagentRunId": null}),
        json!({"type": "TOOL_CALL_CHUNK", "parentMessageId": null}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c2", "toolCallName": "n",
               "parentMessageId": null}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c", "content": "x",
               "role": null}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s", "result": null}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r", "result": null,
               "outcome": {"type": "success", "pendingToolCallIds": null},
               "usage": [{"provider": null, "reasoningTokens": null, "cachedInputTokens": null,
                          "cacheWriteInputTokens": null, "inputTokens": 1}]}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r", "outcome": null}),
        json!({"type": "RUN_ERROR", "message": "m", "usage": null}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "d", "rawEvent": null}),
    ];
    let out = through_boundary(frames, &mut reporter);
    let text = serde_json::to_string(&out).expect("serializes");
    assert!(!text.contains("null"), "{text}");
    assert_eq!(out[6]["usage"][0], json!({"inputTokens": 1}));
    assert_eq!(out[6]["outcome"], json!({"type": "success"}));

    let diagnostics = sink.take();
    let nulls = with_code(&diagnostics, DiagnosticCode::OptionalNull);
    // Every rule but `RUN_STARTED.input` (present here) is met, some of them
    // twice (TOOL_CALL_START's parent, rawEvent): noticed once each.
    assert_eq!(nulls.len(), 19, "{diagnostics:#?}");
    let mut fresh = Boundary::new();
    let mut start = json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "input": null});
    fresh.upgrade(&mut start, &mut reporter);
    assert!(start.get("input").is_none());
    assert_eq!(
        with_code(&sink.take(), DiagnosticCode::OptionalNull).len(),
        1
    );
    let parent = nulls
        .iter()
        .find(|d| d.event_type == Some(EventType::ToolCallStart) && d.pointer == "/parentMessageId")
        .expect("parent noticed");
    assert!(parent.message.contains("parentMessageId"));
    assert!(nulls
        .iter()
        .any(|d| d.pointer == "/input/messages/0/content/0/metadata"));
    assert!(nulls.iter().any(|d| d.pointer == "/input/resume/0/payload"));
    assert!(nulls
        .iter()
        .any(|d| d.pointer == "/usage/0/cacheWriteInputTokens"));

    // The shipped path reads them the same way, noticed once per field per
    // run (DEPRECATIONS.md), whether or not middleware is installed.
    let run_with_nulls = |run: &str| {
        vec![
            json!({"type": "RUN_STARTED", "threadId": "t", "runId": run, "parentRunId": null,
                   "rawEvent": null}),
            json!({"type": "TOOL_CALL_START", "toolCallId": format!("{run}-c1"),
                   "toolCallName": "n", "parentMessageId": null}),
            json!({"type": "TOOL_CALL_END", "toolCallId": format!("{run}-c1")}),
            json!({"type": "TOOL_CALL_START", "toolCallId": format!("{run}-c2"),
                   "toolCallName": "n", "parentMessageId": null}),
            json!({"type": "TOOL_CALL_END", "toolCallId": format!("{run}-c2")}),
            json!({"type": "TOOL_CALL_RESULT", "messageId": format!("{run}-m"),
                   "toolCallId": format!("{run}-c1"), "content": "x", "role": null}),
            json!({"type": "RUN_FINISHED", "threadId": "t", "runId": run, "result": null,
                   "usage": [{"provider": null, "inputTokens": 1}]}),
        ]
    };
    let mut stream = run_with_nulls("r1");
    stream.extend(run_with_nulls("r2"));
    let fast = shipped(&stream);
    fast.assert_ok();
    let slow = boundary_then_value(stream);
    slow.assert_ok();
    assert_eq!(fast.wire(), slow.wire());
    for run in [&fast, &slow] {
        let pointers: Vec<&str> = with_code(&run.diagnostics, DiagnosticCode::OptionalNull)
            .iter()
            .map(|d| d.pointer.as_str())
            .collect();
        // Six fields per run, two runs; the second TOOL_CALL_START's parent
        // is not noticed again.
        assert_eq!(pointers.len(), 12, "{pointers:?}");
        assert_eq!(
            pointers
                .iter()
                .filter(|p| **p == "/parentMessageId")
                .count(),
            2
        );
    }
}

/// covers: VER-05, VER-34
#[test]
fn nulls_outside_the_registry_are_left_alone() {
    let (mut reporter, sink) = collecting();
    let frames = vec![
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        // Required payloads where null is data.
        json!({"type": "STATE_SNAPSHOT", "snapshot": null}),
        json!({"type": "CUSTOM", "name": "n", "value": null}),
        // Null under an open-by-key object is data.
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant",
               "metadata": {"k": null}}),
        // Never tolerated: event metadata, and nulls in required positions.
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "metadata": null}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": null}),
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
               "content": [{"type": "text", "text": "t", "metadata": null}]}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": null, "delta": "d"}),
    ];
    let out = through_boundary(frames.clone(), &mut reporter);
    assert_eq!(out, frames);
    assert!(sink.take().is_empty());

    // What the boundary leaves is enforcement's to judge, on the shipped
    // path and the middleware path alike: a null that is data stays, any
    // other optional null is a malformed known value (BASIC-12, PROC-04).
    let started = run_started("r");
    let open = json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "assistant"});
    let data = vec![
        started.clone(),
        frames[1].clone(),
        frames[2].clone(),
        frames[3].clone(),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m"}),
        run_finished("r"),
    ];
    for run in [shipped(&data), boundary_then_value(data.clone())] {
        run.assert_ok();
        assert_eq!(run.state, Value::Null);
    }
    for (bad, prefix) in [
        (frames[4].clone(), vec![started.clone(), open.clone()]),
        (
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "metadata": null}),
            vec![started.clone()],
        ),
        (
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": null}),
            vec![started.clone()],
        ),
        (
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "u", "role": "user", "content": "hi", "metadata": null}]}),
            vec![started.clone()],
        ),
        (frames[6].clone(), vec![started.clone()]),
        (frames[7].clone(), vec![started.clone(), open.clone()]),
    ] {
        let mut stream = prefix;
        stream.push(bad.clone());
        for run in [shipped(&stream), boundary_then_value(stream.clone())] {
            let code = run.error.as_ref().map(agui_sdk::Error::code);
            assert_eq!(code, Some(ErrorCode::InvalidValue), "{bad}");
        }
    }
}

/// covers: VER-34
#[test]
fn a_conformant_frame_is_untouched_and_draws_nothing() {
    let (mut reporter, sink) = collecting();
    let frames = vec![
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "protocolVersion": "1.0",
        "input": {"threadId": "t", "runId": "r", "messages": [
            {"id": "u", "role": "user", "content": [
                {"type": "text", "text": "hi"},
                {"type": "image", "source": {"type": "url", "value": "https://x"}}
            ]}]}}),
        json!({"type": "REASONING_START", "messageId": "s"}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "n"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "d",
               "subagentRunId": "s1"}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}),
        json!("not an object"),
        json!({"type": 5}),
    ];
    let mut boundary = Boundary::new();
    for frame in &frames {
        let mut copy = frame.clone();
        assert!(boundary.upgrade(&mut copy, &mut reporter));
        assert_eq!(&copy, frame);
    }
    assert!(sink.take().is_empty());
}

// ---------------------------------------------------------------------------
// Era shims
// ---------------------------------------------------------------------------

fn subagent_stream() -> Vec<Value> {
    vec![
        run_started("r-1"),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s1", "name": "researcher"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m1", "role": "assistant",
               "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "Found it.",
               "subagentRunId": "s1"}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m1", "subagentRunId": "s1"}),
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s2", "message": "boom"}),
        json!({"type": "SUBAGENT_FINISHED", "subagentRunId": "s1"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [
            {"id": "m1", "role": "assistant", "content": "Found it.", "subagentRunId": "s1"}
        ]}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r-1", "outcome": {
        "type": "interrupt", "interrupts": [
            {"id": "i1", "reason": "approval", "subagentRunId": "s1"}
        ]}}),
    ]
}

/// covers: VER-03, VER-07, VER-08, VER-09
#[test]
fn a_pre_subagent_peer_loses_subagent_events_with_a_warning_each() {
    for era in [LegacyEra::V0_0_57, LegacyEra::V0_0_47, LegacyEra::V0_0_39] {
        let (mut reporter, sink) = collecting();
        let out = through_era(era, subagent_stream(), &mut reporter);
        assert_eq!(
            types(&out),
            [
                "RUN_STARTED",
                "TEXT_MESSAGE_START",
                "TEXT_MESSAGE_CONTENT",
                "TEXT_MESSAGE_END",
                "MESSAGES_SNAPSHOT",
                "RUN_FINISHED"
            ]
        );
        let diagnostics = sink.take();
        assert_eq!(diagnostics.len(), 3, "{diagnostics:#?}");
        for (diagnostic, (name, event_type)) in diagnostics.iter().zip([
            ("SUBAGENT_STARTED", EventType::SubagentStarted),
            ("SUBAGENT_ERROR", EventType::SubagentError),
            ("SUBAGENT_FINISHED", EventType::SubagentFinished),
        ]) {
            assert_eq!(diagnostic.code, DiagnosticCode::LossyDowngrade);
            assert_eq!(diagnostic.event_type, Some(event_type));
            assert!(diagnostic.message.contains(&format!("dropping {name}")));
            assert!(diagnostic.message.contains("upgrade the peer to 0.0.58"));
            assert!(diagnostic.message.len() <= 192);
        }
    }
}

/// covers: VER-06, VER-32
#[test]
fn stripping_attribution_is_silent_and_touches_nothing_else() {
    let (mut reporter, sink) = collecting();
    let out = through_era(LegacyEra::V0_0_57, subagent_stream(), &mut reporter);
    let text = serde_json::to_string(&out).expect("serializes");
    assert!(!text.contains("subagentRunId"), "{text}");
    // Ids, deltas and the outcome are exactly as sent.
    assert_eq!(
        out[2],
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m1", "delta": "Found it."})
    );
    assert_eq!(
        out[5]["outcome"],
        json!({"type": "interrupt", "interrupts": [{"id": "i1", "reason": "approval"}]})
    );
    let diagnostics = sink.take();
    assert!(
        diagnostics
            .iter()
            .all(|d| d.message.contains("dropping SUBAGENT_")),
        "only the dropped events warn: {diagnostics:#?}"
    );
}

/// covers: VER-05
#[test]
fn era_shims_never_repair_malformed_values() {
    let (mut reporter, sink) = collecting();
    let shims = EraShims::new(LegacyEra::V0_0_39);
    let mut frame = json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "d",
                           "subagentRunId": 5});
    let before = frame.clone();
    assert!(shims.upgrade_event(&mut frame, &mut reporter));
    assert_eq!(
        frame, before,
        "a malformed attribution is left for enforcement"
    );

    let mut input = json!({"threadId": "t", "runId": "r", "messages": [
        {"id": "u", "role": "user", "content": [{"type": "text", "text": 5}, {"type": "image"}]},
        {"id": "v", "role": "user", "content": [{"type": "text", "text": "ok"}, 7]},
        {"id": "w", "role": "user", "content": [{"text": "no type"}]}
    ]});
    let messages = input["messages"].clone();
    shims.downgrade_input(&mut input, &mut reporter);
    assert_eq!(
        input["messages"], messages,
        "malformed content is not flattened"
    );
    assert!(sink.take().is_empty());
}

/// covers: VER-03, VER-04, VER-07, VER-08, VER-09, VER-32
#[test]
fn a_strings_only_peer_gets_flattened_content_and_a_warning_per_dropped_part() {
    let (mut reporter, sink) = collecting();
    let mut input = json!({
        "threadId": "t", "runId": "r", "protocolVersion": "1.0",
        "messages": [
            {"id": "u1", "role": "user", "content": [
                {"type": "text", "text": "What is "},
                {"type": "image", "source": {"type": "url", "value": "https://x/cat.png"}},
                {"type": "text", "text": "this?"},
                {"type": "document", "source": {"type": "data", "value": "AA", "mimeType": "a/b"}}
            ]},
            {"id": "u2", "role": "user", "content": [
                {"type": "audio", "source": {"type": "url", "value": "https://x/a.wav"}}
            ]},
            {"id": "a1", "role": "assistant", "content": "plain", "subagentRunId": "s"},
            {"id": "t1", "role": "tool", "toolCallId": "c", "content": [
                {"type": "text", "text": "result", "metadata": {"k": 1}},
                {"type": "future-part", "x": 1}
            ]}
        ]
    });
    EraShims::new(LegacyEra::V0_0_39).downgrade_input(&mut input, &mut reporter);
    assert_eq!(
        input,
        json!({"threadId": "t", "runId": "r", "messages": [
            {"id": "u1", "role": "user", "content": "What is this?"},
            {"id": "u2", "role": "user", "content": ""},
            {"id": "a1", "role": "assistant", "content": "plain"},
            {"id": "t1", "role": "tool", "toolCallId": "c", "content": "result"}
        ]})
    );
    let diagnostics = sink.take();
    let pointers: Vec<&str> = diagnostics.iter().map(|d| d.pointer.as_str()).collect();
    assert_eq!(
        pointers,
        [
            "/messages/0/content/1",
            "/messages/0/content/3",
            "/messages/1/content/0",
            "/messages/3/content/0/metadata",
            "/messages/3/content/1"
        ]
    );
    for diagnostic in &diagnostics {
        assert_eq!(diagnostic.code, DiagnosticCode::LossyDowngrade);
        assert_eq!(diagnostic.scope, Scope::RunInput);
        assert!(diagnostic.message.contains("0.0.39"));
        assert!(diagnostic.message.contains("upgrade the peer to 0.0.40"));
        assert!(
            !diagnostic.message.contains("https://"),
            "no payload values"
        );
    }
    assert!(diagnostics[0].message.contains("DROPS non-text parts"));
    assert!(diagnostics[0].message.contains("an image part"));
    assert!(diagnostics[4].message.contains("an unrecognised part"));
}

/// covers: VER-03, VER-32
#[test]
fn later_eras_keep_content_parts_and_every_era_omits_the_version() {
    for era in [LegacyEra::V0_0_47, LegacyEra::V0_0_57] {
        let (mut reporter, sink) = collecting();
        let parts = json!([{"type": "text", "text": "a"},
                           {"type": "image", "source": {"type": "url", "value": "u"}}]);
        let mut input = json!({"threadId": "t", "runId": "r", "protocolVersion": "1.0",
                               "messages": [{"id": "u", "role": "user", "content": parts.clone(),
                                             "subagentRunId": "s"}]});
        EraShims::new(era).downgrade_input(&mut input, &mut reporter);
        assert!(input.get("protocolVersion").is_none());
        assert_eq!(
            input["messages"][0],
            json!({"id": "u", "role": "user", "content": parts})
        );
        assert!(sink.take().is_empty(), "both steps are lossless");
    }
}

/// covers: VER-03, VER-07, VER-32
#[test]
fn producer_side_downgrade_flattens_results_and_drops_subagents() {
    let shims = EraShims::new(LegacyEra::V0_0_39);
    let (mut reporter, sink) = collecting();
    let mut started = json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r",
        "protocolVersion": "1.0", "input": {"threadId": "t", "runId": "r", "messages": [
            {"id": "u", "role": "user", "content": [{"type": "text", "text": "q"}]}]}});
    let mut result = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
        "content": [{"type": "text", "text": "r"},
                    {"type": "image", "source": {"type": "url", "value": "u"}}]});
    let mut snapshot = json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "t1", "role": "tool", "toolCallId": "c", "content": [{"type": "text", "text": "s"}]}]});
    let mut subagent = json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s", "name": "n"});
    assert!(shims.downgrade_event(&mut started, &mut reporter));
    assert!(shims.downgrade_event(&mut result, &mut reporter));
    assert!(shims.downgrade_event(&mut snapshot, &mut reporter));
    assert!(!shims.downgrade_event(&mut subagent, &mut reporter));
    assert_eq!(
        started["protocolVersion"], "1.0",
        "a producer declares its own version"
    );
    assert_eq!(started["input"]["messages"][0]["content"], "q");
    assert_eq!(result["content"], "r");
    assert_eq!(snapshot["messages"][0]["content"], "s");
    let diagnostics = sink.take();
    assert_eq!(diagnostics.len(), 2, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].pointer, "/content/1");
    assert_eq!(diagnostics[0].event_type, Some(EventType::ToolCallResult));
    assert_eq!(diagnostics[1].event_type, Some(EventType::SubagentStarted));

    // The consumer direction leaves a strings-only peer's results alone.
    let mut incoming = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
        "content": [{"type": "text", "text": "r"}]});
    let before = incoming.clone();
    assert!(shims.upgrade_event(&mut incoming, &mut reporter));
    assert_eq!(incoming, before);
    assert_eq!(shims.era(), LegacyEra::V0_0_39);
}

/// covers: VER-03, VER-07, VER-08, VER-09, VER-32
#[test]
fn producer_side_downgrade_gives_a_pre_media_reader_legacy_binary_parts() {
    let shims = EraShims::new(LegacyEra::V0_0_47);
    let (mut reporter, sink) = collecting();
    let mut result = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
    "content": [
        {"type": "text", "text": "r"},
        {"type": "image", "id": "p1",
         "source": {"type": "url", "value": "https://x/a.png", "mimeType": "image/png"}},
        {"type": "audio", "source": {"type": "data", "value": "AAAA", "mimeType": "audio/wav"},
         "metadata": {"filename": "a.wav"}},
        {"type": "video", "source": {"type": "url", "value": "https://x/v"}},
        {"type": "document", "source": {"type": "file", "value": "file-1",
         "mimeType": "application/pdf", "provider": "openai"},
         "metadata": {"filename": "d.pdf", "pages": 3}},
        {"type": "image", "source": {"type": "url", "value": 5, "mimeType": "image/png"}},
        {"type": "audio", "source": {"type": "url", "value": "u", "mimeType": 7}}
    ]});
    assert!(shims.downgrade_event(&mut result, &mut reporter));
    assert_eq!(
        result["content"],
        json!([
            {"type": "text", "text": "r"},
            {"type": "binary", "mimeType": "image/png", "url": "https://x/a.png"},
            {"type": "binary", "mimeType": "audio/wav", "data": "AAAA", "filename": "a.wav"},
            {"type": "binary", "mimeType": "application/pdf", "id": "file-1", "filename": "d.pdf"},
            {"type": "image", "source": {"type": "url", "value": 5, "mimeType": "image/png"}},
            {"type": "audio", "source": {"type": "url", "value": "u", "mimeType": 7}}
        ])
    );
    let pointers: Vec<String> = sink
        .take()
        .into_iter()
        .map(|d| {
            assert_eq!(d.code, DiagnosticCode::LossyDowngrade);
            assert!(d.message.contains("0.0.48"), "{}", d.message);
            assert!(!d.message.contains("  "), "{}", d.message);
            assert!(!d.message.contains("https://"), "no payload values");
            d.pointer
        })
        .collect();
    assert_eq!(
        pointers,
        [
            "/content/3",
            "/content/4/metadata",
            "/content/4/source/provider"
        ],
        "no MIME type to carry: dropped, never invented"
    );

    // The same reader gets legacy parts inside snapshots and the input echo;
    // a later reader keeps media parts.
    let mut snapshot = json!({"type": "MESSAGES_SNAPSHOT", "messages": [
        {"id": "u", "role": "user", "content": [
            {"type": "image", "source": {"type": "data", "value": "AA", "mimeType": "image/png"}}]}]});
    let modern = snapshot.clone();
    let mut later = snapshot.clone();
    assert!(shims.downgrade_event(&mut snapshot, &mut reporter));
    assert_eq!(snapshot["messages"][0]["content"][0]["type"], "binary");
    assert!(EraShims::new(LegacyEra::V0_0_57).downgrade_event(&mut later, &mut reporter));
    assert_eq!(later, modern);
    assert!(sink.take().is_empty());
}

/// covers: VER-07
#[test]
fn an_empty_metadata_object_loses_nothing_and_warns_nothing() {
    let shims = EraShims::new(LegacyEra::V0_0_47);
    let (mut reporter, sink) = collecting();
    let mut result = json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
    "content": [
        {"type": "image", "source": {"type": "url", "value": "https://x/a.png", "mimeType": "image/png"},
         "metadata": {}}
    ]});
    assert!(shims.downgrade_event(&mut result, &mut reporter));
    assert_eq!(
        result["content"],
        json!([{"type": "binary", "mimeType": "image/png", "url": "https://x/a.png"}])
    );
    assert!(sink.take().is_empty());
}

/// covers: VER-13, VER-32
#[test]
fn legacy_mirror_matching_keeps_its_exact_semantics() {
    let src = |extra: bool| {
        let mut s = json!({"type": "url", "value": "u", "mimeType": "image/png"});
        if extra {
            s["provider"] = json!("p");
        }
        s
    };
    let legacy = || json!({"type": "binary", "mimeType": "image/png", "url": "u"});
    let run = |parts: Vec<Value>| -> Vec<Value> {
        let mut input = json!({"threadId": "t", "runId": "r", "messages": [
            {"id": "u1", "role": "user", "content": parts}]});
        let (mut reporter, _sink) = collecting();
        Boundary::upgrade_input(&mut input, &mut reporter);
        input["messages"][0]["content"]
            .as_array()
            .expect("parts")
            .clone()
    };
    // (a) a modern source with an extra member is not a mirror.
    assert_eq!(
        run(vec![
            json!({"type": "image", "source": src(true)}),
            legacy()
        ])
        .len(),
        2
    );
    // (b) a match on any modern part counts.
    let mut l = legacy();
    l["filename"] = json!("b");
    let out = run(vec![
        json!({"type": "image", "source": src(false), "metadata": {"filename": "a"}}),
        json!({"type": "image", "source": src(false), "metadata": {"filename": "b"}}),
        l,
    ]);
    assert_eq!(out.len(), 2);
    // (c) no legacy filename: removed next to a named modern part.
    let out = run(vec![
        json!({"type": "image", "source": src(false), "metadata": {"filename": "a"}}),
        legacy(),
    ]);
    assert_eq!(out.len(), 1);
    // (d) a scalar non-string filename compares by value.
    let mut l = legacy();
    l["metadata"] = json!({"filename": 7});
    let out = run(vec![
        json!({"type": "image", "source": src(false), "metadata": {"filename": 7}}),
        l,
    ]);
    assert_eq!(out.len(), 1);
    // (d2) an object filename compares by value, not by its text.
    let mut l = legacy();
    l["metadata"] = json!({"filename": {"a": 1, "b": 2}});
    let out = run(vec![
        json!({"type": "image", "source": src(false), "metadata": {"filename": {"b": 2, "a": 1}}}),
        l,
    ]);
    assert_eq!(out.len(), 1);
    // (d3) -0.0 and 0.0 are equal values, so they mirror.
    let mut l = legacy();
    l["metadata"] =
        json!({"filename": {"k": serde_json::from_str::<Value>("-0.0").expect("number")}});
    let out = run(vec![
        json!({"type": "image", "source": src(false), "metadata": {"filename": {"k": serde_json::from_str::<Value>("0.0").expect("number")}}}),
        l,
    ]);
    assert_eq!(out.len(), 1);
    // (e) a mirror before its modern part is still removed.
    let out = run(vec![
        legacy(),
        json!({"type": "image", "source": src(false)}),
    ]);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["type"], "image");
}

/// covers: VER-03
#[test]
fn only_a_legacy_pin_installs_shims() {
    assert!(EraShims::for_pin(PeerPin::Current).is_none());
    assert!(EraShims::for_pin(PeerPin::Version(ProtocolVersion::new(1, 1))).is_none());
    let shims = EraShims::for_pin(PeerPin::Legacy(LegacyEra::V0_0_47));
    assert_eq!(shims.map(|shims| shims.era()), Some(LegacyEra::V0_0_47));
}

/// covers: VER-10
#[test]
fn a_null_sink_silences_warnings_but_not_the_conversions() {
    let mut reporter = Reporter::new(Arc::new(NullSink), 256);
    let out = through_era(LegacyEra::V0_0_57, subagent_stream(), &mut reporter);
    assert_eq!(out.len(), 6);
    let mut input = json!({"threadId": "t", "runId": "r", "messages": [
        {"id": "u", "role": "user", "content": [{"type": "image", "source": {"type": "url", "value": "u"}}]}]});
    EraShims::new(LegacyEra::V0_0_39).downgrade_input(&mut input, &mut reporter);
    assert_eq!(input["messages"][0]["content"], "");
}

/// covers: VER-11
#[test]
fn lossy_warnings_are_never_deduplicated_or_suppressed() {
    // There is no switch on the shims: whatever sink the reporter has gets
    // every lossy step, the same loss repeated included.
    let (mut reporter, sink) = collecting();
    let shims = EraShims::new(LegacyEra::V0_0_57);
    for _ in 0..3 {
        let mut frame = json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s", "name": "n"});
        assert!(!shims.upgrade_event(&mut frame, &mut reporter));
    }
    assert_eq!(
        with_code(&sink.take(), DiagnosticCode::LossyDowngrade).len(),
        3
    );
}

// ---------------------------------------------------------------------------
// The upstream era fixtures, at the compatibility stage
// ---------------------------------------------------------------------------

/// The era fixtures from the pinned spec cache, sorted by name. `None` only
/// when the cache is absent and not required: `agui_test_support::spec::root`
/// fails the test under `AGUI_REQUIRE_SPEC=1` or on a stale cache, and any
/// read or parse failure below is a failure, never a skip.
fn era_fixtures() -> Option<Vec<Value>> {
    let dir = agui_test_support::spec::streams_dir()?;
    let entries =
        std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    let mut fixtures: Vec<Value> = Vec::new();
    for entry in entries {
        let path = entry
            .unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()))
            .path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_else(|| panic!("non-UTF-8 fixture name {}", path.display()))
            .to_owned();
        if name.starts_with("era-") && name.ends_with(".json") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let fixture = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()));
            fixtures.push(fixture);
        }
    }
    fixtures.sort_by(|a: &Value, b: &Value| a["name"].as_str().cmp(&b["name"].as_str()));
    Some(fixtures)
}

/// covers: VER-03, VER-07, VER-13, VER-14
#[test]
fn era_fixtures_hold_at_the_compatibility_stage() {
    let Some(fixtures) = era_fixtures() else {
        return;
    };
    assert!(
        !fixtures.is_empty(),
        "the pinned spec has no era-* fixtures"
    );
    for fixture in fixtures {
        let name = fixture["name"].as_str().unwrap_or("?");
        let era = fixture["client"]["maxProtocolVersion"]
            .as_str()
            .and_then(|pin| match pin {
                "0.0.39" => Some(LegacyEra::V0_0_39),
                "0.0.47" => Some(LegacyEra::V0_0_47),
                "0.0.57" => Some(LegacyEra::V0_0_57),
                _ => None,
            });
        let (mut reporter, sink) = collecting();

        // Outgoing: the always-on input upgrade, then the era's downgrade.
        let mut input = json!({"threadId": "t", "runId": "r", "protocolVersion": "1.0",
                               "messages": fixture["input"]["messages"].clone()});
        if input["messages"].is_null() {
            input["messages"] = json!([]);
        }
        Boundary::upgrade_input(&mut input, &mut reporter);
        if let Some(era) = era {
            EraShims::new(era).downgrade_input(&mut input, &mut reporter);
        }
        let expect = &fixture["expect"];
        if let Some(messages) = expect["request"].get("messages") {
            assert_eq!(&input["messages"], messages, "{name}: request messages");
        }
        if let Some(paths) = expect["requestAbsentPaths"].as_array() {
            for path in paths {
                let path = path.as_str().unwrap_or_default();
                assert!(dotted(&input, path).is_none(), "{name}: {path} sent");
            }
        }
        if era.is_none() {
            assert!(input.get("protocolVersion").is_some(), "{name}: declared");
        }

        // Incoming: the boundary, then the era's upgrade.
        let stream: Vec<Value> = fixture["stream"].as_array().cloned().unwrap_or_default();
        let out = match era {
            Some(era) => through_era(era, stream, &mut reporter),
            None => through_boundary(stream, &mut reporter),
        };
        let seen = types(&out);
        assert!(
            seen.iter().all(|t| !t.starts_with("THINKING_")),
            "{name}: retired shapes reach middleware"
        );
        for absent in expect["eventTypesAbsent"].as_array().into_iter().flatten() {
            assert!(
                !seen.contains(&absent.as_str().unwrap_or_default()),
                "{name}: {absent}"
            );
        }
        if let Some(expected) = expect["eventTypes"].as_array() {
            let expected: Vec<&str> = expected.iter().filter_map(Value::as_str).collect();
            assert_eq!(seen, expected, "{name}: event types");
        }

        // The shipped path: the same stream as bytes into the processor,
        // pinned as above, with no middleware. It delivers what the
        // compatibility stage handed on, and the history the fixture expects.
        let mut config = PipelineConfig::default();
        config.legacy_peer = era;
        let stream: Vec<Value> = fixture["stream"].as_array().cloned().unwrap_or_default();
        let run = drive_with(Processor::new(config), &stream, false);
        run.assert_ok();
        assert_eq!(run.types(), seen, "{name}: shipped path");
        for (index, wanted) in expect["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let got = &run.messages[index];
            for key in ["role", "content"] {
                assert_eq!(got[key], wanted[key], "{name}: message {index} {key}");
            }
        }

        let diagnostics = sink.take();
        for wanted in expect["warnings"].as_array().into_iter().flatten() {
            let wanted = wanted.as_str().unwrap_or_default();
            assert!(
                diagnostics.iter().any(|d| d.message.contains(wanted)),
                "{name}: no warning containing {wanted:?} in {diagnostics:#?}"
            );
        }
        if expect["noWarnings"] == json!(true) {
            assert!(diagnostics.is_empty(), "{name}: {diagnostics:#?}");
        }
    }
}
