//! Outgoing run input (`basic/processing.mdx`, "Outgoing material"):
//! unrecognised material is removed with a warning immediately before
//! sending; a malformed known field is fatal before anything is sent.

use std::sync::Arc;

use agui_sdk::diagnostics::{CollectingSink, DiagnosticCode, Reporter, Scope};
use agui_sdk::pipeline::enforce::Enforcer;
use agui_sdk::pipeline::Limits;
use agui_sdk::ErrorCode;
use agui_sdk::RunAgentInput;
use serde_json::json;

fn setup() -> (Enforcer, Arc<CollectingSink>, Reporter) {
    let sink = Arc::new(CollectingSink::new());
    let reporter = Reporter::new(sink.clone(), 256);
    (Enforcer::new(Limits::default()), sink, reporter)
}

/// covers: PROC-32
#[test]
fn unrecognised_material_is_removed_before_sending() {
    let (enforcer, _sink, mut reporter) = setup();
    let input = json!({
        "threadId": "t", "runId": "r", "futureField": true,
        "messages": [{"id": "1", "role": "user", "content": "hi", "x": 1},
                     {"id": "2", "role": "narrator", "content": "?"}],
        "forwardedProps": {"keep": null}
    });
    let mut sent = input;
    assert!(
        enforcer
            .enforce_outgoing_value(&mut sent, &mut reporter)
            .is_ok(),
        "input with only unrecognised material must be sendable"
    );
    assert!(sent.get("futureField").is_none());
    assert_eq!(sent["messages"].as_array().map(Vec::len), Some(1));
    assert!(sent["messages"][0].get("x").is_none());
    assert_eq!(sent["forwardedProps"], json!({"keep": null}));
}

/// covers: PROC-33
#[test]
fn removals_from_outgoing_input_are_warned_about() {
    let (enforcer, sink, mut reporter) = setup();
    let input = json!({"threadId": "t", "runId": "r", "messages": [], "futureField": 1});
    let mut input = input;
    assert!(enforcer
        .enforce_outgoing_value(&mut input, &mut reporter)
        .is_ok());
    let d = sink.take();
    assert!(d.iter().any(|d| d.code == DiagnosticCode::UnknownField
        && d.pointer == "/futureField"
        && d.scope == Scope::RunInput));
}

/// covers: PROC-34
#[test]
fn malformed_known_input_field_is_fatal_before_sending() {
    let (enforcer, _sink, mut reporter) = setup();
    for input in [
        json!({"threadId": "t", "runId": "r"}),
        json!({"threadId": 7, "runId": "r", "messages": []}),
        json!({"threadId": "t", "runId": "r", "messages": [{"id": "1", "role": "user"}]}),
        json!({"threadId": "t", "runId": "r", "messages": [], "context": [{"value": "v"}]}),
    ] {
        let mut sent = input.clone();
        let out = enforcer.enforce_outgoing_value(&mut sent, &mut reporter);
        assert!(
            matches!(&out, Err(e) if e.code() == ErrorCode::InvalidRequest),
            "{input}"
        );
    }
}

/// covers: PROC-34
#[test]
fn typed_outgoing_input_is_checked_without_complaint() {
    let (enforcer, sink, mut reporter) = setup();
    let input: RunAgentInput = serde_json::from_value(json!({
        "threadId": "t", "runId": "r",
        "messages": [{"id": "1", "role": "user", "content": "hi"}],
        "forwardedProps": {"keep": null}
    }))
    .unwrap_or_else(|e| panic!("fixture input: {e}"));
    assert!(enforcer.check_outgoing_input(&input, &mut reporter).is_ok());
    assert!(sink.take().is_empty(), "a typed input has nothing to strip");
}

// The client's own send path: the driver checks the input and only then
// opens the transport, so "fatal before anything is sent" is observable as
// a transport that never saw a request.
#[cfg(feature = "client")]
mod support_client;

#[cfg(feature = "client")]
mod client_send {
    use std::future::IntoFuture;

    use agui_sdk::client::middleware::{MiddlewareRequest, Next};
    use agui_sdk::client::{AgentClient, RunRequest};
    use agui_sdk::diagnostics::{CollectingSink, Scope};
    use agui_sdk::pipeline::Limits;
    use agui_sdk::ErrorCode;
    use serde_json::{json, Value};

    use super::support_client::{block_on, mock, run_of};

    fn request() -> RunRequest {
        RunRequest::new().thread_id("t1").run_id("r1").user("hi")
    }

    /// covers: PROC-34
    #[test]
    fn a_malformed_known_field_from_middleware_is_fatal_and_nothing_is_sent() {
        // Middleware hands on JSON, so it can break a known field the typed
        // input could not; enforcement after the chain must refuse it.
        let cases: [(&'static str, Value); 4] = [
            ("threadId", json!(7)),
            ("runId", json!(["r1"])),
            ("messages", json!([{"id": "1", "role": "user"}])),
            ("context", json!([{"value": "v"}])),
        ];
        for (member, bad) in cases {
            let transport = mock(run_of("t1", "r1", vec![]));
            let shown = bad.clone();
            let client = AgentClient::with_transport(transport.clone())
                .diagnostic_sink(agui_sdk::diagnostics::NullSink)
                .middleware(move |mut r: MiddlewareRequest, n: Next| {
                    r.input[member] = bad.clone();
                    n.run(r)
                })
                .build()
                .unwrap_or_else(|e| panic!("{e}"));
            let out = block_on(client.run(request()).into_future());
            assert!(
                matches!(&out, Err(e) if e.code() == ErrorCode::InvalidRequest),
                "{member} = {shown}: {out:?}"
            );
            assert!(
                transport.requests().is_empty(),
                "{member} = {shown} was sent"
            );
        }
    }

    /// covers: PROC-34
    #[test]
    fn a_typed_input_is_checked_on_the_send_path_before_the_transport_opens() {
        // A conformant typed input goes out whole and without complaint.
        let sink = std::sync::Arc::new(CollectingSink::new());
        let transport = mock(run_of("t1", "r1", vec![]));
        let client = AgentClient::with_transport(transport.clone())
            .diagnostic_sink(std::sync::Arc::clone(&sink))
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
        block_on(client.run(request()).into_future()).unwrap_or_else(|e| panic!("{e}"));
        let sent = transport.last_request().unwrap_or_default();
        assert_eq!(sent["threadId"], "t1");
        assert_eq!(sent["messages"][0]["content"], "hi");
        assert!(sink.take().iter().all(|d| d.scope != Scope::RunInput));

        // A typed input cannot hold a wrongly typed known field, but it can
        // break the configured limits; the same check refuses it before the
        // transport is opened.
        let mut limits = Limits::default();
        limits.max_depth = 4;
        let transport = mock(run_of("t1", "r1", vec![]));
        let client = AgentClient::with_transport(transport.clone())
            .diagnostic_sink(agui_sdk::diagnostics::NullSink)
            .limits(limits)
            .build()
            .unwrap_or_else(|e| panic!("{e}"));
        let deep = json!({"a": {"b": {"c": {"d": {"e": 1}}}}});
        let out = block_on(client.run(request().state_value(deep)).into_future());
        assert!(
            matches!(&out, Err(e) if e.code() == ErrorCode::InvalidRequest),
            "{out:?}"
        );
        assert!(transport.requests().is_empty(), "a refused input was sent");
    }
}
