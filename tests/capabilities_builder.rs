//! Capabilities, producer side (`basic/capabilities.mdx`): building a
//! declaration in the schema's shape.

// Test code: a failed unwrap is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use agui_sdk::capability::{
    self, CapabilitiesBuilder, ExecutionCapabilities, ReasoningCapabilities, SubagentInfo,
    TransportCapabilities,
};
use agui_sdk::tool::Tool;
use agui_sdk::ErrorCode;
use serde_json::{json, Value};

/// covers: CAP-01
#[test]
fn an_empty_declaration_is_valid() {
    let caps = CapabilitiesBuilder::new().build().unwrap();
    assert_eq!(serde_json::to_value(&caps).unwrap(), json!({}));
}

/// covers: CAP-03
#[test]
fn the_builder_declares_only_what_the_author_sets() {
    // Nothing is invented: no transport flags, no defaults. The flags that
    // describe the endpoint are derived by the server, not declared here.
    let caps = CapabilitiesBuilder::new()
        .with(|c| c.reasoning = Some(ReasoningCapabilities::default().with_streaming(true)))
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&caps).unwrap(),
        json!({"reasoning": {"streaming": true}})
    );
    assert!(caps.transport.is_none());
    let kept = CapabilitiesBuilder::new()
        .with(|c| c.transport = Some(TransportCapabilities::default().with_websocket(false)))
        .build()
        .unwrap();
    assert_eq!(kept.transport.unwrap().websocket, Some(false));
}

/// What the endpoint serves for `caps`, through `server::http::capabilities_response`.
#[cfg(feature = "server")]
fn served(caps: capability::AgentCapabilities, protobuf: bool) -> Value {
    let mut config = agui_sdk::server::http::ServeConfig::default();
    config.protobuf = protobuf;
    config.capabilities = Some(caps);
    let body = agui_sdk::server::http::capabilities_response(&config).unwrap();
    serde_json::from_str(&body).unwrap()
}

/// covers: CAP-03
#[cfg(feature = "server")]
#[test]
fn the_endpoint_flags_are_what_the_endpoint_serves_not_what_was_declared() {
    // The author claims the opposite of what this endpoint does; the served
    // declaration says what it actually does (SSE always, protobuf only when
    // offered and built), and keeps the rest of the author's claim.
    let claimed = || {
        CapabilitiesBuilder::new()
            .with(|c| {
                c.transport = Some(
                    TransportCapabilities::default()
                        .with_streaming(false)
                        .with_http_binary(true)
                        .with_websocket(false),
                );
                c.reasoning = Some(ReasoningCapabilities::default().with_streaming(true));
            })
            .build()
            .unwrap()
    };
    let sse_only = served(claimed(), false);
    assert_eq!(sse_only["transport"]["streaming"], json!(true));
    assert_eq!(sse_only["transport"]["httpBinary"], json!(false));
    assert_eq!(sse_only["transport"]["websocket"], json!(false));
    assert_eq!(sse_only["reasoning"], json!({"streaming": true}));
    let offered = served(claimed(), true);
    assert_eq!(
        offered["transport"]["httpBinary"],
        json!(cfg!(feature = "proto"))
    );
    // Nothing declared about transport: the endpoint still says what it is.
    let bare = served(CapabilitiesBuilder::new().build().unwrap(), false);
    assert_eq!(
        bare["transport"],
        json!({"streaming": true, "httpBinary": false})
    );
}

/// covers: CAP-05
#[test]
fn agent_tools_are_declared_as_items() {
    let caps = CapabilitiesBuilder::new()
        .agent_tools([
            Tool::new("search", "Search the web").with_parameters(json!({"type": "object"}))
        ])
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&caps).unwrap(),
        json!({"tools": {"items": [{"name": "search", "description": "Search the web", "parameters": {"type": "object"}}]}})
    );
}

/// covers: CAP-09
#[test]
fn resumable_and_push_notifications_may_be_declared() {
    let caps = CapabilitiesBuilder::new()
        .with(|c| {
            c.transport = Some(
                TransportCapabilities::default()
                    .with_resumable(true)
                    .with_push_notifications(true),
            )
        })
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&caps).unwrap(),
        json!({"transport": {"pushNotifications": true, "resumable": true}})
    );
    // The endpoint serves them as declared beside the flags it derives.
    #[cfg(feature = "server")]
    {
        let body = served(caps, false);
        assert_eq!(body["transport"]["resumable"], json!(true));
        assert_eq!(body["transport"]["pushNotifications"], json!(true));
    }
}

/// covers: CAP-12
#[test]
fn built_declarations_have_the_schema_shape() {
    let caps = CapabilitiesBuilder::new()
        .subagents([SubagentInfo::new("researcher").with_description("Gathers sources.")])
        .with(|c| {
            c.reasoning = Some(ReasoningCapabilities::default().with_supported(true));
            c.execution = Some(ExecutionCapabilities::default().with_max_iterations(25));
        })
        .build()
        .unwrap();
    let value: Value = serde_json::to_value(&caps).unwrap();
    assert_eq!(
        value,
        json!({
            "multiAgent": {"subagents": [{"name": "researcher", "description": "Gathers sources."}]},
            "reasoning": {"supported": true},
            "execution": {"maxIterations": 25}
        })
    );
    // What the builder emits, a consumer ingests unchanged.
    let ingested = capability::ingest(value.clone()).unwrap();
    assert!(ingested.diagnostics.is_empty());
    assert_eq!(ingested.capabilities, caps);

    // Edits the schema rejects are caught at build time.
    let too_big = CapabilitiesBuilder::new()
        .with(|c| {
            c.execution = Some(ExecutionCapabilities::default().with_max_iterations(u64::MAX))
        })
        .build()
        .unwrap_err();
    assert_eq!(too_big.code(), ErrorCode::CapabilitiesRejected);
    assert_eq!(too_big.pointer(), Some("/execution/maxIterations"));
    // A `null` open-JSON value is absence, never written as `null`.
    let null_parameters = CapabilitiesBuilder::new()
        .agent_tools([Tool::new("x", "y").with_parameters(Value::Null)])
        .build()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&null_parameters).unwrap(),
        json!({"tools": {"items": [{"name": "x", "description": "y"}]}})
    );
}
