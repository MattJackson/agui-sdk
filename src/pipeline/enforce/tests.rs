//! Unit tests of the enforcement engine.
//!
//! They drive the engine's entry points directly (`event_from_bytes`,
//! `event_from_value`, `document_from_*`), so they exercise every rule
//! without the public adapters' error and diagnostics types. The rule tests
//! that go through the public API live in `tests/processing_enforce.rs` and
//! `tests/processing_input.rs`.
//!
//! Corpus-backed tests read the pinned upstream data from `$AGUI_SPEC_DIR`
//! or `.spec-cache/<commit>` and skip, saying so, when it is absent.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::report::{Finding, Kind};
use super::*;

mod linearity;

// ---- helpers ---------------------------------------------------------------

type Outcome = std::result::Result<Option<Value>, Fault>;

fn lenient() -> Options {
    Options::lenient(true)
}

/// The byte path, building a `Value` so results can be compared.
fn via_bytes(frame: &Value, opts: &Options) -> (Outcome, Vec<Finding>) {
    let bytes = serde_json::to_vec(frame).unwrap_or_default();
    let mut findings = Findings::new(opts.max_findings);
    let out = event_from_bytes::<Value>(&bytes, opts, &mut findings).map(|o| o.0);
    (out, findings.items)
}

/// The `Value` path.
fn via_value(frame: &Value, opts: &Options) -> (Outcome, Vec<Finding>) {
    let mut findings = Findings::new(opts.max_findings);
    let out = event_from_value::<Value>(frame.clone(), opts, &mut findings).map(|o| o.0);
    (out, findings.items)
}

fn sorted(mut f: Vec<Finding>) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = f
        .drain(..)
        .map(|x| (x.code.as_str().to_owned(), x.pointer))
        .collect();
    v.sort();
    v
}

/// Enforces `frame` both ways, checks the two paths agree (PROC-06: the
/// verdict does not depend on how the event arrived), and returns the
/// byte path's result.
fn enforce(frame: Value) -> (Outcome, Vec<Finding>) {
    let opts = lenient();
    let (a, fa) = via_bytes(&frame, &opts);
    let (b, fb) = via_value(&frame, &opts);
    match (&a, &b) {
        (Ok(x), Ok(y)) => assert_eq!(x, y, "paths disagree on {frame}"),
        (Err(x), Err(y)) => assert_eq!(x.kind, y.kind, "paths disagree on {frame}"),
        _ => panic!("paths disagree on {frame}: {a:?} vs {b:?}"),
    }
    assert_eq!(
        sorted(fa.clone()),
        sorted(fb),
        "findings disagree on {frame}"
    );
    (a, fa)
}

fn kept(frame: Value) -> (Value, Vec<Finding>) {
    match enforce(frame.clone()) {
        (Ok(Some(v)), f) => (v, f),
        other => panic!("expected {frame} to be kept, got {other:?}"),
    }
}

fn fatal(frame: Value) -> Fault {
    match enforce(frame.clone()) {
        (Err(f), _) => f,
        other => panic!("expected {frame} to be fatal, got {other:?}"),
    }
}

fn has(findings: &[Finding], code: DiagnosticCode, pointer: &str) -> bool {
    findings
        .iter()
        .any(|f| f.code == code && f.pointer == pointer)
}

// ---- unrecognised material -------------------------------------------------

/// covers: PROC-01, PROC-11, PROC-12, PROC-14
#[test]
fn unknown_event_is_dropped_with_a_warning_naming_its_type() {
    let (out, findings) = enforce(json!({"type": "FUTURE_EVENT", "somethingNew": true}));
    assert_eq!(out, Ok(None));
    assert_eq!(findings.len(), 1);
    assert_eq!(findings[0].code, DiagnosticCode::UnknownEvent);
    assert_eq!(findings[0].pointer, "");
    assert!(
        findings[0].message.contains("FUTURE_EVENT"),
        "{}",
        findings[0].message
    );
}

/// covers: PROC-02, PROC-10, PROC-11, PROC-13, PROC-14
#[test]
fn unknown_property_is_stripped_with_its_path() {
    let (v, findings) = kept(json!({
        "type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "content survives", "futureProp": 1
    }));
    assert_eq!(
        v,
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "content survives"})
    );
    assert!(has(&findings, DiagnosticCode::UnknownField, "/futureProp"));
}

#[test]
fn unknown_properties_are_stripped_at_any_depth() {
    let (v, findings) = kept(json!({
        "type": "MESSAGES_SNAPSHOT",
        "messages": [{"id": "1", "role": "assistant", "x": 1,
                      "toolCalls": [{"id": "c", "type": "function", "y": [1, 2],
                                     "function": {"name": "f", "arguments": "{}", "z": {}}}]}]
    }));
    assert_eq!(
        v["messages"][0],
        json!({"id": "1", "role": "assistant",
               "toolCalls": [{"id": "c", "type": "function",
                              "function": {"name": "f", "arguments": "{}"}}]})
    );
    for p in [
        "/messages/0/x",
        "/messages/0/toolCalls/0/y",
        "/messages/0/toolCalls/0/function/z",
    ] {
        assert!(
            has(&findings, DiagnosticCode::UnknownField, p),
            "{p}: {findings:?}"
        );
    }
}

#[test]
fn unknown_property_pointer_escapes_tilde_and_slash() {
    let (_, findings) = kept(json!({"type": "RUN_ERROR", "message": "m", "a/b~c": 1}));
    assert!(
        has(&findings, DiagnosticCode::UnknownField, "/a~1b~0c"),
        "{findings:?}"
    );
}

/// covers: PROC-03, PROC-18
#[test]
fn unknown_outcome_in_optional_position_removes_only_that_field() {
    let (v, findings) = kept(json!({
        "type": "RUN_FINISHED", "threadId": "t", "runId": "r", "outcome": {"type": "paused"}
    }));
    assert_eq!(
        v,
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r"})
    );
    assert!(has(&findings, DiagnosticCode::UnknownVariant, "/outcome"));
}

/// covers: PROC-03, PROC-20
#[test]
fn unknown_part_in_a_list_is_dropped_and_the_list_survives() {
    let (v, findings) = kept(json!({
        "type": "MESSAGES_SNAPSHOT",
        "messages": [{"id": "m", "role": "user", "content": [
            {"type": "text", "text": "before"},
            {"type": "hologram", "source": {"type": "data", "value": "AAAA", "mimeType": "x/y"}},
            {"type": "text", "text": "after"}
        ]}]
    }));
    assert_eq!(
        v["messages"][0]["content"],
        json!([{"type": "text", "text": "before"}, {"type": "text", "text": "after"}])
    );
    assert!(has(
        &findings,
        DiagnosticCode::UnknownVariant,
        "/messages/0/content/1"
    ));
}

/// covers: PROC-19
#[test]
fn unknown_source_in_required_position_removes_the_part_whole() {
    // `source` is required on a media part, so the part goes, not the field.
    let (v, findings) = kept(json!({
        "type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
        "content": [
            {"type": "image", "source": {"type": "hologram", "value": "x"}},
            {"type": "text", "text": "kept"}
        ]
    }));
    assert_eq!(v["content"], json!([{"type": "text", "text": "kept"}]));
    assert!(
        has(&findings, DiagnosticCode::UnknownVariant, "/content/0"),
        "{findings:?}"
    );
    assert!(
        findings[0].message.contains("`source`"),
        "{}",
        findings[0].message
    );
}

#[test]
fn unknown_message_role_drops_the_message_from_the_list() {
    let (v, findings) = kept(json!({
        "type": "MESSAGES_SNAPSHOT",
        "messages": [{"id": "1", "role": "narrator", "content": "x"},
                     {"id": "2", "role": "user", "content": "y"}]
    }));
    assert_eq!(
        v["messages"],
        json!([{"id": "2", "role": "user", "content": "y"}])
    );
    assert!(has(
        &findings,
        DiagnosticCode::UnknownVariant,
        "/messages/0"
    ));
}

/// covers: PROC-20
#[test]
fn unknown_patch_op_is_dropped_from_the_patch() {
    let (v, findings) = kept(json!({
        "type": "STATE_DELTA",
        "delta": [{"op": "increment", "path": "/count", "value": 1},
                  {"op": "replace", "path": "/count", "value": 5}]
    }));
    assert_eq!(
        v["delta"],
        json!([{"op": "replace", "path": "/count", "value": 5}])
    );
    assert!(has(&findings, DiagnosticCode::UnknownPatchOp, "/delta/0"));
}

/// covers: PROC-15, SCHF-02
#[test]
fn patch_operations_are_open_objects() {
    let frame = json!({
        "type": "STATE_DELTA",
        "delta": [{"op": "remove", "path": "/items/1", "value": "drop-me", "x-ext": {"a": null}}]
    });
    let (v, findings) = kept(frame.clone());
    assert_eq!(v, frame);
    assert!(findings.is_empty(), "{findings:?}");
}

#[test]
fn open_objects_keep_every_member_including_null() {
    let frame = json!({
        "type": "TEXT_MESSAGE_START", "messageId": "m",
        "metadata": {"acme.vendor/traceId": "t", "acme.vendor/parentSpan": null, "n": {"deep": [1]}}
    });
    let (v, findings) = kept(frame.clone());
    assert_eq!(v, frame);
    assert!(findings.is_empty());

    let frame = json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "m", "activityType": "a",
                       "content": {"anything": [null, {"x": 1}]}});
    assert_eq!(kept(frame.clone()).0, frame);

    let frame = json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "interrupt", "interrupts": [{"id": "i", "reason": "r",
            "responseSchema": {"type": "object", "unknownKeyword": true}}]}});
    assert_eq!(kept(frame.clone()).0, frame);
}

/// covers: PROC-17
#[test]
fn unknown_closed_set_value_is_stripped_not_fatal() {
    // D3: the spec wins over the reference implementation's admitted gap.
    let (v, findings) =
        kept(json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "narrator"}));
    assert_eq!(v, json!({"type": "TEXT_MESSAGE_START", "messageId": "m"}));
    assert!(has(&findings, DiagnosticCode::UnknownEnumValue, "/role"));
}

#[test]
fn unknown_closed_set_value_in_required_position_drops_the_container() {
    // `subtype` is required, so the event itself goes.
    let (out, findings) = enforce(json!({
        "type": "REASONING_ENCRYPTED_VALUE", "subtype": "image", "entityId": "e", "encryptedValue": "x"
    }));
    assert_eq!(out, Ok(None));
    assert!(
        has(&findings, DiagnosticCode::UnknownEnumValue, ""),
        "{findings:?}"
    );

    // A resume entry with an unknown status leaves the resume list.
    let mut findings = Findings::new(8);
    let input = json!({"threadId": "t", "runId": "r", "messages": [],
        "resume": [{"interruptId": "i", "status": "deferred"}, {"interruptId": "j", "status": "resolved"}]});
    let out = document_from_value::<Value>(input, run_input_shape(), &lenient(), &mut findings);
    assert_eq!(
        out.ok().flatten().map(|v| v["resume"].clone()),
        Some(json!([{"interruptId": "j", "status": "resolved"}]))
    );
    assert!(has(
        &findings.items,
        DiagnosticCode::UnknownEnumValue,
        "/resume/0"
    ));
}

#[test]
fn an_interrupts_list_below_its_minimum_is_malformed() {
    let (v, findings) = kept(json!({
        "type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "interrupt", "interrupts": [{"id": "i", "reason": "r"}]}
    }));
    assert!(v.get("outcome").is_some());
    assert!(findings.is_empty());
    // An empty `interrupts` was never valid: that is malformed, not dropped.
    let f = fatal(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": {"type": "interrupt", "interrupts": []}}),
    );
    assert_eq!(f.pointer, "/outcome/interrupts");
}

/// covers: EVT-02
#[test]
fn every_event_family_is_accepted() {
    let frames = [
        json!({"type": "RUN_STARTED", "threadId": "t", "runId": "r"}),
        json!({"type": "STEP_STARTED", "stepName": "s"}),
        json!({"type": "TEXT_MESSAGE_CHUNK"}),
        json!({"type": "TOOL_CALL_CHUNK"}),
        json!({"type": "STATE_SNAPSHOT", "snapshot": null}),
        json!({"type": "ACTIVITY_DELTA", "messageId": "m", "activityType": "a", "patch": []}),
        json!({"type": "REASONING_MESSAGE_CHUNK"}),
        json!({"type": "RAW", "event": {"anything": 1}}),
        json!({"type": "CUSTOM", "name": "n", "value": null}),
        json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s", "name": "n"}),
    ];
    for frame in frames {
        let (v, findings) = kept(frame.clone());
        assert_eq!(v, frame);
        assert!(findings.is_empty(), "{frame}: {findings:?}");
    }
    assert_eq!(event_union().len(), 31);
}

// ---- malformed known values ------------------------------------------------

/// covers: PROC-04, PROC-05
#[test]
fn malformed_known_field_is_fatal_and_named() {
    let f = fatal(json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m-1", "delta": 42}));
    assert_eq!(f.kind, FaultKind::Invalid);
    assert_eq!(f.pointer, "/delta");
    assert!(f.message.contains("delta"), "{}", f.message);
    assert!(
        f.message.contains("expected string, received number"),
        "{}",
        f.message
    );
}

/// covers: PROC-04
#[test]
fn malformed_union_value_is_fatal_not_stripped() {
    let f = fatal(
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
        "outcome": [{"type": "interrupt", "interrupts": [{"id": "i", "reason": "approval"}]}]}),
    );
    assert!(
        f.message.contains("expected object, received array"),
        "{}",
        f.message
    );
    let f = fatal(json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r", "outcome": {}}));
    assert!(
        f.message.contains("missing discriminator `type`"),
        "{}",
        f.message
    );
}

/// covers: PROC-04
#[test]
fn missing_required_field_is_fatal_and_named() {
    let f = fatal(json!({"type": "SUBAGENT_STARTED", "subagentRunId": "s"}));
    assert!(f.message.contains("`name`"), "{}", f.message);
}

#[test]
fn wrong_literal_is_malformed_not_unrecognised() {
    let f =
        fatal(json!({"type": "REASONING_MESSAGE_START", "messageId": "m", "role": "assistant"}));
    assert!(
        f.message.contains(r#"expected \"reasoning\""#),
        "{}",
        f.message
    );
    let f = fatal(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
                         "content": "x", "role": "user"}),
    );
    assert!(f.message.contains(r#"expected \"tool\""#), "{}", f.message);
}

/// covers: PROC-17, PROC-19
#[test]
fn role_literal_outside_the_role_set_is_unrecognised_material() {
    // Required literal: an unknown role drops the event (D3) ...
    let (out, findings) =
        enforce(json!({"type": "REASONING_MESSAGE_START", "messageId": "m", "role": "muse"}));
    assert_eq!(out, Ok(None));
    assert!(
        has(&findings, DiagnosticCode::UnknownEnumValue, ""),
        "{findings:?}"
    );
    // ... while another known role is still malformed.
    let f = fatal(json!({"type": "REASONING_MESSAGE_START", "messageId": "m", "role": "user"}));
    assert!(
        f.message.contains(r#"expected \"reasoning\""#),
        "{}",
        f.message
    );
    // Optional literal: the field is stripped.
    let (v, findings) = kept(
        json!({"type": "TOOL_CALL_RESULT", "messageId": "m", "toolCallId": "c",
                                    "content": "x", "role": "oracle"}),
    );
    assert!(v.get("role").is_none());
    assert!(has(&findings, DiagnosticCode::UnknownEnumValue, "/role"));
}

#[test]
fn malformed_values_inside_known_structures_are_fatal() {
    let cases = [
        (
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "timestamp": 1.5}),
            "/timestamp",
        ),
        (
            json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "timestamp": 9007199254740992u64}),
            "/timestamp",
        ),
        (
            json!({"type": "RUN_ERROR", "message": "m", "usage": [{"inputTokens": -1}]}),
            "/usage/0/inputTokens",
        ),
        (
            json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "a", "value": 1}]}),
            "/delta/0/path",
        ),
        (
            json!({"type": "STATE_DELTA", "delta": [{"op": "add", "path": "/~2", "value": 1}]}),
            "/delta/0/path",
        ),
        (
            json!({"type": "STATE_DELTA", "delta": [{"op": "copy", "path": "/a"}]}),
            "/delta/0",
        ),
        (
            json!({"type": "ACTIVITY_SNAPSHOT", "messageId": "m", "activityType": "a", "content": []}),
            "/content",
        ),
        (
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": 7}),
            "/role",
        ),
        (json!({"type": "RAW"}), ""),
        (
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [42]}),
            "/messages/0",
        ),
        (
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "1", "role": "user", "content": {}}]}),
            "/messages/0/content",
        ),
    ];
    for (frame, pointer) in cases {
        let f = fatal(frame.clone());
        assert_eq!(f.kind, FaultKind::Invalid, "{frame}");
        assert_eq!(f.pointer, pointer, "{frame}: {}", f.message);
    }
}

#[test]
fn integral_floats_are_integers() {
    let (v, _) = kept(json!({"type": "TEXT_MESSAGE_END", "messageId": "m", "timestamp": 3.0}));
    assert_eq!(v["timestamp"], json!(3));
}

#[test]
fn frames_that_are_not_events_are_malformed() {
    for frame in [
        json!([1]),
        json!({"x": 1}),
        json!({"type": 3}),
        json!({"type": ""}),
        json!("RUN_STARTED"),
    ] {
        let f = fatal(frame.clone());
        assert_eq!(f.kind, FaultKind::Malformed, "{frame}");
    }
    let mut findings = Findings::new(4);
    for bad in [
        &b"{"[..],
        b"{\"type\":\"RAW\",\"event\":1}x",
        b"",
        b"{\"type\":\"RAW\",\"event\":01}",
    ] {
        let r = event_from_bytes::<Value>(bad, &lenient(), &mut findings);
        assert!(
            matches!(&r, Err(f) if f.kind == FaultKind::Malformed),
            "{bad:?}: {r:?}"
        );
    }
}

// ---- the null shim -----------------------------------------------------------

/// covers: BASIC-12
#[test]
fn only_the_registry_nulls_are_absent_on_receive() {
    let frame = json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n",
                       "parentMessageId": null, "rawEvent": null});
    let (v, findings) = kept(frame.clone());
    assert_eq!(
        v,
        json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n"})
    );
    assert!(has(
        &findings,
        DiagnosticCode::OptionalNull,
        "/parentMessageId"
    ));
    assert!(has(&findings, DiagnosticCode::OptionalNull, "/rawEvent"));

    // Optional, but not on the list: event metadata ("MUST NOT be null"),
    // a text message's role, attribution on a message. Each is the type
    // mismatch the schema says it is, on both paths.
    for (bad, pointer) in [
        (
            json!({"type": "TOOL_CALL_START", "toolCallId": "c", "toolCallName": "n",
                   "metadata": null}),
            "/metadata",
        ),
        (
            json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": null}),
            "/role",
        ),
        (
            json!({"type": "MESSAGES_SNAPSHOT", "messages": [
                {"id": "u", "role": "user", "content": "hi", "subagentRunId": null}]}),
            "/messages/0/subagentRunId",
        ),
        (
            json!({"type": "TOOL_CALL_RESULT", "messageId": "r", "toolCallId": "c",
                   "content": [{"type": "text", "text": "t", "metadata": null}]}),
            "/content/0/metadata",
        ),
    ] {
        for (out, _) in [via_bytes(&bad, &lenient()), via_value(&bad, &lenient())] {
            assert!(
                matches!(&out, Err(fault) if fault.pointer == pointer),
                "{bad}: {out:?}"
            );
        }
    }
    // An event-level attribution null is on the list; a media part's
    // metadata too.
    let (_, findings) = kept(json!({"type": "TEXT_MESSAGE_END", "messageId": "m",
                                    "subagentRunId": null}));
    assert!(has(
        &findings,
        DiagnosticCode::OptionalNull,
        "/subagentRunId"
    ));

    // Required payloads may be null.
    let snap = json!({"type": "STATE_SNAPSHOT", "snapshot": null});
    assert_eq!(kept(snap.clone()).0, snap);
    // A required non-null field may not.
    assert_eq!(
        fatal(json!({"type": "TEXT_MESSAGE_END", "messageId": null})).pointer,
        "/messageId"
    );

    // Without the shim (strict, and the value corpus) the null is malformed.
    let mut f = Findings::new(4);
    let opts = Options::lenient(false);
    let r = event_from_value::<Value>(frame, &opts, &mut f);
    assert!(
        matches!(r, Err(fault) if fault.pointer == "/parentMessageId" || fault.pointer == "/rawEvent")
    );
}

// ---- budgets and the diagnostics cap -------------------------------------------

#[test]
fn container_entry_budget_is_enforced_incrementally() {
    let opts = Options {
        max_entries: 10,
        ..lenient()
    };
    let small = json!({"type": "RAW", "event": [1, 2, 3, 4, 5, 6, 7]});
    let big = json!({"type": "RAW", "event": [1, 2, 3, 4, 5, 6, 7, 8, 9]});
    assert!(via_bytes(&small, &opts).0.is_ok());
    for (out, _) in [via_bytes(&big, &opts), via_value(&big, &opts)] {
        assert!(matches!(out, Err(f) if f.kind == FaultKind::Limit));
    }
    // Stripped material counts too: a budget cannot be dodged by hiding
    // the bulk in an unknown member.
    let hidden = json!({"type": "RAW", "event": 1, "x": [1, 2, 3, 4, 5, 6, 7, 8, 9]});
    for (out, _) in [via_bytes(&hidden, &opts), via_value(&hidden, &opts)] {
        assert!(matches!(out, Err(f) if f.kind == FaultKind::Limit));
    }
}

#[test]
fn default_entry_budget_is_the_limits_default() {
    assert_eq!(Options::lenient(true).max_entries, 150_000);
    let opts = Options {
        max_entries: 150_000,
        ..lenient()
    };
    let mut doc = String::from(r#"{"type":"RAW","event":["#);
    doc.push_str(&"0,".repeat(150_000));
    doc.push_str("0]}");
    let mut f = Findings::new(4);
    let r = event_from_bytes::<IgnoredAny>(doc.as_bytes(), &opts, &mut f);
    assert!(matches!(r, Err(fault) if fault.kind == FaultKind::Limit));
}

#[test]
fn depth_budget_is_enforced() {
    let opts = Options {
        max_depth: 5,
        ..lenient()
    };
    let ok = json!({"type": "RAW", "event": [[[1]]]});
    let deep = json!({"type": "RAW", "event": [[[[[1]]]]]});
    assert!(via_bytes(&ok, &opts).0.is_ok());
    assert!(matches!(via_bytes(&deep, &opts).0, Err(f) if f.kind == FaultKind::Limit));
    assert!(matches!(via_value(&deep, &opts).0, Err(f) if f.kind == FaultKind::Limit));
}

/// A fatal error built from peer-chosen member names renders without raw
/// control characters and with bounded text, while the exact pointer stays
/// available programmatically.
#[test]
fn fatal_error_text_escapes_and_bounds_peer_member_names() {
    let key = format!("x\nWARN fake\u{1b}[2J{}", "y".repeat(5000));
    let check = |fault: Fault, code: ErrorCode, exact: &str| {
        assert_eq!(fault.pointer, exact);
        let error = fault_error(fault, code);
        let shown = error.to_string();
        assert!(
            !shown.contains('\n') && !shown.contains('\u{1b}'),
            "{shown}"
        );
        assert!(shown.len() < 1000, "display is {} bytes", shown.len());
        assert!(
            error.message().len() < 400,
            "message is {}",
            error.message().len()
        );
        assert_eq!(error.pointer(), Some(exact));
    };
    // Invalid: a strict unknown property under a hostile name.
    let mut doc = serde_json::Map::new();
    doc.insert("type".into(), json!("TEXT_MESSAGE_CONTENT"));
    doc.insert("messageId".into(), json!("m"));
    doc.insert("delta".into(), json!("d"));
    doc.insert(key.clone(), json!(1));
    let bytes = serde_json::to_vec(&Value::Object(doc)).unwrap_or_default();
    let mut f = Findings::new(0);
    let strict = document_from_bytes::<IgnoredAny>(
        &bytes,
        Shape::Union(event_union()),
        &Options::strict(),
        &mut f,
    );
    let Err(fault) = strict else {
        panic!("strict accepted an unknown property")
    };
    let exact = format!("/{key}");
    check(fault, ErrorCode::InvalidValue, &exact);
    // Limit: nesting past max_depth under a hostile name.
    let opts = Options {
        max_depth: 5,
        ..lenient()
    };
    // Raw bytes with a fixed member order, so the result does not depend on
    // serde_json's map ordering (`preserve-order`).
    let quoted = serde_json::to_string(&key).unwrap_or_default();
    let limit = |doc: String| {
        let mut f = Findings::new(0);
        let Err(fault) = event_from_bytes::<IgnoredAny>(doc.as_bytes(), &opts, &mut f) else {
            panic!("depth not enforced")
        };
        assert_eq!(fault.kind, FaultKind::Limit);
        fault
    };
    // `type` first: the walk reaches the limit inside the RAW `event` value.
    // The path is not extended into skipped opaque values, so the pointer
    // stops at `/event` and the peer-chosen name is not in it.
    let fault = limit(format!(
        r#"{{"type":"RAW","event":{{{quoted}:[[[[[1]]]]]}}}}"#
    ));
    check(fault, ErrorCode::InvalidValue, "/event");
    // `event` first: the discriminator lookahead skips the value and reports
    // the limit at the root.
    let fault = limit(format!(
        r#"{{"event":{{{quoted}:[[[[[1]]]]]}},"type":"RAW"}}"#
    ));
    check(fault, ErrorCode::InvalidValue, "");
}

#[test]
fn findings_are_capped_and_counted() {
    let opts = Options {
        max_findings: 3,
        ..lenient()
    };
    let mut frame = json!({"type": "RUN_ERROR", "message": "m"});
    for i in 0..10 {
        frame[format!("x{i}")] = json!(i);
    }
    let bytes = serde_json::to_vec(&frame).unwrap_or_default();
    let mut findings = Findings::new(opts.max_findings);
    let r = event_from_bytes::<Value>(&bytes, &opts, &mut findings);
    assert!(r.is_ok());
    assert_eq!(findings.items.len(), 3);
    assert_eq!(findings.suppressed, 7);
}

// ---- strict schema ---------------------------------------------------------------

/// covers: SCHF-01, PROC-16
#[test]
fn strict_validation_rejects_what_enforcement_strips() {
    let doc = br#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m","delta":"d","futureProp":1}"#;
    let mut f = Findings::new(0);
    let strict = document_from_bytes::<IgnoredAny>(
        doc,
        Shape::Union(event_union()),
        &Options::strict(),
        &mut f,
    );
    assert!(matches!(strict, Err(fault) if fault.pointer == "/futureProp"));
    // The parser alone keeps it (the obligation binds the pipeline, PROC-16);
    // enforcement strips it.
    let parsed: Value = serde_json::from_slice(doc).unwrap_or_default();
    assert_eq!(parsed["futureProp"], json!(1));
    let mut f = Findings::new(4);
    let enforced = event_from_bytes::<Value>(doc, &lenient(), &mut f);
    assert!(matches!(enforced, Ok((Some(v), _)) if v.get("futureProp").is_none()));
}

#[test]
fn generated_table_fits_the_walker() {
    use crate::pipeline::shapes::{self, DefKind, Ty};
    fn sorted_by<T>(items: &[T], key: impl Fn(&T) -> &str) -> bool {
        items.windows(2).all(|w| key(&w[0]) < key(&w[1]))
    }
    // The views rely on these readings of the table (see `shape.rs`).
    fn check_ty(at: &str, ty: &Ty) {
        match ty {
            Ty::String { pattern: Some(p) } => {
                let DefKind::Alias(Ty::String {
                    pattern: Some(pointer),
                }) = shapes::JSON_POINTER.def().kind
                else {
                    panic!("JsonPointer is not a patterned string");
                };
                assert_eq!(*p, pointer, "{at}: a pattern other than RFC 6901's");
            }
            Ty::OneOf(arms) => {
                assert!(
                    matches!(
                        arms,
                        [Ty::String { pattern: None }, Ty::Array { .. }]
                            | [Ty::Array { .. }, Ty::String { pattern: None }]
                    ),
                    "{at}: a oneOf other than string-or-array"
                );
                arms.iter().for_each(|t| check_ty(at, t));
            }
            Ty::Array { items, .. } => check_ty(at, items),
            Ty::Pinned { domain, .. } => {
                assert!(matches!(domain.def().kind, DefKind::Enum(_)), "{at}")
            }
            Ty::Enum(set) => assert!(sorted_by(set, |s| s), "{at}"),
            _ => {}
        }
    }
    assert!(sorted_by(&shapes::DEFS, |d| d.name));
    for def in &shapes::DEFS {
        match def.kind {
            DefKind::Object(o) => {
                assert!(sorted_by(o.fields, |f| f.name), "{}", def.name);
                assert!(o.fields.len() <= 64, "{}", def.name);
                for f in o.fields {
                    check_ty(def.name, &f.ty);
                }
            }
            DefKind::Union(u) => {
                assert!(sorted_by(u.arms, |a| a.value), "{}", def.name);
                for a in u.arms {
                    assert!(
                        matches!(a.def.def().kind, DefKind::Object(_)),
                        "{}",
                        def.name
                    );
                }
            }
            DefKind::Enum(set) => assert!(sorted_by(set, |s| s), "{}", def.name),
            DefKind::Alias(ref ty) => check_ty(def.name, ty),
        }
    }
    assert!(shape::anchor("RunFinishedEvent").is_some());
    assert!(shape::anchor("NoSuchThing").is_none());
    // The D3 role rule is expressed by the generator, not by hand.
    let Some((_, start)) = event_union().variant("REASONING_MESSAGE_START") else {
        panic!("no REASONING_MESSAGE_START");
    };
    let role = start.find("role").map(|(_, f)| f.shape);
    assert!(
        matches!(role, Some(Shape::Pinned { value: "reasoning", set }) if set.contains(&"user"))
    );
}

// ---- typed events --------------------------------------------------------------------

#[test]
fn typed_events_are_built_from_enforced_bytes() {
    // Patch operations decode through the hand-written impl; covered there.
    let frames = [
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "a\nb \u{1F600}", "x": 1}),
        json!({"type": "MESSAGES_SNAPSHOT", "messages": [{"id": "1", "role": "user",
               "content": [{"type": "text", "text": "x"}, {"type": "hologram"}]}]}),
        json!({"type": "RUN_FINISHED", "threadId": "t", "runId": "r",
               "outcome": {"type": "interrupt", "interrupts": [{"id": "i", "reason": "x"}]}}),
        json!({"type": "TEXT_MESSAGE_START", "messageId": "m", "role": "narrator"}),
    ];
    for frame in frames {
        let bytes = serde_json::to_vec(&frame).unwrap_or_default();
        let mut f1 = Findings::new(8);
        let typed = event_from_bytes::<Event>(&bytes, &lenient(), &mut f1).map(|o| o.0);
        let mut f2 = Findings::new(8);
        let plain = event_from_bytes::<Value>(&bytes, &lenient(), &mut f2).map(|o| o.0);
        let typed = typed.map(|e| e.map(|e| serde_json::to_value(e).unwrap_or_default()));
        assert_eq!(typed, plain, "{frame}");
        assert_eq!(f1.items, f2.items);
    }
}

// ---- lexer parity with serde_json ----------------------------------------------------

#[test]
fn reader_accepts_and_rejects_json_like_serde_json() {
    let cases: &[&str] = &[
        r#""plain""#,
        r#""esc \" \\ \/ \b \f \n \r \t""#,
        r#""é😀""#,
        r#""\ud83d""#,
        r#""\udc00x""#,
        r#""\x""#,
        "\"tab\there\"",
        r#""é""#,
        "0",
        "-0",
        "01",
        "1.",
        "1.5e3",
        "-12e-2",
        "18446744073709551615",
        "18446744073709551616",
        "-9223372036854775808",
        "-9223372036854775809",
        "1e400",
        "true",
        "tru",
        "null",
        "[1,]",
        "[1 2]",
        "{\"a\":1,}",
        "{\"a\" 1}",
        " [ 1 , { \"b\" : [ ] } ] ",
        "[]x",
        "{}",
    ];
    for case in cases {
        let ours = parse_value(case.as_bytes(), &lenient());
        let theirs = serde_json::from_str::<Value>(case);
        match (&ours, &theirs) {
            (Ok(a), Ok(b)) => assert_eq!(a, b, "{case}"),
            (Err(_), Err(_)) => {}
            _ => panic!("{case}: ours {ours:?} vs serde_json {theirs:?}"),
        }
    }
    let invalid_utf8 = b"\"\xff\"";
    assert!(parse_value(invalid_utf8, &lenient()).is_err());
}

#[test]
fn kind_names_match_messages() {
    assert_eq!(Kind::of(&json!([])).to_string(), "array");
    assert_eq!(Kind::of(&json!(null)).to_string(), "null");
}

// ---- run input -------------------------------------------------------------------------

/// covers: PROC-32, PROC-33
#[test]
fn outgoing_input_is_stripped_with_warnings() {
    let input = json!({
        "threadId": "t", "runId": "r", "futureField": true,
        "messages": [{"id": "1", "role": "user", "content": "hi", "x": 1},
                     {"id": "2", "role": "narrator", "content": "?"}],
        "tools": [{"name": "n", "description": "d", "parameters": {"type": "object", "extra": 1}}],
        "forwardedProps": {"keep": null}
    });
    let mut findings = Findings::new(16);
    let out = document_from_value::<Value>(input, run_input_shape(), &lenient(), &mut findings);
    let v = out.ok().flatten().unwrap_or_default();
    assert_eq!(
        v,
        json!({"threadId": "t", "runId": "r",
               "messages": [{"id": "1", "role": "user", "content": "hi"}],
               "tools": [{"name": "n", "description": "d", "parameters": {"type": "object", "extra": 1}}],
               "forwardedProps": {"keep": null}})
    );
    for p in ["/futureField", "/messages/0/x", "/messages/1"] {
        assert!(
            findings.items.iter().any(|f| f.pointer == p),
            "{p}: {:?}",
            findings.items
        );
    }
}

/// covers: PROC-34
#[test]
fn malformed_known_input_field_is_fatal() {
    let mut findings = Findings::new(16);
    for input in [
        json!({"threadId": "t", "runId": "r"}),
        json!({"threadId": 1, "runId": "r", "messages": []}),
        json!({"threadId": "t", "runId": "r", "messages": [], "context": [{"value": "v"}]}),
        json!({"threadId": "t", "runId": "r", "messages": [], "state": null}),
    ] {
        let out = document_from_value::<Value>(
            input.clone(),
            run_input_shape(),
            &Options::lenient(false),
            &mut findings,
        );
        assert!(out.is_err(), "{input}");
    }
}

#[test]
fn incoming_input_bytes_take_the_same_path() {
    let body = br#"{"threadId":"t","runId":"r","messages":[],"x":1,"state":null}"#;
    let mut findings = Findings::new(16);
    let out =
        document_from_bytes::<RunAgentInput>(body, run_input_shape(), &lenient(), &mut findings);
    let v = out
        .ok()
        .flatten()
        .map(|i| serde_json::to_value(i).unwrap_or_default());
    assert_eq!(v.as_ref().and_then(|v| v.get("x")), None);
    assert_eq!(
        v.as_ref().and_then(|v| v.get("threadId")),
        Some(&json!("t"))
    );
    assert!(has(&findings.items, DiagnosticCode::UnknownField, "/x"));
    assert!(has(&findings.items, DiagnosticCode::OptionalNull, "/state"));
    // `parentRunId: null` is listed on RUN_STARTED only, not on the input.
    let body = br#"{"threadId":"t","runId":"r","messages":[],"parentRunId":null}"#;
    let mut findings = Findings::new(16);
    let out =
        document_from_bytes::<RunAgentInput>(body, run_input_shape(), &lenient(), &mut findings);
    assert!(matches!(out, Err(fault) if fault.pointer == "/parentRunId"));
}

/// A repeated member in run input received by a server is last-wins even
/// when the first occurrence is a bad value, and strict validation rejects it.
///
/// covers: PROC-06
#[test]
fn a_repeated_run_input_member_is_last_wins() {
    let body = br#"{"threadId":5,"threadId":"t","runId":"r","messages":[]}"#;
    let mut findings = Findings::new(16);
    let out =
        document_from_bytes::<RunAgentInput>(body, run_input_shape(), &lenient(), &mut findings);
    let v = out
        .ok()
        .flatten()
        .map(|i| serde_json::to_value(i).unwrap_or_default());
    assert_eq!(
        v.as_ref().and_then(|v| v.get("threadId")),
        Some(&json!("t"))
    );
    assert!(super::validate_strict("RunAgentInput", body).is_err());

    // Both occurrences well-typed: strict rejection can only come from the
    // duplicate itself, and lenient still resolves it last-wins.
    let dup = br#"{"threadId":"a","threadId":"t","runId":"r","messages":[]}"#;
    let Err(e) = super::validate_strict("RunAgentInput", dup) else {
        panic!("strict accepted a repeated member")
    };
    assert!(
        e.message().contains("duplicate member `threadId`"),
        "{}",
        e.message()
    );
    let mut findings = Findings::new(16);
    let out =
        document_from_bytes::<RunAgentInput>(dup, run_input_shape(), &lenient(), &mut findings);
    let v = out
        .ok()
        .flatten()
        .map(|i| serde_json::to_value(i).unwrap_or_default());
    assert_eq!(
        v.as_ref().and_then(|v| v.get("threadId")),
        Some(&json!("t"))
    );
}

// ---- the upstream corpora ------------------------------------------------------------------

/// The pinned spec cache: `AGUI_REQUIRE_SPEC=1` and the `.commit` stamp are
/// honoured, as everywhere else in the suite.
fn spec_root() -> Option<PathBuf> {
    crate::test_spec::root()
}

/// A corpus document; `Null` when it does not exist (an optional
/// `.expect.json`). A file that exists but cannot be read or parsed fails the
/// test rather than reading as an empty expectation.
fn read_json(path: &std::path::Path) -> Value {
    if !path.exists() {
        return Value::Null;
    }
    let text =
        std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    serde_json::from_slice(&text)
        .unwrap_or_else(|e| panic!("{} is not valid JSON: {e}", path.display()))
}

struct ValueDoc {
    anchor: String,
    valid: bool,
    path: PathBuf,
    expect: Value,
}

fn value_corpus() -> Option<Vec<ValueDoc>> {
    let root = spec_root()?.join("spec/1.0/fixtures");
    let manifest_path = root.join("MANIFEST.txt");
    let manifest = std::fs::read_to_string(&manifest_path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest_path.display()));
    let mut docs = Vec::new();
    for line in manifest.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.ends_with(".expect.json") {
            continue;
        }
        let mut parts = line.splitn(3, '/');
        let (Some(anchor), Some(class)) = (parts.next(), parts.next()) else {
            continue;
        };
        let path = root.join(line);
        let expect = read_json(&path.with_extension("expect.json"));
        docs.push(ValueDoc {
            anchor: anchor.to_owned(),
            valid: class == "valid",
            path,
            expect,
        });
    }
    Some(docs)
}

/// The value lane, gate A: strict validation accepts every valid document
/// and rejects every invalid one, at the location the corpus names.
/// covers: SCHF-01
#[test]
fn value_corpus_gate_a_strict() {
    let Some(docs) = value_corpus() else { return };
    assert!(docs.len() > 200, "{}", docs.len());
    for doc in &docs {
        let bytes = std::fs::read(&doc.path).unwrap_or_default();
        let shape = shape::anchor(&doc.anchor);
        assert!(shape.is_some(), "{}", doc.anchor);
        let shape = shape.unwrap_or(Shape::Any);
        let mut f = Findings::new(0);
        let r = document_from_bytes::<IgnoredAny>(&bytes, shape, &Options::strict(), &mut f);
        if doc.valid {
            assert!(r.is_ok(), "{}: {r:?}", doc.path.display());
        } else {
            let at = doc.expect["instanceLocation"].as_str().unwrap_or("");
            match r {
                Err(fault) => assert!(
                    fault.pointer.starts_with(at),
                    "{}: fault at {} but corpus says {at}: {}",
                    doc.path.display(),
                    fault.pointer,
                    fault.message
                ),
                Ok(_) => panic!("{} accepted", doc.path.display()),
            }
        }
    }
}

/// The value lane, gate B: the consumer treatment keeps valid documents
/// unchanged and quiet, strips documents whose only defect is unrecognised
/// material, and rejects the rest.
/// covers: PROC-15, PROC-37
#[test]
fn value_corpus_gate_b_consumer() {
    let Some(docs) = value_corpus() else { return };
    let opts = Options::lenient(false);
    let (mut stripped, mut rejected) = (0, 0);
    for doc in &docs {
        let value = read_json(&doc.path);
        let shape = shape::anchor(&doc.anchor).unwrap_or(Shape::Any);
        let mut findings = Findings::new(32);
        let mut walked = value.clone();
        let verdict = Tree::new(&opts, &mut findings).root(shape, &mut walked);
        // The byte path must reach the same verdict.
        let bytes = std::fs::read(&doc.path).unwrap_or_default();
        let mut f2 = Findings::new(32);
        let streamed = document_from_bytes::<Value>(&bytes, shape, &opts, &mut f2);
        if doc.valid {
            assert_eq!(verdict, Ok(tree::Verdict::Keep), "{}", doc.path.display());
            assert_eq!(walked, value, "{}", doc.path.display());
            assert!(
                findings.is_empty(),
                "{}: {:?}",
                doc.path.display(),
                findings.items
            );
            assert_eq!(streamed.ok().flatten().as_ref(), Some(&value));
            continue;
        }
        let keyword = doc.expect["keyword"].as_str().unwrap_or("");
        match verdict {
            Err(_) => {
                rejected += 1;
                assert!(streamed.is_err(), "{}", doc.path.display());
                assert!(
                    !matches!(keyword, "unevaluatedProperties" | "enum"),
                    "{} is unrecognised material only, but was rejected",
                    doc.path.display()
                );
            }
            Ok(verdict) => {
                stripped += 1;
                // A dropped root is reported by the caller, not the walk.
                assert!(
                    !findings.is_empty() || matches!(verdict, tree::Verdict::Drop(_)),
                    "{} accepted silently",
                    doc.path.display()
                );
                assert!(
                    matches!(
                        keyword,
                        "unevaluatedProperties" | "enum" | "oneOf" | "const"
                    ),
                    "{} ({keyword}) is malformed, but was accepted",
                    doc.path.display()
                );
                assert!(streamed.is_ok(), "{}: {streamed:?}", doc.path.display());
            }
        }
    }
    eprintln!(
        "value corpus: {} documents, {stripped} stripped, {rejected} rejected",
        docs.len()
    );
}

/// The stream lane, enforcement's share: each fixture's events through the
/// byte path (with the null shim, as on receive). Fixtures whose rule is an
/// enforcement verdict are checked for it; quiet fixtures must draw no
/// enforcement diagnostic; nothing else may fail here.
/// covers: PROC-01, PROC-02, PROC-03, PROC-04, PROC-15, PROC-20, PROC-37
#[test]
fn stream_corpus_enforcement_verdicts() {
    let Some(root) = spec_root() else { return };
    let dir = root.join("spec/1.0/conformance/streams");
    let Ok(listing) = std::fs::read_to_string(dir.join("MANIFEST.txt")) else {
        panic!("no stream MANIFEST.txt under {}", dir.display())
    };
    let fatal_here = [
        "malformed-known-field-fatal",
        "unknown-outcome-array-fatal",
        "subagent-started-missing-name-fatal",
        "reasoning-message-wrong-role-fatal",
    ];
    let mut seen = 0;
    for name in listing
        .lines()
        .map(str::trim)
        .filter(|l| l.ends_with(".json"))
    {
        seen += 1;
        let fixture = read_json(&dir.join(name));
        let name = name.trim_end_matches(".json");
        let expect = &fixture["expect"];
        let mut fault = None;
        let mut findings: Vec<Finding> = Vec::new();
        let mut delivered = Vec::new();
        for event in fixture["stream"].as_array().into_iter().flatten() {
            let (out, f) = enforce(event.clone());
            findings.extend(f);
            match out {
                Ok(Some(v)) => delivered.push(v),
                Ok(None) => {}
                Err(e) => {
                    fault = Some(e);
                    break;
                }
            }
        }
        let warnings: Vec<String> = findings
            .iter()
            .map(|f| format!("{} {}", f.pointer, f.message))
            .collect();
        if fatal_here.contains(&name) {
            let Some(fault) = fault else {
                panic!("{name}: expected an enforcement fault")
            };
            let needle = expect["errorContains"].as_str().unwrap_or("");
            assert!(
                fault.message.contains(needle),
                "{name}: {:?} lacks {needle:?}",
                fault.message
            );
            continue;
        }
        assert!(fault.is_none(), "{name}: unexpected fault {fault:?}");
        if name == "unknown-enum-value-role-fatal" {
            // D3: the local override. The spec says strip; so do we.
            assert!(
                warnings.iter().any(|w| w.contains("/role")),
                "{name}: {warnings:?}"
            );
            continue;
        }
        if name.starts_with("era-") {
            // Retired shapes: the compatibility boundary's, before enforcement.
            continue;
        }
        if expect["noWarnings"] == json!(true) {
            assert!(findings.is_empty(), "{name}: {warnings:?}");
        }
        if matches!(
            name,
            "unknown-property-stripped"
                | "unknown-event-dropped"
                | "unknown-outcome-stripped"
                | "unknown-part-in-message-list"
                | "state-delta-unknown-op-dropped"
        ) {
            for w in expect["warnings"].as_array().into_iter().flatten() {
                let w = w.as_str().unwrap_or("");
                assert!(
                    warnings.iter().any(|x| x.contains(w)),
                    "{name}: {w} not in {warnings:?}"
                );
            }
            for t in expect["eventTypesAbsent"].as_array().into_iter().flatten() {
                assert!(
                    delivered.iter().all(|e| &e["type"] != t),
                    "{name}: {t} delivered"
                );
            }
        }
    }
    assert!(seen >= 60, "only {seen} fixtures");
}

// ---- repeated members ----------------------------------------------------

/// A repeated member is resolved the way `JSON.parse` does, the last
/// occurrence winning, whichever path the frame takes and whether or not the
/// first occurrence was itself well formed.
///
/// covers: PROC-06
#[test]
fn a_repeated_member_gets_one_verdict_on_both_paths() {
    type Check = fn(&Event) -> bool;
    let frames: [(&str, Check); 12] = [
        (
            r#"{"type":"RUN_STARTED","threadId":"t","runId":"a","runId":"b"}"#,
            |e| matches!(e, Event::RunStarted(r) if r.run_id.as_str() == "b"),
        ),
        (
            r#"{"type":"TEXT_MESSAGE_END","messageId":"m","type":"STEP_STARTED","stepName":"s"}"#,
            |e| matches!(e, Event::StepStarted(_)),
        ),
        (
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"u","role":"user","content":"x","content":"y"}]}"#,
            |e| serde_json::to_string(e).is_ok_and(|s| s.contains(r#""content":"y""#)),
        ),
        (
            r#"{"messageId":"m","type":"TEXT_MESSAGE_END","type":"TEXT_MESSAGE_END"}"#,
            |e| matches!(e, Event::TextMessageEnd(_)),
        ),
        (
            r#"{"type":"RUN_STARTED","threadId":"t","runId":5,"runId":"b"}"#,
            |e| matches!(e, Event::RunStarted(r) if r.run_id.as_str() == "b"),
        ),
        (
            r#"{"type":"STEP_STARTED","stepName":5,"type":"RUN_STARTED","threadId":"t","runId":"r"}"#,
            |e| matches!(e, Event::RunStarted(_)),
        ),
        (
            r#"{"type":5,"type":"RUN_STARTED","threadId":"t","runId":"r"}"#,
            |e| matches!(e, Event::RunStarted(_)),
        ),
        (
            r#"{"type":"RUN_STARTED","threadId":"t","runId":null,"runId":"b"}"#,
            |e| matches!(e, Event::RunStarted(r) if r.run_id.as_str() == "b"),
        ),
        (
            r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t","runId":"r"}"#,
            |e| matches!(e, Event::RunStarted(_)),
        ),
        (
            r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t","runId":"r","x":null}"#,
            |e| matches!(e, Event::RunStarted(_)),
        ),
        (
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"u","role":"zzz","role":"user","content":"x"}]}"#,
            |e| serde_json::to_string(e).is_ok_and(|s| s.contains(r#""role":"user""#)),
        ),
        (
            r#"{"type":"MESSAGES_SNAPSHOT","messages":[{"id":"u","role":"zzz","role":"user","content":"x","x":null}]}"#,
            |e| serde_json::to_string(e).is_ok_and(|s| s.contains(r#""role":"user""#)),
        ),
    ];
    for (text, check) in frames {
        let opts = lenient();
        let mut f = Findings::new(opts.max_findings);
        let bytes = event_from_bytes::<Event>(text.as_bytes(), &opts, &mut f).map(|o| o.0);
        let mut f = Findings::new(opts.max_findings);
        let value = parse_value(text.as_bytes(), &opts)
            .and_then(|v| event_from_value::<Event>(v, &opts, &mut f))
            .map(|o| o.0);
        let value = value.unwrap_or_else(|e| panic!("value path rejects {text}: {e:?}"));
        let bytes = bytes.unwrap_or_else(|e| panic!("byte path rejects {text}: {e:?}"));
        assert_eq!(bytes, value, "paths disagree on {text}");
        assert!(bytes.as_ref().is_some_and(check), "{text}: {bytes:?}");
    }
}

/// A repeated unknown root discriminator on an otherwise invalid frame is
/// judged by the value walk, so the verdict never names an internal reroute
/// and does not depend on the path taken.
#[test]
fn a_repeated_root_tag_on_an_invalid_frame_has_one_message() {
    for text in [
        r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t"}"#,
        r#"{"type":"FUTURE_EVENT","type":"RUN_STARTED","threadId":"t","x":null}"#,
    ] {
        let opts = lenient();
        let mut f = Findings::new(opts.max_findings);
        let bytes = event_from_bytes::<Event>(text.as_bytes(), &opts, &mut f).map(|o| o.0);
        let mut f = Findings::new(opts.max_findings);
        let value = parse_value(text.as_bytes(), &opts)
            .and_then(|v| event_from_value::<Event>(v, &opts, &mut f))
            .map(|o| o.0);
        let (Err(b), Err(v)) = (bytes, value) else {
            panic!("{text} should be rejected");
        };
        assert!(!format!("{b:?}").contains("internal"), "{text}: {b:?}");
        assert_eq!(format!("{b:?}"), format!("{v:?}"), "{text}");
    }
}

// ---- discriminator order -------------------------------------------------

/// A discriminator that is not the first member is presented first to the
/// typed visitor: the event is the same as with the tag in front, a repeated
/// discriminator is last-wins like every repeated member (strict validation
/// still rejects it), and the members before it are still enforced.
#[test]
fn a_late_discriminator_builds_the_same_event() {
    let parse = |text: &str| {
        let mut f = Findings::new(16);
        let out = event_from_bytes::<Event>(text.as_bytes(), &lenient(), &mut f).map(|o| o.0);
        (out, f.items.len())
    };
    let first = parse(r#"{"type":"TEXT_MESSAGE_CONTENT","messageId":"m","delta":"a\"b"}"#);
    let last = parse(r#"{"messageId":"m","delta":"a\"b","type":"TEXT_MESSAGE_CONTENT"}"#);
    assert!(matches!(&first.0, Ok(Some(_))));
    assert_eq!(first.0.ok(), last.0.ok());

    let duplicate =
        parse(r#"{"messageId":"m","type":"TEXT_MESSAGE_END","type":"TEXT_MESSAGE_END"}"#);
    assert!(
        matches!(duplicate.0, Ok(Some(Event::TextMessageEnd(_)))),
        "a repeated discriminator is last-wins: {:?}",
        duplicate.0
    );
    assert!(validate_strict(
        "TextMessageEndEvent",
        br#"{"messageId":"m","type":"TEXT_MESSAGE_END","type":"TEXT_MESSAGE_END"}"#
    )
    .is_err());

    let (stripped, findings) = parse(r#"{"acme":1,"messageId":"m","type":"TEXT_MESSAGE_END"}"#);
    assert!(matches!(stripped, Ok(Some(_))));
    assert_eq!(
        findings, 1,
        "the unknown member before the tag is still reported"
    );

    // Nested unions too: a message whose `role` comes after `id`.
    let snapshot = parse(
        r#"{"messages":[{"content":"hi","id":"u1","role":"user"}],"type":"MESSAGES_SNAPSHOT"}"#,
    );
    assert!(matches!(snapshot.0, Ok(Some(Event::MessagesSnapshot(_)))));
}

/// The SWAR scan stops exactly where the byte-wise rule does.
#[test]
fn plain_run_matches_the_bytewise_scan() {
    let bytewise = |b: &[u8], i: usize| {
        b[i..]
            .iter()
            .position(|&c| c == b'"' || c == b'\\' || c < 0x20)
            .map_or(b.len(), |p| i + p)
    };
    let mut text: Vec<u8> = (0u8..=255).collect();
    text.extend_from_slice("héllo wörld \u{1F600} plain ascii run".as_bytes());
    for stop in [b'"', b'\\', b'\n', 0x00, 0x1F] {
        for at in 0..text.len() {
            let mut b = text
                .iter()
                .map(|&c| {
                    if c < 0x20 || c == b'"' || c == b'\\' {
                        b'x'
                    } else {
                        c
                    }
                })
                .collect::<Vec<u8>>();
            b[at] = stop;
            for start in [0, 1, 7, 8, 9].into_iter().filter(|&s| s <= at) {
                assert_eq!(
                    stream::plain_run(&b, start),
                    bytewise(&b, start),
                    "stop {stop} at {at} from {start}"
                );
            }
        }
    }
    assert_eq!(stream::plain_run(b"no terminator here", 0), 18);
}
