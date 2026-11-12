//! Enforcement rules (`basic/processing.mdx`) through the public API:
//! unrecognised material is dropped or stripped with a warning, open objects
//! are preserved, malformed known values are fatal.

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, Diagnostic, DiagnosticCode, Reporter};
use agui_sdk::pipeline::enforce::{enforce_document, validate_strict, Enforcer};
use agui_sdk::pipeline::Limits;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};

/// What enforcing one frame produced, in comparable form.
struct Run {
    /// `Ok(None)`: dropped. `Err`: the fatal verdict.
    out: Result<Option<Value>, Fatal>,
    diagnostics: Vec<Diagnostic>,
}

struct Fatal {
    code: ErrorCode,
    message: String,
    pointer: Option<String>,
}

fn enforce_both(frame: &Value) -> Run {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let enforcer = Enforcer::new(Limits::default());
    let bytes = serde_json::to_vec(frame).unwrap_or_default();
    let from_bytes = enforcer.enforce_event_json(&bytes, &mut reporter);
    let first = sink.take();
    let from_value = enforcer.enforce_event_value(frame.clone(), &mut reporter);
    let second = sink.take();
    let convert = |r: agui_sdk::Result<Option<agui_sdk::Event>>| match r {
        Ok(e) => Ok(e.map(|e| serde_json::to_value(e).unwrap_or_default())),
        Err(e) => Err(Fatal {
            code: e.code(),
            message: e.message().to_owned(),
            pointer: e.pointer().map(str::to_owned),
        }),
    };
    let (a, b) = (convert(from_bytes), convert(from_value));
    // The same verdict however the event arrived (PROC-06).
    match (&a, &b) {
        (Ok(x), Ok(y)) => assert_eq!(x, y, "paths disagree on {frame}"),
        (Err(x), Err(y)) => assert_eq!(x.code, y.code, "paths disagree on {frame}"),
        _ => panic!("paths disagree on {frame}"),
    }
    let codes = |d: &[Diagnostic]| {
        let mut v: Vec<_> = d.iter().map(|d| (d.code, d.pointer.clone())).collect();
        v.sort();
        v
    };
    assert_eq!(
        codes(&first),
        codes(&second),
        "diagnostics disagree on {frame}"
    );
    Run {
        out: a,
        diagnostics: first,
    }
}

fn kept(frame: Value) -> (Value, Vec<Diagnostic>) {
    let run = enforce_both(&frame);
    match run.out {
        Ok(Some(v)) => (v, run.diagnostics),
        _ => panic!("{frame} was not kept"),
    }
}

fn fatal(frame: Value) -> Fatal {
    match enforce_both(&frame).out {
        Err(f) => f,
        Ok(_) => panic!("{frame} was not fatal"),
    }
}

fn warned(d: &[Diagnostic], code: DiagnosticCode, pointer: &str) -> bool {
    d.iter().any(|d| d.code == code && d.pointer == pointer)
}

/// covers: PROC-01, PROC-11, PROC-12, PROC-14
#[test]
fn unknown_event_is_dropped_with_a_warning_naming_the_type() {
    let run = enforce_both(&json!({"type": "FUTURE_EVENT", "somethingNew": true}));
    assert!(
        matches!(run.out, Ok(None)),
        "the event must not reach the application"
    );
    assert_eq!(run.diagnostics.len(), 1);
    assert_eq!(run.diagnostics[0].code, DiagnosticCode::UnknownEvent);
    assert!(run.diagnostics[0].message.contains("FUTURE_EVENT"));
}

/// covers: PROC-02, PROC-10, PROC-11, PROC-13, PROC-14
#[test]
fn unknown_property_is_stripped_and_its_path_named() {
    let (v, d) = kept(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m",
                             "delta": "content survives", "futureProp": 1}));
    assert!(v.get("futureProp").is_none());
    assert_eq!(v["delta"], json!("content survives"));
    assert!(warned(&d, DiagnosticCode::UnknownField, "/futureProp"));
}

/// covers: PROC-03, PROC-18
#[test]
fn unknown_union_member_in_optional_position_removes_the_field() {
    let (v, d) = kept(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
                             "outcome": {"type": "paused"}}),
    );
    assert!(v.get("outcome").is_none());
    assert_eq!(v["runId"], json!("r"));
    assert!(warned(&d, DiagnosticCode::UnknownVariant, "/outcome"));
}

/// covers: PROC-19
#[test]
fn unknown_member_in_required_position_removes_its_container() {
    let (v, d) = kept(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
        "content": [{"type": "image", "source": {"type": "hologram", "value": "x"}},
                    {"type": "text", "text": "kept"}]}),
    );
    assert_eq!(v["content"], json!([{"type": "text", "text": "kept"}]));
    assert!(warned(&d, DiagnosticCode::UnknownVariant, "/content/0"));
}

/// covers: PROC-20
#[test]
fn unknown_list_element_is_dropped_and_the_list_survives() {
    let (v, d) = kept(
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "m", "role": "user",
        "content": [{"type": "text", "text": "before"}, {"type": "hologram"},
                    {"type": "text", "text": "after"}]}]}),
    );
    assert_eq!(
        v["messages"][0]["content"].as_array().map(Vec::len),
        Some(2)
    );
    assert!(warned(
        &d,
        DiagnosticCode::UnknownVariant,
        "/messages/0/content/1"
    ));

    let (v, d) = kept(json!({"type": "STATE_DELTA", "delta": [
        {"op": "increment", "path": "/count", "value": 1},
        {"op": "replace", "path": "/count", "value": 5}]}));
    assert_eq!(
        v["delta"],
        json!([{"op": "replace", "path": "/count", "value": 5}])
    );
    assert!(warned(&d, DiagnosticCode::UnknownPatchOp, "/delta/0"));
}

/// covers: PROC-17
#[test]
fn unknown_closed_set_value_is_stripped_not_fatal() {
    let (v, d) = kept(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "narrator"}));
    assert!(v.get("role").is_none() || v["role"] == json!("assistant"));
    assert!(warned(&d, DiagnosticCode::UnknownEnumValue, "/role"));
}

/// covers: PROC-15, SCHF-02
#[test]
fn open_objects_keep_every_member() {
    let frame = json!({"type": "STATE_DELTA",
                       "delta": [{"op": "remove", "path": "/items/1", "value": "drop-me"}]});
    let (v, d) = kept(frame.clone());
    assert_eq!(v["delta"][0]["value"], json!("drop-me"));
    assert!(d.is_empty());
    let (v, d) = kept(json!({"type": "TEXT_MESSAGE_START", "messageId": "m",
                             "metadata": {"acme/trace": "t", "acme/parent": null}}));
    assert_eq!(
        v["metadata"],
        json!({"acme/trace": "t", "acme/parent": null})
    );
    assert!(d.is_empty());
}

/// covers: PROC-04, PROC-05
#[test]
fn malformed_known_value_is_fatal_and_never_coerced() {
    let f = fatal(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m-1", "delta": 42}));
    assert_eq!(f.code, ErrorCode::InvalidValue);
    assert_eq!(f.pointer.as_deref(), Some("/delta"));
    assert!(f.message.contains("delta"));

    let f = fatal(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": [{"type": "interrupt", "interrupts": [{"id": "i", "reason": "approval"}]}]}),
    );
    assert!(
        f.message.contains("expected object, received array"),
        "{}",
        f.message
    );

    let f = fatal(json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s"}));
    assert!(f.message.contains("name"), "{}", f.message);

    let f =
        fatal(json!({"type": "REASONING_MESSAGE_START", "messageId": "m", "role": "assistant"}));
    assert!(
        f.message.contains(r#"expected \"reasoning\""#),
        "{}",
        f.message
    );
}

/// A streamed text message's role is drawn from the message roles: one the
/// schema excludes there (`tool`, `reasoning`, `activity`) is a known value
/// in the wrong place, so malformed and fatal, on a chunk as on a start and
/// through the processor as through the enforcer; only a value outside every
/// role is unrecognised and stripped (decision D8).
///
/// covers: PROC-04, PROC-17, PROC-26
#[test]
fn a_known_role_a_text_message_cannot_take_is_malformed() {
    use agui_sdk::pipeline::{PipelineConfig, Processor};

    for role in ["tool", "reasoning", "activity"] {
        for frame in [
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": role}),
            json!({"type": "TEXT_MESSAGE_CHUNK", "messageId": "m", "role": role, "delta": "d"}),
        ] {
            let f = fatal(frame.clone());
            assert_eq!(f.code, ErrorCode::InvalidValue, "{frame}");
            assert_eq!(f.pointer.as_deref(), Some("/role"), "{frame}");
            assert!(f.message.contains(r#"\"assistant\""#), "{}", f.message);

            let sink = Arc::new(CollectingSink::new());
            let mut reporter = Reporter::new(sink.clone(), 256);
            let mut processor = Processor::new(PipelineConfig::default());
            let mut out = Vec::new();
            let started = br#"{"type":"RUN_STARTED","threadId":"t","runId":"r"}"#;
            assert!(processor
                .push_json(started, &mut reporter, &mut out)
                .is_ok());
            let bytes = serde_json::to_vec(&frame).unwrap_or_default();
            let verdict = processor.push_json(&bytes, &mut reporter, &mut out);
            assert_eq!(
                verdict.map_err(|e| e.code()),
                Err(ErrorCode::InvalidValue),
                "{frame}"
            );
            assert!(sink
                .take()
                .iter()
                .all(|d| d.code != DiagnosticCode::UnknownEnumValue));
        }
    }
    // Every role a text message may take is kept as sent.
    for role in ["assistant", "developer", "system", "user"] {
        let (v, d) = kept(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": role}));
        assert_eq!(v["role"], role);
        assert!(d.is_empty());
    }
}

/// covers: PROC-16
#[test]
fn the_parser_alone_keeps_what_enforcement_strips() {
    let frame = json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "futureProp": 1});
    // The parser alone: the raw parse keeps the unknown member, and the typed
    // event parse accepts the frame rather than rejecting it.
    let raw = serde_json::to_vec(&frame).unwrap_or_else(|e| panic!("{e}"));
    let parsed: Value = serde_json::from_slice(&raw).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(parsed["futureProp"], 1);
    assert!(serde_json::from_slice::<agui_sdk::Event>(&raw).is_ok());
    // Enforcement strips it.
    let (v, _) = kept(frame);
    assert!(v.get("futureProp").is_none());
}

/// covers: PROC-37
#[test]
fn a_conformant_stream_draws_no_diagnostics() {
    for frame in [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r", "protocolVersion": "1.0"}),
        json!({"type": "STEP_STARTED", "stepName": "answer"}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m-1", "role": "assistant"}),
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m-1", "delta": "Hello, "}),
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m-1"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": {"answered": true}}),
        json!({"type": "STEP_FINISHED", "stepName": "answer"}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"}),
    ] {
        let (v, d) = kept(frame.clone());
        assert_eq!(v, frame);
        assert!(d.is_empty(), "{frame}");
    }
}

/// covers: EVT-02
#[test]
fn every_event_family_is_well_formed_material() {
    for frame in [
        json!({"type": "RUN_ERROR", "message": "m"}),
        json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n"}),
        json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "m", "activityType": "a", "content": {}}),
        json!({"type": "REASONING_START", "messageId": "m"}),
        json!({"type": "RAW", "event": {}}),
        json!({"type": "CUSTOM", "name": "n", "value": 1}),
        json!({"type": "SUBAGENT_ERROR", "subagentRunId": "s", "message": "m"}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": []}),
    ] {
        let (_, d) = kept(frame.clone());
        assert!(d.is_empty(), "{frame}");
    }
}

/// covers: SCHF-01, SCHF-02
#[test]
fn strict_validation_is_for_authoring_not_the_receive_path() {
    let doc = br#"{"type":"TEXT_MESSAGE_END","messageId":"m","futureProp":1}"#;
    let strict = validate_strict("TextMessageEndEvent", doc);
    assert!(
        matches!(&strict, Err(e) if e.code() == ErrorCode::InvalidValue
        && e.pointer() == Some("/futureProp"))
    );
    assert!(validate_strict(
        "TextMessageEndEvent",
        br#"{"type":"TEXT_MESSAGE_END","messageId":"m"}"#
    )
    .is_ok());
    assert!(
        matches!(validate_strict("NoSuchAnchor", b"{}"), Err(e) if e.code() == ErrorCode::InvalidConfig)
    );
    // Open patch operations pass strict validation with extra members.
    assert!(validate_strict(
        "StateDeltaEvent",
        br#"{"type":"STATE_DELTA","delta":[{"op":"remove","path":"/a","value":1}]}"#
    )
    .is_ok());

    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink.clone(), 256);
    let out = enforce_document(
        "TextMessageEndEvent",
        json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "futureProp": 1}),
        &mut reporter,
    );
    assert!(matches!(out, Ok(v) if v == json!({"type": "TEXT_MESSAGE_END", "messageId": "m"})));
    assert!(warned(
        &sink.take(),
        DiagnosticCode::UnknownField,
        "/futureProp"
    ));
}

/// A repeated member is last-wins whether or not another member sends the
/// frame down the value walk: a peer cannot pick the verdict by appending a
/// `null`.
///
/// covers: PROC-06
#[test]
fn a_repeated_member_is_not_path_dependent() {
    use agui_sdk::pipeline::{PipelineConfig, Processor};

    let run_id = |frame: &str| {
        let sink = Arc::new(CollectingSink::new());
        let mut reporter = Reporter::new(sink, 256);
        let mut processor = Processor::new(PipelineConfig::default());
        let mut out = Vec::new();
        processor
            .push_json(frame.as_bytes(), &mut reporter, &mut out)
            .map_err(|e| e.code())?;
        match out.first() {
            Some(p) => Ok(serde_json::to_value(&*p.event).unwrap_or_default()["runId"].clone()),
            None => Err(ErrorCode::InvalidValue),
        }
    };
    for (plain, with_null) in [
        (
            r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t","runId":"b"}"#,
            r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t","runId":"b","x":null}"#,
        ),
        (
            r#"{"type":"RUN_STARTED","threadId":"t","runId":"a","runId":"b"}"#,
            r#"{"type":"RUN_STARTED","threadId":"t","runId":"a","runId":"b","x":null}"#,
        ),
        (
            r#"{"type":"RUN_STARTED","threadId":"t","runId":5,"runId":"b"}"#,
            r#"{"type":"RUN_STARTED","threadId":"t","runId":5,"runId":"b","x":null}"#,
        ),
    ] {
        assert_eq!(run_id(plain), Ok(json!("b")), "{plain}");
        assert_eq!(run_id(with_null), Ok(json!("b")), "{with_null}");
    }
}

/// The public `# Errors` contract: an oversized frame is `FrameTooLarge`.
///
/// covers: PROC-06
#[test]
fn enforce_event_json_refuses_an_oversized_frame_as_frame_too_large() {
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let mut limits = Limits::default();
    limits.max_frame_bytes = 8;
    let enforcer = Enforcer::new(limits);
    let code = enforcer
        .enforce_event_json(br#"{"type":"X"}"#, &mut reporter)
        .map(|_| ())
        .map_err(|e| e.code());
    assert_eq!(code, Err(ErrorCode::FrameTooLarge));
}

/// A truncated frame is a JSON syntax fault, named the same whichever path
/// reads it: a member `null` (which routes the frame through the
/// compatibility boundary) does not change the message.
#[test]
fn a_truncated_frame_reads_the_same_with_or_without_a_null() {
    use agui_sdk::diagnostics::NullSink;
    use agui_sdk::pipeline::{PipelineConfig, Processor};
    let message = |frame: &[u8]| {
        let mut processor = Processor::new(PipelineConfig::default());
        let mut reporter = Reporter::new(Arc::new(NullSink), 16);
        let Err(err) = processor.push_json(frame, &mut reporter, &mut Vec::new()) else {
            panic!("a truncated frame was accepted")
        };
        assert_eq!(err.code(), ErrorCode::MalformedEvent);
        err.to_string()
    };
    let with_null = message(br#"{"type":"RUN_STARTED","extra":null,"threadId":"t"#);
    let without = message(br#"{"type":"RUN_STARTED","extra":1234,"threadId":"t"#);
    assert_eq!(with_null, without);
}

/// A `null` where a fixed string belongs is named the same on both paths:
/// the expected literal, not just "string".
#[test]
fn a_null_in_a_pinned_member_names_the_literal_on_both_paths() {
    let frame = json!({"type": "REASONING_MESSAGE_START", "messageId": "m", "role": null});
    let sink = Arc::new(CollectingSink::new());
    let mut reporter = Reporter::new(sink, 256);
    let enforcer = Enforcer::new(Limits::default());
    let bytes = serde_json::to_vec(&frame).unwrap_or_default();
    let message = |r: agui_sdk::Result<Option<agui_sdk::Event>>| match r {
        Err(e) => e.message().to_owned(),
        Ok(_) => panic!("{frame} was not fatal"),
    };
    let from_bytes = message(enforcer.enforce_event_json(&bytes, &mut reporter));
    let from_value = message(enforcer.enforce_event_value(frame.clone(), &mut reporter));
    assert_eq!(from_bytes, from_value);
}
