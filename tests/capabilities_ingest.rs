//! Capabilities, consumer side (`basic/capabilities.mdx`): reading and
//! validating a received declaration.

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Mutex;

use agui_sdk::capability::{self, AgentCapabilities, Support};
use agui_sdk::diagnostics::{Diagnostic, DiagnosticCode};
use agui_sdk::event::{Event, EventType};
use agui_sdk::ErrorCode;
use serde_json::{json, Value};

fn ingest_ok(value: Value) -> capability::Ingested {
    capability::ingest(value).expect("declaration accepted")
}

fn rejected_at(value: Value) -> String {
    let error = capability::ingest(value).expect_err("declaration rejected");
    assert_eq!(error.code(), ErrorCode::CapabilitiesRejected);
    error.pointer().unwrap_or_default().to_owned()
}

/// covers: CAP-01
#[test]
fn every_group_and_field_is_optional() {
    let empty = ingest_ok(json!({}));
    assert_eq!(empty.capabilities, AgentCapabilities::default());
    assert!(empty.diagnostics.is_empty());
    let groups = ingest_ok(json!({
        "identity": {}, "transport": {}, "tools": {}, "output": {}, "state": {},
        "multiAgent": {}, "reasoning": {}, "multimodal": {"input": {}, "output": {}},
        "execution": {}, "humanInTheLoop": {}, "custom": {}
    }));
    assert!(groups.diagnostics.is_empty());
    assert!(groups.capabilities.reasoning.is_some());
}

/// covers: CAP-02
#[test]
fn omitted_means_undeclared_not_unsupported() {
    let caps = ingest_ok(json!({"reasoning": {"supported": false}})).capabilities;
    let reasoning = caps.reasoning.unwrap_or_default();
    assert_eq!(Support::of(reasoning.supported), Support::Unsupported);
    assert_eq!(Support::of(reasoning.streaming), Support::Undeclared);
    assert_eq!(Support::of(Some(true)), Support::Supported);
    assert_eq!(Support::of(None), Support::Undeclared);
    assert!(caps.human_in_the_loop.is_none());
}

/// covers: CAP-04
#[test]
fn declarations_never_gate_the_event_stream() {
    // An agent that says it does not reason may still send reasoning events;
    // parsing them owes nothing to the declaration.
    let caps = ingest_ok(json!({"reasoning": {"supported": false}, "state": {"deltas": false}}));
    assert_eq!(
        Support::of(caps.capabilities.reasoning.and_then(|r| r.supported)),
        Support::Unsupported
    );
    for doc in [
        json!({"type": "REASONING_START", "messageId": "n1"}),
        json!({"type": "STATE_DELTA", "delta": []}),
    ] {
        assert!(serde_json::from_value::<Event>(doc).is_ok());
    }
}

/// covers: CAP-05
#[test]
fn agent_tools_stay_apart_from_input_tools() {
    let caps =
        ingest_ok(json!({"tools": {"items": [{"name": "search", "description": "Search"}]}}))
            .capabilities;
    let items = caps.tools.and_then(|t| t.items).unwrap_or_default();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].name, "search");
    let input = agui_sdk::RunAgentInput::new("t", "r", Vec::new());
    assert!(
        input.tools().is_empty() && input.tools.is_none(),
        "nothing merges declared tools into run input"
    );
}

/// covers: CAP-07
#[test]
fn structured_output_is_a_declaration_only() {
    let caps = ingest_ok(json!({"output": {"structuredOutput": true}})).capabilities;
    assert_eq!(caps.output.and_then(|o| o.structured_output), Some(true));
    // No standard event has a structured-output member to carry.
    let text: Event = serde_json::from_value(json!({
        "type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "{}", "structuredOutput": true
    }))
    .unwrap();
    let mut out = Vec::new();
    text.write_json(&mut out);
    assert!(!String::from_utf8(out).unwrap().contains("structuredOutput"));
}

/// covers: CAP-08
#[test]
fn multimodal_output_is_a_declaration_only() {
    let caps =
        ingest_ok(json!({"multimodal": {"output": {"image": true, "audio": true}}})).capabilities;
    let output = caps.multimodal.and_then(|m| m.output).unwrap_or_default();
    assert_eq!(Support::of(output.image), Support::Supported);
    // Text deltas stay text; there is no media member on any standard event.
    let content: Event = serde_json::from_value(
        json!({"type": "TEXT_MESSAGE_CONTENT", "messageId": "m", "delta": "x"}),
    )
    .unwrap();
    assert_eq!(content.event_type(), EventType::TextMessageContent);
}

/// covers: CAP-10
#[test]
fn resumable_and_push_notifications_are_flags_only() {
    let caps = ingest_ok(json!({"transport": {"resumable": true, "pushNotifications": true}}))
        .capabilities;
    let transport = caps.transport.unwrap_or_default();
    assert_eq!(Support::of(transport.resumable), Support::Supported);
    assert_eq!(
        Support::of(transport.push_notifications),
        Support::Supported
    );
    // Reading them changes nothing else in the declaration.
    assert_eq!(transport.streaming, None);
    assert_eq!(transport.http_binary, None);
}

/// covers: CAP-11
#[test]
fn open_objects_keep_unrecognised_members() {
    let doc = json!({
        "identity": {"name": "a", "metadata": {"region": null, "acme.tier": 2, "deep": {"x": [1]}}},
        "custom": {"vendor:feature": true, "nested": {"a": null}}
    });
    let ingested = ingest_ok(doc.clone());
    assert!(ingested.diagnostics.is_empty());
    assert_eq!(serde_json::to_value(&ingested.capabilities).unwrap(), doc);
}

/// covers: CAP-12
#[test]
fn declarations_use_the_schema_shape() {
    let Some(documents) = support::value_documents(true) else {
        return;
    };
    for doc in documents.iter().filter(|d| d.anchor == "AgentCapabilities") {
        let ingested = capability::ingest_slice(&doc.bytes).expect(&doc.relative);
        assert!(ingested.diagnostics.is_empty(), "{}", doc.relative);
        let original: Value = serde_json::from_slice(&doc.bytes).unwrap();
        assert_eq!(
            serde_json::to_value(&ingested.capabilities).unwrap(),
            original,
            "{}",
            doc.relative
        );
    }
}

/// covers: CAP-13
#[test]
fn unrecognised_groups_and_fields_do_not_reject() {
    let ingested = ingest_ok(json!({
        "teleportation": {"supported": true},
        "transport": {"streaming": true, "quantum": "yes"}
    }));
    assert_eq!(
        ingested.capabilities.transport.and_then(|t| t.streaming),
        Some(true)
    );
    let pointers: Vec<&str> = ingested
        .diagnostics
        .iter()
        .map(|d| d.pointer.as_str())
        .collect();
    assert_eq!(pointers, ["/teleportation", "/transport/quantum"]);
}

/// covers: CAP-14
#[test]
fn unknown_members_are_dropped_and_listed() {
    let ingested = ingest_ok(json!({"execution": {"maxIterations": 3, "gpu": true}}));
    let back = serde_json::to_value(&ingested.capabilities).unwrap();
    assert_eq!(back, json!({"execution": {"maxIterations": 3}}));
    assert_eq!(ingested.diagnostics.len(), 1);
    assert_eq!(ingested.diagnostics[0].code, DiagnosticCode::UnknownField);
}

/// covers: CAP-15
#[test]
fn malformed_known_fields_reject_the_declaration() {
    assert_eq!(
        rejected_at(json!({"transport": {"streaming": "yes"}})),
        "/transport/streaming"
    );
    assert_eq!(
        rejected_at(json!({"execution": {"maxIterations": 2.5}})),
        "/execution/maxIterations"
    );
    assert_eq!(
        rejected_at(json!({"execution": {"maxIterations": 9_007_199_254_740_992_u64}})),
        "/execution/maxIterations"
    );
    assert_eq!(
        rejected_at(json!({"execution": {"maxExecutionTime": -1}})),
        "/execution/maxExecutionTime"
    );
    assert_eq!(rejected_at(json!({"custom": null})), "/custom");
    assert_eq!(
        rejected_at(json!({"identity": {"metadata": null}})),
        "/identity/metadata"
    );
    assert_eq!(
        rejected_at(json!({"multiAgent": {"subagents": [{"description": "no name"}]}})),
        "/multiAgent/subagents/0/name"
    );
    assert_eq!(
        rejected_at(json!({"tools": {"items": [{"name": "x"}]}})),
        "/tools/items/0/description"
    );
    assert_eq!(
        rejected_at(
            json!({"tools": {"items": [{"name": "x", "description": "d", "parameters": null}]}})
        ),
        "/tools/items/0/parameters"
    );
    assert_eq!(
        rejected_at(json!({"output": {"supportedMimeTypes": ["a", 1]}})),
        "/output/supportedMimeTypes"
    );
    assert_eq!(rejected_at(json!([])), "");
    // Limits at the maximum are fine; an integral float is an integer.
    let ok = ingest_ok(
        json!({"execution": {"maxIterations": 9_007_199_254_740_991_u64, "maxExecutionTime": 5.0}}),
    );
    assert_eq!(
        ok.capabilities.execution.and_then(|e| e.max_execution_time),
        Some(5)
    );

    // The upstream invalid documents for capability anchors are all rejected,
    // except those whose only defect is an unrecognised member (CAP-13).
    let Some(documents) = support::value_documents(false) else {
        return;
    };
    for doc in documents.iter().filter(|d| d.anchor == "AgentCapabilities") {
        let result = capability::ingest_slice(&doc.bytes);
        if doc.relative.contains("old-subagents-key") {
            assert!(result.is_ok(), "{}", doc.relative);
        } else {
            assert!(result.is_err(), "{}", doc.relative);
        }
    }
}

/// covers: CAP-16
#[test]
fn rejected_declaration_falls_back_to_undeclared() {
    let seen: Mutex<Vec<Diagnostic>> = Mutex::new(Vec::new());
    let sink = |d: &Diagnostic| seen.lock().unwrap().push(d.clone());
    let caps = capability::ingest_or_undeclared(json!({"transport": {"streaming": 1}}), &sink);
    assert_eq!(caps, AgentCapabilities::default());
    let seen = seen.into_inner().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].code, DiagnosticCode::CapabilitiesIgnored);
    assert_eq!(seen[0].pointer, "/transport/streaming");

    let quiet = |_: &Diagnostic| panic!("no report for an acceptable declaration");
    let caps = capability::ingest_or_undeclared(json!({"state": {"snapshots": true}}), &quiet);
    assert_eq!(caps.state.and_then(|s| s.snapshots), Some(true));

    // Bytes that are not JSON are a peer declaration the consumer cannot
    // validate (rejected, CAP-16), not an SDK-side JSON failure.
    for bytes in [&b"{not json"[..], b"<html>"] {
        let not_json = capability::ingest_slice(bytes).expect_err("bytes are not JSON");
        assert_eq!(not_json.code(), ErrorCode::CapabilitiesRejected);
    }
}

/// covers: CHG-05
#[test]
fn old_sub_agents_key_is_not_read() {
    let ingested = ingest_ok(
        json!({"multiAgent": {"supported": true, "subAgents": [{"name": "researcher"}]}}),
    );
    let multi = ingested.capabilities.multi_agent.unwrap_or_default();
    assert_eq!(multi.subagents, None);
    assert_eq!(multi.supported, Some(true));
    assert_eq!(ingested.diagnostics.len(), 1);
    assert_eq!(ingested.diagnostics[0].pointer, "/multiAgent/subAgents");
}

/// covers: CAP-01
#[test]
fn unknown_member_diagnostics_are_capped() {
    let mut group = serde_json::Map::new();
    for i in 0..1000 {
        group.insert(format!("k{i}"), json!(1));
    }
    let done = ingest_ok(json!({"transport": Value::Object(group)}));
    let cap = agui_sdk::diagnostics::DEFAULT_MAX_PER_EVENT;
    assert_eq!(done.diagnostics.len(), cap + 1);
    let last = done.diagnostics.last().unwrap();
    assert_eq!(last.code, DiagnosticCode::Limit);
    assert_eq!(last.scope, agui_sdk::diagnostics::Scope::Capabilities);
    assert!(
        last.message.contains(&(1000 - cap).to_string()),
        "{}",
        last.message
    );
    assert!(!last.message.contains("per-event"), "{}", last.message);
    assert!(last.message.contains("per-declaration"), "{}", last.message);
}
