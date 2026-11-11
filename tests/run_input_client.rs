//! Run input, client side (`basic/run-input.mdx`): what `RunRequest` and
//! the client's outgoing pipeline put on the wire.

#![cfg(feature = "client")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support_client;

use std::future::IntoFuture;
use std::sync::Arc;

use agui_sdk::client::middleware::{MiddlewareRequest, Next};
use agui_sdk::client::{AgentClient, RunRequest};
use agui_sdk::content::{
    ContentPart, DataSource, FileSource, MediaPart, PartSource, TextPart, UrlSource,
};
use agui_sdk::diagnostics::{CollectingSink, NullSink};
use agui_sdk::interrupt::ResumeEntry;
use agui_sdk::message::{ActivityMessage, UserMessage};
use agui_sdk::run_input::Context;
use agui_sdk::tool::Tool;
use agui_sdk::version::{LegacyEra, PeerPin};
use agui_sdk::{ErrorCode, Message};
use serde_json::{json, Value};
use support_client::*;

/// Sends `request` and returns the body the transport received.
fn sent(request: RunRequest) -> Value {
    sent_with(request, |b| b)
}

fn sent_with(
    request: RunRequest,
    configure: impl FnOnce(
        agui_sdk::client::ClientBuilder<MockTransport>,
    ) -> agui_sdk::client::ClientBuilder<MockTransport>,
) -> Value {
    let transport = mock(run_of("t1", "r1", vec![]));
    let client =
        configure(AgentClient::with_transport(transport.clone()).diagnostic_sink(NullSink))
            .build()
            .unwrap();
    let _ = block_on(client.run(request).into_future());
    assert_eq!(transport.requests().len(), 1);
    transport.last_request().unwrap()
}

fn base() -> RunRequest {
    RunRequest::new().thread_id("t1").run_id("r1")
}

/// covers: RUNIN-01
#[test]
fn one_input_opens_each_exchange() {
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let run = client.run(base().user("hi"));
    assert!(
        transport.requests().is_empty(),
        "nothing before the first poll"
    );
    block_on(run.into_future()).unwrap();
    assert_eq!(transport.requests().len(), 1);
    block_on(client.run(base().user("again")).into_future()).unwrap();
    assert_eq!(transport.requests().len(), 2);
}

/// covers: RUNIN-03
#[test]
fn thread_and_run_ids_are_always_present() {
    let body = sent(base());
    assert_eq!(body["threadId"], "t1");
    assert_eq!(body["runId"], "r1");
    // Unset ids are minted, and the handle reports the same ones.
    let transport = mock(vec![]);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let run = client.run(RunRequest::new());
    let (thread, id) = (run.thread_id().clone(), run.run_id().clone());
    let _ = block_on(run.into_future());
    let body = transport.last_request().unwrap();
    assert_eq!(body["threadId"], thread.as_str());
    assert_eq!(body["runId"], id.as_str());
    assert_eq!(id.as_str().len(), 32);
}

/// covers: RUNIN-07
#[test]
fn parent_run_id_is_sent_only_when_set() {
    assert!(sent(base()).get("parentRunId").is_none());
    assert_eq!(sent(base().parent_run_id("r0"))["parentRunId"], "r0");
}

/// covers: RUNIN-08
#[test]
fn the_client_declares_protocol_1_0_except_to_a_legacy_peer() {
    assert_eq!(sent(base())["protocolVersion"], "1.0");
    let legacy = sent_with(base(), |b| {
        b.peer_version(PeerPin::Legacy(LegacyEra::V0_0_47))
    });
    assert!(legacy.get("protocolVersion").is_none());
    // The middleware path declares it too.
    let with_mw = sent_with(base(), |b| {
        b.middleware(|r: MiddlewareRequest, n: Next| n.run(r))
    });
    assert_eq!(with_mw["protocolVersion"], "1.0");
}

/// covers: RUNIN-09
#[test]
fn messages_are_the_whole_conversation_in_order() {
    let body = sent(
        base()
            .message(Message::system("be brief"))
            .user("one")
            .message(Message::assistant("two"))
            .user("three"),
    );
    let roles: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(roles, ["system", "user", "assistant", "user"]);
    assert_eq!(body["messages"][3]["content"], "three");
}

/// covers: RUNIN-12
#[test]
fn activity_messages_are_stripped_before_sending_but_kept_locally() {
    let activity = ActivityMessage::new("a1", "progress", agui_sdk::json::JsonObject::new());
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let report = block_on(
        client
            .run(base().user("hi").message(activity).user("bye"))
            .into_future(),
    )
    .unwrap();
    let body = transport.last_request().unwrap();
    let roles: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].clone())
        .collect();
    assert_eq!(roles, [json!("user"), json!("user")]);
    assert_eq!(report.messages.len(), 3, "the local history keeps it");
}

fn file_part(provider: &str) -> ContentPart {
    ContentPart::Document(MediaPart::new(PartSource::File(
        FileSource::new("file-abc123").with_provider(provider),
    )))
}

/// covers: RUNIN-13, RUNIN-14
#[test]
fn file_sources_are_handed_on_untouched() {
    let user = UserMessage::new(
        "u1",
        vec![ContentPart::Text(TextPart::new("see")), file_part("openai")].into(),
    );
    let body = sent(base().message(user));
    let source = &body["messages"][0]["content"][1]["source"];
    assert_eq!(
        source,
        &json!({"type": "file", "value": "file-abc123", "provider": "openai"})
    );
}

/// covers: RUNIN-18
#[test]
fn an_unknown_source_arm_is_not_carried_through() {
    let sink = Arc::new(CollectingSink::new());
    let transport = mock(run_of("t1", "r1", vec![]));
    let client = AgentClient::with_transport(transport.clone())
        .diagnostic_sink(Arc::clone(&sink))
        .middleware(|mut r: MiddlewareRequest, n: Next| {
            r.input["messages"][0]["content"][1]["source"] =
                json!({"type": "ftp", "value": "ftp://x"});
            n.run(r)
        })
        .build()
        .unwrap();
    let user = UserMessage::new(
        "u1",
        vec![ContentPart::Text(TextPart::new("see")), file_part("openai")].into(),
    );
    block_on(client.run(base().message(user)).into_future()).unwrap();
    // Sent without the part whose source arm is unknown, with a warning
    // (D3: an unknown closed-set value in a required position drops its
    // container, here the part).
    let body = transport.last_request().unwrap();
    assert!(!body.to_string().contains("ftp"), "{body}");
    assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    assert!(!sink.take().is_empty());
}

/// covers: RUNIN-19
#[test]
fn tools_go_out_with_name_description_and_schema() {
    let tool = Tool::new("pick", "Pick a file").with_parameters(json!({"type": "object"}));
    let body = sent(base().tool(tool).tools([Tool::new("noop", "Nothing")]));
    assert_eq!(
        body["tools"],
        json!([
            {"name": "pick", "description": "Pick a file", "parameters": {"type": "object"}},
            {"name": "noop", "description": "Nothing"}
        ])
    );
}

/// covers: RUNIN-21
#[test]
fn absent_lists_are_omitted_not_sent_empty() {
    let body = sent(base());
    for field in ["tools", "context", "resume", "state", "forwardedProps"] {
        assert!(body.get(field).is_none(), "{field}");
    }
    let body = sent(base().context(Context::new("locale", "en-GB")));
    assert_eq!(
        body["context"],
        json!([{"description": "locale", "value": "en-GB"}])
    );
}

/// covers: RUNIN-26
#[test]
fn forwarded_props_null_means_omit_and_inner_nulls_survive() {
    assert!(sent(base().forwarded_props_value(Value::Null))
        .get("forwardedProps")
        .is_none());
    let body = sent(base().forwarded_props(&json!({"a": null, "b": [null, 1]})));
    assert_eq!(body["forwardedProps"], json!({"a": null, "b": [null, 1]}));
    assert_eq!(
        sent(base().forwarded_props_value(json!("text")))["forwardedProps"],
        "text"
    );
}

/// covers: RUNIN-26
#[test]
fn typed_state_or_props_that_fail_to_serialize_fail_the_request() {
    struct Bad;
    impl serde::Serialize for Bad {
        fn serialize<S: serde::Serializer>(&self, _s: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("no"))
        }
    }
    let err = base().state(&Bad).into_input().unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
    let err = base().forwarded_props(&Bad).into_input().unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
    let transport = mock(vec![]);
    let client = AgentClient::with_transport(transport.clone())
        .build()
        .unwrap();
    let err = block_on(client.run(base().state(&Bad)).into_future()).unwrap_err();
    assert_eq!(err.code(), ErrorCode::InvalidRequest);
    assert!(transport.requests().is_empty());
    // State set from a value; null reads as absent (RUNIN-24).
    assert_eq!(
        sent(base().state(&json!({"x": 1})))["state"],
        json!({"x": 1})
    );
    assert!(sent(base().state_value(Value::Null)).get("state").is_none());
}

/// covers: RUNIN-27
#[test]
fn forwarded_props_reach_the_wire_unaltered_through_middleware() {
    let props = json!({"z": 1, "a": {"nested": null}, "list": [3, 2, 1]});
    let body = sent_with(base().forwarded_props_value(props.clone()), |b| {
        b.middleware(|r: MiddlewareRequest, n: Next| n.run(r))
    });
    assert_eq!(body["forwardedProps"], props);
}

/// covers: RUNIN-28
#[test]
fn resume_entries_are_sent_as_given() {
    let body = sent(
        base()
            .resume(ResumeEntry::resolved("i1", json!({"ok": true})))
            .resume(ResumeEntry::cancelled("i2")),
    );
    assert_eq!(
        body["resume"],
        json!([
            {"interruptId": "i1", "status": "resolved", "payload": {"ok": true}},
            {"interruptId": "i2", "status": "cancelled"}
        ])
    );
}

/// covers: CHG-04
#[test]
fn input_messages_carry_multimodal_parts() {
    let parts = vec![
        ContentPart::Text(TextPart::new("look")),
        ContentPart::Image(MediaPart::new(PartSource::Url(UrlSource::new(
            "https://x/i.png",
        )))),
        ContentPart::Audio(MediaPart::new(PartSource::Data(DataSource::new(
            "AAAA",
            "audio/wav",
        )))),
        ContentPart::Video(MediaPart::new(PartSource::Url(
            UrlSource::new("https://x/v.mp4").with_mime_type("video/mp4"),
        ))),
        file_part("anthropic"),
    ];
    let body = sent(base().user(parts));
    let types: Vec<_> = body["messages"][0]["content"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(types, ["text", "image", "audio", "video", "document"]);
    assert_eq!(
        body["messages"][0]["content"][2]["source"]["mimeType"],
        "audio/wav"
    );
}
